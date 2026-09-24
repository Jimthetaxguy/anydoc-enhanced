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
    /// recoverable omission. This state is not currently emitted by AnyDoc.
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
    let metadata = std::fs::metadata(&canonical).map_err(|_| DocumentError::InputUnavailable)?;
    if metadata.len() > MAX_DOCUMENT_SIZE {
        return Err(DocumentError::ResourceLimit);
    }
    let bytes = std::fs::read(&canonical).map_err(|_| DocumentError::InputUnavailable)?;
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
    let content = String::from_utf8_lossy(&content).to_ascii_lowercase();
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

fn xml_attribute_value(event: &quick_xml::events::BytesStart<'_>, wanted: &[u8]) -> Option<String> {
    event.attributes().flatten().find_map(|attribute| {
        (xml_local_name(attribute.key.as_ref()) == wanted)
            .then(|| String::from_utf8_lossy(attribute.value.as_ref()).into_owned())
    })
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
                for attribute in event.attributes().flatten() {
                    if matches!(xml_local_name(attribute.key.as_ref()), b"href" | b"src") {
                        references
                            .push(String::from_utf8_lossy(attribute.value.as_ref()).into_owned());
                    }
                }
                buffer.clear();
            }
            Ok(quick_xml::events::Event::Eof) => return references,
            Ok(_) => buffer.clear(),
            Err(_) => return references,
        }
    }
}

fn odf_internal_target(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.starts_with('#') || is_external_uri(value) {
        return None;
    }
    let path = value.split_once('#').map_or(value, |(path, _)| path);
    resolve_package_target("content.xml", path)
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

fn xml_element_tags<'a>(xml: &'a str, element_name: &str) -> Vec<&'a str> {
    let needle = format!("<{element_name}");
    let mut tags = Vec::new();
    let mut cursor = 0;
    while let Some(offset) = xml[cursor..].find(&needle) {
        let start = cursor + offset;
        let end = match xml[start..].find('>') {
            Some(end) => start + end + 1,
            None => break,
        };
        let after_name = xml.as_bytes().get(start + needle.len()).copied();
        if after_name.is_some_and(|byte| byte.is_ascii_whitespace() || byte == b'>' || byte == b'/')
        {
            tags.push(&xml[start..end]);
        }
        cursor = end;
    }
    tags
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

fn xml_has_ooxml_external_relationship(bytes: &[u8]) -> bool {
    if !xml_is_well_formed(bytes) {
        return false;
    }
    let mut reader = quick_xml::Reader::from_reader(Cursor::new(bytes));
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(quick_xml::events::Event::Start(event))
            | Ok(quick_xml::events::Event::Empty(event))
                if xml_local_name(event.name().as_ref()).eq_ignore_ascii_case(b"Relationship") =>
            {
                let external = event.attributes().flatten().any(|attribute| {
                    let name = xml_local_name(attribute.key.as_ref());
                    let value = String::from_utf8_lossy(attribute.value.as_ref());
                    (name.eq_ignore_ascii_case(b"TargetMode")
                        && value.trim().eq_ignore_ascii_case("External"))
                        || (name.eq_ignore_ascii_case(b"Target") && ooxml_external_target(&value))
                });
                if external {
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

fn xml_attribute<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    for quote in ['"', '\''] {
        let marker = format!("{name}={quote}");
        if let Some(start) = tag.find(&marker) {
            let value_start = start + marker.len();
            if let Some(end) = tag[value_start..].find(quote) {
                return Some(&tag[value_start..value_start + end]);
            }
        }
    }
    None
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

fn validate_pptx_slide_targets(
    presentation: &[u8],
    presentation_rels: &[u8],
    slide_parts: &HashSet<String>,
) -> Result<bool, DocumentError> {
    if !xml_is_well_formed(presentation) || !xml_is_well_formed(presentation_rels) {
        return Err(DocumentError::Malformed);
    }
    let presentation = String::from_utf8_lossy(presentation);
    let presentation_rels = String::from_utf8_lossy(presentation_rels);
    let mut slide_relationships = HashMap::new();
    for tag in xml_element_tags(&presentation_rels, "Relationship") {
        let Some(id) = xml_attribute(tag, "Id") else {
            continue;
        };
        let Some(rel_type) = xml_attribute(tag, "Type") else {
            continue;
        };
        if rel_type.ends_with("/slide") {
            if let Some(target) = xml_attribute(tag, "Target") {
                slide_relationships.insert(id.to_string(), target.to_string());
            }
        }
    }

    let mut slide_tags = xml_element_tags(&presentation, "p:sldId");
    if slide_tags.is_empty() {
        slide_tags = xml_element_tags(&presentation, "sldId");
    }
    if slide_tags.is_empty() {
        return Err(DocumentError::Malformed);
    }
    let mut incomplete = false;
    for tag in slide_tags {
        let Some(relation_id) = xml_attribute(tag, "r:id") else {
            incomplete = true;
            continue;
        };
        let Some(target) = slide_relationships.get(relation_id) else {
            incomplete = true;
            continue;
        };
        let Some(part) = resolve_package_target("ppt/presentation.xml", target) else {
            incomplete = true;
            continue;
        };
        if !slide_parts.contains(&part) {
            incomplete = true;
        }
    }
    Ok(incomplete)
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

fn epub_resolve_local(base_part: &str, value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.starts_with('#') || epub_is_external_uri(value) {
        return None;
    }
    let path = value.split_once('#').map_or(value, |(path, _)| path);
    if path.is_empty() || path.contains('?') || path.starts_with('/') {
        return None;
    }
    resolve_package_target(base_part, path)
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

    let container = epub_read_part(&mut archive, "META-INF/container.xml")?;
    let roots = epub_rootfile_paths(&container)?;
    if roots.len() != 1 || roots[0].starts_with('/') {
        return Err(DocumentError::Malformed);
    }
    let opf_path = resolve_package_target("", &roots[0]).ok_or(DocumentError::Malformed)?;
    if !archive_names.contains(&opf_path) {
        return Err(DocumentError::Malformed);
    }
    let opf = epub_read_part(&mut archive, &opf_path)?;
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
        match epub_read_part(&mut archive, &target)
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
                match epub_read_part(&mut archive, &nav_target).and_then(|nav| {
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
                && lower_name.starts_with("xl/worksheets/")
                && lower_name.ends_with(".xml"))
            || (matches!(kind, DocumentKind::Pptx)
                && (lower_name == "ppt/presentation.xml"
                    || lower_name == "ppt/_rels/presentation.xml.rels"
                    || (lower_name.starts_with("ppt/slides/") && lower_name.ends_with(".xml"))))
            || (matches!(kind, DocumentKind::Docx) && lower_name == "word/document.xml");
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
            let lower_content = String::from_utf8_lossy(&content).to_ascii_lowercase();
            if matches!(kind, DocumentKind::Docx)
                && lower_name == "word/document.xml"
                && !xml_is_well_formed(&content)
            {
                return Err(DocumentError::Malformed);
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
                result.external_relationships |= xml_has_ooxml_external_relationship(&content);
            }
            if lower_content.contains("macroenabled") {
                result.active_content = true;
            }
            if matches!(kind, DocumentKind::Xlsx) {
                result.hidden_content |= xml_has_hidden_content(lower_content.as_bytes());
                if lower_name.starts_with("xl/worksheets/") {
                    result.missing_formula_cache |=
                        xml_has_uncached_formula(lower_content.as_bytes());
                }
            }
            if matches!(kind, DocumentKind::Pptx) {
                if lower_name == "ppt/presentation.xml" {
                    ppt_presentation = Some(content.clone());
                } else if lower_name == "ppt/_rels/presentation.xml.rels" {
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
            for reference in odf_references {
                let reference = reference.trim();
                if reference.is_empty() || reference.starts_with('#') {
                    continue;
                }
                let Some(target) = odf_internal_target(reference) else {
                    continue;
                };
                if !archive_names.contains(&target) {
                    result.missing_required_content = true;
                }
            }
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
    let bytes = tokio::fs::read(&canonical)
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
    let preflight = if kind == DocumentKind::Csv {
        PackagePreflight::default()
    } else {
        preflight_package(&bytes, kind, variant)?
    };
    if preflight.active_content {
        return Err(DocumentError::ActiveContentDisabled);
    }
    if kind == DocumentKind::Xlsx
        && (preflight.hidden_content
            || preflight.missing_formula_cache
            || preflight.external_relationships)
    {
        return Err(DocumentError::IncompleteConversion);
    }
    if kind == DocumentKind::Pptx
        && (preflight.hidden_content
            || preflight.external_relationships
            || preflight.missing_required_content)
    {
        return Err(DocumentError::IncompleteConversion);
    }
    if kind == DocumentKind::Ods
        && (preflight.hidden_content
            || preflight.missing_formula_cache
            || preflight.external_relationships
            || preflight.missing_required_content)
    {
        return Err(DocumentError::IncompleteConversion);
    }
    if kind == DocumentKind::Odt
        && (preflight.hidden_content
            || preflight.external_relationships
            || preflight.missing_required_content
            || preflight.unsupported_content)
    {
        return Err(DocumentError::IncompleteConversion);
    }
    if kind == DocumentKind::Odp
        && (preflight.hidden_content
            || preflight.external_relationships
            || preflight.missing_required_content)
    {
        return Err(DocumentError::IncompleteConversion);
    }
    if kind == DocumentKind::Epub
        && (preflight.hidden_content
            || preflight.external_relationships
            || preflight.missing_required_content)
    {
        return Err(DocumentError::IncompleteConversion);
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
        completeness: Completeness::Complete,
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

/// Run a PDF job in the private worker under the PDF lane's own concurrency
/// bound, so PDF calls and document conversions cannot starve each other.
pub(crate) async fn run_pdf_worker_job(job: WorkerJob) -> Result<WorkerResponse, DocumentError> {
    if !worker_sandbox_available() {
        return Err(DocumentError::WorkerUnavailable);
    }
    let executable = worker_executable()?;
    let permit = pdf_worker_semaphore()
        .acquire_owned()
        .await
        .map_err(|_| DocumentError::WorkerBusy)?;
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
        match preflight_package(bytes, kind, variant) {
            Err(error) => worker_response_for_error(&error),
            Ok(preflight) if preflight.active_content => {
                worker_response_for_error(&DocumentError::ActiveContentDisabled)
            }
            Ok(preflight)
                if (kind == DocumentKind::Pptx
                    && (preflight.hidden_content
                        || preflight.external_relationships
                        || preflight.missing_required_content))
                    || (kind == DocumentKind::Xlsx
                        && (preflight.hidden_content
                            || preflight.missing_formula_cache
                            || preflight.external_relationships))
                    || (kind == DocumentKind::Ods
                        && (preflight.hidden_content
                            || preflight.missing_formula_cache
                            || preflight.external_relationships
                            || preflight.missing_required_content))
                    || (kind == DocumentKind::Odt
                        && (preflight.hidden_content
                            || preflight.external_relationships
                            || preflight.missing_required_content
                            || preflight.unsupported_content))
                    || (kind == DocumentKind::Odp
                        && (preflight.hidden_content
                            || preflight.external_relationships
                            || preflight.missing_required_content))
                    || (kind == DocumentKind::Epub
                        && (preflight.hidden_content
                            || preflight.external_relationships
                            || preflight.missing_required_content)) =>
            {
                worker_response_for_error(&DocumentError::IncompleteConversion)
            }
            Ok(_) => match anydoc::to_markdown_bytes(bytes, Some(format)) {
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

fn sanitize_markdown(markdown: &str) -> (String, bool) {
    static URL: OnceLock<Regex> = OnceLock::new();
    static PATH: OnceLock<Regex> = OnceLock::new();
    static HTML: OnceLock<Regex> = OnceLock::new();
    let url =
        URL.get_or_init(|| Regex::new(r"(?i)(?:https?|ftp)://[^\s)\]>]+").expect("URL regex"));
    let path = PATH.get_or_init(|| {
        Regex::new(r"(?:/Users|/private|/tmp|/var/folders)/[^\s)\]>]+").expect("path regex")
    });
    let html = HTML.get_or_init(|| Regex::new(r"</?[A-Za-z][^>]*>").expect("HTML regex"));
    let sanitized = url
        .replace_all(markdown, "[external URL removed]")
        .into_owned();
    let sanitized = path
        .replace_all(&sanitized, "[local path removed]")
        .into_owned();
    let sanitized = html.replace_all(&sanitized, "").into_owned();
    let changed = sanitized != markdown;
    (sanitized, changed)
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
        assert!(xml_has_ooxml_external_relationship(
            br#"<Relationships><Relationship Id='rId1' TargetMode = 'External' Target='slide-link.bin'/></Relationships>"#
        ));
        assert!(xml_has_ooxml_external_relationship(
            br#"<Relationships><Relationship Id="rId2" Target="https://example.invalid/image.png"/></Relationships>"#
        ));
        assert!(!xml_has_ooxml_external_relationship(
            br#"<Relationships><Relationship Id="rId3" Target="slides/slide1.xml"/></Relationships>"#
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
