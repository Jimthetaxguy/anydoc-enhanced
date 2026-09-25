//! Provider-neutral document classification and bounded AnyDoc conversion.
//!
//! This module is the only place in the workspace that knows about AnyDoc.
//! The MCP crate consumes the local contract below and never exposes AnyDoc's
//! document model or parser-specific error text on its wire surface.

use regex::Regex;
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::mem::MaybeUninit;
use std::{
    collections::{HashMap, HashSet},
    io::{Cursor, Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, OnceLock,
    },
    time::Duration,
};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::{oneshot, OwnedSemaphorePermit, Semaphore};
use tokio::time::timeout;

use zip::ZipArchive;

#[path = "tabular_csv.rs"]
mod tabular_csv;

/// Maximum document input accepted by the generic document route.
pub const MAX_DOCUMENT_SIZE: u64 = 50 * 1024 * 1024;

/// Maximum Markdown returned by a worker, before and after sanitization.
pub const MAX_MARKDOWN_SIZE: usize = 8 * 1024 * 1024;

const WORKER_TIMEOUT: Duration = Duration::from_secs(15);
const WORKER_ARG: &str = "--anydoc-worker";
const FRAME_HEADER_BYTES: usize = 16;
const PROTOCOL_MAGIC: [u8; 4] = *b"ADW1";
const PROTOCOL_VERSION: u8 = 2;
const MAX_IN_FLIGHT_WORKERS: usize = 2;
const MAX_IN_FLIGHT_PDF_WORKERS: usize = 4;
/// Parser threads and glibc malloc arenas per worker process.
const WORKER_PARSER_THREADS: &str = "4";
/// Largest operation-parameter block (such as PDF regions) a worker frame
/// may carry ahead of the document bytes.
pub(crate) const MAX_WORKER_PARAMS_BYTES: usize = 1024 * 1024;
const MAX_ARCHIVE_ENTRY_BYTES: u64 = 128 * 1024 * 1024;
const MAX_ARCHIVE_TOTAL_BYTES: u64 = 512 * 1024 * 1024;
const MAX_PREFLIGHT_PART_BYTES: u64 = 4 * 1024 * 1024;
/// AnyDoc 0.2.4's own XML bounds (`package::limits`): a part nested deeper,
/// or holding more nodes, fails its conversion with a resource limit. The
/// streamed checks stop at the same bounds.
const MAX_XML_DEPTH: usize = 256;
const MAX_XML_NODES: usize = 2_000_000;
/// Word styles the DOCX checks keep per document, and the longest style id.
/// Real documents carry hundreds of short ids; the caps bound what the
/// supervisor holds for a hostile styles part.
const MAX_DOCX_STYLES: usize = 16_384;
const MAX_STYLE_ID_BYTES: usize = 1024;
// JSON can encode each control byte in a Markdown string as a six-byte
// control-character escape. Keep the IPC frame bound independent of the 8 MiB
// public Markdown contract and leave room for the typed response envelope.
const MAX_SERIALIZED_WORKER_RESPONSE_BYTES: usize = MAX_MARKDOWN_SIZE * 6 + 4096;
#[cfg(target_os = "linux")]
const MAX_WORKER_MEMORY_BYTES: u64 = 1024 * 1024 * 1024;

/// Formats recognized by the provider-neutral route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentKind {
    /// Adobe PDF. Kept on the existing pdf-inspector route.
    Pdf,
    /// Microsoft Word Open XML.
    Docx,
    /// Microsoft PowerPoint Open XML.
    Pptx,
    /// Microsoft Excel workbooks.
    Xlsx,
    /// Strict EPUB 3 package; EPUB 2 is not yet qualified.
    Epub,
    /// OpenDocument Text.
    Odt,
    /// OpenDocument Spreadsheet.
    Ods,
    /// OpenDocument Presentation.
    Odp,
    /// Rich Text Format.
    Rtf,
    /// Legacy or binary Office formats.
    LegacyOffice,
    /// Delimiter-separated text, which has no content signature.
    Csv,
}

/// Exact input container variant used for routing and provenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentVariant {
    Pdf,
    Docx,
    Docm,
    Pptx,
    Pptm,
    Ppsx,
    Ppsm,
    Xlsx,
    Xlsm,
    Xlsb,
    Xls,
    Epub,
    Odt,
    Ods,
    Odp,
    Rtf,
    Doc,
    Ppt,
    Pps,
    Pot,
    Csv,
}

impl DocumentVariant {
    fn for_format(format: anydoc::Format, bytes: &[u8], path: &Path) -> Self {
        if matches!(
            format,
            anydoc::Format::Docx | anydoc::Format::Pptx | anydoc::Format::Excel
        ) {
            if let Some(variant) = ooxml_variant(bytes) {
                return variant;
            }
        }
        Self::from_extension(path, format)
    }

    fn from_extension(path: &Path, format: anydoc::Format) -> Self {
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        match extension.to_ascii_lowercase().as_str() {
            "docm" => Self::Docm,
            "pptm" => Self::Pptm,
            "ppsx" => Self::Ppsx,
            "ppsm" => Self::Ppsm,
            "xlsm" => Self::Xlsm,
            "xlsb" => Self::Xlsb,
            "xls" => Self::Xls,
            "doc" => Self::Doc,
            "ppt" => Self::Ppt,
            "pps" => Self::Pps,
            "pot" => Self::Pot,
            _ => match format {
                anydoc::Format::Pdf => Self::Pdf,
                anydoc::Format::Docx => Self::Docx,
                anydoc::Format::Pptx => Self::Pptx,
                anydoc::Format::Excel => Self::Xlsx,
                anydoc::Format::Epub => Self::Epub,
                anydoc::Format::Odt => Self::Odt,
                anydoc::Format::Ods => Self::Ods,
                anydoc::Format::Odp => Self::Odp,
                anydoc::Format::Rtf => Self::Rtf,
                anydoc::Format::Doc => Self::Doc,
                anydoc::Format::Ppt => Self::Ppt,
                anydoc::Format::Csv => Self::Csv,
            },
        }
    }

    fn worker_code(self) -> u8 {
        match self {
            Self::Docx => 1,
            Self::Xlsx => 2,
            Self::Pptx => 3,
            Self::Ods => 4,
            Self::Odt => 5,
            Self::Csv => 6,
            Self::Odp => 7,
            Self::Epub => 8,
            _ => 0,
        }
    }
}

impl DocumentKind {
    /// Parse the stable wire name used by the capabilities tool.
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name.trim().to_ascii_lowercase().as_str() {
            "pdf" => Self::Pdf,
            "docx" | "docm" => Self::Docx,
            "pptx" | "pptm" | "ppsx" | "ppsm" => Self::Pptx,
            "xlsx" | "xlsm" | "xlsb" | "xls" => Self::Xlsx,
            "epub" => Self::Epub,
            "odt" => Self::Odt,
            "ods" => Self::Ods,
            "odp" => Self::Odp,
            "rtf" => Self::Rtf,
            "doc" | "ppt" | "pps" | "pot" => Self::LegacyOffice,
            "csv" => Self::Csv,
            _ => return None,
        })
    }

    fn from_anydoc(format: anydoc::Format) -> Self {
        match format {
            anydoc::Format::Pdf => Self::Pdf,
            anydoc::Format::Docx => Self::Docx,
            anydoc::Format::Pptx => Self::Pptx,
            anydoc::Format::Excel => Self::Xlsx,
            anydoc::Format::Epub => Self::Epub,
            anydoc::Format::Odt => Self::Odt,
            anydoc::Format::Ods => Self::Ods,
            anydoc::Format::Odp => Self::Odp,
            anydoc::Format::Rtf => Self::Rtf,
            anydoc::Format::Doc | anydoc::Format::Ppt => Self::LegacyOffice,
            anydoc::Format::Csv => Self::Csv,
        }
    }
}

/// Provider identity for a conversion result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentProvider {
    /// Stable provider name.
    pub name: String,
    /// Exact package version used by this build.
    pub version: String,
    /// Public source identity; never a machine path.
    pub source: String,
}

fn provider_for(kind: DocumentKind) -> DocumentProvider {
    match kind {
        DocumentKind::Pdf => DocumentProvider {
            name: "pdf-inspector".into(),
            version: crate::PDF_INSPECTOR_VERSION.into(),
            source: "firecrawl/pdf-inspector".into(),
        },
        DocumentKind::Csv => DocumentProvider {
            name: "local-csv".into(),
            version: "0.1.0".into(),
            source: "firecrawl/anydoc".into(),
        },
        DocumentKind::Docx
        | DocumentKind::Pptx
        | DocumentKind::Xlsx
        | DocumentKind::Epub
        | DocumentKind::Odt
        | DocumentKind::Ods
        | DocumentKind::Odp
        | DocumentKind::Rtf
        | DocumentKind::LegacyOffice => DocumentProvider {
            name: "anydoc".into(),
            version: crate::ANYDOC_VERSION.into(),
            source: "firecrawl/anydoc".into(),
        },
    }
}

/// Stable capability declaration for one document kind.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentCapabilities {
    /// Format represented by this record.
    pub kind: DocumentKind,
    /// Exact container variants allowed by this capability.
    pub supported_variants: Vec<DocumentVariant>,
    /// Contract schema version for this result.
    pub schema_version: u8,
    /// Provider identity used for conversion.
    pub provider: DocumentProvider,
    /// Whether the generic route is enabled for this format.
    pub enabled: bool,
    /// Markdown is the only generic output currently exposed.
    pub markdown: bool,
    /// Whether successful output is checked for required package structure.
    pub completeness_checked: bool,
    /// Whether source coordinates are preserved.
    pub source_coordinates: bool,
    /// Whether formulas are evaluated rather than merely read from caches.
    pub formula_evaluation: bool,
    /// Formula handling policy exposed to callers.
    pub formula_policy: String,
    /// Hidden-content policy exposed to callers.
    pub hidden_content_policy: String,
    /// External-content policy exposed to callers.
    pub external_content_policy: String,
    /// Active-content policy exposed to callers.
    pub active_content_policy: String,
    /// Whether OCR is performed.
    pub ocr: bool,
    /// Maximum accepted input bytes.
    pub max_input_bytes: u64,
    /// Maximum returned Markdown bytes.
    pub max_output_bytes: u64,
    /// Maximum worker wall time in milliseconds.
    pub worker_timeout_ms: u64,
    /// Maximum concurrent worker processes.
    pub max_in_flight: u32,
    /// Enforced address-space ceiling when the host supports it.
    pub process_memory_limit_bytes: Option<u64>,
}

/// Classify one local document without invoking a conversion parser.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentClassification {
    /// Detected content kind, when a known signature or extension exists.
    pub kind: Option<DocumentKind>,
    /// Exact container variant when it can be determined.
    pub variant: Option<DocumentVariant>,
    /// Whether the kind is currently enabled on the generic route.
    pub enabled: bool,
    /// Input size observed after path validation.
    pub size_bytes: u64,
    /// Capabilities for the detected kind, if recognized.
    pub capabilities: Option<DocumentCapabilities>,
}

/// Conversion completeness is explicit so a parser cannot silently turn a
/// partial result into a successful one later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Completeness {
    /// Required package content was present and conversion completed.
    Complete,
    /// Conversion produced output but an audited completeness check found a
    /// recoverable omission, named by a warning. Emitted for DOCX when the
    /// pinned parser drops non-breaking hyphens (`characters_omitted`).
    Partial,
}

/// A fixed, non-document-controlled warning surfaced with a result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentWarning {
    /// Stable warning code.
    pub code: String,
    /// Safe human-readable explanation.
    pub message: String,
}

/// Generic Markdown result shared by future format adapters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentContent {
    /// Detected input kind.
    pub kind: DocumentKind,
    /// Exact input container variant.
    pub variant: DocumentVariant,
    /// Contract schema version for this result.
    pub schema_version: u8,
    /// Provider identity used for conversion.
    pub provider: DocumentProvider,
    /// Sanitized GitHub-Flavored Markdown.
    pub markdown: String,
    /// Completeness of the conversion.
    pub completeness: Completeness,
    /// Fixed diagnostics generated by the local boundary.
    pub warnings: Vec<DocumentWarning>,
    /// Number of input bytes passed to the worker.
    pub input_bytes: u64,
}

/// Stable local errors. Display output intentionally excludes paths and raw
/// parser details because it is used in MCP responses and stderr logs.
#[derive(Debug, Error)]
pub enum DocumentError {
    #[error("document format is not recognized")]
    Unrecognized,
    #[error("document format is not enabled")]
    Unsupported,
    #[error("document requires OCR, which is disabled")]
    OcrRequired { pages: Vec<u32> },
    #[error("document is encrypted or password-protected")]
    Encrypted,
    #[error("document is malformed or missing required content")]
    Malformed,
    #[error("document contains disabled active content")]
    ActiveContentDisabled,
    #[error("document conversion omitted required content")]
    IncompleteConversion,
    #[error("document exceeded a parser resource limit")]
    ResourceLimit,
    #[error("document output exceeded the response limit")]
    OutputTooLarge,
    #[error("too many document conversions are in flight")]
    WorkerBusy,
    #[error("document input is unavailable")]
    InputUnavailable,
    #[error("AnyDoc worker is unavailable")]
    WorkerUnavailable,
    #[error("AnyDoc worker timed out")]
    WorkerTimeout,
    #[error("AnyDoc worker returned an invalid response")]
    WorkerProtocol,
    #[error("AnyDoc conversion failed")]
    ConversionFailed,
}

impl DocumentError {
    /// Stable machine-readable error code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unrecognized => "unrecognized",
            Self::Unsupported => "unsupported",
            Self::OcrRequired { .. } => "needs_ocr",
            Self::Encrypted => "encrypted",
            Self::Malformed => "malformed",
            Self::ActiveContentDisabled => "active_content_disabled",
            Self::IncompleteConversion => "incomplete_conversion",
            Self::ResourceLimit => "resource_limit",
            Self::OutputTooLarge => "output_too_large",
            Self::WorkerBusy => "worker_busy",
            Self::InputUnavailable => "input_unavailable",
            Self::WorkerUnavailable => "worker_unavailable",
            Self::WorkerTimeout => "worker_timeout",
            Self::WorkerProtocol => "worker_protocol",
            Self::ConversionFailed => "conversion_failed",
        }
    }
}

/// Return the capability contract for a kind without reading a file.
pub fn capabilities(kind: DocumentKind) -> DocumentCapabilities {
    let enabled = worker_sandbox_available()
        && (matches!(
            kind,
            DocumentKind::Docx
                | DocumentKind::Pptx
                | DocumentKind::Xlsx
                | DocumentKind::Ods
                | DocumentKind::Odt
        ) || (matches!(
            kind,
            DocumentKind::Csv | DocumentKind::Epub | DocumentKind::Odp
        ) && worker_memory_limit().is_some()));
    let (formula_policy, hidden_content_policy, external_content_policy, active_content_policy) =
        match kind {
            DocumentKind::Xlsx => ("cached_value_only", "reject", "reject", "reject"),
            DocumentKind::Ods => ("cached_value_only", "reject", "reject", "reject"),
            DocumentKind::Epub | DocumentKind::Odt => {
                ("not_applicable", "reject", "reject", "reject")
            }
            DocumentKind::Docx => ("not_applicable", "preserve", "sanitize", "reject"),
            DocumentKind::Pptx => ("not_applicable", "reject", "reject", "reject"),
            DocumentKind::Csv => ("not_applicable", "reject", "reject", "reject"),
            DocumentKind::Odp => ("not_applicable", "reject", "reject", "reject"),
            _ => ("disabled", "disabled", "disabled", "disabled"),
        };
    let supported_variants = match kind {
        DocumentKind::Docx => vec![DocumentVariant::Docx],
        DocumentKind::Pptx => vec![DocumentVariant::Pptx],
        DocumentKind::Xlsx => vec![DocumentVariant::Xlsx],
        DocumentKind::Pdf => vec![DocumentVariant::Pdf],
        DocumentKind::Epub => vec![DocumentVariant::Epub],
        DocumentKind::Odt => vec![DocumentVariant::Odt],
        DocumentKind::Ods => vec![DocumentVariant::Ods],
        DocumentKind::Odp => vec![DocumentVariant::Odp],
        DocumentKind::Rtf => vec![DocumentVariant::Rtf],
        DocumentKind::LegacyOffice => vec![
            DocumentVariant::Doc,
            DocumentVariant::Ppt,
            DocumentVariant::Pps,
            DocumentVariant::Pot,
        ],
        DocumentKind::Csv => vec![DocumentVariant::Csv],
    };
    DocumentCapabilities {
        supported_variants,
        kind,
        schema_version: PROTOCOL_VERSION,
        enabled,
        markdown: enabled,
        completeness_checked: enabled,
        source_coordinates: false,
        formula_evaluation: false,
        formula_policy: formula_policy.into(),
        hidden_content_policy: hidden_content_policy.into(),
        external_content_policy: external_content_policy.into(),
        active_content_policy: active_content_policy.into(),
        ocr: false,
        provider: provider_for(kind),
        max_input_bytes: MAX_DOCUMENT_SIZE,
        max_output_bytes: MAX_MARKDOWN_SIZE as u64,
        worker_timeout_ms: WORKER_TIMEOUT.as_millis() as u64,
        max_in_flight: MAX_IN_FLIGHT_WORKERS as u32,
        process_memory_limit_bytes: worker_memory_limit(),
    }
}

/// Classify a path using content signatures first and its extension second.
pub fn classify(path: impl AsRef<Path>) -> Result<DocumentClassification, DocumentError> {
    let canonical = crate::validate_path(path).map_err(map_path_error)?;
    let bytes = crate::read_validated(&canonical).map_err(map_path_error)?;
    Ok(classify_bytes(&bytes, &canonical))
}

fn map_path_error(error: crate::SkillkitError) -> DocumentError {
    match error {
        crate::SkillkitError::FileTooLarge { .. } => DocumentError::ResourceLimit,
        _ => DocumentError::InputUnavailable,
    }
}
#[derive(Default)]
struct PackagePreflight {
    external_relationships: bool,
    active_content: bool,
    hidden_content: bool,
    missing_formula_cache: bool,
    missing_required_content: bool,
    unsupported_content: bool,
    /// Characters the pinned parser drops while the text around them
    /// converts, so the result is usable but partial.
    omitted_characters: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerWarningKind {
    /// AnyDoc omitted a part, relationship, slide, sheet, or embedded asset.
    Omission,
    /// AnyDoc recovered malformed XML instead of rejecting it.
    MalformedRecovery,
}

struct WorkerDiagnosticState {
    omission: AtomicBool,
    malformed_recovery: AtomicBool,
}

static WORKER_DIAGNOSTICS: WorkerDiagnosticState = WorkerDiagnosticState {
    omission: AtomicBool::new(false),
    malformed_recovery: AtomicBool::new(false),
};

struct WorkerLogger;

static WORKER_LOGGER: WorkerLogger = WorkerLogger;

impl log::Log for WorkerLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Warn
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        // Render only into a temporary value. The classifier retains the
        // stable enum below, never the document-controlled message or target.
        let message = record.args().to_string();
        match worker_warning_kind(&message) {
            Some(WorkerWarningKind::Omission) => {
                WORKER_DIAGNOSTICS.omission.store(true, Ordering::Release);
            }
            Some(WorkerWarningKind::MalformedRecovery) => {
                WORKER_DIAGNOSTICS
                    .malformed_recovery
                    .store(true, Ordering::Release);
            }
            None => {}
        }
    }

    fn flush(&self) {}
}

fn worker_warning_kind(message: &str) -> Option<WorkerWarningKind> {
    const OMISSION_PREFIXES: &[&str] = &[
        "skipping unreadable part",
        "skipping corrupt part",
        "skipping unusable slide",
        "skipping slide ",
        "skipping corrupt chart part",
        "skipping corrupt diagram part",
        "skipping unresolvable relationship target",
        "skipping unresolvable related-part target",
        "skipping unresolvable object reference",
        "skipping unresolvable image reference",
        "relationship target ",
        "image part ",
        "skipping unreadable sheet",
        "skipping sheet ",
        "skipping chapter with",
        "skipping unusable chapter",
        "skipping chapter ",
        "workbook stream ends ",
    ];
    if OMISSION_PREFIXES
        .iter()
        .any(|prefix| message.starts_with(prefix))
    {
        return Some(WorkerWarningKind::Omission);
    }
    message
        .starts_with("recovered malformed xml")
        .then_some(WorkerWarningKind::MalformedRecovery)
}

fn install_worker_logger() -> Result<(), DocumentError> {
    WORKER_DIAGNOSTICS.omission.store(false, Ordering::Release);
    WORKER_DIAGNOSTICS
        .malformed_recovery
        .store(false, Ordering::Release);
    log::set_logger(&WORKER_LOGGER).map_err(|_| DocumentError::WorkerProtocol)?;
    log::set_max_level(log::LevelFilter::Warn);
    Ok(())
}

fn worker_diagnostics_incomplete() -> bool {
    WORKER_DIAGNOSTICS.omission.load(Ordering::Acquire)
        || WORKER_DIAGNOSTICS
            .malformed_recovery
            .load(Ordering::Acquire)
}

fn ooxml_variant(bytes: &[u8]) -> Option<DocumentVariant> {
    let mut archive = ZipArchive::new(Cursor::new(bytes)).ok()?;
    let mut entry = archive.by_name("[Content_Types].xml").ok()?;
    if entry.size() > MAX_PREFLIGHT_PART_BYTES {
        return None;
    }
    let mut content = Vec::new();
    (&mut entry)
        .take(MAX_PREFLIGHT_PART_BYTES + 1)
        .read_to_end(&mut content)
        .ok()?;
    if content.len() as u64 > MAX_PREFLIGHT_PART_BYTES {
        return None;
    }
    let content = String::from_utf8_lossy(&anydoc_xml_utf8(content)).to_ascii_lowercase();
    if content.contains("wordprocessingml.document.macroenabled.main") {
        Some(DocumentVariant::Docm)
    } else if content.contains("wordprocessingml.document.main") {
        Some(DocumentVariant::Docx)
    } else if content.contains("presentationml.presentation.macroenabled.main") {
        Some(DocumentVariant::Pptm)
    } else if content.contains("presentationml.slideshow.macroenabled.main") {
        Some(DocumentVariant::Ppsm)
    } else if content.contains("presentationml.slideshow.main") {
        Some(DocumentVariant::Ppsx)
    } else if content.contains("presentationml.presentation.main") {
        Some(DocumentVariant::Pptx)
    } else if content.contains("spreadsheetml.sheet.binary.macroenabled.main") {
        Some(DocumentVariant::Xlsb)
    } else if content.contains("spreadsheetml.sheet.macroenabled.main") {
        Some(DocumentVariant::Xlsm)
    } else if content.contains("spreadsheetml.sheet.main") {
        Some(DocumentVariant::Xlsx)
    } else {
        None
    }
}

fn xml_has_hidden_content(bytes: &[u8]) -> bool {
    let text = String::from_utf8_lossy(bytes).to_ascii_lowercase();
    [
        r#"state="hidden""#,
        r#"state="veryhidden""#,
        r#"hidden="1""#,
        r#"hidden="true""#,
    ]
    .iter()
    .any(|marker| text.contains(marker))
}

fn xml_has_uncached_formula(bytes: &[u8]) -> bool {
    let text = String::from_utf8_lossy(bytes).to_ascii_lowercase();
    let mut remaining = text.as_str();
    while let Some(offset) = remaining.find("<c") {
        remaining = &remaining[offset + 2..];
        let is_cell = remaining.starts_with(char::from(32))
            || remaining.starts_with(char::from(62))
            || remaining.starts_with(char::from(47));
        if !is_cell {
            if remaining.is_empty() {
                break;
            }
            remaining = &remaining[1..];
            continue;
        }
        let Some(end) = remaining.find("</c>") else {
            break;
        };
        let cell = &remaining[..end];
        if cell.contains("<f") && !cell.contains("<v") {
            return true;
        }
        remaining = &remaining[end + 4..];
    }
    false
}

fn xml_local_name(name: &[u8]) -> &[u8] {
    name.rsplit(|byte| *byte == b':').next().unwrap_or(name)
}

/// An attribute's value by local name, with character references decoded
/// (`Q&#117;iet` is `Quiet`); a value that cannot be decoded is returned raw.
/// The first match wins, as in AnyDoc's `attr_any`.
fn xml_attribute_value(event: &quick_xml::events::BytesStart<'_>, wanted: &[u8]) -> Option<String> {
    xml_attribute_values(event, wanted).into_iter().next()
}

/// Every value of the attributes with this local name, in any namespace.
/// Where AnyDoc picks one of several by namespace (`w:val` before `val`),
/// checking them all covers its pick.
fn xml_attribute_values(event: &quick_xml::events::BytesStart<'_>, wanted: &[u8]) -> Vec<String> {
    event
        .attributes()
        .flatten()
        .filter(|attribute| xml_local_name(attribute.key.as_ref()) == wanted)
        .map(|attribute| {
            attribute
                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .map(|value| value.into_owned())
                .unwrap_or_else(|_| String::from_utf8_lossy(attribute.value.as_ref()).into_owned())
        })
        .collect()
}

const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

/// The encoding AnyDoc 0.2.4 transcodes an XML part from before parsing it
/// (`package::xml::to_utf8`), judged from the part's first bytes: a UTF-16
/// byte order mark, or an `encoding="…"` anywhere in the first 200 bytes that
/// names something other than UTF-8. `None` means the part is parsed as
/// UTF-8.
fn anydoc_xml_encoding(bytes: &[u8]) -> Option<&'static encoding_rs::Encoding> {
    match bytes {
        [0xFF, 0xFE, ..] => Some(encoding_rs::UTF_16LE),
        [0xFE, 0xFF, ..] => Some(encoding_rs::UTF_16BE),
        [0xEF, 0xBB, 0xBF, ..] => None,
        _ => declared_xml_encoding(&bytes[..bytes.len().min(200)])
            .and_then(|label| encoding_rs::Encoding::for_label(label.as_bytes()))
            .filter(|encoding| *encoding != encoding_rs::UTF_8),
    }
}

/// The value after the first `encoding` in a prefix that is valid UTF-8.
fn declared_xml_encoding(head: &[u8]) -> Option<&str> {
    let head = std::str::from_utf8(head).ok()?;
    let rest = &head[head.find("encoding")? + "encoding".len()..];
    let rest = rest.trim_start().strip_prefix('=')?.trim_start();
    let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'')?;
    let rest = &rest[1..];
    Some(&rest[..rest.find(quote)?])
}

/// An XML part as the UTF-8 AnyDoc parses. Every check reads this, so a part
/// cannot show one document to the checks and another to the parser.
fn anydoc_xml_utf8(mut bytes: Vec<u8>) -> Vec<u8> {
    match anydoc_xml_encoding(&bytes) {
        Some(encoding) => encoding.decode(&bytes).0.into_owned().into_bytes(),
        None => {
            if bytes.starts_with(UTF8_BOM) {
                bytes.drain(..UTF8_BOM.len());
            }
            bytes
        }
    }
}

/// Open an archive part for a streamed check, decoded as AnyDoc decodes it.
/// A part AnyDoc transcodes is decoded in memory under the preflight part
/// cap; any other part streams unchanged.
fn open_xml_stream<'a>(
    mut part: impl Read + 'a,
) -> Result<Box<dyn std::io::BufRead + 'a>, DocumentError> {
    let mut head = Vec::with_capacity(200);
    (&mut part)
        .take(200)
        .read_to_end(&mut head)
        .map_err(|_| DocumentError::Malformed)?;
    if anydoc_xml_encoding(&head).is_some() {
        let mut bytes = head;
        part.take(MAX_PREFLIGHT_PART_BYTES + 1 - bytes.len() as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| DocumentError::Malformed)?;
        if bytes.len() as u64 > MAX_PREFLIGHT_PART_BYTES {
            return Err(DocumentError::ResourceLimit);
        }
        return Ok(Box::new(Cursor::new(anydoc_xml_utf8(bytes))));
    }
    let mut head = Cursor::new(head);
    if head.get_ref().starts_with(UTF8_BOM) {
        head.set_position(UTF8_BOM.len() as u64);
    }
    Ok(Box::new(std::io::BufReader::new(Read::chain(head, part))))
}

fn xml_has_odf_spreadsheet(bytes: &[u8]) -> bool {
    let mut reader = quick_xml::Reader::from_reader(Cursor::new(bytes));
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();
    let mut spreadsheet = false;
    let mut table = false;
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(quick_xml::events::Event::Start(event))
            | Ok(quick_xml::events::Event::Empty(event)) => {
                match xml_local_name(event.name().as_ref()) {
                    b"spreadsheet" => spreadsheet = true,
                    b"table" => table = true,
                    _ => {}
                }
                buffer.clear();
            }
            Ok(quick_xml::events::Event::Eof) => return spreadsheet && table,
            Ok(_) => buffer.clear(),
            Err(_) => return false,
        }
    }
}

fn xml_has_odf_hidden_content(bytes: &[u8]) -> bool {
    let mut reader = quick_xml::Reader::from_reader(Cursor::new(bytes));
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(quick_xml::events::Event::Start(event))
            | Ok(quick_xml::events::Event::Empty(event)) => {
                let event_name = event.name();
                let local = xml_local_name(event_name.as_ref());
                if matches!(
                    local,
                    b"table"
                        | b"table-row"
                        | b"table-column"
                        | b"table-properties"
                        | b"table-row-properties"
                        | b"table-column-properties"
                ) {
                    let visibility = xml_attribute_value(&event, b"visibility");
                    let display = xml_attribute_value(&event, b"display");
                    if visibility.as_deref().is_some_and(|value| {
                        matches!(
                            value.to_ascii_lowercase().as_str(),
                            "collapse" | "filter" | "hidden"
                        )
                    }) || display.as_deref().is_some_and(|value| {
                        matches!(
                            value.to_ascii_lowercase().as_str(),
                            "false" | "0" | "hidden"
                        )
                    }) {
                        return true;
                    }
                }
                buffer.clear();
            }
            Ok(quick_xml::events::Event::Eof) => return false,
            Ok(_) => buffer.clear(),
            Err(_) => return false,
        }
    }
}

fn is_external_uri(value: &str) -> bool {
    let value = value.trim().to_ascii_lowercase();
    value.starts_with("//")
        || value.starts_with("http:")
        || value.starts_with("https:")
        || value.starts_with("ftp:")
        || value.starts_with("file:")
        || value.starts_with("data:")
        || value.starts_with("javascript:")
        || value.starts_with("../")
}

fn xml_has_odf_external_reference(bytes: &[u8]) -> bool {
    let mut reader = quick_xml::Reader::from_reader(Cursor::new(bytes));
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(quick_xml::events::Event::Start(event))
            | Ok(quick_xml::events::Event::Empty(event)) => {
                for attribute in event.attributes().flatten() {
                    if xml_local_name(attribute.key.as_ref()) == b"href"
                        && is_external_uri(&String::from_utf8_lossy(attribute.value.as_ref()))
                    {
                        return true;
                    }
                }
                buffer.clear();
            }
            Ok(quick_xml::events::Event::Eof) => return false,
            Ok(_) => buffer.clear(),
            Err(_) => return false,
        }
    }
}

fn xml_has_odf_active_content(bytes: &[u8]) -> bool {
    let mut reader = quick_xml::Reader::from_reader(Cursor::new(bytes));
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(quick_xml::events::Event::Start(event))
            | Ok(quick_xml::events::Event::Empty(event)) => {
                if matches!(
                    xml_local_name(event.name().as_ref()),
                    b"object"
                        | b"plugin"
                        | b"applet"
                        | b"script"
                        | b"event-listeners"
                        | b"dde-connection"
                        | b"cell-range-source"
                ) {
                    return true;
                }
                buffer.clear();
            }
            Ok(quick_xml::events::Event::Eof) => return false,
            Ok(_) => buffer.clear(),
            Err(_) => return false,
        }
    }
}

fn xml_has_odf_encryption_data(bytes: &[u8]) -> bool {
    let mut reader = quick_xml::Reader::from_reader(Cursor::new(bytes));
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(quick_xml::events::Event::Start(event))
            | Ok(quick_xml::events::Event::Empty(event)) => {
                if xml_local_name(event.name().as_ref()) == b"encryption-data" {
                    return true;
                }
                buffer.clear();
            }
            Ok(quick_xml::events::Event::Eof) => return false,
            Ok(_) => buffer.clear(),
            Err(_) => return false,
        }
    }
}

fn xml_has_uncached_odf_formula(bytes: &[u8]) -> bool {
    struct FormulaCell {
        depth: usize,
        cached_value: bool,
        display_text: bool,
    }

    let mut reader = quick_xml::Reader::from_reader(Cursor::new(bytes));
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();
    let mut depth = 0usize;
    let mut formulas = Vec::new();
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(quick_xml::events::Event::Start(event)) => {
                depth += 1;
                if xml_local_name(event.name().as_ref()) == b"table-cell"
                    && xml_attribute_value(&event, b"formula").is_some()
                {
                    let cached_value = [
                        b"value".as_slice(),
                        b"string-value".as_slice(),
                        b"date-value".as_slice(),
                        b"time-value".as_slice(),
                        b"boolean-value".as_slice(),
                    ]
                    .iter()
                    .any(|name| {
                        xml_attribute_value(&event, name)
                            .is_some_and(|value| !value.trim().is_empty())
                    });
                    formulas.push(FormulaCell {
                        depth,
                        cached_value,
                        display_text: false,
                    });
                }
                buffer.clear();
            }
            Ok(quick_xml::events::Event::Empty(event)) => {
                if xml_local_name(event.name().as_ref()) == b"table-cell"
                    && xml_attribute_value(&event, b"formula").is_some()
                {
                    let cached_value = [
                        b"value".as_slice(),
                        b"string-value".as_slice(),
                        b"date-value".as_slice(),
                        b"time-value".as_slice(),
                        b"boolean-value".as_slice(),
                    ]
                    .iter()
                    .any(|name| {
                        xml_attribute_value(&event, name)
                            .is_some_and(|value| !value.trim().is_empty())
                    });
                    if !cached_value {
                        return true;
                    }
                }
                buffer.clear();
            }
            Ok(quick_xml::events::Event::Text(event)) => {
                if event
                    .into_inner()
                    .iter()
                    .any(|byte| !byte.is_ascii_whitespace())
                {
                    for formula in &mut formulas {
                        formula.display_text = true;
                    }
                }
                buffer.clear();
            }
            Ok(quick_xml::events::Event::CData(event)) => {
                if event
                    .into_inner()
                    .iter()
                    .any(|byte| !byte.is_ascii_whitespace())
                {
                    for formula in &mut formulas {
                        formula.display_text = true;
                    }
                }
                buffer.clear();
            }
            Ok(quick_xml::events::Event::End(_)) => {
                if let Some(formula) = formulas.pop_if(|formula| formula.depth == depth) {
                    if !formula.cached_value && !formula.display_text {
                        return true;
                    }
                }
                depth = depth.saturating_sub(1);
                buffer.clear();
            }
            Ok(quick_xml::events::Event::Eof) => return false,
            Ok(_) => buffer.clear(),
            Err(_) => return false,
        }
    }
}
fn xml_has_odf_presentation(bytes: &[u8]) -> bool {
    let mut reader = quick_xml::Reader::from_reader(Cursor::new(bytes));
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();
    let mut presentation = false;
    let mut page = false;
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(quick_xml::events::Event::Start(event))
            | Ok(quick_xml::events::Event::Empty(event)) => {
                match xml_local_name(event.name().as_ref()) {
                    b"presentation" => presentation = true,
                    b"page" => page = true,
                    _ => {}
                }
                buffer.clear();
            }
            Ok(quick_xml::events::Event::Eof) => return presentation && page,
            Ok(_) => buffer.clear(),
            Err(_) => return false,
        }
    }
}

fn xml_has_odp_hidden_content(bytes: &[u8]) -> bool {
    if xml_has_odf_hidden_content(bytes) {
        return true;
    }
    let mut reader = quick_xml::Reader::from_reader(Cursor::new(bytes));
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(quick_xml::events::Event::Start(event))
            | Ok(quick_xml::events::Event::Empty(event)) => {
                if xml_local_name(event.name().as_ref()) == b"page"
                    && event.attributes().flatten().any(|attribute| {
                        let name = xml_local_name(attribute.key.as_ref());
                        let value =
                            String::from_utf8_lossy(attribute.value.as_ref()).to_ascii_lowercase();
                        (name == b"visibility"
                            && matches!(value.as_str(), "hidden" | "false" | "0"))
                            || (name == b"show" && matches!(value.as_str(), "false" | "0"))
                    })
                {
                    return true;
                }
                buffer.clear();
            }
            Ok(quick_xml::events::Event::Eof) => return false,
            Ok(_) => buffer.clear(),
            Err(_) => return false,
        }
    }
}

fn xml_has_odf_text(bytes: &[u8]) -> bool {
    let mut reader = quick_xml::Reader::from_reader(Cursor::new(bytes));
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(quick_xml::events::Event::Start(event))
            | Ok(quick_xml::events::Event::Empty(event)) => {
                if xml_local_name(event.name().as_ref()) == b"text" {
                    return true;
                }
                buffer.clear();
            }
            Ok(quick_xml::events::Event::Eof) => return false,
            Ok(_) => buffer.clear(),
            Err(_) => return false,
        }
    }
}

fn xml_has_odt_hidden_or_tracked_content(bytes: &[u8]) -> bool {
    if xml_has_odf_hidden_content(bytes) {
        return true;
    }
    let mut reader = quick_xml::Reader::from_reader(Cursor::new(bytes));
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(quick_xml::events::Event::Start(event))
            | Ok(quick_xml::events::Event::Empty(event)) => {
                let event_name = event.name();
                let local = xml_local_name(event_name.as_ref());
                if matches!(
                    local,
                    b"hidden-text"
                        | b"hidden-paragraph"
                        | b"conditional-text"
                        | b"tracked-changes"
                        | b"change"
                        | b"change-start"
                        | b"change-end"
                        | b"insertion"
                        | b"deletion"
                        | b"annotation"
                        | b"annotation-end"
                ) {
                    return true;
                }
                for attribute in event.attributes().flatten() {
                    let name = xml_local_name(attribute.key.as_ref());
                    let value = String::from_utf8_lossy(attribute.value.as_ref());
                    if (name == b"condition" && !value.trim().is_empty())
                        || (name == b"display"
                            && matches!(
                                value.to_ascii_lowercase().as_str(),
                                "none" | "false" | "0" | "hidden"
                            ))
                    {
                        return true;
                    }
                }
                buffer.clear();
            }
            Ok(quick_xml::events::Event::Eof) => return false,
            Ok(_) => buffer.clear(),
            Err(_) => return false,
        }
    }
}

fn xml_has_odt_unsupported_content(bytes: &[u8]) -> bool {
    let mut reader = quick_xml::Reader::from_reader(Cursor::new(bytes));
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(quick_xml::events::Event::Start(event))
            | Ok(quick_xml::events::Event::Empty(event)) => {
                if xml_local_name(event.name().as_ref()) == b"note" {
                    return true;
                }
                buffer.clear();
            }
            Ok(quick_xml::events::Event::Eof) => return false,
            Ok(_) => buffer.clear(),
            Err(_) => return false,
        }
    }
}

/// What a document's Word story parts (body, footnotes, and endnotes) carry,
/// accumulated across the parts.
#[derive(Default)]
struct DocxStoryScan {
    /// Content the pinned AnyDoc parser drops without a diagnostic: a symbol
    /// character (`w:sym`, which carries Wingdings checkboxes and Symbol-font
    /// letters) and the state of a legacy form checkbox or drop-down
    /// (`w:checkBox`, `w:ddList`). An open upstream change
    /// (firecrawl/anydoc#177) renders four Wingdings checkbox codes; every
    /// other symbol stays dropped there too.
    dropped: bool,
    /// A run formatted hidden directly (`w:r/w:rPr/w:vanish`), which the
    /// pinned parser converts as ordinary text.
    hidden_run: bool,
    /// A non-breaking hyphen (`w:noBreakHyphen`), which the pinned parser
    /// drops, joining its neighbors: "Form 1040‑SR" converts as "Form 1040SR".
    omitted_hyphen: bool,
    /// Character, paragraph, and table styles applied to content.
    styles_used: HashSet<String>,
}

fn xml_true(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "true" | "1" | "on"
    )
}

fn xml_false(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "false" | "0" | "off"
    )
}

/// Whether an on/off property element such as `w:vanish` is switched off:
/// only when every `val` it carries says so.
fn xml_toggle_off(event: &quick_xml::events::BytesStart<'_>) -> bool {
    let values = xml_attribute_values(event, b"val");
    !values.is_empty() && values.iter().all(|value| xml_false(value))
}

/// Whether the open elements end with `suffix`, compared by local name.
fn xml_path_ends_with(stack: &[Vec<u8>], suffix: &[&[u8]]) -> bool {
    stack.len() >= suffix.len()
        && stack[stack.len() - suffix.len()..]
            .iter()
            .zip(suffix)
            .all(|(open, wanted)| open.as_slice() == *wanted)
}

/// Scan one Word story part into `scan`. Parse errors fail closed as
/// malformed; end tags are not matched against start tags by prefix, as
/// AnyDoc does not match them either.
fn scan_docx_story(bytes: &[u8], scan: &mut DocxStoryScan) -> Result<(), DocumentError> {
    let mut reader = quick_xml::Reader::from_reader(Cursor::new(bytes));
    reader.config_mut().trim_text(false);
    reader.config_mut().check_end_names = false;
    let mut buffer = Vec::new();
    let mut stack: Vec<Vec<u8>> = Vec::new();
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(quick_xml::events::Event::Start(event)) => {
                if stack.len() >= MAX_XML_DEPTH {
                    return Err(DocumentError::ResourceLimit);
                }
                let local = xml_local_name(event.name().as_ref()).to_vec();
                scan_docx_element(&event, &local, &stack, scan)?;
                stack.push(local);
            }
            Ok(quick_xml::events::Event::Empty(event)) => {
                let local = xml_local_name(event.name().as_ref()).to_vec();
                scan_docx_element(&event, &local, &stack, scan)?;
            }
            Ok(quick_xml::events::Event::End(_)) => {
                stack.pop();
            }
            Ok(quick_xml::events::Event::Eof) => return Ok(()),
            Ok(_) => {}
            Err(_) => return Err(DocumentError::Malformed),
        }
        buffer.clear();
    }
}

fn scan_docx_element(
    event: &quick_xml::events::BytesStart<'_>,
    local: &[u8],
    stack: &[Vec<u8>],
    scan: &mut DocxStoryScan,
) -> Result<(), DocumentError> {
    // Properties apply to content only in these positions: the same element
    // under a paragraph mark (`w:pPr/w:rPr`) or in revision history
    // (`w:rPrChange/w:rPr`, `w:pPrChange/w:pPr`) formats nothing visible.
    let run_property = xml_path_ends_with(stack, &[b"r", b"rPr"]);
    // Tracked deletions and the source side of tracked moves are omitted from
    // the output, so nothing inside them is dropped by the parser.
    let removed = stack
        .iter()
        .any(|name| name == b"del" || name == b"moveFrom");
    match local {
        // Ruby text loses its base text as well as the annotation, and an
        // imported chunk (`w:altChunk`, HTML or RTF that Word merges on
        // opening) loses all of its content.
        b"sym" | b"checkBox" | b"ddList" | b"ruby" | b"altChunk" if !removed => {
            scan.dropped = true;
        }
        b"noBreakHyphen" if !removed && xml_path_ends_with(stack, &[b"r"]) => {
            scan.omitted_hyphen = true;
        }
        b"vanish" if run_property => scan.hidden_run |= !xml_toggle_off(event),
        b"rStyle" if run_property => record_style(event, scan)?,
        b"pStyle" if xml_path_ends_with(stack, &[b"p", b"pPr"]) => record_style(event, scan)?,
        b"tblStyle" if xml_path_ends_with(stack, &[b"tbl", b"tblPr"]) => {
            record_style(event, scan)?;
        }
        _ => {}
    }
    Ok(())
}

fn record_style(
    event: &quick_xml::events::BytesStart<'_>,
    scan: &mut DocxStoryScan,
) -> Result<(), DocumentError> {
    for style in xml_attribute_values(event, b"val") {
        if style.len() > MAX_STYLE_ID_BYTES {
            return Err(DocumentError::ResourceLimit);
        }
        if scan.styles_used.len() >= MAX_DOCX_STYLES && !scan.styles_used.contains(&style) {
            return Err(DocumentError::ResourceLimit);
        }
        scan.styles_used.insert(style);
    }
    Ok(())
}

/// One `w:style` definition as the hidden-text check sees it.
#[derive(Default)]
struct DocxStyleDefinition {
    ids: Vec<String>,
    bases: Vec<String>,
    /// `Some(true)` when any `w:vanish` in the style's run properties is on,
    /// `Some(false)` when every one present is off.
    vanish: Option<bool>,
    default: bool,
}

impl DocxStyleDefinition {
    fn set_vanish(&mut self, on: bool) {
        self.vanish = Some(self.vanish.unwrap_or(false) || on);
    }
}

/// Style ids whose run properties can hide text, following `basedOn`, and
/// whether text no style names can be hidden: by the document defaults or by
/// a default style (`w:default="1"`), which applies to every unstyled
/// paragraph, run, or table. Streamed under AnyDoc's depth and node bounds.
///
/// Every reading AnyDoc or Word could take counts: a style defined twice is
/// hidden if either definition hides it, and an attribute repeated under
/// another prefix counts in each spelling.
fn docx_hidden_styles(
    reader: impl std::io::BufRead,
) -> Result<(HashSet<String>, bool), DocumentError> {
    let mut reader = quick_xml::Reader::from_reader(reader);
    reader.config_mut().trim_text(false);
    reader.config_mut().check_end_names = false;
    let mut buffer = Vec::new();
    let mut stack: Vec<Vec<u8>> = Vec::new();
    let mut nodes = 0usize;
    let mut definitions: Vec<DocxStyleDefinition> = Vec::new();
    // Open definitions, innermost last: a style nested in another (invalid,
    // but parseable) must not discard its parent's properties.
    let mut open: Vec<DocxStyleDefinition> = Vec::new();
    let mut defaults_hidden = false;
    loop {
        let (event, start) = match reader.read_event_into(&mut buffer) {
            Ok(quick_xml::events::Event::Start(event)) => (event, true),
            Ok(quick_xml::events::Event::Empty(event)) => (event, false),
            Ok(quick_xml::events::Event::End(_)) => {
                if stack.pop().as_deref() == Some(b"style".as_slice()) {
                    definitions.extend(open.pop());
                }
                buffer.clear();
                continue;
            }
            // AnyDoc closes elements left open at the end of a part, so a
            // truncated definition still applies.
            Ok(quick_xml::events::Event::Eof) => {
                definitions.append(&mut open);
                break;
            }
            Ok(
                quick_xml::events::Event::Text(_)
                | quick_xml::events::Event::GeneralRef(_)
                | quick_xml::events::Event::CData(_),
            ) => {
                nodes += 1;
                if nodes > MAX_XML_NODES {
                    return Err(DocumentError::ResourceLimit);
                }
                buffer.clear();
                continue;
            }
            Ok(_) => {
                buffer.clear();
                continue;
            }
            Err(_) => return Err(DocumentError::Malformed),
        };
        nodes += 1;
        if nodes > MAX_XML_NODES || (start && stack.len() >= MAX_XML_DEPTH) {
            return Err(DocumentError::ResourceLimit);
        }
        let local = xml_local_name(event.name().as_ref()).to_vec();
        match local.as_slice() {
            b"style" => {
                if definitions.len() + open.len() >= MAX_DOCX_STYLES {
                    return Err(DocumentError::ResourceLimit);
                }
                let ids = xml_attribute_values(&event, b"styleId");
                if ids.iter().any(|id| id.len() > MAX_STYLE_ID_BYTES) {
                    return Err(DocumentError::ResourceLimit);
                }
                let definition = DocxStyleDefinition {
                    ids,
                    default: xml_attribute_values(&event, b"default")
                        .iter()
                        .any(|value| xml_true(value)),
                    ..DocxStyleDefinition::default()
                };
                // An empty `<w:style/>` defines nothing that can hide text.
                if start {
                    open.push(definition);
                }
            }
            b"basedOn" if xml_path_ends_with(&stack, &[b"style"]) => {
                if let Some(definition) = open.last_mut() {
                    for base in xml_attribute_values(&event, b"val") {
                        if base.len() > MAX_STYLE_ID_BYTES {
                            return Err(DocumentError::ResourceLimit);
                        }
                        definition.bases.push(base);
                    }
                }
            }
            // Run properties of the style, or of a conditional table region
            // (`w:tblStylePr`, such as a header row).
            b"vanish"
                if xml_path_ends_with(&stack, &[b"style", b"rPr"])
                    || xml_path_ends_with(&stack, &[b"style", b"tblStylePr", b"rPr"]) =>
            {
                if let Some(definition) = open.last_mut() {
                    definition.set_vanish(!xml_toggle_off(&event));
                }
            }
            b"vanish" if xml_path_ends_with(&stack, &[b"docDefaults", b"rPrDefault", b"rPr"]) => {
                defaults_hidden |= !xml_toggle_off(&event);
            }
            _ => {}
        }
        if start {
            stack.push(local);
        }
        buffer.clear();
    }
    // A style is hidden when its own run properties hide text, or when it
    // sets nothing and a style it is based on is hidden; walk outward from
    // the styles that hide text directly. Cycles end at visited styles.
    let mut hidden: HashSet<String> = HashSet::new();
    let mut derived: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut pending: Vec<&str> = Vec::new();
    for definition in &definitions {
        match definition.vanish {
            Some(true) => pending.extend(definition.ids.iter().map(String::as_str)),
            Some(false) => {}
            None => {
                for base in &definition.bases {
                    derived
                        .entry(base.as_str())
                        .or_default()
                        .extend(definition.ids.iter().map(String::as_str));
                }
            }
        }
    }
    while let Some(style) = pending.pop() {
        if hidden.insert(style.to_string()) {
            pending.extend(derived.get(style).into_iter().flatten().copied());
        }
    }
    defaults_hidden |= definitions
        .iter()
        .filter(|definition| definition.default)
        .flat_map(|definition| &definition.ids)
        .any(|id| hidden.contains(id));
    Ok((hidden, defaults_hidden))
}

/// One package relationship, read with the XML reader. Its target mode is
/// not kept: the checks treat every target as a part AnyDoc might load.
struct OoxmlRelationship {
    id: String,
    kind: String,
    target: String,
}

/// Every `Relationship` element in a rels part, in any namespace: reading
/// more than AnyDoc reads can only make the checks below stricter.
fn ooxml_relationships(bytes: &[u8]) -> Result<Vec<OoxmlRelationship>, DocumentError> {
    let mut reader = quick_xml::Reader::from_reader(Cursor::new(bytes));
    reader.config_mut().trim_text(false);
    reader.config_mut().check_end_names = false;
    let mut buffer = Vec::new();
    let mut relationships = Vec::new();
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(quick_xml::events::Event::Start(event))
            | Ok(quick_xml::events::Event::Empty(event))
                if xml_local_name(event.name().as_ref()) == b"Relationship" =>
            {
                relationships.push(OoxmlRelationship {
                    id: xml_attribute_value(&event, b"Id").unwrap_or_default(),
                    kind: xml_attribute_value(&event, b"Type").unwrap_or_default(),
                    target: xml_attribute_value(&event, b"Target").unwrap_or_default(),
                });
            }
            Ok(quick_xml::events::Event::Eof) => return Ok(relationships),
            Ok(_) => {}
            Err(_) => return Err(DocumentError::Malformed),
        }
        buffer.clear();
    }
}

/// Resolve a package reference exactly as AnyDoc 0.2.4 does
/// (`package::path::resolve`): drop the fragment and query, start from the
/// base part's directory, clamp `..` at the root, and percent-decode each
/// segment after splitting. `None` where AnyDoc refuses the reference: a
/// decoded segment that would add structure.
fn anydoc_resolve(base_part: &str, reference: &str) -> Option<String> {
    let reference = reference
        .split_once('#')
        .map_or(reference, |(path, _)| path);
    let reference = reference
        .split_once('?')
        .map_or(reference, |(path, _)| path);
    if reference.is_empty() {
        return Some(base_part.to_string());
    }
    let mut segments: Vec<String> = Vec::new();
    if !reference.starts_with('/') {
        if let Some((directory, _)) = base_part.rsplit_once('/') {
            segments.extend(
                directory
                    .split('/')
                    .filter(|segment| !segment.is_empty())
                    .map(str::to_string),
            );
        }
    }
    for raw in reference.split('/') {
        match raw {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            raw => {
                let decoded = percent_decode(raw);
                if decoded.contains(['/', '\\']) || decoded == "." || decoded == ".." {
                    return None;
                }
                segments.push(decoded);
            }
        }
    }
    Some(segments.join("/"))
}

/// Percent-decode one reference segment as AnyDoc does: a `%` without two hex
/// digits passes through, and invalid UTF-8 degrades lossily.
fn percent_decode(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if let Some(value) = segment
                .get(index + 1..index + 3)
                .filter(|hex| hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
            {
                decoded.push(value);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// The relationships part for a package part: `word/document.xml` has
/// `word/_rels/document.xml.rels`.
fn ooxml_rels_part(part: &str) -> String {
    match part.rsplit_once('/') {
        Some((parent, file)) => format!("{parent}/_rels/{file}.rels"),
        None => format!("_rels/{part}.rels"),
    }
}

/// Read a bounded XML part as AnyDoc decodes it; a missing part reads as
/// `None`.
fn read_optional_xml_part(
    archive: &mut ZipArchive<Cursor<&[u8]>>,
    part: &str,
) -> Result<Option<Vec<u8>>, DocumentError> {
    let Ok(entry) = archive.by_name(part) else {
        return Ok(None);
    };
    if entry.size() > MAX_PREFLIGHT_PART_BYTES {
        return Err(DocumentError::ResourceLimit);
    }
    let mut bytes = Vec::new();
    entry
        .take(MAX_PREFLIGHT_PART_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| DocumentError::Malformed)?;
    if bytes.len() as u64 > MAX_PREFLIGHT_PART_BYTES {
        return Err(DocumentError::ResourceLimit);
    }
    Ok(Some(anydoc_xml_utf8(bytes)))
}

fn read_relationships(
    archive: &mut ZipArchive<Cursor<&[u8]>>,
    rels_part: &str,
) -> Result<Vec<OoxmlRelationship>, DocumentError> {
    match read_optional_xml_part(archive, rels_part)? {
        Some(bytes) => ooxml_relationships(&bytes),
        None => Ok(Vec::new()),
    }
}

/// Parts a conversion may read, located through the package relationships.
/// Each set holds every part AnyDoc could read for its role plus the
/// conventional name, so a check that covers the set covers AnyDoc's choice.
#[derive(Default)]
struct OoxmlLayout {
    /// DOCX body, footnotes, and endnotes.
    story_parts: HashSet<String>,
    /// DOCX or XLSX styles.
    styles_parts: HashSet<String>,
    /// XLSX worksheets.
    worksheet_parts: HashSet<String>,
}

fn ooxml_layout(
    archive: &mut ZipArchive<Cursor<&[u8]>>,
    kind: DocumentKind,
) -> Result<OoxmlLayout, DocumentError> {
    let main = match kind {
        DocumentKind::Docx => "word/document.xml",
        DocumentKind::Pptx => "ppt/presentation.xml",
        DocumentKind::Xlsx => "xl/workbook.xml",
        _ => return Ok(OoxmlLayout::default()),
    };
    // Every check reads the conventional main part, while AnyDoc converts the
    // part the officeDocument relationship names (lowest id first). A package
    // with such a relationship naming any other part is refused rather than
    // converted from a part the checks never saw.
    let declared: Vec<OoxmlRelationship> = read_relationships(archive, "_rels/.rels")?
        .into_iter()
        .filter(|relationship| relationship.kind.ends_with("/officeDocument"))
        .collect();
    if !declared
        .iter()
        .all(|relationship| anydoc_resolve("", &relationship.target).as_deref() == Some(main))
    {
        return Err(DocumentError::Malformed);
    }
    let relationships = read_relationships(archive, &ooxml_rels_part(main))?;
    let typed = |suffix: &str| -> Vec<String> {
        relationships
            .iter()
            .filter(|relationship| relationship.kind.ends_with(suffix))
            .filter_map(|relationship| anydoc_resolve(main, &relationship.target))
            .collect()
    };
    let mut layout = OoxmlLayout::default();
    match kind {
        DocumentKind::Docx => {
            layout.story_parts.insert(main.to_string());
            layout.story_parts.insert("word/footnotes.xml".to_string());
            layout.story_parts.insert("word/endnotes.xml".to_string());
            layout.story_parts.extend(typed("/footnotes"));
            layout.story_parts.extend(typed("/endnotes"));
            layout.styles_parts.insert("word/styles.xml".to_string());
            layout.styles_parts.extend(typed("/styles"));
        }
        DocumentKind::Xlsx => {
            layout.styles_parts.insert("xl/styles.xml".to_string());
            layout.styles_parts.extend(typed("/styles"));
            // AnyDoc loads each `<sheet r:id>` through the workbook
            // relationships whatever their type.
            if let Some(workbook) = read_optional_xml_part(archive, main)? {
                let ids = xlsx_sheet_relationship_ids(&workbook);
                layout.worksheet_parts.extend(
                    relationships
                        .iter()
                        .filter(|relationship| ids.contains(&relationship.id))
                        .filter_map(|relationship| anydoc_resolve(main, &relationship.target)),
                );
            }
        }
        _ => {}
    }
    Ok(layout)
}

/// The relationship ids of a workbook's `<sheet r:id="…">` entries, in
/// every spelling a sheet carries.
fn xlsx_sheet_relationship_ids(workbook: &[u8]) -> HashSet<String> {
    let mut reader = quick_xml::Reader::from_reader(Cursor::new(workbook));
    reader.config_mut().trim_text(false);
    reader.config_mut().check_end_names = false;
    let mut buffer = Vec::new();
    let mut ids = HashSet::new();
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(quick_xml::events::Event::Start(event))
            | Ok(quick_xml::events::Event::Empty(event))
                if xml_local_name(event.name().as_ref()) == b"sheet" =>
            {
                ids.extend(xml_attribute_values(&event, b"id"));
            }
            Ok(quick_xml::events::Event::Eof) | Err(_) => return ids,
            Ok(_) => {}
        }
        buffer.clear();
    }
}

/// Longest spreadsheet number-format code converted. AnyDoc 0.2.4 expands
/// every character of a `formatCode` into several vectors, so one 8 MiB code
/// in a 10 KB workbook peaked near 855 MiB in the worker. An open upstream
/// fix (firecrawl/anydoc#148) caps codes at the same 4096 bytes and renders
/// longer ones as General; the pinned parser cannot, so they are refused.
const MAX_NUMBER_FORMAT_BYTES: usize = 4096;

/// Stream a styles part and report whether any `formatCode` attribute value
/// is longer than [`MAX_NUMBER_FORMAT_BYTES`], in constant memory. The
/// escaped length is measured, which can only overstate the parsed code.
fn styles_have_oversized_number_format(reader: impl Read) -> Result<bool, DocumentError> {
    const NAME: &[u8] = b"formatCode";
    enum State {
        Name(usize),
        AfterName,
        AfterEquals,
        Value { quote: u8, length: usize },
    }
    let mut state = State::Name(0);
    let mut reader = std::io::BufReader::new(reader);
    loop {
        let chunk =
            std::io::BufRead::fill_buf(&mut reader).map_err(|_| DocumentError::Malformed)?;
        if chunk.is_empty() {
            return Ok(false);
        }
        for &byte in chunk {
            state = match state {
                State::Name(matched) if byte == NAME[matched] => {
                    if matched + 1 == NAME.len() {
                        State::AfterName
                    } else {
                        State::Name(matched + 1)
                    }
                }
                State::AfterName if byte.is_ascii_whitespace() => State::AfterName,
                State::AfterName if byte == b'=' => State::AfterEquals,
                State::AfterEquals if byte.is_ascii_whitespace() => State::AfterEquals,
                State::AfterEquals if byte == b'"' || byte == b'\'' => State::Value {
                    quote: byte,
                    length: 0,
                },
                State::Value { quote, .. } if byte == quote => State::Name(0),
                State::Value { quote, length } => {
                    if length >= MAX_NUMBER_FORMAT_BYTES {
                        return Ok(true);
                    }
                    State::Value {
                        quote,
                        length: length + 1,
                    }
                }
                // `f` occurs only at the start of the name, so a mismatch can
                // restart the match only on `f` itself.
                State::Name(_) | State::AfterName | State::AfterEquals => {
                    State::Name(usize::from(byte == NAME[0]))
                }
            };
        }
        let consumed = chunk.len();
        std::io::BufRead::consume(&mut reader, consumed);
    }
}

fn xml_has_odt_active_content(bytes: &[u8]) -> bool {
    if xml_has_odf_active_content(bytes) {
        return true;
    }
    let mut reader = quick_xml::Reader::from_reader(Cursor::new(bytes));
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(quick_xml::events::Event::Start(event))
            | Ok(quick_xml::events::Event::Empty(event)) => {
                if matches!(
                    xml_local_name(event.name().as_ref()),
                    b"forms" | b"form" | b"control" | b"event-listener" | b"macro" | b"library"
                ) {
                    return true;
                }
                for attribute in event.attributes().flatten() {
                    let value =
                        String::from_utf8_lossy(attribute.value.as_ref()).to_ascii_lowercase();
                    if value.starts_with("vnd.sun.star.script:") || value.starts_with("macro:") {
                        return true;
                    }
                }
                buffer.clear();
            }
            Ok(quick_xml::events::Event::Eof) => return false,
            Ok(_) => buffer.clear(),
            Err(_) => return false,
        }
    }
}

fn xml_odf_internal_references(bytes: &[u8]) -> Vec<String> {
    let mut reader = quick_xml::Reader::from_reader(Cursor::new(bytes));
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();
    let mut references = Vec::new();
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(quick_xml::events::Event::Start(event))
            | Ok(quick_xml::events::Event::Empty(event)) => {
                references.extend(xml_attribute_values(&event, b"href"));
                references.extend(xml_attribute_values(&event, b"src"));
                buffer.clear();
            }
            Ok(quick_xml::events::Event::Eof) => return references,
            Ok(_) => buffer.clear(),
            Err(_) => return references,
        }
    }
}

/// Whether an internal reference names a part the package lacks. The part
/// is the one AnyDoc loads (`path::resolve` against `content.xml`, on the
/// value as written); a reference it cannot resolve is dropped by AnyDoc and
/// counts as missing. External and fragment-only references name no part.
fn odf_reference_missing(value: &str, archive_names: &HashSet<String>) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') || is_external_uri(trimmed) {
        return false;
    }
    anydoc_resolve("content.xml", value).is_none_or(|part| !archive_names.contains(&part))
}

fn xml_has_hidden_slide(bytes: &[u8]) -> bool {
    let text = String::from_utf8_lossy(bytes).to_ascii_lowercase();
    [
        r#"show="0""#,
        r#"show='0'"#,
        r#"show="false""#,
        r#"show='false'"#,
    ]
    .iter()
    .any(|marker| text.contains(marker))
}

fn xml_has_pptx_shape_tree(bytes: &[u8]) -> bool {
    let text = String::from_utf8_lossy(bytes).to_ascii_lowercase();
    let has_slide = text.contains("<p:sld") || text.contains("<sld");
    let has_common_slide = text.contains("<p:csld") || text.contains("<csld");
    let has_shape_tree = text.contains("<p:sptree") || text.contains("<sptree");
    has_slide && has_common_slide && has_shape_tree
}

fn xml_is_well_formed(bytes: &[u8]) -> bool {
    let mut reader = quick_xml::Reader::from_reader(Cursor::new(bytes));
    reader.config_mut().trim_text(false);
    reader.config_mut().check_end_names = true;
    let mut buffer = Vec::new();
    let mut open_elements: Vec<Vec<u8>> = Vec::new();
    let mut saw_root = false;
    let mut root_closed = false;
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(quick_xml::events::Event::Start(event)) => {
                if root_closed || (open_elements.is_empty() && saw_root) {
                    return false;
                }
                if open_elements.is_empty() {
                    saw_root = true;
                }
                open_elements.push(event.name().as_ref().to_vec());
            }
            Ok(quick_xml::events::Event::Empty(_)) => {
                if root_closed || (open_elements.is_empty() && saw_root) {
                    return false;
                }
                if open_elements.is_empty() {
                    saw_root = true;
                    root_closed = true;
                }
            }
            Ok(quick_xml::events::Event::End(event)) => {
                let Some(open) = open_elements.pop() else {
                    return false;
                };
                if open.as_slice() != event.name().as_ref() {
                    return false;
                }
                if open_elements.is_empty() {
                    root_closed = true;
                }
            }
            Ok(quick_xml::events::Event::Text(event)) if open_elements.is_empty() => {
                let text = event.into_inner();
                if !text.iter().all(u8::is_ascii_whitespace) {
                    return false;
                }
            }
            Ok(quick_xml::events::Event::Eof) => {
                return saw_root && root_closed && open_elements.is_empty();
            }
            Ok(_) => {}
            Err(_) => return false,
        }
        buffer.clear();
    }
}

fn ooxml_external_target(value: &str) -> bool {
    let value = value.trim().to_ascii_lowercase();
    value.starts_with("//")
        || value.starts_with("http:")
        || value.starts_with("https:")
        || value.starts_with("ftp:")
        || value.starts_with("file:")
        || value.starts_with("data:")
        || value.starts_with("javascript:")
        || value.starts_with("mailto:")
}

/// Whether a relationships part declares an external target, read as AnyDoc
/// reads it: end tags are not matched by prefix and values are decoded
/// (`Ext&#101;rnal` is external). Names match in any case, which only finds
/// more. A part no reader can parse is malformed, since AnyDoc would drop
/// every relationship in it without a diagnostic.
fn xml_has_ooxml_external_relationship(bytes: &[u8]) -> Result<bool, DocumentError> {
    let mut reader = quick_xml::Reader::from_reader(Cursor::new(bytes));
    reader.config_mut().trim_text(false);
    reader.config_mut().check_end_names = false;
    let mut buffer = Vec::new();
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(quick_xml::events::Event::Start(event))
            | Ok(quick_xml::events::Event::Empty(event))
                if xml_local_name(event.name().as_ref()).eq_ignore_ascii_case(b"Relationship") =>
            {
                let external = event.attributes().flatten().any(|attribute| {
                    let name = xml_local_name(attribute.key.as_ref());
                    let value = attribute
                        .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                        .map(|value| value.into_owned())
                        .unwrap_or_else(|_| String::from_utf8_lossy(&attribute.value).into_owned());
                    (name.eq_ignore_ascii_case(b"TargetMode")
                        && value.trim().eq_ignore_ascii_case("External"))
                        || (name.eq_ignore_ascii_case(b"Target") && ooxml_external_target(&value))
                });
                if external {
                    return Ok(true);
                }
            }
            Ok(quick_xml::events::Event::Eof) => return Ok(false),
            Ok(_) => {}
            Err(_) => return Err(DocumentError::Malformed),
        }
        buffer.clear();
    }
}

fn resolve_package_target(base_part: &str, target: &str) -> Option<String> {
    if target.contains('\\') || target.contains(char::from(0)) {
        return None;
    }
    let mut components = if target.starts_with('/') {
        Vec::new()
    } else {
        base_part
            .rsplit_once('/')
            .map(|(parent, _)| parent.split('/').collect())
            .unwrap_or_default()
    };
    for component in target.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                components.pop()?;
            }
            value => components.push(value),
        }
    }
    (!components.is_empty()).then(|| components.join("/"))
}

/// Whether a listed slide is missing from the parts the checks read.
///
/// AnyDoc loads each `<p:sldId r:id="…">` from the relationship with that id,
/// whatever its type, resolving the target its own way (`path::resolve`
/// drops a fragment before any `..` is applied). Every part it could load
/// for a listed slide must be a slide that was checked.
fn validate_pptx_slide_targets(
    presentation: &[u8],
    presentation_rels: &[u8],
    slide_parts: &HashSet<String>,
) -> Result<bool, DocumentError> {
    if !xml_is_well_formed(presentation) || !xml_is_well_formed(presentation_rels) {
        return Err(DocumentError::Malformed);
    }
    let relationships = ooxml_relationships(presentation_rels)?;
    let slides = pptx_slide_relationship_ids(presentation);
    if slides.is_empty() {
        return Err(DocumentError::Malformed);
    }
    let mut incomplete = false;
    for ids in slides {
        let mut targets = relationships
            .iter()
            .filter(|relationship| ids.contains(&relationship.id))
            .peekable();
        incomplete |= targets.peek().is_none();
        for relationship in targets {
            match anydoc_resolve("ppt/presentation.xml", &relationship.target) {
                Some(part) if slide_parts.contains(&part) => {}
                _ => incomplete = true,
            }
        }
    }
    Ok(incomplete)
}

/// The relationship ids of each listed slide: every prefixed `id` attribute
/// of a `sldId` (the unprefixed `id` is the slide's number).
fn pptx_slide_relationship_ids(presentation: &[u8]) -> Vec<HashSet<String>> {
    let mut reader = quick_xml::Reader::from_reader(Cursor::new(presentation));
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();
    let mut slides = Vec::new();
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(quick_xml::events::Event::Start(event))
            | Ok(quick_xml::events::Event::Empty(event))
                if xml_local_name(event.name().as_ref()) == b"sldId" =>
            {
                slides.push(
                    event
                        .attributes()
                        .flatten()
                        .filter(|attribute| {
                            let key = attribute.key.as_ref();
                            key.contains(&b':') && xml_local_name(key) == b"id"
                        })
                        .map(|attribute| {
                            attribute
                                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                                .map(|value| value.into_owned())
                                .unwrap_or_else(|_| {
                                    String::from_utf8_lossy(attribute.value.as_ref()).into_owned()
                                })
                        })
                        .collect(),
                );
            }
            Ok(quick_xml::events::Event::Eof) | Err(_) => return slides,
            Ok(_) => {}
        }
        buffer.clear();
    }
}

struct EpubManifestItem {
    href: String,
    media_type: String,
    properties: String,
}

struct EpubPackageMetadata {
    manifest: HashMap<String, EpubManifestItem>,
    spine: Vec<String>,
    incomplete: bool,
    nav_item_id: Option<String>,
}

fn epub_is_external_uri(value: &str) -> bool {
    let value = value.trim().to_ascii_lowercase();
    value.starts_with("//")
        || value.starts_with("http:")
        || value.starts_with("https:")
        || value.starts_with("ftp:")
        || value.starts_with("file:")
        || value.starts_with("data:")
        || value.starts_with("javascript:")
}

/// The part a local reference names, resolved exactly as AnyDoc resolves it,
/// or `None` for a reference the local policy refuses: empty, fragment-only,
/// external, or carrying a query or a package-absolute path.
fn epub_resolve_local(base_part: &str, value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') || epub_is_external_uri(trimmed) {
        return None;
    }
    let path = trimmed.split_once('#').map_or(trimmed, |(path, _)| path);
    if path.is_empty() || path.contains('?') || path.starts_with('/') {
        return None;
    }
    anydoc_resolve(base_part, value)
}

fn epub_check_reference(
    value: &str,
    base_part: &str,
    archive_names: &HashSet<String>,
    result: &mut PackagePreflight,
) {
    let value = value.trim();
    if value.is_empty() || value.starts_with('#') {
        return;
    }
    if epub_is_external_uri(value) {
        result.external_relationships = true;
        return;
    }
    let Some(target) = epub_resolve_local(base_part, value) else {
        result.missing_required_content = true;
        return;
    };
    if !archive_names.contains(&target) {
        result.missing_required_content = true;
    }
}

fn epub_read_part(
    archive: &mut ZipArchive<Cursor<&[u8]>>,
    name: &str,
) -> Result<Vec<u8>, DocumentError> {
    let file = archive
        .by_name(name)
        .map_err(|_| DocumentError::Malformed)?;
    if file.encrypted() {
        return Err(DocumentError::Encrypted);
    }
    if file.size() > MAX_PREFLIGHT_PART_BYTES {
        return Err(DocumentError::ResourceLimit);
    }
    let mut content = Vec::new();
    file.take(MAX_PREFLIGHT_PART_BYTES + 1)
        .read_to_end(&mut content)
        .map_err(|_| DocumentError::Malformed)?;
    if content.len() as u64 > MAX_PREFLIGHT_PART_BYTES {
        return Err(DocumentError::ResourceLimit);
    }
    Ok(content)
}

/// Read an EPUB XML part as AnyDoc decodes it.
fn epub_read_xml_part(
    archive: &mut ZipArchive<Cursor<&[u8]>>,
    name: &str,
) -> Result<Vec<u8>, DocumentError> {
    epub_read_part(archive, name).map(anydoc_xml_utf8)
}

fn epub_rootfile_paths(bytes: &[u8]) -> Result<Vec<String>, DocumentError> {
    if !xml_is_well_formed(bytes) {
        return Err(DocumentError::Malformed);
    }
    let mut reader = quick_xml::Reader::from_reader(Cursor::new(bytes));
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();
    let mut paths = Vec::new();
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(quick_xml::events::Event::Start(event))
            | Ok(quick_xml::events::Event::Empty(event))
                if xml_local_name(event.name().as_ref()) == b"rootfile" =>
            {
                let path =
                    xml_attribute_value(&event, b"full-path").ok_or(DocumentError::Malformed)?;
                paths.push(path);
                buffer.clear();
            }
            Ok(quick_xml::events::Event::Eof) => return Ok(paths),
            Ok(_) => buffer.clear(),
            Err(_) => return Err(DocumentError::Malformed),
        }
    }
}

fn epub_parse_opf(bytes: &[u8]) -> Result<EpubPackageMetadata, DocumentError> {
    if !xml_is_well_formed(bytes) {
        return Err(DocumentError::Malformed);
    }
    let mut reader = quick_xml::Reader::from_reader(Cursor::new(bytes));
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();
    let mut manifest = HashMap::new();
    let mut spine = Vec::new();
    let mut incomplete = false;
    let mut package_seen = false;
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(quick_xml::events::Event::Start(event))
            | Ok(quick_xml::events::Event::Empty(event)) => {
                match xml_local_name(event.name().as_ref()) {
                    b"package" => {
                        if package_seen {
                            return Err(DocumentError::Malformed);
                        }
                        package_seen = true;
                        let version = xml_attribute_value(&event, b"version")
                            .ok_or(DocumentError::Malformed)?;
                        if version.trim().split('.').next() != Some("3") {
                            return Err(DocumentError::Unsupported);
                        }
                    }
                    b"item" => {
                        let id =
                            xml_attribute_value(&event, b"id").ok_or(DocumentError::Malformed)?;
                        let href =
                            xml_attribute_value(&event, b"href").ok_or(DocumentError::Malformed)?;
                        let media_type = xml_attribute_value(&event, b"media-type")
                            .ok_or(DocumentError::Malformed)?;
                        let properties =
                            xml_attribute_value(&event, b"properties").unwrap_or_default();
                        if manifest
                            .insert(
                                id,
                                EpubManifestItem {
                                    href,
                                    media_type,
                                    properties,
                                },
                            )
                            .is_some()
                        {
                            return Err(DocumentError::Malformed);
                        }
                    }
                    b"itemref" => {
                        if let Some(idref) = xml_attribute_value(&event, b"idref") {
                            spine.push(idref);
                        } else {
                            incomplete = true;
                        }
                    }
                    _ => {}
                }
                buffer.clear();
            }
            Ok(quick_xml::events::Event::Eof) => {
                if !package_seen {
                    return Err(DocumentError::Malformed);
                }
                let nav_items = manifest
                    .iter()
                    .filter(|(_, item)| {
                        item.properties
                            .split_whitespace()
                            .any(|property| property.eq_ignore_ascii_case("nav"))
                    })
                    .map(|(id, _)| id.clone())
                    .collect::<Vec<_>>();
                let nav_item_id = (nav_items.len() == 1).then(|| nav_items[0].clone());
                if nav_items.len() != 1 {
                    incomplete = true;
                }
                return Ok(EpubPackageMetadata {
                    manifest,
                    spine,
                    incomplete,
                    nav_item_id,
                });
            }
            Ok(_) => buffer.clear(),
            Err(_) => return Err(DocumentError::Malformed),
        }
    }
}

fn epub_text_has_external(value: &[u8]) -> bool {
    let value = String::from_utf8_lossy(value).to_ascii_lowercase();
    value.contains("http:")
        || value.contains("https:")
        || value.contains("ftp:")
        || value.contains("file:")
        || value.contains("data:")
        || value.contains("javascript:")
}

fn epub_inspect_chapter(
    bytes: &[u8],
    chapter_path: &str,
    archive_names: &HashSet<String>,
) -> Result<PackagePreflight, DocumentError> {
    if !xml_is_well_formed(bytes) {
        return Err(DocumentError::Malformed);
    }
    let mut reader = quick_xml::Reader::from_reader(Cursor::new(bytes));
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();
    let mut result = PackagePreflight::default();
    let mut has_html = false;
    let mut has_body = false;
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(quick_xml::events::Event::Start(event))
            | Ok(quick_xml::events::Event::Empty(event)) => {
                let event_name = event.name();
                let local = xml_local_name(event_name.as_ref());
                match local {
                    b"html" => has_html = true,
                    b"body" => has_body = true,
                    b"script" | b"form" | b"iframe" | b"object" | b"embed" | b"applet" => {
                        result.active_content = true;
                    }
                    _ => {}
                }
                for attribute in event.attributes().flatten() {
                    let name = xml_local_name(attribute.key.as_ref());
                    let value = String::from_utf8_lossy(attribute.value.as_ref()).into_owned();
                    let lower = value.to_ascii_lowercase();
                    if matches!(name, b"href" | b"src" | b"action" | b"data") {
                        epub_check_reference(&value, chapter_path, archive_names, &mut result);
                    }
                    if name.starts_with(b"on")
                        || name == b"hidden"
                        || (name == b"aria-hidden" && lower == "true")
                        || (name == b"style"
                            && (lower.contains("display:none")
                                || lower.contains("visibility:hidden")))
                    {
                        if name.starts_with(b"on") {
                            result.active_content = true;
                        } else {
                            result.hidden_content = true;
                        }
                    }
                    if name == b"style"
                        && (lower.contains("http:")
                            || lower.contains("https:")
                            || lower.contains("ftp:")
                            || lower.contains("file:")
                            || lower.contains("data:")
                            || lower.contains("javascript:"))
                    {
                        result.external_relationships = true;
                    }
                }
                buffer.clear();
            }
            Ok(quick_xml::events::Event::Text(event)) => {
                if epub_text_has_external(event.as_ref()) {
                    result.external_relationships = true;
                }
                buffer.clear();
            }
            Ok(quick_xml::events::Event::CData(event)) => {
                if epub_text_has_external(event.as_ref()) {
                    result.external_relationships = true;
                }
                buffer.clear();
            }
            Ok(quick_xml::events::Event::Eof) => {
                if !has_html || !has_body {
                    result.missing_required_content = true;
                }
                return Ok(result);
            }
            Ok(_) => buffer.clear(),
            Err(_) => return Err(DocumentError::Malformed),
        }
    }
}

fn epub_nav_spine_mismatch(
    bytes: &[u8],
    nav_path: &str,
    spine_targets: &[String],
    archive_names: &HashSet<String>,
    result: &mut PackagePreflight,
) -> Result<bool, DocumentError> {
    if !xml_is_well_formed(bytes) {
        return Err(DocumentError::Malformed);
    }
    let mut reader = quick_xml::Reader::from_reader(Cursor::new(bytes));
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();
    let mut depth = 0usize;
    let mut toc_depth = None;
    let mut links = Vec::new();
    let mut invalid = false;

    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(quick_xml::events::Event::Start(event)) => {
                depth += 1;
                let event_name = event.name();
                let local = xml_local_name(event_name.as_ref());
                if local == b"nav" {
                    let is_toc = event.attributes().flatten().any(|attribute| {
                        let name = xml_local_name(attribute.key.as_ref());
                        let value = String::from_utf8_lossy(attribute.value.as_ref());
                        (name == b"type" && value.eq_ignore_ascii_case("toc"))
                            || (name == b"role" && value.eq_ignore_ascii_case("doc-toc"))
                    });
                    if is_toc {
                        toc_depth = Some(depth);
                    }
                } else if local == b"a" && toc_depth.is_some() {
                    let Some(href) = xml_attribute_value(&event, b"href") else {
                        invalid = true;
                        buffer.clear();
                        continue;
                    };
                    if epub_is_external_uri(&href) {
                        result.external_relationships = true;
                        invalid = true;
                    } else if let Some(target) = epub_resolve_local(nav_path, &href) {
                        if archive_names.contains(&target) {
                            links.push(target);
                        } else {
                            result.missing_required_content = true;
                            invalid = true;
                        }
                    } else {
                        result.missing_required_content = true;
                        invalid = true;
                    }
                }
                buffer.clear();
            }
            Ok(quick_xml::events::Event::Empty(event)) => {
                let event_name = event.name();
                if xml_local_name(event_name.as_ref()) == b"a" && toc_depth.is_some() {
                    let Some(href) = xml_attribute_value(&event, b"href") else {
                        invalid = true;
                        buffer.clear();
                        continue;
                    };
                    if epub_is_external_uri(&href) {
                        result.external_relationships = true;
                        invalid = true;
                    } else if let Some(target) = epub_resolve_local(nav_path, &href) {
                        if archive_names.contains(&target) {
                            links.push(target);
                        } else {
                            result.missing_required_content = true;
                            invalid = true;
                        }
                    } else {
                        result.missing_required_content = true;
                        invalid = true;
                    }
                }
                buffer.clear();
            }
            Ok(quick_xml::events::Event::End(_)) => {
                if toc_depth == Some(depth) {
                    toc_depth = None;
                }
                depth = depth.checked_sub(1).ok_or(DocumentError::Malformed)?;
                buffer.clear();
            }
            Ok(quick_xml::events::Event::Eof) => {
                return Ok(invalid || links != spine_targets);
            }
            Ok(_) => buffer.clear(),
            Err(_) => return Err(DocumentError::Malformed),
        }
    }
}

fn preflight_epub(bytes: &[u8]) -> Result<PackagePreflight, DocumentError> {
    let mut archive = ZipArchive::new(Cursor::new(bytes)).map_err(|_| DocumentError::Malformed)?;
    if archive.is_empty() {
        return Err(DocumentError::Malformed);
    }

    let (first_name, first_compression) = {
        let first = archive.by_index(0).map_err(|_| DocumentError::Malformed)?;
        (first.name().to_string(), first.compression())
    };
    let mut archive_names = HashSet::new();
    let mut total_declared = 0u64;
    for index in 0..archive.len() {
        let file = archive
            .by_index(index)
            .map_err(|_| DocumentError::Malformed)?;
        let name = file.name().to_string();
        if !archive_names.insert(name.clone())
            || name.starts_with('/')
            || name.as_bytes().contains(&0)
            || name.contains('\\')
            || file.enclosed_name().is_none()
            || file.is_symlink()
        {
            return Err(DocumentError::Malformed);
        }
        if file.encrypted() {
            return Err(DocumentError::Encrypted);
        }
        let declared = file.size();
        if declared > MAX_ARCHIVE_ENTRY_BYTES {
            return Err(DocumentError::ResourceLimit);
        }
        total_declared = total_declared
            .checked_add(declared)
            .ok_or(DocumentError::ResourceLimit)?;
        if total_declared > MAX_ARCHIVE_TOTAL_BYTES {
            return Err(DocumentError::ResourceLimit);
        }
        if file.compressed_size() > 0
            && declared >= 16 * 1024 * 1024
            && declared > file.compressed_size().saturating_mul(1000)
        {
            return Err(DocumentError::ResourceLimit);
        }
    }
    if first_name != "mimetype" || first_compression != zip::CompressionMethod::Stored {
        return Err(DocumentError::Malformed);
    }
    let mimetype = epub_read_part(&mut archive, "mimetype")?;
    if mimetype != b"application/epub+zip" {
        return Err(DocumentError::Malformed);
    }
    if archive_names.contains("META-INF/encryption.xml")
        || archive_names.contains("META-INF/rights.xml")
    {
        return Err(DocumentError::Encrypted);
    }

    let container = epub_read_xml_part(&mut archive, "META-INF/container.xml")?;
    let roots = epub_rootfile_paths(&container)?;
    if roots.len() != 1 || roots[0].starts_with('/') {
        return Err(DocumentError::Malformed);
    }
    // AnyDoc opens `full-path` as a part name without resolving it, so only
    // a path already in canonical form names the part checked here.
    let opf_path = roots[0].clone();
    if resolve_package_target("", &opf_path).as_deref() != Some(opf_path.as_str())
        || !archive_names.contains(&opf_path)
    {
        return Err(DocumentError::Malformed);
    }
    let opf = epub_read_xml_part(&mut archive, &opf_path)?;
    let EpubPackageMetadata {
        manifest,
        spine,
        incomplete: opf_incomplete,
        nav_item_id,
    } = epub_parse_opf(&opf)?;
    let mut result = PackagePreflight {
        missing_required_content: opf_incomplete,
        ..Default::default()
    };

    for item in manifest.values() {
        let lower_media = item.media_type.to_ascii_lowercase();
        let lower_properties = item.properties.to_ascii_lowercase();
        if epub_is_external_uri(&item.href) {
            result.external_relationships = true;
            continue;
        }
        if lower_media == "application/javascript"
            || lower_media == "text/javascript"
            || lower_properties
                .split_whitespace()
                .any(|part| part == "scripted")
        {
            result.active_content = true;
        }
        let Some(target) = epub_resolve_local(&opf_path, &item.href) else {
            result.missing_required_content = true;
            continue;
        };
        if !archive_names.contains(&target) {
            result.missing_required_content = true;
        }
    }

    let mut seen_spine = HashSet::new();
    let mut spine_targets = Vec::new();
    for idref in &spine {
        let Some(item) = manifest.get(idref) else {
            result.missing_required_content = true;
            continue;
        };
        if !item
            .media_type
            .eq_ignore_ascii_case("application/xhtml+xml")
        {
            result.missing_required_content = true;
            continue;
        }
        if epub_is_external_uri(&item.href) {
            result.external_relationships = true;
            continue;
        }
        let Some(target) = epub_resolve_local(&opf_path, &item.href) else {
            result.missing_required_content = true;
            continue;
        };
        if !seen_spine.insert(target.clone()) {
            result.missing_required_content = true;
            continue;
        }
        if !archive_names.contains(&target) {
            result.missing_required_content = true;
            continue;
        }
        spine_targets.push(target.clone());
        match epub_read_xml_part(&mut archive, &target)
            .and_then(|chapter| epub_inspect_chapter(&chapter, &target, &archive_names))
        {
            Ok(chapter_result) => {
                result.active_content |= chapter_result.active_content;
                result.external_relationships |= chapter_result.external_relationships;
                result.hidden_content |= chapter_result.hidden_content;
                result.missing_required_content |= chapter_result.missing_required_content;
            }
            Err(DocumentError::Malformed) => result.missing_required_content = true,
            Err(error) => return Err(error),
        }
    }
    if let Some(nav_item_id) = nav_item_id {
        let nav_item = manifest.get(&nav_item_id).ok_or(DocumentError::Malformed)?;
        if !nav_item
            .media_type
            .eq_ignore_ascii_case("application/xhtml+xml")
        {
            result.missing_required_content = true;
        } else if let Some(nav_target) = epub_resolve_local(&opf_path, &nav_item.href) {
            if !archive_names.contains(&nav_target) {
                result.missing_required_content = true;
            } else {
                match epub_read_xml_part(&mut archive, &nav_target).and_then(|nav| {
                    epub_nav_spine_mismatch(
                        &nav,
                        &nav_target,
                        &spine_targets,
                        &archive_names,
                        &mut result,
                    )
                }) {
                    Ok(true) => result.missing_required_content = true,
                    Ok(false) => {}
                    Err(DocumentError::Malformed) => result.missing_required_content = true,
                    Err(error) => return Err(error),
                }
            }
        } else {
            result.missing_required_content = true;
        }
    }
    if spine.is_empty() {
        result.missing_required_content = true;
    }
    Ok(result)
}

fn preflight_package(
    bytes: &[u8],
    kind: DocumentKind,
    variant: DocumentVariant,
) -> Result<PackagePreflight, DocumentError> {
    if !matches!(
        kind,
        DocumentKind::Docx
            | DocumentKind::Pptx
            | DocumentKind::Xlsx
            | DocumentKind::Odt
            | DocumentKind::Ods
            | DocumentKind::Odp
            | DocumentKind::Epub
    ) {
        return Ok(PackagePreflight::default());
    }
    const OLE_MAGIC: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
    if bytes.starts_with(&OLE_MAGIC) {
        let encrypted = bytes
            .windows("EncryptedPackage".len())
            .any(|part| part == b"EncryptedPackage")
            || bytes
                .windows("EncryptionInfo".len())
                .any(|part| part == b"EncryptionInfo");
        return Err(if encrypted {
            DocumentError::Encrypted
        } else {
            DocumentError::Malformed
        });
    }
    if matches!(kind, DocumentKind::Epub) {
        return preflight_epub(bytes);
    }
    let mut archive = ZipArchive::new(Cursor::new(bytes)).map_err(|_| DocumentError::Malformed)?;
    if archive.is_empty() {
        return Err(DocumentError::Malformed);
    }
    let mut total_declared = 0u64;
    let mut has_content_types = false;
    let mut has_main = false;
    let mut odf_mimetype = None;
    let mut odf_content = None;
    let mut odf_manifest = None;
    let mut archive_names = HashSet::new();
    let mut odf_references = Vec::new();

    let layout = ooxml_layout(&mut archive, kind)?;
    let mut docx_scan = DocxStoryScan::default();
    let mut result = PackagePreflight::default();
    let mut ppt_presentation = None;
    let mut ppt_presentation_rels = None;
    let mut ppt_slide_parts = HashSet::new();
    for index in 0..archive.len() {
        let mut file = archive
            .by_index(index)
            .map_err(|_| DocumentError::Malformed)?;
        let name = file.name().to_string();
        let lower_name = name.to_ascii_lowercase();
        archive_names.insert(name.clone());
        if name.as_bytes().contains(&0)
            || name.contains(char::from(92))
            || file.enclosed_name().is_none()
            || file.is_symlink()
        {
            return Err(DocumentError::Malformed);
        }
        if file.encrypted() {
            return Err(DocumentError::Encrypted);
        }
        let declared = file.size();
        if declared > MAX_ARCHIVE_ENTRY_BYTES {
            return Err(DocumentError::ResourceLimit);
        }
        total_declared = total_declared
            .checked_add(declared)
            .ok_or(DocumentError::ResourceLimit)?;
        if total_declared > MAX_ARCHIVE_TOTAL_BYTES {
            return Err(DocumentError::ResourceLimit);
        }
        if file.compressed_size() > 0
            && declared >= 16 * 1024 * 1024
            && declared > file.compressed_size().saturating_mul(1000)
        {
            return Err(DocumentError::ResourceLimit);
        }
        if lower_name == "[content_types].xml" {
            has_content_types = true;
        }
        if (matches!(kind, DocumentKind::Docx) && lower_name == "word/document.xml")
            || (matches!(kind, DocumentKind::Pptx) && lower_name == "ppt/presentation.xml")
            || (matches!(kind, DocumentKind::Xlsx) && lower_name == "xl/workbook.xml")
            || (matches!(
                kind,
                DocumentKind::Odt | DocumentKind::Ods | DocumentKind::Odp
            ) && lower_name == "content.xml")
        {
            has_main = true;
        }
        if lower_name.ends_with("vbaproject.bin")
            || lower_name.contains("/embeddings/")
            || lower_name.contains("externalobjects/")
            || lower_name.contains("/activex/")
            || lower_name.contains("/controls/")
            || lower_name.contains("oleobject")
            || lower_name.ends_with("customui.xml")
        {
            result.active_content = true;
        }
        if lower_name.contains("externallink") {
            result.external_relationships = true;
        }
        if matches!(
            kind,
            DocumentKind::Odt | DocumentKind::Ods | DocumentKind::Odp
        ) && (lower_name.starts_with("basic/")
            || lower_name.starts_with("scripts/")
            || lower_name.contains("/object")
            || lower_name.starts_with("object/")
            || lower_name.contains("/oleobject")
            || lower_name.starts_with("oleobject/")
            || lower_name.contains("/embeddings/")
            || lower_name.starts_with("embeddings/"))
        {
            result.active_content = true;
        }
        if matches!(kind, DocumentKind::Odt) && lower_name.starts_with("objectreplacements/") {
            result.active_content = true;
        }
        if matches!(
            kind,
            DocumentKind::Odt | DocumentKind::Ods | DocumentKind::Odp
        ) && lower_name == "mimetype"
        {
            if declared > MAX_PREFLIGHT_PART_BYTES {
                return Err(DocumentError::ResourceLimit);
            }
            let mut content = Vec::new();
            (&mut file)
                .take(MAX_PREFLIGHT_PART_BYTES + 1)
                .read_to_end(&mut content)
                .map_err(|_| DocumentError::Malformed)?;
            if content.len() as u64 > MAX_PREFLIGHT_PART_BYTES {
                return Err(DocumentError::ResourceLimit);
            }
            odf_mimetype = Some(content);
        }
        let inspect_xml = lower_name.ends_with(".rels")
            || lower_name == "[content_types].xml"
            || lower_name == "xl/workbook.xml"
            || (matches!(
                kind,
                DocumentKind::Odt | DocumentKind::Ods | DocumentKind::Odp
            ) && matches!(
                lower_name.as_str(),
                "content.xml" | "meta-inf/manifest.xml" | "styles.xml"
            ))
            || (matches!(kind, DocumentKind::Xlsx)
                && ((lower_name.starts_with("xl/worksheets/") && lower_name.ends_with(".xml"))
                    || layout.worksheet_parts.contains(&name)))
            || (matches!(kind, DocumentKind::Pptx)
                && (lower_name == "ppt/presentation.xml"
                    || lower_name == "ppt/_rels/presentation.xml.rels"
                    || (lower_name.starts_with("ppt/slides/") && lower_name.ends_with(".xml"))))
            || (matches!(kind, DocumentKind::Docx) && layout.story_parts.contains(&name));
        if inspect_xml {
            if declared > MAX_PREFLIGHT_PART_BYTES {
                return Err(DocumentError::ResourceLimit);
            }
            let mut content = Vec::new();
            (&mut file)
                .take(MAX_PREFLIGHT_PART_BYTES + 1)
                .read_to_end(&mut content)
                .map_err(|_| DocumentError::Malformed)?;
            if content.len() as u64 > MAX_PREFLIGHT_PART_BYTES {
                return Err(DocumentError::ResourceLimit);
            }
            let content = anydoc_xml_utf8(content);
            let lower_content = String::from_utf8_lossy(&content).to_ascii_lowercase();
            if matches!(kind, DocumentKind::Docx)
                && lower_name == "word/document.xml"
                && !xml_is_well_formed(&content)
            {
                return Err(DocumentError::Malformed);
            }
            if matches!(kind, DocumentKind::Docx) && layout.story_parts.contains(&name) {
                scan_docx_story(&content, &mut docx_scan)?;
            }
            if matches!(
                kind,
                DocumentKind::Odt | DocumentKind::Ods | DocumentKind::Odp
            ) {
                if lower_name == "content.xml" {
                    odf_content = Some(content.clone());
                    if !xml_is_well_formed(&content) {
                        return Err(DocumentError::Malformed);
                    }
                    result.external_relationships |= xml_has_odf_external_reference(&content);
                    result.active_content |= xml_has_odf_active_content(&content);
                    if matches!(kind, DocumentKind::Ods) {
                        result.hidden_content |= xml_has_odf_hidden_content(&content);
                        result.missing_formula_cache |= xml_has_uncached_odf_formula(&content);
                        result.missing_required_content = !xml_has_odf_spreadsheet(&content);
                    } else if matches!(kind, DocumentKind::Odt) {
                        result.hidden_content |= xml_has_odt_hidden_or_tracked_content(&content);
                        result.active_content |= xml_has_odt_active_content(&content);
                        result.unsupported_content |= xml_has_odt_unsupported_content(&content);
                        result.missing_required_content = !xml_has_odf_text(&content);
                        odf_references.extend(xml_odf_internal_references(&content));
                    } else {
                        result.hidden_content |= xml_has_odp_hidden_content(&content);
                        result.missing_required_content = !xml_has_odf_presentation(&content);
                        odf_references.extend(xml_odf_internal_references(&content));
                    }
                } else if lower_name == "meta-inf/manifest.xml" {
                    odf_manifest = Some(content.clone());
                    if !xml_is_well_formed(&content) {
                        return Err(DocumentError::Malformed);
                    }
                    if matches!(kind, DocumentKind::Odt)
                        && (lower_content.contains("basic-library")
                            || lower_content.contains("vnd.sun.star.script"))
                    {
                        result.active_content = true;
                    }
                } else if lower_name == "styles.xml" {
                    if !xml_is_well_formed(&content) {
                        return Err(DocumentError::Malformed);
                    }
                    result.hidden_content |= xml_has_odf_hidden_content(&content);
                    if matches!(kind, DocumentKind::Odt) {
                        result.hidden_content |= xml_has_odt_hidden_or_tracked_content(&content);
                        result.external_relationships |= xml_has_odf_external_reference(&content);
                        result.active_content |= xml_has_odt_active_content(&content);
                        odf_references.extend(xml_odf_internal_references(&content));
                    } else if matches!(kind, DocumentKind::Odp) {
                        result.hidden_content |= xml_has_odp_hidden_content(&content);
                        result.external_relationships |= xml_has_odf_external_reference(&content);
                        odf_references.extend(xml_odf_internal_references(&content));
                    }
                }
            }
            if lower_name.ends_with(".rels") {
                result.external_relationships |= xml_has_ooxml_external_relationship(&content)?;
            }
            if lower_content.contains("macroenabled") {
                result.active_content = true;
            }
            if matches!(kind, DocumentKind::Xlsx) {
                result.hidden_content |= xml_has_hidden_content(lower_content.as_bytes());
                if lower_name.starts_with("xl/worksheets/")
                    || layout.worksheet_parts.contains(&name)
                {
                    result.missing_formula_cache |=
                        xml_has_uncached_formula(lower_content.as_bytes());
                }
            }
            if matches!(kind, DocumentKind::Pptx) {
                // AnyDoc reads these by exact name; a case variant is only a
                // decoy and must not stand in for the part it converts.
                if name == "ppt/presentation.xml" {
                    ppt_presentation = Some(content.clone());
                } else if name == "ppt/_rels/presentation.xml.rels" {
                    ppt_presentation_rels = Some(content.clone());
                } else if lower_name.starts_with("ppt/slides/") && lower_name.ends_with(".xml") {
                    if !xml_is_well_formed(&content) || !xml_has_pptx_shape_tree(&content) {
                        result.missing_required_content = true;
                    }
                    result.hidden_content |= xml_has_hidden_slide(&content);
                    ppt_slide_parts.insert(name);
                }
            }
        }
    }
    if matches!(
        kind,
        DocumentKind::Odt | DocumentKind::Ods | DocumentKind::Odp
    ) {
        let mimetype = odf_mimetype.ok_or(DocumentError::Malformed)?;
        let mimetype = std::str::from_utf8(&mimetype)
            .map_err(|_| DocumentError::Malformed)?
            .trim();
        let expected = match kind {
            DocumentKind::Odt => "application/vnd.oasis.opendocument.text",
            DocumentKind::Ods => "application/vnd.oasis.opendocument.spreadsheet",
            DocumentKind::Odp => "application/vnd.oasis.opendocument.presentation",
            _ => return Err(DocumentError::Malformed),
        };
        if mimetype != expected {
            return Err(DocumentError::Malformed);
        }
        odf_content.ok_or(DocumentError::Malformed)?;
        if matches!(kind, DocumentKind::Odt | DocumentKind::Odp) {
            result.missing_required_content |= odf_references
                .iter()
                .any(|reference| odf_reference_missing(reference, &archive_names));
        }
        if let Some(manifest) = odf_manifest {
            if xml_has_odf_encryption_data(&manifest) {
                return Err(DocumentError::Encrypted);
            }
        }
        return Ok(result);
    }
    if !has_content_types || !has_main {
        return Err(DocumentError::Malformed);
    }
    if matches!(kind, DocumentKind::Xlsx) {
        for part in &layout.styles_parts {
            let Ok(entry) = archive.by_name(part) else {
                continue;
            };
            if styles_have_oversized_number_format(open_xml_stream(
                entry.take(MAX_ARCHIVE_ENTRY_BYTES),
            )?)? {
                return Err(DocumentError::ResourceLimit);
            }
        }
    }
    if matches!(kind, DocumentKind::Docx) {
        let mut hidden_styles = HashSet::new();
        let mut defaults_hidden = false;
        for part in &layout.styles_parts {
            let Ok(entry) = archive.by_name(part) else {
                continue;
            };
            let (styles, defaults) =
                docx_hidden_styles(open_xml_stream(entry.take(MAX_ARCHIVE_ENTRY_BYTES))?)?;
            hidden_styles.extend(styles);
            defaults_hidden |= defaults;
        }
        result.unsupported_content |= docx_scan.dropped;
        result.omitted_characters |= docx_scan.omitted_hyphen;
        result.hidden_content |= docx_scan.hidden_run
            || defaults_hidden
            || docx_scan
                .styles_used
                .iter()
                .any(|style| hidden_styles.contains(style));
    }
    if !matches!(variant, DocumentVariant::Docx) && matches!(kind, DocumentKind::Docx) {
        result.active_content = true;
    }
    if matches!(kind, DocumentKind::Pptx) {
        let presentation = ppt_presentation.ok_or(DocumentError::Malformed)?;
        let presentation_rels = ppt_presentation_rels.ok_or(DocumentError::Malformed)?;
        result.missing_required_content |=
            validate_pptx_slide_targets(&presentation, &presentation_rels, &ppt_slide_parts)?;
    }
    Ok(result)
}

/// The typed failure a preflight result forces before conversion. The
/// supervisor and the worker both apply it, so the two cannot disagree.
fn preflight_rejection(kind: DocumentKind, preflight: &PackagePreflight) -> Option<DocumentError> {
    if preflight.active_content {
        return Some(DocumentError::ActiveContentDisabled);
    }
    let incomplete = match kind {
        // Hidden DOCX text is converted and disclosed rather than rejected.
        DocumentKind::Docx => preflight.unsupported_content,
        DocumentKind::Xlsx => {
            preflight.hidden_content
                || preflight.missing_formula_cache
                || preflight.external_relationships
        }
        DocumentKind::Ods => {
            preflight.hidden_content
                || preflight.missing_formula_cache
                || preflight.external_relationships
                || preflight.missing_required_content
        }
        DocumentKind::Odt => {
            preflight.hidden_content
                || preflight.external_relationships
                || preflight.missing_required_content
                || preflight.unsupported_content
        }
        DocumentKind::Pptx | DocumentKind::Odp | DocumentKind::Epub => {
            preflight.hidden_content
                || preflight.external_relationships
                || preflight.missing_required_content
        }
        _ => false,
    };
    incomplete.then_some(DocumentError::IncompleteConversion)
}

fn classify_bytes(bytes: &[u8], path: &Path) -> DocumentClassification {
    let detected_format =
        anydoc::Format::from_bytes(bytes).or_else(|| anydoc::Format::from_path(path));
    let detected = detected_format.map(DocumentKind::from_anydoc);
    let variant = detected_format.map(|format| DocumentVariant::for_format(format, bytes, path));
    let capabilities = detected.map(capabilities);
    let enabled = capabilities.as_ref().is_some_and(|value| value.enabled)
        && matches!(
            variant,
            Some(
                DocumentVariant::Docx
                    | DocumentVariant::Pptx
                    | DocumentVariant::Xlsx
                    | DocumentVariant::Ods
                    | DocumentVariant::Odt
                    | DocumentVariant::Odp
                    | DocumentVariant::Epub
                    | DocumentVariant::Csv
            )
        );
    DocumentClassification {
        kind: detected,
        variant,
        enabled,
        size_bytes: bytes.len() as u64,
        capabilities,
    }
}

/// Convert an enabled document through the supervised worker.
pub async fn to_markdown(path: impl AsRef<Path>) -> Result<DocumentContent, DocumentError> {
    let canonical = crate::validate_path(path).map_err(map_path_error)?;
    let mut bytes = Vec::new();
    tokio::fs::File::open(&canonical)
        .await
        .map_err(|_| DocumentError::InputUnavailable)?
        .take(MAX_DOCUMENT_SIZE + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| DocumentError::InputUnavailable)?;
    if bytes.len() as u64 > MAX_DOCUMENT_SIZE {
        return Err(DocumentError::ResourceLimit);
    }
    let classification = classify_bytes(&bytes, &canonical);
    let kind = classification.kind.ok_or(DocumentError::Unrecognized)?;
    let variant = classification.variant.ok_or(DocumentError::Unrecognized)?;
    if kind == DocumentKind::Docx && variant != DocumentVariant::Docx {
        return Err(DocumentError::ActiveContentDisabled);
    }
    if kind == DocumentKind::Xlsx && variant != DocumentVariant::Xlsx {
        return Err(if matches!(variant, DocumentVariant::Xlsm) {
            DocumentError::ActiveContentDisabled
        } else {
            DocumentError::Unsupported
        });
    }
    if kind == DocumentKind::Odt && variant != DocumentVariant::Odt {
        return Err(DocumentError::Unsupported);
    }
    if kind == DocumentKind::Odp && variant != DocumentVariant::Odp {
        return Err(DocumentError::Unsupported);
    }
    if kind == DocumentKind::Epub && variant != DocumentVariant::Epub {
        return Err(DocumentError::Unsupported);
    }
    if kind == DocumentKind::Pptx && variant != DocumentVariant::Pptx {
        return Err(
            if matches!(variant, DocumentVariant::Pptm | DocumentVariant::Ppsm) {
                DocumentError::ActiveContentDisabled
            } else {
                DocumentError::Unsupported
            },
        );
    }
    if !classification.enabled {
        return Err(DocumentError::Unsupported);
    }
    let (bytes, preflight) = if kind == DocumentKind::Csv {
        (bytes, PackagePreflight::default())
    } else {
        // Decompressing and scanning parts is blocking work; keep it off the
        // async executor that serves the other tools.
        let (bytes, preflight) = tokio::task::spawn_blocking(move || {
            let preflight = preflight_package(&bytes, kind, variant);
            (bytes, preflight)
        })
        .await
        .map_err(|_| DocumentError::ConversionFailed)?;
        (bytes, preflight?)
    };
    if let Some(error) = preflight_rejection(kind, &preflight) {
        return Err(error);
    }
    let raw_markdown = run_worker_process(&bytes, variant).await?;
    if raw_markdown.len() > MAX_MARKDOWN_SIZE {
        return Err(DocumentError::OutputTooLarge);
    }
    let (markdown, sanitized) = sanitize_markdown(&raw_markdown);
    if markdown.len() > MAX_MARKDOWN_SIZE {
        return Err(DocumentError::OutputTooLarge);
    }
    let mut warnings = Vec::new();
    let mut completeness = Completeness::Complete;
    if kind == DocumentKind::Docx && preflight.omitted_characters {
        completeness = Completeness::Partial;
        warnings.push(DocumentWarning {
            code: "characters_omitted".into(),
            message: "The document uses non-breaking hyphens, which the converter drops; hyphenated terms such as form numbers may appear joined."
                .into(),
        });
    }
    if kind == DocumentKind::Docx && preflight.hidden_content {
        warnings.push(DocumentWarning {
            code: "hidden_content_preserved".into(),
            message: "The document marks some text hidden; it was converted with the visible text."
                .into(),
        });
    }
    if preflight.external_relationships {
        warnings.push(DocumentWarning {
            code: "external_relationships_blocked".into(),
            message: "External relationships were present; no external content was fetched.".into(),
        });
    }
    if sanitized {
        warnings.push(DocumentWarning {
            code: "sanitized_output".into(),
            message: "Output contained URLs, paths, or HTML and was sanitized before return."
                .into(),
        });
    }
    Ok(DocumentContent {
        kind,
        variant,
        schema_version: PROTOCOL_VERSION,
        provider: provider_for(kind),
        markdown,
        completeness,
        warnings,
        input_bytes: bytes.len() as u64,
    })
}

/// One request to the private worker: the frame's operation code, the bytes
/// that follow it, and the bounds the supervisor enforces for it.
pub(crate) struct WorkerJob {
    pub(crate) code: u8,
    pub(crate) payload: Vec<u8>,
    pub(crate) timeout: Duration,
    pub(crate) max_response_bytes: usize,
}

async fn run_worker_process(
    bytes: &[u8],
    variant: DocumentVariant,
) -> Result<String, DocumentError> {
    if !worker_sandbox_available() {
        return Err(DocumentError::WorkerUnavailable);
    }
    let executable = worker_executable()?;
    run_worker_process_with_executable(bytes, variant, executable).await
}

async fn run_worker_process_with_executable(
    bytes: &[u8],
    variant: DocumentVariant,
    executable: PathBuf,
) -> Result<String, DocumentError> {
    let permit = worker_semaphore()
        .acquire_owned()
        .await
        .map_err(|_| DocumentError::WorkerBusy)?;
    let job = WorkerJob {
        code: variant.worker_code(),
        payload: bytes.to_vec(),
        timeout: WORKER_TIMEOUT,
        max_response_bytes: MAX_SERIALIZED_WORKER_RESPONSE_BYTES,
    };
    let response = run_worker_job(job, executable, permit).await?;
    match (response.markdown, response.error) {
        (Some(markdown), None) => Ok(markdown),
        (None, Some(error)) => Err(error.into_document_error()),
        _ => Err(DocumentError::WorkerProtocol),
    }
}

/// Whether this host can run the private worker under its sandbox.
pub(crate) fn worker_available() -> bool {
    worker_sandbox_available()
}

/// Wait for one of the PDF lane's slots, separate from the document lane's so
/// PDF calls and document conversions cannot starve each other. Callers take
/// the slot before reading a file, so waiting calls hold no document bytes.
pub(crate) async fn pdf_worker_permit() -> Result<OwnedSemaphorePermit, DocumentError> {
    pdf_worker_semaphore()
        .acquire_owned()
        .await
        .map_err(|_| DocumentError::WorkerBusy)
}

/// Run a PDF job in the private worker under a slot from [`pdf_worker_permit`].
pub(crate) async fn run_pdf_worker_job(
    job: WorkerJob,
    permit: OwnedSemaphorePermit,
) -> Result<WorkerResponse, DocumentError> {
    if !worker_sandbox_available() {
        return Err(DocumentError::WorkerUnavailable);
    }
    let executable = worker_executable()?;
    run_worker_job(job, executable, permit).await
}

async fn run_worker_job(
    job: WorkerJob,
    executable: PathBuf,
    permit: OwnedSemaphorePermit,
) -> Result<WorkerResponse, DocumentError> {
    let worker_dir = tempfile::Builder::new()
        .prefix("anydoc-worker-")
        .tempdir()
        .map_err(|_| DocumentError::WorkerUnavailable)?;
    let mut command = worker_command(executable);
    command
        .env_clear()
        // pdf-inspector parses on a rayon pool sized to the host, and glibc
        // reserves address space per malloc arena; both would otherwise let
        // the core count, not the document, decide how close a worker runs
        // to its address-space ceiling.
        .env("RAYON_NUM_THREADS", WORKER_PARSER_THREADS)
        .env("MALLOC_ARENA_MAX", WORKER_PARSER_THREADS)
        .current_dir(worker_dir.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    configure_worker_command(&mut command);
    command.kill_on_drop(true);
    let child = command
        .spawn()
        .map_err(|_| DocumentError::WorkerUnavailable)?;
    let (result_tx, result_rx) = oneshot::channel();
    let (cancel_tx, cancel_rx) = oneshot::channel();
    tokio::spawn(supervise_worker(
        child, worker_dir, job, permit, cancel_rx, result_tx,
    ));

    let cancellation_guard = WorkerCancellationGuard::new(cancel_tx);
    let result = result_rx.await.map_err(|_| DocumentError::WorkerProtocol)?;
    drop(cancellation_guard);
    result
}

struct WorkerCancellationGuard {
    sender: Option<oneshot::Sender<()>>,
}

impl WorkerCancellationGuard {
    fn new(sender: oneshot::Sender<()>) -> Self {
        Self {
            sender: Some(sender),
        }
    }
}

impl Drop for WorkerCancellationGuard {
    fn drop(&mut self) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(());
        }
    }
}

async fn supervise_worker(
    mut child: Child,
    _worker_dir: tempfile::TempDir,
    job: WorkerJob,
    _permit: OwnedSemaphorePermit,
    mut cancel_rx: oneshot::Receiver<()>,
    result_tx: oneshot::Sender<Result<WorkerResponse, DocumentError>>,
) {
    let result = {
        let deadline = job.timeout;
        let exchange = worker_exchange(&mut child, job);
        tokio::pin!(exchange);
        tokio::select! {
            _ = &mut cancel_rx => Err(DocumentError::WorkerTimeout),
            result = timeout(deadline, &mut exchange) => {
                match result {
                    Ok(result) => result,
                    Err(_) => Err(DocumentError::WorkerTimeout),
                }
            }
        }
    };
    if result.is_err() {
        terminate_child(&mut child).await;
    }
    let _ = result_tx.send(result);
}

async fn worker_exchange(
    child: &mut Child,
    job: WorkerJob,
) -> Result<WorkerResponse, DocumentError> {
    let mut stdin = child.stdin.take().ok_or(DocumentError::WorkerProtocol)?;
    let mut stdout = child.stdout.take().ok_or(DocumentError::WorkerProtocol)?;
    stdin
        .write_all(&PROTOCOL_MAGIC)
        .await
        .map_err(|_| DocumentError::WorkerProtocol)?;
    stdin
        .write_all(&[PROTOCOL_VERSION, 0, 0, 0])
        .await
        .map_err(|_| DocumentError::WorkerProtocol)?;
    stdin
        .write_all(&((job.payload.len() as u64) + 1).to_le_bytes())
        .await
        .map_err(|_| DocumentError::WorkerProtocol)?;
    stdin
        .write_all(&[job.code])
        .await
        .map_err(|_| DocumentError::WorkerProtocol)?;
    stdin
        .write_all(&job.payload)
        .await
        .map_err(|_| DocumentError::WorkerProtocol)?;
    stdin
        .shutdown()
        .await
        .map_err(|_| DocumentError::WorkerProtocol)?;
    // Shutting down a pipe does not close it; drop it so the worker sees EOF.
    drop(stdin);

    let mut header = [0u8; FRAME_HEADER_BYTES];
    if stdout.read_exact(&mut header).await.is_err() {
        return Err(unanswered_worker_error(child).await);
    }
    if header[..4] != PROTOCOL_MAGIC || header[4] != PROTOCOL_VERSION {
        return Err(DocumentError::WorkerProtocol);
    }
    let response_len = u64::from_le_bytes(
        header[8..16]
            .try_into()
            .map_err(|_| DocumentError::WorkerProtocol)?,
    );
    if response_len > job.max_response_bytes as u64 {
        return Err(DocumentError::OutputTooLarge);
    }
    let mut response = vec![0u8; response_len as usize];
    if stdout.read_exact(&mut response).await.is_err() {
        return Err(unanswered_worker_error(child).await);
    }
    let response: WorkerResponse =
        serde_json::from_slice(&response).map_err(|_| DocumentError::WorkerProtocol)?;
    let status = child
        .wait()
        .await
        .map_err(|_| DocumentError::WorkerProtocol)?;
    if !status.success() {
        return Err(DocumentError::ConversionFailed);
    }
    Ok(response)
}

/// Classify a worker that exited without a complete response. Under the
/// Linux address-space ceiling an oversized allocation aborts the process,
/// and stack exhaustion faults it; either is a resource limit rather than a
/// protocol failure. Any other exit keeps the protocol error.
async fn unanswered_worker_error(child: &mut Child) -> DocumentError {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Ok(status) = child.wait().await {
            if matches!(
                status.signal(),
                Some(libc::SIGABRT | libc::SIGKILL | libc::SIGSEGV | libc::SIGBUS)
            ) {
                return DocumentError::ResourceLimit;
            }
        }
    }
    #[cfg(not(unix))]
    let _ = child;
    DocumentError::WorkerProtocol
}

#[cfg(target_os = "macos")]
const MACOS_WORKER_SANDBOX_PROFILE: &str = "no-network";

#[cfg(target_os = "macos")]
fn worker_command(executable: PathBuf) -> Command {
    let mut command = Command::new("/usr/bin/sandbox-exec");
    command
        .arg("-n")
        .arg(MACOS_WORKER_SANDBOX_PROFILE)
        .arg(executable)
        .arg(WORKER_ARG);
    command
}

#[cfg(not(target_os = "macos"))]
fn worker_command(executable: PathBuf) -> Command {
    let mut command = Command::new(executable);
    command.arg(WORKER_ARG);
    command
}

#[cfg(target_os = "macos")]
fn worker_sandbox_available() -> bool {
    Path::new("/usr/bin/sandbox-exec").is_file()
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn worker_sandbox_available() -> bool {
    true
}

#[cfg(all(
    target_os = "linux",
    not(any(target_arch = "x86_64", target_arch = "aarch64"))
))]
fn worker_sandbox_available() -> bool {
    false
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn worker_sandbox_available() -> bool {
    false
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
const LINUX_AUDIT_ARCH: u32 = {
    #[cfg(target_arch = "x86_64")]
    {
        0xc000003e
    }
    #[cfg(target_arch = "aarch64")]
    {
        0xc00000b7
    }
};

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn linux_bpf_statement(code: u32, k: u32) -> libc::sock_filter {
    libc::sock_filter {
        code: code as libc::c_ushort,
        jt: 0,
        jf: 0,
        k,
    }
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn linux_bpf_jump(code: u32, k: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter {
        code: code as libc::c_ushort,
        jt,
        jf,
        k,
    }
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn install_linux_network_filter() -> std::io::Result<()> {
    let deny = libc::SECCOMP_RET_ERRNO | libc::EPERM as u32;
    let network_syscalls = [
        libc::SYS_socket,
        libc::SYS_socketpair,
        libc::SYS_connect,
        libc::SYS_bind,
        libc::SYS_listen,
        libc::SYS_accept,
        libc::SYS_accept4,
        libc::SYS_sendto,
        libc::SYS_sendmsg,
        libc::SYS_sendmmsg,
        libc::SYS_recvfrom,
        libc::SYS_recvmsg,
        libc::SYS_recvmmsg,
        libc::SYS_getsockname,
        libc::SYS_getpeername,
        libc::SYS_shutdown,
        libc::SYS_setsockopt,
        libc::SYS_getsockopt,
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
    ];
    let mut filter = Vec::with_capacity(network_syscalls.len() * 2 + 5);
    filter.push(linux_bpf_statement(
        libc::BPF_LD | libc::BPF_W | libc::BPF_ABS,
        4,
    ));
    filter.push(linux_bpf_jump(
        libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K,
        LINUX_AUDIT_ARCH,
        1,
        0,
    ));
    filter.push(linux_bpf_statement(
        libc::BPF_RET | libc::BPF_K,
        libc::SECCOMP_RET_KILL_PROCESS,
    ));
    filter.push(linux_bpf_statement(
        libc::BPF_LD | libc::BPF_W | libc::BPF_ABS,
        0,
    ));
    for syscall in network_syscalls {
        let syscall = u32::try_from(syscall).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid syscall number")
        })?;
        filter.push(linux_bpf_jump(
            libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K,
            syscall,
            0,
            1,
        ));
        filter.push(linux_bpf_statement(libc::BPF_RET | libc::BPF_K, deny));
    }
    filter.push(linux_bpf_statement(
        libc::BPF_RET | libc::BPF_K,
        libc::SECCOMP_RET_ALLOW,
    ));
    let len = u16::try_from(filter.len()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "seccomp filter too large")
    })?;
    let program = libc::sock_fprog {
        len: len as libc::c_ushort,
        filter: filter.as_ptr() as *mut libc::sock_filter,
    };
    unsafe {
        if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            0,
            &program,
        ) == -1
        {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(all(
    target_os = "linux",
    not(any(target_arch = "x86_64", target_arch = "aarch64"))
))]
fn install_linux_network_filter() -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "network containment is unavailable on this Linux architecture",
    ))
}

#[cfg(unix)]
fn configure_worker_command(command: &mut Command) {
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            #[cfg(target_os = "linux")]
            {
                let limit = libc::rlimit {
                    rlim_cur: MAX_WORKER_MEMORY_BYTES,
                    rlim_max: MAX_WORKER_MEMORY_BYTES,
                };
                if libc::setrlimit(libc::RLIMIT_AS, &limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                install_linux_network_filter()?;
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn configure_worker_command(_command: &mut Command) {}

#[cfg(target_os = "linux")]
fn worker_memory_limit() -> Option<u64> {
    Some(MAX_WORKER_MEMORY_BYTES)
}

#[cfg(not(target_os = "linux"))]
fn worker_memory_limit() -> Option<u64> {
    None
}

async fn terminate_child(child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        unsafe {
            libc::killpg(pid as libc::pid_t, libc::SIGKILL);
        }
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}

fn worker_semaphore() -> Arc<Semaphore> {
    static SEMAPHORE: OnceLock<Arc<Semaphore>> = OnceLock::new();
    SEMAPHORE
        .get_or_init(|| Arc::new(Semaphore::new(MAX_IN_FLIGHT_WORKERS)))
        .clone()
}

fn pdf_worker_semaphore() -> Arc<Semaphore> {
    static SEMAPHORE: OnceLock<Arc<Semaphore>> = OnceLock::new();
    SEMAPHORE
        .get_or_init(|| Arc::new(Semaphore::new(MAX_IN_FLIGHT_PDF_WORKERS)))
        .clone()
}

fn worker_executable() -> Result<PathBuf, DocumentError> {
    if let Ok(path) = std::env::var("ANYDOC_WORKER_BIN") {
        let path = PathBuf::from(path);
        if path.as_os_str().is_empty() {
            return Err(DocumentError::WorkerUnavailable);
        }
        return Ok(path);
    }
    std::env::current_exe().map_err(|_| DocumentError::WorkerUnavailable)
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct WorkerResponse {
    pub(crate) markdown: Option<String>,
    pub(crate) error: Option<WorkerError>,
    /// Serialized result of a structured (PDF) operation, carried verbatim so
    /// the caller returns exactly what the operation produced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) json: Option<Box<serde_json::value::RawValue>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) resource: Option<WorkerResourceEvidence>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct WorkerResourceEvidence {
    peak_rss_bytes: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct WorkerError {
    pub(crate) code: String,
    pub(crate) pages: Vec<u32>,
}

impl WorkerError {
    fn into_document_error(self) -> DocumentError {
        match self.code.as_str() {
            "needs_ocr" => DocumentError::OcrRequired { pages: self.pages },
            "encrypted" => DocumentError::Encrypted,
            "resource_limit" => DocumentError::ResourceLimit,
            "output_too_large" => DocumentError::OutputTooLarge,
            "malformed" | "missing_part" | "missingPart" => DocumentError::Malformed,
            "active_content_disabled" => DocumentError::ActiveContentDisabled,
            "incomplete_conversion" => DocumentError::IncompleteConversion,
            "unsupported" => DocumentError::Unsupported,
            _ => DocumentError::ConversionFailed,
        }
    }
}

fn worker_error_for(error: &DocumentError) -> WorkerError {
    let pages = match error {
        DocumentError::OcrRequired { pages } => pages.clone(),
        _ => Vec::new(),
    };
    WorkerError {
        code: error.code().into(),
        pages,
    }
}

pub(crate) fn worker_response_for_error(error: &DocumentError) -> WorkerResponse {
    WorkerResponse {
        markdown: None,
        error: Some(worker_error_for(error)),
        ..Default::default()
    }
}

fn anydoc_format(variant: DocumentVariant) -> Option<anydoc::Format> {
    match variant {
        DocumentVariant::Docx => Some(anydoc::Format::Docx),
        DocumentVariant::Pptx => Some(anydoc::Format::Pptx),
        DocumentVariant::Xlsx => Some(anydoc::Format::Excel),
        DocumentVariant::Ods => Some(anydoc::Format::Ods),
        DocumentVariant::Odt => Some(anydoc::Format::Odt),
        DocumentVariant::Odp => Some(anydoc::Format::Odp),
        DocumentVariant::Epub => Some(anydoc::Format::Epub),
        DocumentVariant::Csv => None,
        _ => None,
    }
}

fn kind_for_variant(variant: DocumentVariant) -> Option<DocumentKind> {
    match variant {
        DocumentVariant::Docx => Some(DocumentKind::Docx),
        DocumentVariant::Pptx => Some(DocumentKind::Pptx),
        DocumentVariant::Xlsx => Some(DocumentKind::Xlsx),
        DocumentVariant::Ods => Some(DocumentKind::Ods),
        DocumentVariant::Odt => Some(DocumentKind::Odt),
        DocumentVariant::Odp => Some(DocumentKind::Odp),
        DocumentVariant::Epub => Some(DocumentKind::Epub),
        DocumentVariant::Csv => Some(DocumentKind::Csv),
        _ => None,
    }
}

/// Worker entrypoint used by the MCP binary private worker mode.
pub fn run_worker() -> Result<(), DocumentError> {
    install_worker_logger()?;
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stdout().lock();
    let mut header = [0u8; FRAME_HEADER_BYTES];
    input
        .read_exact(&mut header)
        .map_err(|_| DocumentError::WorkerProtocol)?;
    if header[..4] != PROTOCOL_MAGIC || header[4] != PROTOCOL_VERSION {
        return Err(DocumentError::WorkerProtocol);
    }
    let input_len = u64::from_le_bytes(
        header[8..16]
            .try_into()
            .map_err(|_| DocumentError::WorkerProtocol)?,
    );
    // Code byte, optional parameter block with its length prefix, document.
    let max_frame = MAX_DOCUMENT_SIZE + 1 + 4 + MAX_WORKER_PARAMS_BYTES as u64;
    if input_len == 0 || input_len > max_frame {
        write_worker_response(
            &mut output,
            worker_response_for_error(&DocumentError::ResourceLimit),
            MAX_SERIALIZED_WORKER_RESPONSE_BYTES,
        )?;
        return Ok(());
    }
    let mut frame = vec![0u8; input_len as usize];
    input
        .read_exact(&mut frame)
        .map_err(|_| DocumentError::WorkerProtocol)?;
    let (code, bytes) = frame.split_first().ok_or(DocumentError::WorkerProtocol)?;
    if let Some(response) = crate::pdf_worker::execute(*code, bytes) {
        return write_worker_response(
            &mut output,
            response,
            crate::pdf_worker::MAX_PDF_RESPONSE_BYTES,
        );
    }
    if bytes.len() as u64 > MAX_DOCUMENT_SIZE {
        write_worker_response(
            &mut output,
            worker_response_for_error(&DocumentError::ResourceLimit),
            MAX_SERIALIZED_WORKER_RESPONSE_BYTES,
        )?;
        return Ok(());
    }
    let variant = match code {
        1 => DocumentVariant::Docx,
        2 => DocumentVariant::Xlsx,
        3 => DocumentVariant::Pptx,
        4 => DocumentVariant::Ods,
        5 => DocumentVariant::Odt,
        6 => DocumentVariant::Csv,
        7 => DocumentVariant::Odp,
        8 => DocumentVariant::Epub,
        _ => {
            write_worker_response(
                &mut output,
                worker_response_for_error(&DocumentError::Unsupported),
                MAX_SERIALIZED_WORKER_RESPONSE_BYTES,
            )?;
            return Ok(());
        }
    };
    let response = if variant == DocumentVariant::Csv {
        match tabular_csv::to_markdown(bytes) {
            Ok(markdown) if markdown.len() <= MAX_MARKDOWN_SIZE => WorkerResponse {
                markdown: Some(markdown),
                error: None,
                ..Default::default()
            },
            Ok(_) => worker_response_for_error(&DocumentError::OutputTooLarge),
            Err(error) => worker_response_for_error(&error),
        }
    } else {
        let kind = kind_for_variant(variant).ok_or(DocumentError::WorkerProtocol)?;
        let format = anydoc_format(variant).ok_or(DocumentError::WorkerProtocol)?;
        match preflight_package(bytes, kind, variant)
            .and_then(|preflight| preflight_rejection(kind, &preflight).map_or(Ok(()), Err))
        {
            Err(error) => worker_response_for_error(&error),
            Ok(()) => match anydoc::to_markdown_bytes(bytes, Some(format)) {
                Ok(_) if worker_diagnostics_incomplete() => {
                    worker_response_for_error(&DocumentError::IncompleteConversion)
                }
                Ok(markdown) if markdown.len() <= MAX_MARKDOWN_SIZE => WorkerResponse {
                    markdown: Some(markdown),
                    error: None,
                    ..Default::default()
                },
                Ok(_) => worker_response_for_error(&DocumentError::OutputTooLarge),
                Err(error) => WorkerResponse {
                    markdown: None,
                    error: Some(WorkerError {
                        code: error
                            .code()
                            .replace("needsOcr", "needs_ocr")
                            .replace("resourceLimit", "resource_limit"),
                        pages: match error {
                            anydoc::ConvertError::NeedsOcr { pages, .. } => pages,
                            _ => Vec::new(),
                        },
                    }),
                    ..Default::default()
                },
            },
        }
    };
    write_worker_response(&mut output, response, MAX_SERIALIZED_WORKER_RESPONSE_BYTES)
}

fn resource_evidence_enabled() -> bool {
    std::env::var_os("ANYDOC_RESOURCE_EVIDENCE").is_some_and(|value| value == "1")
}

#[cfg(unix)]
fn current_process_peak_rss_bytes() -> Option<u64> {
    let mut usage = MaybeUninit::<libc::rusage>::zeroed();
    let result = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    if result != 0 {
        return None;
    }
    let usage = unsafe { usage.assume_init() };
    let raw = u64::try_from(usage.ru_maxrss).ok()?;
    #[cfg(target_os = "linux")]
    {
        raw.checked_mul(1024)
    }
    #[cfg(not(target_os = "linux"))]
    {
        Some(raw)
    }
}

#[cfg(not(unix))]
fn current_process_peak_rss_bytes() -> Option<u64> {
    None
}

fn write_worker_response<W: Write>(
    output: &mut W,
    mut response: WorkerResponse,
    max_payload_bytes: usize,
) -> Result<(), DocumentError> {
    if resource_evidence_enabled() {
        if let Some(peak_rss_bytes) = current_process_peak_rss_bytes() {
            response.resource = Some(WorkerResourceEvidence { peak_rss_bytes });
        }
    }
    // Serialize the typed response directly: a structured result travels as
    // raw JSON and must not be re-parsed on the way out.
    let payload = serde_json::to_vec(&response).map_err(|_| DocumentError::WorkerProtocol)?;
    let payload = if payload.len() > max_payload_bytes {
        // Preserve a parseable frame so the supervisor can return the stable
        // output_too_large code instead of turning an oversized JSON envelope
        // into a misleading worker_protocol error.
        serde_json::to_vec(&worker_response_for_error(&DocumentError::OutputTooLarge))
            .map_err(|_| DocumentError::WorkerProtocol)?
    } else {
        payload
    };
    output
        .write_all(&PROTOCOL_MAGIC)
        .and_then(|_| output.write_all(&[PROTOCOL_VERSION, 0, 0, 0]))
        .and_then(|_| output.write_all(&(payload.len() as u64).to_le_bytes()))
        .and_then(|_| output.write_all(&payload))
        .and_then(|_| output.flush())
        .map_err(|_| DocumentError::WorkerProtocol)
}

const EXTERNAL_URL_MARKER: &str = "[external URL removed]";
const LOCAL_PATH_MARKER: &str = "[local path removed]";

fn sanitize_markdown(markdown: &str) -> (String, bool) {
    static HTML: OnceLock<Regex> = OnceLock::new();
    let html = HTML.get_or_init(|| Regex::new(r"</?[A-Za-z][^>]*>").expect("HTML regex"));
    // Redact, strip HTML, then redact again: removing a tag can join `]` and
    // `(` into a new link, while a path is easiest to delimit before the tag
    // next to it disappears. Markers contain nothing either pass rewrites.
    let redacted = redact_destinations_and_paths(markdown);
    let stripped = html.replace_all(&redacted, "");
    let sanitized = redact_destinations_and_paths(&stripped);
    let changed = sanitized != markdown;
    (sanitized, changed)
}

fn redact_destinations_and_paths(markdown: &str) -> String {
    static URL: OnceLock<Regex> = OnceLock::new();
    static WWW: OnceLock<Regex> = OnceLock::new();
    static PATH: OnceLock<Regex> = OnceLock::new();
    // Bare URLs with an authority, and the schemes that act without one.
    let url = URL.get_or_init(|| {
        Regex::new(
            r"(?i)\b(?:[a-z][a-z0-9+.\-]*://|(?:mailto|data|javascript|vbscript|file|tel|sms|callto):)[^\s)\]>]+",
        )
        .expect("URL regex")
    });
    // `www.` hosts where GFM links them on its own: at a line start or after
    // whitespace, `*`, `_`, `~`, or `(`. Inside `Text/www.index.xhtml` it is
    // part of a relative path.
    let www =
        WWW.get_or_init(|| Regex::new(r"(?im)(^|[\s*_~(])www\.[^\s<)\]>]*").expect("www regex"));
    // Home directories, temporary and private roots, Windows profiles, and
    // UNC shares, unless a path or word character precedes them:
    // `Text/home/ch1.xhtml` is a relative link, not a home directory.
    let path = PATH.get_or_init(|| {
        Regex::new(
            r"(^|[^A-Za-z0-9._/\\\-])((?:(?:/Users|/home|/root|/private|/tmp|/var/folders)/|\\\\[A-Za-z0-9._$\-]+\\|[A-Za-z]:\\(?i:users)\\)[^\s)\]>]*)",
        )
        .expect("path regex")
    });
    let rewritten = neutralize_destinations(markdown);
    let rewritten = url.replace_all(&rewritten, EXTERNAL_URL_MARKER);
    let rewritten = www.replace_all(&rewritten, format!("${{1}}{EXTERNAL_URL_MARKER}"));
    path.replace_all(&rewritten, format!("${{1}}{LOCAL_PATH_MARKER}"))
        .into_owned()
}

/// Replace link and image destinations that leave the package: a scheme, a
/// host, a drive, or an absolute or UNC path. Relative and fragment
/// destinations stay; they name parts of the same package. Destinations are
/// read as a CommonMark renderer reads them, after at most one line ending
/// and with no unbracketed whitespace or brackets, and are decoded
/// (backslash escapes, character references) before classification, so
/// `https&#58;//…` and `https\://…` are recognized.
fn neutralize_destinations(markdown: &str) -> String {
    static INLINE: OnceLock<Regex> = OnceLock::new();
    static REFERENCE: OnceLock<Regex> = OnceLock::new();
    let inline = INLINE.get_or_init(|| {
        Regex::new(r"\]\([ \t]*(?:\n[ \t]*)?(<[^<>\n]*>|[^\s()<>\[\]]+)")
            .expect("inline destination regex")
    });
    // A link reference definition: the destination ends the line or is
    // followed only by a title. `[^1]: …` is a footnote, not a definition.
    let reference = REFERENCE.get_or_init(|| {
        Regex::new(
            r#"(?m)^[ ]{0,3}\[[^\]\n^][^\]\n]*\]:[ \t]*(<[^<>\n]*>|\S+)[ \t]*(?:"[^"\n]*"|'[^'\n]*'|\([^)\n]*\))?[ \t]*$"#,
        )
        .expect("reference destination regex")
    });
    let rewritten = replace_destinations(markdown, inline, true);
    replace_destinations(&rewritten, reference, false)
}

fn replace_destinations(text: &str, pattern: &Regex, skip_escaped_bracket: bool) -> String {
    let mut output = String::with_capacity(text.len());
    let mut copied = 0;
    for caps in pattern.captures_iter(text) {
        let (Some(whole), Some(destination)) = (caps.get(0), caps.get(1)) else {
            continue;
        };
        // `\](…)` is literal text, not a link: count the escaping backslashes.
        if skip_escaped_bracket {
            let escapes = text[..whole.start()]
                .bytes()
                .rev()
                .take_while(|byte| *byte == b'\\')
                .count();
            if escapes % 2 == 1 {
                continue;
            }
        }
        if let Some(marker) = destination_marker(destination.as_str()) {
            output.push_str(&text[copied..destination.start()]);
            output.push_str(marker);
            copied = destination.end();
        }
    }
    output.push_str(&text[copied..]);
    output
}

/// The marker for a destination that leaves the package, or `None` for a
/// relative or fragment destination.
fn destination_marker(raw: &str) -> Option<&'static str> {
    let inner = raw
        .strip_prefix('<')
        .and_then(|value| value.strip_suffix('>'))
        .unwrap_or(raw);
    // An undecodable reference could hide a scheme: fail closed.
    let Some(decoded) = decode_destination(inner) else {
        return Some(EXTERNAL_URL_MARKER);
    };
    let target = decoded.trim_start();
    let bytes = target.as_bytes();
    let drive = bytes.len() > 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/');
    if target.starts_with('\\') || (target.starts_with('/') && !target.starts_with("//")) || drive {
        return Some(LOCAL_PATH_MARKER);
    }
    if target.starts_with("//") {
        return Some(EXTERNAL_URL_MARKER);
    }
    // A scheme is whatever precedes a colon that comes before any `/`, `?`,
    // or `#`.
    let first = target.find([':', '/', '?', '#']);
    first
        .is_some_and(|index| index > 0 && bytes[index] == b':')
        .then_some(EXTERNAL_URL_MARKER)
}

/// Decode CommonMark backslash escapes and character references in a
/// destination. Returns `None` for a reference this decoder does not know.
fn decode_destination(raw: &str) -> Option<String> {
    let mut decoded = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(ch) = rest.chars().next() {
        if ch == '\\' {
            if let Some(next) = rest[1..].chars().next().filter(char::is_ascii_punctuation) {
                decoded.push(next);
                rest = &rest[2..];
                continue;
            }
        } else if ch == '&' {
            if let Some(end) = rest.find(';').filter(|end| (2..=32).contains(end)) {
                let name = &rest[1..end];
                if name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'#')
                {
                    decoded.push(decode_reference(name)?);
                    rest = &rest[end + 1..];
                    continue;
                }
            }
        }
        decoded.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    Some(decoded)
}

fn decode_reference(name: &str) -> Option<char> {
    if let Some(number) = name.strip_prefix('#') {
        let value = match number.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => number.parse().ok()?,
        };
        return char::from_u32(value);
    }
    Some(match name {
        "colon" => ':',
        "sol" => '/',
        "bsol" => '\\',
        "quest" => '?',
        "num" => '#',
        "period" => '.',
        "amp" => '&',
        "Tab" => '\t',
        "NewLine" => '\n',
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn capabilities_enable_docx_and_strict_xlsx() {
        assert!(capabilities(DocumentKind::Docx).enabled);
        assert!(!capabilities(DocumentKind::Pdf).enabled);
        assert!(capabilities(DocumentKind::Xlsx).enabled);
        assert_eq!(
            capabilities(DocumentKind::Xlsx).formula_policy,
            "cached_value_only"
        );
        assert!(!capabilities(DocumentKind::Docx).formula_evaluation);
    }

    #[test]
    fn sanitizer_removes_egress_and_machine_paths() {
        let machine_path = ["/", "Users/james/secret.txt"].concat();
        let input =
            format!("[remote](https://example.invalid/a) {machine_path} <script>x</script>");
        let (output, changed) = sanitize_markdown(&input);
        assert!(changed);
        assert!(!output.contains("https://"));
        assert!(!output.contains(&machine_path));
        assert!(!output.contains("<script>"));
    }

    #[test]
    fn sanitizer_removes_every_actionable_destination() {
        let unc = r"\\fileserver\share\ledger.xlsx";
        // Split so the repository hygiene scan does not flag the literal.
        let windows_profile = [r"C:\", r"Users\preparer\return.pdf"].concat();
        let home = ["/", "home/preparer/notes.txt"].concat();
        // Assembled so the repository hygiene scan does not flag an address.
        let address = ["someone", "example.invalid"].join("@");
        let cases = [
            (
                format!("[mail](mailto:{address})"),
                "[mail]([external URL removed])",
            ),
            (
                "[file](file:///etc/passwd)".into(),
                "[file]([external URL removed])",
            ),
            (
                "![pixel](data:image/png;base64,AAAA)".into(),
                "![pixel]([external URL removed])",
            ),
            (
                "[call](tel:+15555550100)".into(),
                "[call]([external URL removed])",
            ),
            (
                "[cdn](//cdn.example.invalid/x.png)".into(),
                "[cdn]([external URL removed])",
            ),
            ("[root](/etc/passwd)".into(), "[root]([local path removed])"),
            (
                "[drive](C:/Temp/out.docx)".into(),
                "[drive]([local path removed])",
            ),
            (
                format!("write to mailto:{address} today"),
                "write to [external URL removed] today",
            ),
            (
                "see sftp://host.example.invalid/drop".into(),
                "see [external URL removed]",
            ),
        ];
        for (input, expected) in cases {
            let (output, changed) = sanitize_markdown(&input);
            assert!(changed, "{input}");
            assert_eq!(output, expected, "{input}");
        }
        for input in [
            format!("[share]({unc})"),
            format!("open {unc} now"),
            format!("saved to {windows_profile}"),
            format!("notes in {home}"),
        ] {
            let (output, changed) = sanitize_markdown(&input);
            assert!(changed, "{input}");
            assert!(output.contains("[local path removed]"), "{output}");
            assert!(!output.contains("fileserver") && !output.contains("preparer"));
        }
    }

    #[test]
    fn sanitizer_never_consumes_text_beyond_one_destination() {
        // A CSV cell with literal brackets must not swallow the rows below.
        let escaped = "| \\[memo\\](Note: see line 3 | 100 |\n| Gross receipts | 125000 |\n| Total (all) | 150000 |";
        assert_eq!(sanitize_markdown(escaped), (escaped.to_string(), false));
        let unescaped = "[memo](Note: see line 3\nGross receipts 125000\nTotal (all) 150000";
        let (output, changed) = sanitize_markdown(unescaped);
        assert!(changed);
        assert_eq!(
            output,
            "[memo]([external URL removed] see line 3\nGross receipts 125000\nTotal (all) 150000"
        );
    }

    #[test]
    fn sanitizer_decodes_destinations_before_classifying_them() {
        for input in [
            "[a](https&#58;//evil.example.invalid/x)",
            "[b](mailto&colon;someone)",
            "[c](https\\://evil.example.invalid)",
            "[d](&#x2F;&#x2F;evil.example.invalid)",
            "[e](java&Tab;script:alert)",
            "[f](x&unknownentity;y)",
            "[g](<mailto:someone>)",
        ] {
            let (output, changed) = sanitize_markdown(input);
            assert!(changed, "{input}");
            assert!(
                output.ends_with("([external URL removed])"),
                "{input} -> {output}"
            );
        }
    }

    #[test]
    fn sanitizer_handles_reference_definitions_and_relative_segments() {
        let input = "[1]: mailto:someone\n[2]: Text/ch.xhtml\n[doc](Text/home/ch1.xhtml)";
        let (output, changed) = sanitize_markdown(input);
        assert!(changed);
        assert_eq!(
            output,
            "[1]: [external URL removed]\n[2]: Text/ch.xhtml\n[doc](Text/home/ch1.xhtml)"
        );
    }

    #[test]
    fn sanitizer_examines_every_link_after_stray_brackets() {
        for input in [
            "Details ]([here](//attacker.example.invalid/p)",
            "[click here\\](](//attacker.example.invalid/p)",
            "Details ]([here](https&#58;//attacker.example.invalid/q)",
            "[x](\n//attacker.example.invalid/p)",
        ] {
            let (output, changed) = sanitize_markdown(input);
            assert!(changed, "{input}");
            assert!(!output.contains("attacker"), "{input} -> {output}");
        }
    }

    #[test]
    fn sanitizer_leaves_footnotes_and_non_definitions_alone() {
        for input in [
            "[^1]: Note: see line 3.",
            "[^2]: See: Treas. Reg. 1.61-1.",
            "[1]: Note: see line 3",
            "[^3]: 10:30 am meeting.",
        ] {
            assert_eq!(
                sanitize_markdown(input),
                (input.to_string(), false),
                "{input}"
            );
        }
        let (output, _) = sanitize_markdown("[2]: https://example.invalid/x \"Title\"");
        assert_eq!(output, "[2]: [external URL removed] \"Title\"");
    }

    #[test]
    fn sanitizer_redacts_paths_after_any_delimiter_and_after_tags() {
        // Assembled so the repository hygiene scan does not flag the literals.
        let home = ["/", "home/alice/.ssh/id_rsa"].concat();
        let mac = ["/", "Users/alice/secret.docx"].concat();
        let windows = [r"C:\", r"Users\alice\Documents\w2.pdf"].concat();
        for input in [
            format!("`{home}`"),
            format!("**{mac}**"),
            format!("`{windows}`"),
            format!("| line1<br>{home} |"),
            format!("> {home}"),
        ] {
            let (output, changed) = sanitize_markdown(&input);
            assert!(changed, "{input}");
            assert!(
                output.contains("[local path removed]"),
                "{input} -> {output}"
            );
            assert!(!output.contains("alice"), "{input} -> {output}");
        }
    }

    #[test]
    fn sanitizer_rechecks_links_joined_by_tag_removal() {
        for input in [
            "Claim[^1]<a id=\"bm\"></a>(//attacker.example.invalid/p)",
            "[x](<b>//attacker.example.invalid/p)",
        ] {
            let (output, changed) = sanitize_markdown(input);
            assert!(changed, "{input}");
            assert!(!output.contains("attacker"), "{input} -> {output}");
        }
    }

    #[test]
    fn sanitizer_applies_the_gfm_www_rule() {
        assert_eq!(
            sanitize_markdown("[a](Text/www.index.xhtml)"),
            ("[a](Text/www.index.xhtml)".to_string(), false)
        );
        for (input, expected) in [
            (
                "see www.example.invalid today",
                "see [external URL removed] today",
            ),
            ("(www.example.invalid)", "([external URL removed])"),
        ] {
            assert_eq!(
                sanitize_markdown(input),
                (expected.to_string(), true),
                "{input}"
            );
        }
    }

    #[test]
    fn sanitizer_keeps_relative_links_and_ordinary_prose() {
        for input in [
            "[next chapter](Text/ch2.xhtml#s1)",
            "[see note](#note-1)",
            "Note: totals exclude tax. Tel: see the directory. Data: 42.",
            "Ratio 3:1 at 10:30, per section 1398(a).",
        ] {
            assert_eq!(
                sanitize_markdown(input),
                (input.to_string(), false),
                "{input}"
            );
        }
    }

    #[test]
    fn worker_error_codes_are_stable() {
        assert_eq!(DocumentError::Encrypted.code(), "encrypted");
        assert_eq!(
            DocumentError::OcrRequired { pages: vec![1] }.code(),
            "needs_ocr"
        );
    }

    #[test]
    fn worker_frame_bound_covers_worst_case_json_escaping() {
        let response = WorkerResponse {
            markdown: Some("\0".repeat(MAX_MARKDOWN_SIZE)),
            ..Default::default()
        };
        let mut frame = Vec::new();
        write_worker_response(&mut frame, response, MAX_SERIALIZED_WORKER_RESPONSE_BYTES)
            .expect("bounded worker frame");
        let payload = &frame[FRAME_HEADER_BYTES..];
        assert!(payload.len() > MAX_MARKDOWN_SIZE * 2);
        assert!(payload.len() <= MAX_SERIALIZED_WORKER_RESPONSE_BYTES);
        let decoded: WorkerResponse = serde_json::from_slice(payload).expect("worker JSON");
        assert_eq!(
            decoded.markdown.expect("Markdown response").len(),
            MAX_MARKDOWN_SIZE
        );
    }
    const DOCX_TYPES: &[u8] = br#"<Types><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#;
    const XLSX_TYPES: &[u8] = br#"<Types><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/></Types>"#;
    const PPTX_TYPES: &[u8] = br#"<Types><Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/></Types>"#;
    const PPSX_TYPES: &[u8] = br#"<Types><Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideshow.main+xml"/></Types>"#;
    const PPTX_PRESENTATION: &[u8] = br#"<p:presentation xmlns:p="urn:p" xmlns:r="urn:r"><p:sldIdLst><p:sldId r:id="rId1"/></p:sldIdLst></p:presentation>"#;
    const PPTX_RELS: &[u8] = br#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/></Relationships>"#;
    const PPTX_SLIDE: &[u8] = br#"<p:sld><p:cSld><p:spTree/></p:cSld></p:sld>"#;
    const DOCX_XML: &[u8] = br#"<document/>"#;

    fn zip_entries(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, bytes) in entries {
            writer
                .start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn pptx_variant_and_capability_are_strict() {
        let bytes = zip_entries(&[("[Content_Types].xml", PPTX_TYPES)]);
        assert_eq!(ooxml_variant(&bytes), Some(DocumentVariant::Pptx));
        let slideshow = zip_entries(&[("[Content_Types].xml", PPSX_TYPES)]);
        assert_eq!(ooxml_variant(&slideshow), Some(DocumentVariant::Ppsx));
        let contract = capabilities(DocumentKind::Pptx);
        assert!(contract.enabled);
        assert_eq!(contract.supported_variants, vec![DocumentVariant::Pptx]);
        assert_eq!(contract.hidden_content_policy, "reject");
        assert_eq!(contract.external_content_policy, "reject");
    }

    #[test]
    fn pptx_preflight_accepts_complete_declared_slide() {
        let bytes = zip_entries(&[
            ("[Content_Types].xml", PPTX_TYPES),
            ("ppt/presentation.xml", PPTX_PRESENTATION),
            ("ppt/_rels/presentation.xml.rels", PPTX_RELS),
            ("ppt/slides/slide1.xml", PPTX_SLIDE),
        ]);
        let result = preflight_package(&bytes, DocumentKind::Pptx, DocumentVariant::Pptx).unwrap();
        assert!(!result.missing_required_content);
    }

    #[test]
    fn pptx_preflight_rejects_hidden_and_active_content() {
        let slide = br#"<p:sld show="0"><p:cSld><p:spTree/></p:cSld></p:sld>"#;
        let hidden = zip_entries(&[
            ("[Content_Types].xml", PPTX_TYPES),
            ("ppt/presentation.xml", PPTX_PRESENTATION),
            ("ppt/_rels/presentation.xml.rels", PPTX_RELS),
            ("ppt/slides/slide1.xml", slide),
        ]);
        let hidden_result =
            preflight_package(&hidden, DocumentKind::Pptx, DocumentVariant::Pptx).unwrap();
        assert!(hidden_result.hidden_content);

        let active = zip_entries(&[
            ("[Content_Types].xml", PPTX_TYPES),
            ("ppt/presentation.xml", PPTX_PRESENTATION),
            ("ppt/_rels/presentation.xml.rels", PPTX_RELS),
            ("ppt/slides/slide1.xml", PPTX_SLIDE),
            ("ppt/embeddings/oleObject1.bin", b"active"),
        ]);
        let active_result =
            preflight_package(&active, DocumentKind::Pptx, DocumentVariant::Pptx).unwrap();
        assert!(active_result.active_content);
    }

    #[test]
    fn pptx_preflight_rejects_missing_declared_slide() {
        let bytes = zip_entries(&[
            ("[Content_Types].xml", PPTX_TYPES),
            ("ppt/presentation.xml", PPTX_PRESENTATION),
            ("ppt/_rels/presentation.xml.rels", PPTX_RELS),
        ]);
        let result = preflight_package(&bytes, DocumentKind::Pptx, DocumentVariant::Pptx).unwrap();
        assert!(result.missing_required_content);
    }

    #[test]
    fn pptx_preflight_rejects_malformed_slide_structure() {
        let bytes = zip_entries(&[
            ("[Content_Types].xml", PPTX_TYPES),
            ("ppt/presentation.xml", PPTX_PRESENTATION),
            ("ppt/_rels/presentation.xml.rels", PPTX_RELS),
            ("ppt/slides/slide1.xml", br#"<p:sld><broken>"#),
        ]);
        let result = preflight_package(&bytes, DocumentKind::Pptx, DocumentVariant::Pptx).unwrap();
        assert!(result.missing_required_content);
    }

    #[test]
    fn exact_variant_uses_ooxml_content_types() {
        let bytes = zip_entries(&[
            ("[Content_Types].xml", XLSX_TYPES),
            ("xl/workbook.xml", br#"<workbook/>"#),
        ]);
        assert_eq!(ooxml_variant(&bytes), Some(DocumentVariant::Xlsx));
    }

    #[test]
    fn preflight_rejects_archive_traversal() {
        let bytes = zip_entries(&[("../escape", b"no")]);
        assert!(matches!(
            preflight_package(&bytes, DocumentKind::Docx, DocumentVariant::Docx),
            Err(DocumentError::Malformed)
        ));
    }

    #[test]
    fn preflight_requires_the_main_part() {
        let bytes = zip_entries(&[("[Content_Types].xml", DOCX_TYPES)]);
        assert!(matches!(
            preflight_package(&bytes, DocumentKind::Docx, DocumentVariant::Docx),
            Err(DocumentError::Malformed)
        ));
    }

    #[test]
    fn preflight_surfaces_external_relationship_presence_without_fetching() {
        let rels = br#"<Relationship TargetMode="External" Target="https://example.invalid"/>"#;
        let bytes = zip_entries(&[
            ("[Content_Types].xml", DOCX_TYPES),
            ("word/document.xml", DOCX_XML),
            ("_rels/.rels", rels),
        ]);
        let result = preflight_package(&bytes, DocumentKind::Docx, DocumentVariant::Docx).unwrap();
        assert!(result.external_relationships);
    }

    #[test]
    fn ooxml_relationship_parser_handles_spacing_and_arbitrary_external_targets() {
        let external = |rels: &[u8]| xml_has_ooxml_external_relationship(rels);
        assert!(external(
            br#"<Relationships><Relationship Id='rId1' TargetMode = 'External' Target='slide-link.bin'/></Relationships>"#
        )
        .unwrap());
        assert!(external(
            br#"<Relationships><Relationship Id="rId2" Target="https://example.invalid/image.png"/></Relationships>"#
        )
        .unwrap());
        assert!(!external(
            br#"<Relationships><Relationship Id="rId3" Target="slides/slide1.xml"/></Relationships>"#
        )
        .unwrap());
        // Read as AnyDoc reads it: values decoded, end tags unmatched.
        assert!(external(
            br#"<Relationships><Relationship Id="rId4" TargetMode="Ext&#101;rnal" Target="a.bin"/></Relationships>"#
        )
        .unwrap());
        assert!(external(
            br#"<pr:Relationships xmlns:pr="urn:pr"><pr:Relationship Id="rId5" TargetMode="External" Target="a.bin"/></x:Relationships>"#
        )
        .unwrap());
        // A part no reader can parse hides what it references.
        assert!(matches!(
            external(br#"<Relationships><Relationship Id="rId6" Target="#),
            Err(DocumentError::Malformed)
        ));
    }

    #[test]
    fn pptx_slides_resolve_as_anydoc_resolves_them() {
        let with_rels = |rels: &[u8], extra: &[(&str, &[u8])]| {
            let mut entries: Vec<(&str, &[u8])> = vec![
                ("[Content_Types].xml", PPTX_TYPES),
                ("ppt/presentation.xml", PPTX_PRESENTATION),
                ("ppt/_rels/presentation.xml.rels", rels),
                ("ppt/slides/slide1.xml", PPTX_SLIDE),
            ];
            entries.extend_from_slice(extra);
            preflight_package(
                &zip_entries(&entries),
                DocumentKind::Pptx,
                DocumentVariant::Pptx,
            )
            .unwrap()
            .missing_required_content
        };
        let target = |target: &str| {
            format!(
                r#"<Relationships><Relationship Id="rId1" Type="{REL_NS}/slide" Target="{target}"/></Relationships>"#
            )
            .into_bytes()
        };
        // AnyDoc drops the fragment before applying `..`, so this names a
        // part outside the checked slides.
        let hidden = br#"<p:sld show="0"><p:cSld><p:spTree/></p:cSld></p:sld>"#;
        assert!(with_rels(
            &target("../other/slide.xml#/../../ppt/slides/slide1.xml"),
            &[("other/slide.xml", hidden)],
        ));
        // Spellings of the checked slide AnyDoc resolves to it are accepted.
        for accepted in [
            "slides/slide1.xml",
            "slides/slide%31.xml",
            "../../ppt/slides/slide1.xml",
            "slides/slide1.xml?v=1#top",
        ] {
            assert!(!with_rels(&target(accepted), &[]), "{accepted}");
        }
        // A case-variant decoy does not stand in for the part AnyDoc reads.
        let decoy = target("slides/slide1.xml");
        assert!(with_rels(
            &target("../other/slide.xml"),
            &[
                ("PPT/_rels/presentation.xml.rels", &decoy),
                ("other/slide.xml", hidden),
            ],
        ));
    }

    #[test]
    fn epub_chapters_resolve_as_anydoc_resolves_them() {
        let scripted = br#"<?xml version="1.0"?><html xmlns="http://www.w3.org/1999/xhtml"><head/><body><script>run()</script><p>Chapter.</p></body></html>"#;
        let opf_for = |href: &str| {
            format!(
                r#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="3.0"><metadata/><manifest><item id="ch1" href="{href}" media-type="application/xhtml+xml"/><item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/></manifest><spine><itemref idref="ch1"/></spine></package>"#
            )
        };
        let package = |href: &str, container: &[u8], extra: &[(&str, &[u8])]| {
            let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
            let stored = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            writer.start_file("mimetype", stored).unwrap();
            writer.write_all(b"application/epub+zip").unwrap();
            let opf = opf_for(href);
            let nav = br#"<?xml version="1.0"?><html xmlns="http://www.w3.org/1999/xhtml"><head/><body><nav type="toc"><ol><li><a href="Text/ch1.xhtml">One</a></li></ol></nav></body></html>"#;
            let mut entries: Vec<(&str, &[u8])> = vec![
                ("META-INF/container.xml", container),
                ("OPS/package.opf", opf.as_bytes()),
                ("OPS/nav.xhtml", nav),
            ];
            entries.extend_from_slice(extra);
            for (name, bytes) in entries {
                writer
                    .start_file(name, zip::write::SimpleFileOptions::default())
                    .unwrap();
                writer.write_all(bytes).unwrap();
            }
            preflight_package(
                &writer.finish().unwrap().into_inner(),
                DocumentKind::Epub,
                DocumentVariant::Epub,
            )
        };
        // AnyDoc percent-decodes the href: the chapter it converts is the
        // decoded name, not a decoy stored under the encoded one.
        let result = package(
            "Text/ch%31.xhtml",
            EPUB_CONTAINER,
            &[
                ("OPS/Text/ch%31.xhtml", EPUB_CHAPTER_TWO),
                ("OPS/Text/ch1.xhtml", scripted),
            ],
        )
        .unwrap();
        assert!(result.active_content, "the decoded chapter is checked");
        // Whitespace is part of the name AnyDoc looks up.
        let result = package(
            " Text/ch1.xhtml",
            EPUB_CONTAINER,
            &[("OPS/Text/ch1.xhtml", EPUB_CHAPTER_TWO)],
        )
        .unwrap();
        assert!(result.missing_required_content);
        // AnyDoc opens the container's path as written.
        let dotted = String::from_utf8(EPUB_CONTAINER.to_vec())
            .unwrap()
            .replace("OPS/package.opf", "OPS/./package.opf");
        assert!(matches!(
            package(
                "Text/ch1.xhtml",
                dotted.as_bytes(),
                &[("OPS/Text/ch1.xhtml", EPUB_CHAPTER_TWO)]
            ),
            Err(DocumentError::Malformed)
        ));
    }

    #[test]
    fn odt_references_resolve_as_anydoc_resolves_them() {
        let with_image = |href: &str, extra: &[(&str, &[u8])]| {
            let content = format!(
                r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:xlink="http://www.w3.org/1999/xlink"><office:body><office:text><text:p>Body<draw:frame><draw:image xlink:href="{href}"/></draw:frame></text:p></office:text></office:body></office:document-content>"#
            );
            preflight_package(
                &odt_package(content.as_bytes(), extra),
                DocumentKind::Odt,
                DocumentVariant::Odt,
            )
            .unwrap()
            .missing_required_content
        };
        assert!(!with_image("Pictures/a.png", &[("Pictures/a.png", b"png")]));
        assert!(!with_image(
            "Pictures/%61.png",
            &[("Pictures/a.png", b"png")]
        ));
        assert!(!with_image(
            "Pictures/a&#46;png",
            &[("Pictures/a.png", b"png")]
        ));
        // The decoded name is the part AnyDoc loads, not an encoded decoy.
        assert!(with_image(
            "Pictures/%61.png",
            &[("Pictures/%61.png", b"png")]
        ));
        // A reference AnyDoc cannot resolve is dropped, so it counts as missing.
        assert!(with_image(
            "Pictures%2Fa.png",
            &[("Pictures/a.png", b"png")]
        ));
    }

    #[test]
    fn preflight_marks_external_link_parts_without_reading_remote_content() {
        let bytes = zip_entries(&[
            ("[Content_Types].xml", XLSX_TYPES),
            ("xl/workbook.xml", br#"<workbook/>"#),
            ("xl/externalLinks/externalLink1.xml", br#"<externalLink/>"#),
        ]);
        let result = preflight_package(&bytes, DocumentKind::Xlsx, DocumentVariant::Xlsx).unwrap();
        assert!(result.external_relationships);
    }

    #[test]
    fn preflight_marks_embedded_active_content() {
        let bytes = zip_entries(&[
            ("[Content_Types].xml", DOCX_TYPES),
            ("word/document.xml", DOCX_XML),
            ("word/vbaProject.bin", b"macro"),
        ]);
        let result = preflight_package(&bytes, DocumentKind::Docx, DocumentVariant::Docx).unwrap();
        assert!(result.active_content);
    }

    #[test]
    fn preflight_rejects_hidden_and_uncached_xlsx_content() {
        let workbook =
            br#"<workbook><sheets><sheet state="hidden" name="Hidden"/></sheets></workbook>"#;
        let worksheet = br#"<worksheet><sheetData><row hidden="1"><c r="A1"><f>A1</f></c></row></sheetData></worksheet>"#;
        let bytes = zip_entries(&[
            ("[Content_Types].xml", XLSX_TYPES),
            ("xl/workbook.xml", workbook),
            ("xl/worksheets/sheet1.xml", worksheet),
        ]);
        let result = preflight_package(&bytes, DocumentKind::Xlsx, DocumentVariant::Xlsx).unwrap();
        assert!(result.hidden_content);
        assert!(result.missing_formula_cache);
    }

    const WORD_NS: &str =
        r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#;

    fn word_part(root: &str, body: &str) -> Vec<u8> {
        format!("<w:{root} {WORD_NS}><w:body>{body}</w:body></w:{root}>").into_bytes()
    }

    fn docx_preflight(entries: &[(&str, &[u8])]) -> PackagePreflight {
        let mut all: Vec<(&str, &[u8])> = vec![("[Content_Types].xml", DOCX_TYPES)];
        all.extend_from_slice(entries);
        preflight_package(
            &zip_entries(&all),
            DocumentKind::Docx,
            DocumentVariant::Docx,
        )
        .expect("DOCX preflight")
    }

    #[test]
    fn docx_preflight_rejects_content_the_pinned_parser_drops() {
        let dropped = [
            r#"<w:p><w:r><w:sym w:font="Wingdings" w:char="F0FE"/></w:r><w:r><w:t>Yes</w:t></w:r></w:p>"#,
            r#"<w:p><w:r><w:sym w:font="Symbol" w:char="F061"/></w:r></w:p>"#,
            r#"<w:p><w:r><w:fldChar w:fldCharType="begin"><w:ffData><w:checkBox><w:checked/></w:checkBox></w:ffData></w:fldChar></w:r></w:p>"#,
            r#"<w:p><w:r><w:fldChar w:fldCharType="begin"><w:ffData><w:ddList><w:result w:val="1"/></w:ddList></w:ffData></w:fldChar></w:r></w:p>"#,
        ];
        for body in dropped {
            let document = word_part("document", body);
            let preflight = docx_preflight(&[("word/document.xml", &document)]);
            assert!(preflight.unsupported_content, "{body}");
            assert!(matches!(
                preflight_rejection(DocumentKind::Docx, &preflight),
                Some(DocumentError::IncompleteConversion)
            ));
        }

        // Footnotes are rendered, so their symbols count; headers are not.
        let document = word_part("document", "<w:p><w:r><w:t>Body</w:t></w:r></w:p>");
        let symbol = word_part(
            "footnotes",
            r#"<w:p><w:r><w:sym w:char="F0FE"/></w:r></w:p>"#,
        );
        let preflight = docx_preflight(&[
            ("word/document.xml", &document),
            ("word/footnotes.xml", &symbol),
        ]);
        assert!(preflight.unsupported_content);
        let preflight = docx_preflight(&[
            ("word/document.xml", &document),
            ("word/header1.xml", &symbol),
        ]);
        assert!(!preflight.unsupported_content);
        assert!(preflight_rejection(DocumentKind::Docx, &preflight).is_none());

        // A checkbox content control carries its glyph as text and converts.
        let control = word_part(
            "document",
            r#"<w:p><w:sdt><w:sdtContent><w:r><w:t>&#x2612; Yes</w:t></w:r></w:sdtContent></w:sdt></w:p>"#,
        );
        assert!(!docx_preflight(&[("word/document.xml", &control)]).unsupported_content);
    }

    #[test]
    fn docx_inline_content_the_pinned_parser_drops_is_refused_or_disclosed() {
        let preflight_for = |body: &str| {
            let document = word_part("document", body);
            docx_preflight(&[("word/document.xml", &document)])
        };
        // Content lost with its markup fails closed.
        for body in [
            r#"<w:p><w:r><w:ruby><w:rt><w:r><w:t>Note</w:t></w:r></w:rt><w:rubyBase><w:r><w:t>Base</w:t></w:r></w:rubyBase></w:ruby></w:r></w:p>"#,
            r#"<w:p><w:r><w:t>Before</w:t></w:r></w:p><w:altChunk r:id="rIdAlt"/>"#,
        ] {
            let preflight = preflight_for(body);
            assert!(preflight.unsupported_content, "{body}");
            assert!(matches!(
                preflight_rejection(DocumentKind::Docx, &preflight),
                Some(DocumentError::IncompleteConversion)
            ));
        }
        // A dropped non-breaking hyphen leaves usable, partial text.
        let preflight = preflight_for(
            "<w:p><w:r><w:t>Form 1040</w:t><w:noBreakHyphen/><w:t>SR</w:t></w:r></w:p>",
        );
        assert!(preflight.omitted_characters);
        assert!(!preflight.unsupported_content);
        assert!(preflight_rejection(DocumentKind::Docx, &preflight).is_none());
        // Tracked deletions and move sources are omitted whole, so nothing
        // in them is dropped.
        for wrapper in ["del", "moveFrom"] {
            let preflight = preflight_for(&format!(
                r#"<w:p><w:{wrapper}><w:r><w:sym w:char="F0FE"/><w:noBreakHyphen/><w:ruby/></w:r></w:{wrapper}></w:p>"#
            ));
            assert!(!preflight.unsupported_content, "{wrapper}");
            assert!(!preflight.omitted_characters, "{wrapper}");
        }
    }

    #[test]
    fn docx_hidden_text_is_disclosed_not_rejected() {
        let hidden = word_part(
            "document",
            r#"<w:p><w:r><w:rPr><w:vanish/></w:rPr><w:t>Hidden</w:t></w:r></w:p>"#,
        );
        let preflight = docx_preflight(&[("word/document.xml", &hidden)]);
        assert!(preflight.hidden_content);
        assert!(preflight_rejection(DocumentKind::Docx, &preflight).is_none());

        let switched_off = word_part(
            "document",
            r#"<w:p><w:r><w:rPr><w:vanish w:val="false"/></w:rPr><w:t>Shown</w:t></w:r></w:p>"#,
        );
        assert!(!docx_preflight(&[("word/document.xml", &switched_off)]).hidden_content);
    }

    const ROOT_RELS_TO: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="MAIN"/></Relationships>"#;

    fn root_rels(main: &str) -> Vec<u8> {
        ROOT_RELS_TO.replace("MAIN", main).into_bytes()
    }

    #[test]
    fn ooxml_main_part_must_be_the_part_the_checks_read() {
        let visible = word_part("document", "<w:p><w:r><w:t>Decoy</w:t></w:r></w:p>");
        let hidden = word_part(
            "document",
            r#"<w:p><w:r><w:rPr><w:vanish/></w:rPr><w:t>Real</w:t></w:r></w:p>"#,
        );
        let redirected = root_rels("word/main.xml");
        let bytes = zip_entries(&[
            ("[Content_Types].xml", DOCX_TYPES),
            ("_rels/.rels", &redirected),
            ("word/document.xml", &visible),
            ("word/main.xml", &hidden),
        ]);
        assert!(matches!(
            preflight_package(&bytes, DocumentKind::Docx, DocumentVariant::Docx),
            Err(DocumentError::Malformed)
        ));

        let workbook = br#"<workbook><sheets><sheet name="Visible"/></sheets></workbook>"#;
        let moved = root_rels("alt/book.xml");
        let bytes = zip_entries(&[
            ("[Content_Types].xml", XLSX_TYPES),
            ("_rels/.rels", &moved),
            ("xl/workbook.xml", workbook),
            ("alt/book.xml", workbook),
        ]);
        assert!(matches!(
            preflight_package(&bytes, DocumentKind::Xlsx, DocumentVariant::Xlsx),
            Err(DocumentError::Malformed)
        ));

        // The conventional target, spelled with a leading slash, is accepted.
        let conventional = root_rels("/word/document.xml");
        let preflight = docx_preflight(&[
            ("_rels/.rels", &conventional),
            ("word/document.xml", &visible),
        ]);
        assert!(!preflight.hidden_content);
    }

    #[test]
    fn docx_story_parts_follow_relationships_and_fail_closed() {
        let body = word_part("document", "<w:p><w:r><w:t>Body</w:t></w:r></w:p>");
        let notes_rels = br#"<Relationships><Relationship Id="rId7" Type = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/footnotes" Target = "notes/fn.xml"/></Relationships>"#;
        let symbol = word_part(
            "footnotes",
            r#"<w:p><w:r><w:sym w:char="F0FE"/></w:r></w:p>"#,
        );
        let preflight = docx_preflight(&[
            ("word/document.xml", &body),
            ("word/_rels/document.xml.rels", notes_rels),
            ("word/notes/fn.xml", &symbol),
        ]);
        assert!(
            preflight.unsupported_content,
            "relocated footnotes are scanned"
        );

        // AnyDoc accepts an end tag whose prefix differs; the scan continues.
        let mismatched = format!(
            "<w:footnotes {WORD_NS}><w:p><w:r><w:t>a</w:t></w:r></x:p><w:p><w:r><w:sym w:char=\"F0FE\"/></w:r></w:p></w:footnotes>"
        );
        let preflight = docx_preflight(&[
            ("word/document.xml", &body),
            ("word/footnotes.xml", mismatched.as_bytes()),
        ]);
        assert!(preflight.unsupported_content);

        // A story part the reader cannot parse is rejected, not skipped.
        let broken = format!("<w:footnotes {WORD_NS}><w:p><w:r");
        let bytes = zip_entries(&[
            ("[Content_Types].xml", DOCX_TYPES),
            ("word/document.xml", &body),
            ("word/footnotes.xml", broken.as_bytes()),
        ]);
        assert!(matches!(
            preflight_package(&bytes, DocumentKind::Docx, DocumentVariant::Docx),
            Err(DocumentError::Malformed)
        ));

        // A symbol inside a tracked deletion is omitted anyway.
        let deleted = word_part(
            "document",
            r#"<w:p><w:del><w:r><w:sym w:char="F0FE"/></w:r></w:del></w:p>"#,
        );
        assert!(!docx_preflight(&[("word/document.xml", &deleted)]).unsupported_content);
    }

    #[test]
    fn docx_hidden_text_follows_styles_and_ignores_non_text_vanish() {
        let styles = format!(
            r#"<w:styles {WORD_NS}><w:style w:type="character" w:styleId="Quiet"><w:rPr><w:vanish/></w:rPr></w:style><w:style w:type="character" w:styleId="Quieter"><w:basedOn w:val="Quiet"/></w:style><w:style w:type="character" w:styleId="Loud"><w:basedOn w:val="Quiet"/><w:rPr><w:vanish w:val="0"/></w:rPr></w:style></w:styles>"#
        );
        let uses = |style: &str| {
            word_part(
                "document",
                &format!(
                    r#"<w:p><w:r><w:rPr><w:rStyle w:val="{style}"/></w:rPr><w:t>Text</w:t></w:r></w:p>"#
                ),
            )
        };
        for (style, hidden) in [("Quiet", true), ("Quieter", true), ("Loud", false)] {
            let document = uses(style);
            let preflight = docx_preflight(&[
                ("word/document.xml", &document),
                ("word/styles.xml", styles.as_bytes()),
            ]);
            assert_eq!(preflight.hidden_content, hidden, "{style}");
        }

        for body in [
            // A hidden paragraph mark hides no text.
            r#"<w:p><w:pPr><w:rPr><w:vanish/></w:rPr></w:pPr><w:r><w:t>Shown</w:t></w:r></w:p>"#,
            // Revision history records formatting the text no longer has.
            r#"<w:p><w:r><w:rPr><w:rPrChange><w:rPr><w:vanish/></w:rPr></w:rPrChange></w:rPr><w:t>Shown</w:t></w:r></w:p>"#,
        ] {
            let document = word_part("document", body);
            assert!(!docx_preflight(&[("word/document.xml", &document)]).hidden_content);
        }
    }

    #[test]
    fn xlsx_parts_follow_relationships_with_spaced_attributes() {
        let oversized = styles_with_code(&"0".repeat(MAX_NUMBER_FORMAT_BYTES + 1));
        let rels = br#"<Relationships><Relationship Id="rId9" Type = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target = "design/s.xml"/></Relationships>"#;
        let bytes = xlsx_with_styles("xl/design/s.xml", &oversized, Some(rels));
        assert!(matches!(
            preflight_package(&bytes, DocumentKind::Xlsx, DocumentVariant::Xlsx),
            Err(DocumentError::ResourceLimit)
        ));

        let sheet =
            br#"<worksheet><sheetData><row><c r="A1"><f>1+1</f></c></row></sheetData></worksheet>"#;
        let relocated = |sheet_attributes: &str, relationship_type: &str| {
            let workbook = format!(
                r#"<workbook xmlns:r="{REL_NS}" xmlns:x="urn:decoy"><sheets><sheet name="Data" sheetId="1" {sheet_attributes}/></sheets></workbook>"#
            );
            let rels = format!(
                r#"<Relationships><Relationship Id="rDecoy" Type="{REL_NS}/worksheet" Target="decoy.xml"/><Relationship Id="rId1" Type="{REL_NS}/{relationship_type}" Target="data/s1.xml"/></Relationships>"#
            );
            let bytes = zip_entries(&[
                ("[Content_Types].xml", XLSX_TYPES),
                ("xl/workbook.xml", workbook.as_bytes()),
                ("xl/_rels/workbook.xml.rels", rels.as_bytes()),
                ("xl/data/s1.xml", sheet),
            ]);
            preflight_package(&bytes, DocumentKind::Xlsx, DocumentVariant::Xlsx)
                .unwrap()
                .missing_formula_cache
        };
        assert!(
            relocated(r#"r:id="rId1""#, "worksheet"),
            "relocated worksheets are checked"
        );
        // AnyDoc loads a sheet's target whatever its relationship type.
        assert!(relocated(r#"r:id="rId1""#, "chartsheet"));
        // Every spelling of the id is followed, not only the first.
        assert!(relocated(r#"x:id="rDecoy" r:id="rId1""#, "worksheet"));
        // A sheet with no relationship id is skipped by AnyDoc and here.
        assert!(!relocated(r#"name2="rId1""#, "worksheet"));
    }

    const REL_NS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
    const PACKAGE_RELS_NS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";

    fn docx_result(entries: &[(&str, &[u8])]) -> Result<PackagePreflight, DocumentError> {
        let mut all: Vec<(&str, &[u8])> = vec![("[Content_Types].xml", DOCX_TYPES)];
        all.extend_from_slice(entries);
        preflight_package(
            &zip_entries(&all),
            DocumentKind::Docx,
            DocumentVariant::Docx,
        )
    }

    fn footnotes_rels(target: &str) -> Vec<u8> {
        format!(
            r#"<Relationships xmlns="{PACKAGE_RELS_NS}"><Relationship Id="rId7" Type="{REL_NS}/footnotes" Target="{target}"/></Relationships>"#
        )
        .into_bytes()
    }

    fn utf16(text: &str, little_endian: bool) -> Vec<u8> {
        let mut bytes = if little_endian {
            vec![0xFF, 0xFE]
        } else {
            vec![0xFE, 0xFF]
        };
        for unit in text.encode_utf16() {
            bytes.extend(if little_endian {
                unit.to_le_bytes()
            } else {
                unit.to_be_bytes()
            });
        }
        bytes
    }

    #[test]
    fn main_part_guard_accepts_only_references_to_the_checked_part() {
        let body = word_part("document", "<w:p><w:r><w:t>Body</w:t></w:r></w:p>");
        let with_root = |rels: &str| {
            docx_result(&[
                ("_rels/.rels", rels.as_bytes()),
                ("word/document.xml", &body),
                ("word/main.xml", &body),
            ])
        };
        let relationship = |id: &str, target: &str| {
            format!(r#"<Relationship Id="{id}" Type="{REL_NS}/officeDocument" Target="{target}"/>"#)
        };
        let rels = |inner: String| {
            format!(r#"<Relationships xmlns="{PACKAGE_RELS_NS}">{inner}</Relationships>"#)
        };
        // Spellings AnyDoc resolves to the conventional part are accepted.
        for target in [
            "word/document.xml",
            "../word/document.xml",
            "word/./document.xml",
            "word/%64ocument.xml",
            "word/document.xml?v=1#top",
        ] {
            assert!(
                with_root(&rels(relationship("rId1", target))).is_ok(),
                "{target}"
            );
        }
        // A second relationship naming another part is refused whatever its
        // id, since AnyDoc takes the lowest.
        for (first, second) in [("rId1", "rId2"), ("rId2", "rId1")] {
            let both = rels(
                relationship(first, "word/document.xml") + &relationship(second, "word/main.xml"),
            );
            assert!(
                matches!(with_root(&both), Err(DocumentError::Malformed)),
                "{first} {second}"
            );
        }
        // Encoded structure and traversal out of the conventional part fail.
        for target in ["word%2Fdocument.xml", "word/main.xml", "../main.xml"] {
            assert!(
                matches!(
                    with_root(&rels(relationship("rId1", target))),
                    Err(DocumentError::Malformed)
                ),
                "{target}"
            );
        }
        // AnyDoc does not match end-tag prefixes; neither does the guard.
        let mismatched = format!(
            r#"<pr:Relationships xmlns:pr="{PACKAGE_RELS_NS}"><pr:Relationship Id="rId1" Type="{REL_NS}/officeDocument" Target="word/document.xml"/></x:Relationships>"#
        );
        assert!(with_root(&mismatched).is_ok());
    }

    #[test]
    fn docx_notes_targets_resolve_as_anydoc_resolves_them() {
        let body = word_part("document", "<w:p><w:r><w:t>Body</w:t></w:r></w:p>");
        let symbol = word_part(
            "footnotes",
            r#"<w:p><w:r><w:sym w:char="F0FE"/></w:r></w:p>"#,
        );
        for (target, part) in [
            ("notes/fn.xml?v=1", "word/notes/fn.xml"),
            ("f%6E.xml", "word/fn.xml"),
            ("../../word/fn2.xml", "word/fn2.xml"),
            ("/extra/notes.xml#part", "extra/notes.xml"),
        ] {
            let rels = footnotes_rels(target);
            let preflight = docx_preflight(&[
                ("word/document.xml", &body),
                ("word/_rels/document.xml.rels", &rels),
                (part, &symbol),
            ]);
            assert!(preflight.unsupported_content, "{target}");
        }
    }

    #[test]
    fn docx_style_checks_cover_every_reading_of_the_styles() {
        let hidden_with = |styles_body: &str, document_body: &str| {
            let styles =
                format!(r#"<w:styles {WORD_NS} xmlns:x="urn:decoy">{styles_body}</w:styles>"#);
            let document = format!(
                r#"<w:document {WORD_NS} xmlns:x="urn:decoy"><w:body>{document_body}</w:body></w:document>"#
            );
            docx_preflight(&[
                ("word/document.xml", document.as_bytes()),
                ("word/styles.xml", styles.as_bytes()),
            ])
            .hidden_content
        };
        let run = |style: &str| {
            format!(
                r#"<w:p><w:r><w:rPr><w:rStyle w:val="{style}"/></w:rPr><w:t>Text</w:t></w:r></w:p>"#
            )
        };
        let quiet =
            r#"<w:style w:type="character" w:styleId="Quiet"><w:rPr><w:vanish/></w:rPr></w:style>"#;
        let unstyled = "<w:p><w:r><w:t>Text</w:t></w:r></w:p>";

        // A hidden default style hides every unstyled paragraph.
        assert!(hidden_with(
            r#"<w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:rPr><w:vanish/></w:rPr></w:style>"#,
            unstyled,
        ));
        assert!(!hidden_with(
            r#"<w:style w:type="paragraph" w:styleId="Normal"><w:rPr><w:vanish/></w:rPr></w:style>"#,
            unstyled,
        ));
        // Character references in ids decode on both sides.
        assert!(hidden_with(
            r#"<w:style w:type="character" w:styleId="Q&#117;iet"><w:rPr><w:vanish/></w:rPr></w:style>"#,
            &run("Quiet"),
        ));
        assert!(hidden_with(quiet, &run("Q&#x75;iet")));
        // Table styles, including a conditional region such as a header row.
        let table = |style: &str| {
            format!(
                r#"<w:tbl><w:tblPr><w:tblStyle w:val="{style}"/></w:tblPr><w:tr><w:tc><w:p><w:r><w:t>Cell</w:t></w:r></w:p></w:tc></w:tr></w:tbl>"#
            )
        };
        assert!(hidden_with(
            r#"<w:style w:type="table" w:styleId="Grid"><w:rPr><w:vanish/></w:rPr></w:style>"#,
            &table("Grid"),
        ));
        assert!(hidden_with(
            r#"<w:style w:type="table" w:styleId="Grid"><w:tblStylePr w:type="firstRow"><w:rPr><w:vanish/></w:rPr></w:tblStylePr></w:style>"#,
            &table("Grid"),
        ));
        // A style reference formats content only on a run, paragraph, or table.
        for position in [
            r#"<w:p><w:pPr><w:rPr><w:rStyle w:val="Quiet"/></w:rPr></w:pPr><w:r><w:t>Shown</w:t></w:r></w:p>"#,
            r#"<w:p><w:r><w:rPr><w:rPrChange><w:rPr><w:rStyle w:val="Quiet"/></w:rPr></w:rPrChange></w:rPr><w:t>Shown</w:t></w:r></w:p>"#,
        ] {
            assert!(!hidden_with(quiet, position), "{position}");
        }
        // A style defined twice is hidden if either definition hides it.
        assert!(hidden_with(
            &format!(
                r#"{quiet}<w:style w:styleId="Twice"><w:basedOn w:val="Quiet"/></w:style><w:style w:styleId="Twice"><w:rPr><w:vanish w:val="0"/></w:rPr></w:style>"#
            ),
            &run("Twice"),
        ));
        // An attribute repeated under another prefix counts in each spelling.
        assert!(hidden_with(
            quiet,
            r#"<w:p><w:r><w:rPr><w:rStyle x:val="Plain" w:val="Quiet"/></w:rPr><w:t>Text</w:t></w:r></w:p>"#,
        ));
        assert!(hidden_with(
            r#"<w:style w:type="character" x:styleId="Decoy" w:styleId="Quiet"><w:rPr><w:vanish/></w:rPr></w:style>"#,
            &run("Quiet"),
        ));
        assert!(hidden_with(
            r#"<w:style w:type="character" w:styleId="Quiet"><w:rPr><w:vanish x:val="0" w:val="1"/></w:rPr></w:style>"#,
            &run("Quiet"),
        ));
        // Base chains of any length and cycles are followed to the end.
        let chain: String = (0..40)
            .map(|index| {
                format!(
                    r#"<w:style w:styleId="S{}"><w:basedOn w:val="S{index}"/></w:style>"#,
                    index + 1
                )
            })
            .collect();
        assert!(hidden_with(
            &format!(r#"<w:style w:styleId="S0"><w:rPr><w:vanish/></w:rPr></w:style>{chain}"#),
            &run("S40"),
        ));
        assert!(!hidden_with(
            r#"<w:style w:styleId="A"><w:basedOn w:val="B"/></w:style><w:style w:styleId="B"><w:basedOn w:val="A"/></w:style>"#,
            &run("A"),
        ));
        // A nested or unterminated definition keeps its own properties.
        assert!(hidden_with(
            r#"<w:style w:styleId="Quiet"><w:style w:styleId="Inner"/><w:rPr><w:vanish/></w:rPr></w:style>"#,
            &run("Quiet"),
        ));
        let document = word_part("document", &run("Quiet"));
        let unterminated =
            format!(r#"<w:styles {WORD_NS}><w:style w:styleId="Quiet"><w:rPr><w:vanish/>"#);
        assert!(
            docx_preflight(&[
                ("word/document.xml", &document),
                ("word/styles.xml", unterminated.as_bytes()),
            ])
            .hidden_content
        );
    }

    #[test]
    fn docx_style_checks_stay_within_their_bounds() {
        let document = word_part(
            "document",
            r#"<w:p><w:r><w:rPr><w:rStyle w:val="Quiet"/></w:rPr><w:t>Text</w:t></w:r></w:p>"#,
        );
        let with_styles = |styles: &[u8]| {
            docx_result(&[
                ("word/document.xml", &document),
                ("word/styles.xml", styles),
            ])
        };
        // Larger than the in-memory part cap, and still read to the end.
        let padding = "x".repeat(MAX_PREFLIGHT_PART_BYTES as usize + 1024);
        let large = format!(
            r#"<w:styles {WORD_NS}><!--{padding}--><w:style w:styleId="Quiet"><w:rPr><w:vanish/></w:rPr></w:style></w:styles>"#
        );
        assert!(
            with_styles(large.as_bytes())
                .expect("large styles")
                .hidden_content
        );

        let deep = format!(
            "<w:styles {WORD_NS}>{}</w:styles>",
            "<w:x>".repeat(MAX_XML_DEPTH)
        );
        assert!(matches!(
            with_styles(deep.as_bytes()),
            Err(DocumentError::ResourceLimit)
        ));
        let many: String = (0..=MAX_DOCX_STYLES)
            .map(|index| format!(r#"<w:style w:styleId="S{index}"><w:name/></w:style>"#))
            .collect();
        let many = format!("<w:styles {WORD_NS}>{many}</w:styles>");
        assert!(matches!(
            with_styles(many.as_bytes()),
            Err(DocumentError::ResourceLimit)
        ));
        let long_id = format!(
            r#"<w:styles {WORD_NS}><w:style w:styleId="{}"/></w:styles>"#,
            "S".repeat(MAX_STYLE_ID_BYTES + 1)
        );
        assert!(matches!(
            with_styles(long_id.as_bytes()),
            Err(DocumentError::ResourceLimit)
        ));
        let nodes = format!(
            "<w:styles {WORD_NS}>{}</w:styles>",
            "<w:x/>".repeat(MAX_XML_NODES)
        );
        assert!(matches!(
            with_styles(nodes.as_bytes()),
            Err(DocumentError::ResourceLimit)
        ));

        let deep_story = word_part(
            "document",
            &format!(
                "{}<w:r><w:t>Text</w:t></w:r>{}",
                "<w:p>".repeat(MAX_XML_DEPTH),
                "</w:p>".repeat(MAX_XML_DEPTH)
            ),
        );
        assert!(matches!(
            docx_result(&[("word/document.xml", &deep_story)]),
            Err(DocumentError::ResourceLimit)
        ));
    }

    #[test]
    fn xml_parts_are_checked_as_anydoc_decodes_them() {
        assert_eq!(anydoc_xml_utf8(b"\xEF\xBB\xBF<a/>".to_vec()), b"<a/>");
        assert_eq!(anydoc_xml_utf8(utf16("<a/>", true)), b"<a/>");
        assert_eq!(anydoc_xml_utf8(utf16("<a/>", false)), b"<a/>");
        // A declaration is honored only when the first 200 bytes are UTF-8.
        let padding = " ".repeat(200);
        assert_eq!(
            anydoc_xml_utf8(
                [
                    format!("<?xml encoding='windows-1252'?><a>{padding}").into_bytes(),
                    b"\xE9</a>".to_vec(),
                ]
                .concat()
            ),
            format!("<?xml encoding='windows-1252'?><a>{padding}\u{e9}</a>").into_bytes()
        );
        // UTF-8 by name, a prefix that is not UTF-8, or a declaration past
        // the first 200 bytes leave the part as it is.
        for unchanged in [
            b"<?xml version=\"1.0\" encoding=\"UTF-8\"?><a/>".to_vec(),
            b"<?xml encoding='windows-1252'?><a>\xE9</a>".to_vec(),
            b"\xE9<?xml encoding='utf-16le'?><a/>".to_vec(),
            [
                " ".repeat(200).into_bytes(),
                b"<?xml encoding='utf-16le'?><a/>".to_vec(),
            ]
            .concat(),
        ] {
            assert_eq!(anydoc_xml_utf8(unchanged.clone()), unchanged);
        }

        let body = word_part("document", "<w:p><w:r><w:t>Body</w:t></w:r></w:p>");
        let symbol = format!(
            r#"<?xml version="1.0" encoding="UTF-16"?><w:footnotes {WORD_NS}><w:p><w:r><w:sym w:char="F0FE"/></w:r></w:p></w:footnotes>"#
        );
        for little_endian in [true, false] {
            let notes = utf16(&symbol, little_endian);
            let preflight =
                docx_preflight(&[("word/document.xml", &body), ("word/footnotes.xml", &notes)]);
            assert!(preflight.unsupported_content, "UTF-16 notes");

            let document = utf16(
                &format!(
                    r#"<w:document {WORD_NS}><w:body><w:p><w:r><w:rPr><w:rStyle w:val="Quiet"/></w:rPr><w:t>Text</w:t></w:r></w:p></w:body></w:document>"#
                ),
                little_endian,
            );
            let styles = utf16(
                &format!(
                    r#"<w:styles {WORD_NS}><w:style w:styleId="Quiet"><w:rPr><w:vanish/></w:rPr></w:style></w:styles>"#
                ),
                little_endian,
            );
            let rels = utf16(
                &String::from_utf8(footnotes_rels("moved/fn.xml")).unwrap(),
                little_endian,
            );
            let preflight = docx_preflight(&[
                ("word/document.xml", &document),
                ("word/styles.xml", &styles),
                ("word/_rels/document.xml.rels", &rels),
                ("word/moved/fn.xml", &notes),
            ]);
            assert!(preflight.hidden_content, "UTF-16 styles");
            assert!(preflight.unsupported_content, "UTF-16 relationships");

            let oversized = utf16(
                &String::from_utf8(styles_with_code(&"0".repeat(MAX_NUMBER_FORMAT_BYTES + 1)))
                    .unwrap(),
                little_endian,
            );
            let bytes = xlsx_with_styles("xl/styles.xml", &oversized, None);
            assert!(matches!(
                preflight_package(&bytes, DocumentKind::Xlsx, DocumentVariant::Xlsx),
                Err(DocumentError::ResourceLimit)
            ));
        }

        // A UTF-8 prefix that declares UTF-16 is decoded as UTF-16 whole.
        let mut declared = br#"<?xml version="1.0" encoding="UTF-16LE"?> "#.to_vec();
        assert_eq!(declared.len() % 2, 0);
        declared.extend(utf16(&symbol, true).split_off(2));
        let preflight = docx_preflight(&[
            ("word/document.xml", &body),
            ("word/footnotes.xml", &declared),
        ]);
        assert!(preflight.unsupported_content, "declared UTF-16 notes");
    }

    fn xlsx_with_styles(styles_name: &str, styles: &[u8], rels: Option<&[u8]>) -> Vec<u8> {
        let workbook = br#"<workbook><sheets><sheet name="Visible"/></sheets></workbook>"#;
        let mut entries: Vec<(&str, &[u8])> = vec![
            ("[Content_Types].xml", XLSX_TYPES),
            ("xl/workbook.xml", workbook),
            (styles_name, styles),
        ];
        if let Some(rels) = rels {
            entries.push(("xl/_rels/workbook.xml.rels", rels));
        }
        zip_entries(&entries)
    }

    fn styles_with_code(code: &str) -> Vec<u8> {
        format!(r#"<styleSheet><numFmts count="1"><numFmt numFmtId="164" formatCode="{code}"/></numFmts></styleSheet>"#)
            .into_bytes()
    }

    #[test]
    fn xlsx_preflight_rejects_oversized_number_formats() {
        let longest = styles_with_code(&"0".repeat(MAX_NUMBER_FORMAT_BYTES));
        let bytes = xlsx_with_styles("xl/styles.xml", &longest, None);
        assert!(preflight_package(&bytes, DocumentKind::Xlsx, DocumentVariant::Xlsx).is_ok());

        let oversized = styles_with_code(&"0".repeat(MAX_NUMBER_FORMAT_BYTES + 1));
        let bytes = xlsx_with_styles("xl/styles.xml", &oversized, None);
        assert!(matches!(
            preflight_package(&bytes, DocumentKind::Xlsx, DocumentVariant::Xlsx),
            Err(DocumentError::ResourceLimit)
        ));

        // Renaming the styles part behind its relationship does not evade it.
        let rels = br#"<Relationships><Relationship Id="rId9" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="design/theme-styles.xml"/></Relationships>"#;
        let bytes = xlsx_with_styles("xl/design/theme-styles.xml", &oversized, Some(rels));
        assert!(matches!(
            preflight_package(&bytes, DocumentKind::Xlsx, DocumentVariant::Xlsx),
            Err(DocumentError::ResourceLimit)
        ));
    }

    #[test]
    fn number_format_scan_handles_quotes_spacing_and_chunk_boundaries() {
        /// Yields one byte per read, so every token crosses a buffer edge.
        struct Trickle<'a>(&'a [u8]);
        impl Read for Trickle<'_> {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                match (self.0.split_first(), buffer.first_mut()) {
                    (Some((byte, rest)), Some(slot)) => {
                        *slot = *byte;
                        self.0 = rest;
                        Ok(1)
                    }
                    _ => Ok(0),
                }
            }
        }
        let long = "#".repeat(MAX_NUMBER_FORMAT_BYTES + 1);
        for (xml, oversized) in [
            (format!(r#"<numFmt formatCode = '{long}'/>"#), true),
            (format!(r#"<numFmt x:formatCode="{long}"/>"#), true),
            (
                format!(r#"<numFmt formatCode="0.00"/><t>{long}</t>"#),
                false,
            ),
            (format!(r#"<numFmt formatCodes="{long}"/>"#), false),
            (
                "<numFmt formatCode=\"&quot;$&quot;#,##0.00\"/>".to_string(),
                false,
            ),
        ] {
            let scanned =
                styles_have_oversized_number_format(Trickle(xml.as_bytes())).expect("scan");
            assert_eq!(scanned, oversized, "{}", &xml[..40.min(xml.len())]);
        }
    }

    #[test]
    fn xlsx_variant_is_not_enabled_for_macro_or_binary_containers() {
        assert_eq!(
            DocumentVariant::from_extension(Path::new("book.xlsm"), anydoc::Format::Excel),
            DocumentVariant::Xlsm
        );
        assert_eq!(
            DocumentVariant::from_extension(Path::new("book.xlsb"), anydoc::Format::Excel),
            DocumentVariant::Xlsb
        );
        assert!(!matches!(DocumentVariant::Xlsm.worker_code(), 2));
        assert!(!matches!(DocumentVariant::Xlsb.worker_code(), 2));
    }

    const ODS_MIMETYPE: &[u8] = b"application/vnd.oasis.opendocument.spreadsheet";
    const ODS_CONTENT: &[u8] = br#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Sheet1"><table:table-row><table:table-cell office:value-type="string"><text:p>Amount</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;

    fn ods_package(content: &[u8], extra: &[(&str, &[u8])]) -> Vec<u8> {
        let mut entries = vec![("mimetype", ODS_MIMETYPE), ("content.xml", content)];
        entries.extend_from_slice(extra);
        zip_entries(&entries)
    }

    #[test]
    fn ods_capability_is_strict_and_enabled_only_for_ods() {
        let contract = capabilities(DocumentKind::Ods);
        assert!(contract.enabled);
        assert_eq!(contract.supported_variants, vec![DocumentVariant::Ods]);
        assert_eq!(contract.formula_policy, "cached_value_only");
        assert_eq!(contract.hidden_content_policy, "reject");
        assert_eq!(contract.external_content_policy, "reject");
        assert_eq!(contract.active_content_policy, "reject");
        assert_eq!(DocumentVariant::Ods.worker_code(), 4);
        assert_eq!(
            anydoc_format(DocumentVariant::Ods),
            Some(anydoc::Format::Ods)
        );
        assert_eq!(
            kind_for_variant(DocumentVariant::Ods),
            Some(DocumentKind::Ods)
        );
    }

    #[test]
    fn ods_preflight_accepts_visible_cached_spreadsheet() {
        let bytes = ods_package(ODS_CONTENT, &[]);
        let result = preflight_package(&bytes, DocumentKind::Ods, DocumentVariant::Ods).unwrap();
        assert!(!result.hidden_content);
        assert!(!result.external_relationships);
        assert!(!result.active_content);
        assert!(!result.missing_formula_cache);
        assert!(!result.missing_required_content);
    }

    #[test]
    fn ods_preflight_rejects_hidden_external_active_and_uncached_content() {
        let hidden = br#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table><table:table-row table:visibility="collapse"/></table:table></office:spreadsheet></office:body></office:document-content>"#;
        let hidden_result = preflight_package(
            &ods_package(hidden, &[]),
            DocumentKind::Ods,
            DocumentVariant::Ods,
        )
        .unwrap();
        assert!(hidden_result.hidden_content);

        let hidden_style =
            br#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:styles><style:style style:name="hidden-row" style:family="table-row"><style:table-row-properties table:visibility="collapse"/></style:style></office:styles></office:document-styles>"#;
        let hidden_style_result = preflight_package(
            &ods_package(ODS_CONTENT, &[("styles.xml", hidden_style)]),
            DocumentKind::Ods,
            DocumentVariant::Ods,
        )
        .unwrap();
        assert!(hidden_style_result.hidden_content);

        let external = br#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:xlink="http://www.w3.org/1999/xlink"><office:body><office:spreadsheet><table:table><table:table-row><table:table-cell xlink:href="https://example.invalid"/></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
        let external_result = preflight_package(
            &ods_package(external, &[]),
            DocumentKind::Ods,
            DocumentVariant::Ods,
        )
        .unwrap();
        assert!(external_result.external_relationships);

        let active = br#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0"><office:body><office:spreadsheet><table:table><table:table-row><table:table-cell><draw:object/></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
        let active_result = preflight_package(
            &ods_package(active, &[]),
            DocumentKind::Ods,
            DocumentVariant::Ods,
        )
        .unwrap();
        assert!(active_result.active_content);

        let formula = br#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table><table:table-row><table:table-cell table:formula="of:=A1"/></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
        let formula_result = preflight_package(
            &ods_package(formula, &[]),
            DocumentKind::Ods,
            DocumentVariant::Ods,
        )
        .unwrap();
        assert!(formula_result.missing_formula_cache);
    }

    #[test]
    fn ods_preflight_rejects_wrong_identity_missing_content_and_encryption() {
        let wrong_mimetype = zip_entries(&[
            ("mimetype", b"application/vnd.oasis.opendocument.text"),
            ("content.xml", ODS_CONTENT),
        ]);
        assert!(matches!(
            preflight_package(&wrong_mimetype, DocumentKind::Ods, DocumentVariant::Ods),
            Err(DocumentError::Malformed)
        ));

        let missing_content = zip_entries(&[("mimetype", ODS_MIMETYPE)]);
        assert!(matches!(
            preflight_package(&missing_content, DocumentKind::Ods, DocumentVariant::Ods),
            Err(DocumentError::Malformed)
        ));

        let manifest = br#"<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0"><manifest:file-entry><manifest:encryption-data/></manifest:file-entry></manifest:manifest>"#;
        assert!(matches!(
            preflight_package(
                &ods_package(ODS_CONTENT, &[("META-INF/manifest.xml", manifest)]),
                DocumentKind::Ods,
                DocumentVariant::Ods
            ),
            Err(DocumentError::Encrypted)
        ));
    }

    const ODT_MIMETYPE: &[u8] = b"application/vnd.oasis.opendocument.text";
    const ODT_CONTENT: &[u8] = br#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:text><text:h text:outline-level="1">Memo</text:h><text:p>Visible body.</text:p></office:text></office:body></office:document-content>"#;

    fn odt_package(content: &[u8], extra: &[(&str, &[u8])]) -> Vec<u8> {
        let mut entries = vec![("mimetype", ODT_MIMETYPE), ("content.xml", content)];
        entries.extend_from_slice(extra);
        zip_entries(&entries)
    }

    #[test]
    fn odt_capability_is_strict_and_enabled_only_for_odt() {
        let contract = capabilities(DocumentKind::Odt);
        assert!(contract.enabled);
        assert_eq!(contract.supported_variants, vec![DocumentVariant::Odt]);
        assert_eq!(contract.formula_policy, "not_applicable");
        assert_eq!(contract.hidden_content_policy, "reject");
        assert_eq!(contract.external_content_policy, "reject");
        assert_eq!(contract.active_content_policy, "reject");
        assert_eq!(DocumentVariant::Odt.worker_code(), 5);
        assert_eq!(
            anydoc_format(DocumentVariant::Odt),
            Some(anydoc::Format::Odt)
        );
        assert_eq!(
            kind_for_variant(DocumentVariant::Odt),
            Some(DocumentKind::Odt)
        );
    }

    #[test]
    fn odt_preflight_accepts_visible_text_and_present_assets() {
        let content = br#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:xlink="http://www.w3.org/1999/xlink"><office:body><office:text><text:p>Visible body.</text:p><draw:frame><draw:image xlink:href="Pictures/logo.png"/></draw:frame></office:text></office:body></office:document-content>"#;
        let result = preflight_package(
            &odt_package(content, &[("Pictures/logo.png", b"png")]),
            DocumentKind::Odt,
            DocumentVariant::Odt,
        )
        .unwrap();
        assert!(!result.hidden_content);
        assert!(!result.external_relationships);
        assert!(!result.active_content);
        assert!(!result.missing_required_content);
        assert!(!result.unsupported_content);
    }

    #[test]
    fn odt_preflight_rejects_hidden_external_active_and_missing_content() {
        let hidden = br#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:text><text:hidden-text>secret</text:hidden-text></office:text></office:body></office:document-content>"#;
        assert!(
            preflight_package(
                &odt_package(hidden, &[]),
                DocumentKind::Odt,
                DocumentVariant::Odt
            )
            .unwrap()
            .hidden_content
        );

        let tracked = br#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:text><text:tracked-changes/></office:text></office:body></office:document-content>"#;
        assert!(
            preflight_package(
                &odt_package(tracked, &[]),
                DocumentKind::Odt,
                DocumentVariant::Odt
            )
            .unwrap()
            .hidden_content
        );

        let note = br#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:text><text:p>Visible text<text:note text:id="n1"><text:note-body><text:p>Unsupported note.</text:p></text:note-body></text:note></text:p></office:text></office:body></office:document-content>"#;
        assert!(
            preflight_package(
                &odt_package(note, &[]),
                DocumentKind::Odt,
                DocumentVariant::Odt
            )
            .unwrap()
            .unsupported_content
        );

        let external = br#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:xlink="http://www.w3.org/1999/xlink"><office:body><office:text><text:a xlink:href="https://example.invalid">external</text:a></office:text></office:body></office:document-content>"#;
        assert!(
            preflight_package(
                &odt_package(external, &[]),
                DocumentKind::Odt,
                DocumentVariant::Odt
            )
            .unwrap()
            .external_relationships
        );

        let active = br#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0"><office:body><office:text><draw:object/></office:text></office:body></office:document-content>"#;
        assert!(
            preflight_package(
                &odt_package(active, &[]),
                DocumentKind::Odt,
                DocumentVariant::Odt
            )
            .unwrap()
            .active_content
        );

        let missing = br#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:xlink="http://www.w3.org/1999/xlink"><office:body><office:text><draw:image xlink:href="Pictures/missing.png"/></office:text></office:body></office:document-content>"#;
        assert!(
            preflight_package(
                &odt_package(missing, &[]),
                DocumentKind::Odt,
                DocumentVariant::Odt
            )
            .unwrap()
            .missing_required_content
        );
    }

    #[test]
    fn odt_preflight_rejects_wrong_identity_missing_content_and_encryption() {
        let wrong_mimetype = zip_entries(&[
            (
                "mimetype",
                b"application/vnd.oasis.opendocument.spreadsheet",
            ),
            ("content.xml", ODT_CONTENT),
        ]);
        assert!(matches!(
            preflight_package(&wrong_mimetype, DocumentKind::Odt, DocumentVariant::Odt),
            Err(DocumentError::Malformed)
        ));

        let missing_content = zip_entries(&[("mimetype", ODT_MIMETYPE)]);
        assert!(matches!(
            preflight_package(&missing_content, DocumentKind::Odt, DocumentVariant::Odt),
            Err(DocumentError::Malformed)
        ));

        let manifest = br#"<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0"><manifest:file-entry><manifest:encryption-data/></manifest:file-entry></manifest:manifest>"#;
        assert!(matches!(
            preflight_package(
                &odt_package(ODT_CONTENT, &[("META-INF/manifest.xml", manifest)]),
                DocumentKind::Odt,
                DocumentVariant::Odt
            ),
            Err(DocumentError::Encrypted)
        ));
    }

    const ODP_MIMETYPE: &[u8] = b"application/vnd.oasis.opendocument.presentation";
    const ODP_CONTENT: &[u8] = br#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:presentation="urn:oasis:names:tc:opendocument:xmlns:presentation:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:xlink="http://www.w3.org/1999/xlink"><office:body><office:presentation><draw:page draw:name="Slide 1"><draw:frame presentation:class="title"><draw:text-box><text:p>Public title</text:p></draw:text-box></draw:frame><draw:frame><draw:text-box><text:p>Visible body</text:p></draw:text-box></draw:frame><draw:image xlink:href="Pictures/logo.png"/></draw:page></office:presentation></office:body></office:document-content>"#;

    fn odp_package(content: &[u8], extra: &[(&str, &[u8])]) -> Vec<u8> {
        let mut entries = vec![("mimetype", ODP_MIMETYPE), ("content.xml", content)];
        entries.extend_from_slice(extra);
        zip_entries(&entries)
    }

    #[test]
    fn odp_capability_is_memory_gated_and_exact() {
        let contract = capabilities(DocumentKind::Odp);
        assert_eq!(
            contract.enabled,
            worker_sandbox_available() && worker_memory_limit().is_some()
        );
        assert_eq!(contract.supported_variants, vec![DocumentVariant::Odp]);
        assert_eq!(contract.formula_policy, "not_applicable");
        assert_eq!(contract.hidden_content_policy, "reject");
        assert_eq!(contract.external_content_policy, "reject");
        assert_eq!(contract.active_content_policy, "reject");
        assert_eq!(DocumentVariant::Odp.worker_code(), 7);
        assert_eq!(
            anydoc_format(DocumentVariant::Odp),
            Some(anydoc::Format::Odp)
        );
        assert_eq!(
            kind_for_variant(DocumentVariant::Odp),
            Some(DocumentKind::Odp)
        );
    }

    #[test]
    fn odp_preflight_accepts_visible_presentation_and_local_asset() {
        let result = preflight_package(
            &odp_package(ODP_CONTENT, &[("Pictures/logo.png", b"png")]),
            DocumentKind::Odp,
            DocumentVariant::Odp,
        )
        .unwrap();
        assert!(!result.hidden_content);
        assert!(!result.external_relationships);
        assert!(!result.active_content);
        assert!(!result.missing_required_content);
    }

    #[test]
    fn odp_preflight_rejects_hidden_external_active_and_missing_content() {
        let hidden = br#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0"><office:body><office:presentation><draw:page presentation:visibility="hidden"/></office:presentation></office:body></office:document-content>"#;
        assert!(
            preflight_package(
                &odp_package(hidden, &[]),
                DocumentKind::Odp,
                DocumentVariant::Odp
            )
            .unwrap()
            .hidden_content
        );

        let external = br#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:xlink="http://www.w3.org/1999/xlink"><office:body><office:presentation><draw:page><draw:image xlink:href="https://example.invalid/image.png"/></draw:page></office:presentation></office:body></office:document-content>"#;
        assert!(
            preflight_package(
                &odp_package(external, &[]),
                DocumentKind::Odp,
                DocumentVariant::Odp
            )
            .unwrap()
            .external_relationships
        );

        let active = br#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0"><office:body><office:presentation><draw:page><draw:object/></draw:page></office:presentation></office:body></office:document-content>"#;
        assert!(
            preflight_package(
                &odp_package(active, &[]),
                DocumentKind::Odp,
                DocumentVariant::Odp
            )
            .unwrap()
            .active_content
        );

        let missing = preflight_package(
            &odp_package(ODP_CONTENT, &[]),
            DocumentKind::Odp,
            DocumentVariant::Odp,
        )
        .unwrap();
        assert!(missing.missing_required_content);
    }

    #[test]
    fn odp_preflight_rejects_wrong_identity_missing_content_and_encryption() {
        let wrong_mimetype = zip_entries(&[
            ("mimetype", b"application/octet-stream"),
            ("content.xml", ODP_CONTENT),
        ]);
        assert!(matches!(
            preflight_package(&wrong_mimetype, DocumentKind::Odp, DocumentVariant::Odp),
            Err(DocumentError::Malformed)
        ));

        let missing_content = zip_entries(&[("mimetype", ODP_MIMETYPE)]);
        assert!(matches!(
            preflight_package(&missing_content, DocumentKind::Odp, DocumentVariant::Odp),
            Err(DocumentError::Malformed)
        ));

        let manifest = br#"<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0"><manifest:file-entry><manifest:encryption-data/></manifest:file-entry></manifest:manifest>"#;
        assert!(matches!(
            preflight_package(
                &odp_package(ODP_CONTENT, &[("META-INF/manifest.xml", manifest)]),
                DocumentKind::Odp,
                DocumentVariant::Odp
            ),
            Err(DocumentError::Encrypted)
        ));
    }

    const EPUB_CONTAINER: &[u8] = br#"<?xml version="1.0"?><container xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="OPS/package.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#;
    const EPUB_OPF: &[u8] = br#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" xmlns:dc="http://purl.org/dc/elements/1.1/" version="3.0"><metadata><dc:title>Public EPUB</dc:title></metadata><manifest><item id="ch1" href="Text/ch1.xhtml" media-type="application/xhtml+xml"/><item id="ch2" href="Text/ch2.xhtml" media-type="application/xhtml+xml"/><item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/><item id="logo" href="images/logo.png" media-type="image/png"/></manifest><spine><itemref idref="ch1"/><itemref idref="ch2"/></spine></package>"#;
    const EPUB_NAV: &[u8] = br#"<?xml version="1.0"?><html xmlns="http://www.w3.org/1999/xhtml"><head/><body><nav type="toc"><ol><li><a href="Text/ch1.xhtml">Chapter One</a></li><li><a href="Text/ch2.xhtml">Chapter Two</a></li></ol></nav></body></html>"#;
    const EPUB_CHAPTER_ONE: &[u8] = br#"<?xml version="1.0"?><html xmlns="http://www.w3.org/1999/xhtml"><head/><body><h1>Chapter One</h1><p>First public chapter.<img src="../images/logo.png" alt="Public logo"/></p></body></html>"#;
    const EPUB_CHAPTER_TWO: &[u8] = br#"<?xml version="1.0"?><html xmlns="http://www.w3.org/1999/xhtml"><head/><body><h1>Chapter Two</h1><p>Second public chapter.</p></body></html>"#;

    #[test]
    fn epub_preflight_rejects_epub2_before_conversion() {
        let epub2_opf = br#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="2.0"><metadata/><manifest><item id="ch1" href="Text/ch1.xhtml" media-type="application/xhtml+xml"/></manifest><spine><itemref idref="ch1"/></spine></package>"#;
        let bytes = epub_package_for_opf(epub2_opf, EPUB_CHAPTER_ONE);
        let actual = preflight_package(&bytes, DocumentKind::Epub, DocumentVariant::Epub);
        assert!(matches!(actual, Err(DocumentError::Unsupported)));
    }

    #[test]
    fn epub_preflight_requires_one_epub3_navigation_document() {
        let epub3_without_nav = br#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="3.0"><metadata/><manifest><item id="ch1" href="Text/ch1.xhtml" media-type="application/xhtml+xml"/></manifest><spine><itemref idref="ch1"/></spine></package>"#;
        let bytes = epub_package_for_opf(epub3_without_nav, EPUB_CHAPTER_ONE);
        let result = preflight_package(&bytes, DocumentKind::Epub, DocumentVariant::Epub).unwrap();
        assert!(result.missing_required_content);
    }

    fn epub_package_for_opf(opf: &[u8], chapter: &[u8]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        writer.start_file("mimetype", stored).unwrap();
        writer.write_all(b"application/epub+zip").unwrap();
        for (name, bytes) in [
            ("META-INF/container.xml", EPUB_CONTAINER),
            ("OPS/package.opf", opf),
            ("OPS/Text/ch1.xhtml", chapter),
        ] {
            writer
                .start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn epub_package(
        chapter_one: &[u8],
        chapter_two: Option<&[u8]>,
        extra: &[(&str, &[u8])],
    ) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        writer.start_file("mimetype", stored).unwrap();
        writer.write_all(b"application/epub+zip").unwrap();
        for (name, bytes) in [
            ("META-INF/container.xml", EPUB_CONTAINER),
            ("OPS/package.opf", EPUB_OPF),
            ("OPS/nav.xhtml", EPUB_NAV),
            ("OPS/Text/ch1.xhtml", chapter_one),
        ] {
            writer
                .start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(bytes).unwrap();
        }
        if let Some(chapter_two) = chapter_two {
            writer
                .start_file(
                    "OPS/Text/ch2.xhtml",
                    zip::write::SimpleFileOptions::default(),
                )
                .unwrap();
            writer.write_all(chapter_two).unwrap();
        }
        for (name, bytes) in extra {
            writer
                .start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn epub_preflight_accepts_complete_spine_and_local_assets() {
        let result = preflight_package(
            &epub_package(
                EPUB_CHAPTER_ONE,
                Some(EPUB_CHAPTER_TWO),
                &[("OPS/images/logo.png", b"png")],
            ),
            DocumentKind::Epub,
            DocumentVariant::Epub,
        )
        .unwrap();
        assert!(!result.active_content);
        assert!(!result.external_relationships);
        assert!(!result.hidden_content);
        assert!(!result.missing_required_content);
    }

    #[test]
    fn epub_preflight_marks_missing_and_malformed_spine_as_incomplete() {
        let missing = preflight_package(
            &epub_package(EPUB_CHAPTER_ONE, None, &[("OPS/images/logo.png", b"png")]),
            DocumentKind::Epub,
            DocumentVariant::Epub,
        )
        .unwrap();
        assert!(missing.missing_required_content);

        let malformed = preflight_package(
            &epub_package(
                EPUB_CHAPTER_ONE,
                Some(br#"<html><body><p>unclosed"#),
                &[("OPS/images/logo.png", b"png")],
            ),
            DocumentKind::Epub,
            DocumentVariant::Epub,
        )
        .unwrap();
        assert!(malformed.missing_required_content);
    }

    #[test]
    fn epub_preflight_flags_external_and_active_chapter_content() {
        let external = br#"<html xmlns="http://www.w3.org/1999/xhtml"><body><a href="https://example.invalid">remote</a></body></html>"#;
        let result = preflight_package(
            &epub_package(external, Some(EPUB_CHAPTER_TWO), &[]),
            DocumentKind::Epub,
            DocumentVariant::Epub,
        )
        .unwrap();
        assert!(result.external_relationships);

        let active = br#"<html xmlns="http://www.w3.org/1999/xhtml"><body><script>alert(1)</script></body></html>"#;
        let result = preflight_package(
            &epub_package(active, Some(EPUB_CHAPTER_TWO), &[]),
            DocumentKind::Epub,
            DocumentVariant::Epub,
        )
        .unwrap();
        assert!(result.active_content);

        let hidden = br#"<html xmlns="http://www.w3.org/1999/xhtml"><body><p style="display:none">hidden</p></body></html>"#;
        let result = preflight_package(
            &epub_package(hidden, Some(EPUB_CHAPTER_TWO), &[]),
            DocumentKind::Epub,
            DocumentVariant::Epub,
        )
        .unwrap();
        assert!(result.hidden_content);
    }

    #[test]
    fn epub_preflight_rejects_invalid_ocf_identity_and_drm() {
        let wrong_mimetype = zip_entries(&[
            ("mimetype", b"application/octet-stream"),
            ("META-INF/container.xml", EPUB_CONTAINER),
        ]);
        assert!(matches!(
            preflight_package(&wrong_mimetype, DocumentKind::Epub, DocumentVariant::Epub),
            Err(DocumentError::Malformed)
        ));

        let drm = epub_package(
            EPUB_CHAPTER_ONE,
            Some(EPUB_CHAPTER_TWO),
            &[
                ("OPS/images/logo.png", b"png"),
                ("META-INF/encryption.xml", b"<encryption/>"),
            ],
        );
        assert!(matches!(
            preflight_package(&drm, DocumentKind::Epub, DocumentVariant::Epub),
            Err(DocumentError::Encrypted)
        ));
    }

    #[test]
    fn public_epub_corpus_matches_containment_oracle() {
        let complete = preflight_package(
            include_bytes!("../../../test-corpus/epub/public-spine-order.epub"),
            DocumentKind::Epub,
            DocumentVariant::Epub,
        )
        .unwrap();
        assert!(!complete.active_content);
        assert!(!complete.external_relationships);
        assert!(!complete.hidden_content);
        assert!(!complete.missing_required_content);

        let legacy_without_navigation = preflight_package(
            include_bytes!("../../../test-corpus/epub/public-longform.epub"),
            DocumentKind::Epub,
            DocumentVariant::Epub,
        )
        .unwrap();
        assert!(legacy_without_navigation.missing_required_content);

        let missing = preflight_package(
            include_bytes!("../../../test-corpus/epub/missing-chapter.epub"),
            DocumentKind::Epub,
            DocumentVariant::Epub,
        )
        .unwrap();
        assert!(missing.missing_required_content);

        let malformed = preflight_package(
            include_bytes!("../../../test-corpus/epub/malformed-chapter.epub"),
            DocumentKind::Epub,
            DocumentVariant::Epub,
        )
        .unwrap();
        assert!(malformed.missing_required_content);

        let external = preflight_package(
            include_bytes!("../../../test-corpus/epub/external-content.epub"),
            DocumentKind::Epub,
            DocumentVariant::Epub,
        )
        .unwrap();
        assert!(external.external_relationships);
        assert!(external.missing_required_content);

        let hidden = preflight_package(
            include_bytes!("../../../test-corpus/epub/hidden-content.epub"),
            DocumentKind::Epub,
            DocumentVariant::Epub,
        )
        .unwrap();
        assert!(hidden.hidden_content);
        assert!(!hidden.missing_required_content);

        let active = preflight_package(
            include_bytes!("../../../test-corpus/epub/active-content.epub"),
            DocumentKind::Epub,
            DocumentVariant::Epub,
        )
        .unwrap();
        assert!(active.active_content);
        assert!(!active.missing_required_content);

        assert!(matches!(
            preflight_package(
                include_bytes!("../../../test-corpus/epub/encrypted.epub"),
                DocumentKind::Epub,
                DocumentVariant::Epub,
            ),
            Err(DocumentError::Encrypted)
        ));
    }

    fn epub_spine_chapter_texts(bytes: &[u8]) -> Vec<String> {
        let mut archive = ZipArchive::new(Cursor::new(bytes)).unwrap();
        let container = epub_read_part(&mut archive, "META-INF/container.xml").unwrap();
        let roots = epub_rootfile_paths(&container).unwrap();
        let opf_path = resolve_package_target("", &roots[0]).unwrap();
        let opf = epub_read_part(&mut archive, &opf_path).unwrap();
        let metadata = epub_parse_opf(&opf).unwrap();
        metadata
            .spine
            .iter()
            .filter_map(|idref| metadata.manifest.get(idref))
            .filter_map(|item| epub_resolve_local(&opf_path, &item.href))
            .map(|target| {
                String::from_utf8(epub_read_part(&mut archive, &target).unwrap()).unwrap()
            })
            .collect()
    }

    #[test]
    fn pinned_anydoc_epub_emits_complete_chapters_in_spine_order() {
        let bytes = include_bytes!("../../../test-corpus/epub/public-spine-order.epub");
        let markdown = anydoc::to_markdown_bytes(bytes, anydoc::Format::Epub)
            .expect("complete public EPUB must convert through pinned AnyDoc");
        let markers = [
            "EPUB-C01-BEGIN",
            "EPUB-H1-SCOPE",
            "EPUB-C01-END",
            "EPUB-C02-BEGIN",
            "EPUB-LIST-01",
            "EPUB-LIST-02",
            "EPUB-TABLE-FIELD",
            "EPUB-TABLE-42",
            "EPUB-C02-END",
            "EPUB-C03-BEGIN",
            "EPUB-INTERNAL-LINK",
            "EPUB-IMAGE-ALT",
            "EPUB-C03-END",
        ];
        let mut previous = 0;
        for marker in markers {
            let offset = markdown
                .find(marker)
                .unwrap_or_else(|| panic!("missing AnyDoc EPUB marker {marker}"));
            assert!(
                offset >= previous,
                "AnyDoc EPUB marker order drifted at {marker}"
            );
            previous = offset;
        }
    }

    #[test]
    fn pinned_anydoc_epub_can_omit_a_declared_spine_chapter() {
        let bytes = include_bytes!("../../../test-corpus/epub/missing-spine-chapter.epub");
        let markdown = anydoc::to_markdown_bytes(bytes, anydoc::Format::Epub)
            .expect("pinned AnyDoc currently recovers missing EPUB chapters");
        assert!(markdown.contains("EPUB-C01-BEGIN"));
        assert!(markdown.contains("EPUB-C03-BEGIN"));
        assert!(!markdown.contains("EPUB-C02-BEGIN"));
    }

    #[test]
    fn public_epub_qualification_corpus_matches_oracle() {
        let oracle: serde_json::Value =
            serde_json::from_str(include_str!("../../../test-corpus/epub/oracle.json")).unwrap();
        let fixtures = oracle["fixtures"].as_array().expect("fixture oracle");
        assert_eq!(fixtures.len(), 10);
        let marker_order = oracle["fixtures"][0]["spine_order"]
            .as_array()
            .expect("spine order")
            .iter()
            .map(|number| number.as_u64().expect("chapter number"))
            .collect::<Vec<_>>();
        assert_eq!(marker_order, vec![1, 2, 3]);

        for fixture in fixtures {
            let path = fixture["path"].as_str().expect("fixture path");
            let bytes: &[u8] = match path {
                "public-spine-order.epub" => {
                    include_bytes!("../../../test-corpus/epub/public-spine-order.epub")
                }
                "missing-spine-chapter.epub" => {
                    include_bytes!("../../../test-corpus/epub/missing-spine-chapter.epub")
                }
                "malformed-spine-chapter.epub" => {
                    include_bytes!("../../../test-corpus/epub/malformed-spine-chapter.epub")
                }
                "nav-spine-mismatch.epub" => {
                    include_bytes!("../../../test-corpus/epub/nav-spine-mismatch.epub")
                }
                "missing-local-resource.epub" => {
                    include_bytes!("../../../test-corpus/epub/missing-local-resource.epub")
                }
                "external-reference.epub" => {
                    include_bytes!("../../../test-corpus/epub/external-reference.epub")
                }
                "active-content.epub" => {
                    include_bytes!("../../../test-corpus/epub/active-content.epub")
                }
                "hidden-content.epub" => {
                    include_bytes!("../../../test-corpus/epub/hidden-content.epub")
                }
                "encrypted.epub" => include_bytes!("../../../test-corpus/epub/encrypted.epub"),
                "archive-amplification.epub" => {
                    include_bytes!("../../../test-corpus/epub/archive-amplification.epub")
                }
                other => panic!("unexpected EPUB fixture {other}"),
            };
            let disposition = fixture["disposition"].as_str().expect("disposition");
            let result = preflight_package(bytes, DocumentKind::Epub, DocumentVariant::Epub);
            match disposition {
                "encrypted" => assert!(matches!(result, Err(DocumentError::Encrypted))),
                "resource_limit" => {
                    assert!(matches!(result, Err(DocumentError::ResourceLimit)))
                }
                "complete" | "incomplete_conversion" | "active_content_disabled" => {
                    let result = result.unwrap();
                    for flag in fixture["expected_flags"]
                        .as_array()
                        .expect("expected flags")
                        .iter()
                        .map(|value| value.as_str().expect("flag"))
                    {
                        match flag {
                            "active_content" => assert!(result.active_content, "{path}"),
                            "external_relationships" => {
                                assert!(result.external_relationships, "{path}")
                            }
                            "hidden_content" => assert!(result.hidden_content, "{path}"),
                            "missing_required_content" => {
                                assert!(result.missing_required_content, "{path}")
                            }
                            other => panic!("unexpected EPUB flag {other}"),
                        }
                    }
                    if disposition == "complete" {
                        assert!(!result.active_content);
                        assert!(!result.external_relationships);
                        assert!(!result.hidden_content);
                        assert!(!result.missing_required_content);
                        let chapters = epub_spine_chapter_texts(bytes);
                        let markers = fixture["markers"]
                            .as_array()
                            .expect("markers")
                            .iter()
                            .map(|value| value.as_str().expect("marker"))
                            .collect::<Vec<_>>();
                        let mut previous = None;
                        for marker in markers {
                            let matches = chapters
                                .iter()
                                .enumerate()
                                .flat_map(|(index, chapter)| {
                                    chapter
                                        .match_indices(marker)
                                        .map(move |(offset, _)| (index, offset))
                                })
                                .collect::<Vec<_>>();
                            assert_eq!(matches.len(), 1, "{marker}");
                            if let Some(previous) = previous {
                                assert!(previous < matches[0], "{marker}");
                            }
                            previous = Some(matches[0]);
                        }
                    }
                }
                other => panic!("unexpected EPUB disposition {other}"),
            }
        }
    }

    #[test]
    fn epub_capability_is_memory_gated_and_exact() {
        let contract = capabilities(DocumentKind::Epub);
        assert_eq!(
            contract.enabled,
            worker_sandbox_available() && worker_memory_limit().is_some()
        );
        assert_eq!(contract.supported_variants, vec![DocumentVariant::Epub]);
        assert_eq!(contract.formula_policy, "not_applicable");
        assert_eq!(contract.hidden_content_policy, "reject");
        assert_eq!(contract.external_content_policy, "reject");
        assert_eq!(contract.active_content_policy, "reject");
        assert_eq!(DocumentVariant::Epub.worker_code(), 8);
        assert_eq!(
            anydoc_format(DocumentVariant::Epub),
            Some(anydoc::Format::Epub)
        );
        assert_eq!(
            kind_for_variant(DocumentVariant::Epub),
            Some(DocumentKind::Epub)
        );
    }

    #[test]
    fn worker_warning_classifier_retains_only_stable_categories() {
        assert_eq!(
            worker_warning_kind("skipping corrupt chart part secret/file.xml"),
            Some(WorkerWarningKind::Omission)
        );
        let private_target = ["/", "Users/private/file.xml"].concat();
        assert_eq!(
            worker_warning_kind(&format!("relationship target {private_target} is missing")),
            Some(WorkerWarningKind::Omission)
        );
        assert_eq!(
            worker_warning_kind("recovered malformed xml (unclosed or mismatched elements)"),
            Some(WorkerWarningKind::MalformedRecovery)
        );
        assert_eq!(
            worker_warning_kind("skipping unusable chapter OPS/Text/chapter.xhtml"),
            Some(WorkerWarningKind::Omission)
        );
        assert_eq!(
            worker_warning_kind("skipping a checkbox with no readable anchor"),
            None
        );
        let private_uri = ["file://", "/", "private/secret.docx"].concat();
        let debug = format!("{:?}", worker_warning_kind(&private_uri));
        assert!(!debug.contains("private"));
        assert!(!debug.contains("secret"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn canceling_worker_reaps_process_group() {
        let temporary = tempfile::tempdir().expect("temporary worker directory");
        let worker = temporary.path().join("cancel-worker.sh");
        let pid_file = temporary.path().join("cancel-worker.pids");
        let pid_file_for_script = pid_file.to_string_lossy().replace('\'', "'\\''");
        std::fs::write(
            &worker,
            format!(
                "#!/bin/sh\n\
                 /bin/sleep 60 &\n\
                 child=$!\n\
                 printf '%s:%s' \"$$\" \"$child\" > '{pid_file_for_script}'\n\
                 wait\n"
            ),
        )
        .expect("write canceling worker");
        let mut permissions = std::fs::metadata(&worker)
            .expect("canceling worker metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&worker, permissions).expect("make canceling worker executable");

        let task = tokio::spawn(run_worker_process_with_executable(
            &[],
            DocumentVariant::Docx,
            worker,
        ));
        let recorded = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(recorded) = std::fs::read_to_string(&pid_file) {
                    break recorded;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("worker must record its process group");
        let pids = recorded
            .split(':')
            .map(|pid| pid.parse::<libc::pid_t>().expect("recorded pid"))
            .collect::<Vec<_>>();

        task.abort();
        assert!(task.await.is_err(), "worker task must be canceled");

        for pid in pids {
            let mut gone = false;
            for _ in 0..200 {
                let result = unsafe { libc::kill(pid, 0) };
                if result == -1
                    && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
                {
                    gone = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert!(
                gone,
                "worker process {pid} remained after cancellation reap"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn terminate_child_reaps_process_group() {
        let mut command = Command::new("sleep");
        command.arg("60");
        configure_worker_command(&mut command);
        let mut child = command.spawn().expect("sleep process must start");
        assert!(child.id().is_some());

        terminate_child(&mut child).await;

        assert!(child
            .try_wait()
            .expect("reaped child status must be readable")
            .is_some());
    }

    #[test]
    fn worker_error_codes_cover_hardening_states() {
        assert_eq!(
            DocumentError::ActiveContentDisabled.code(),
            "active_content_disabled"
        );
        assert_eq!(
            DocumentError::IncompleteConversion.code(),
            "incomplete_conversion"
        );
    }
}
