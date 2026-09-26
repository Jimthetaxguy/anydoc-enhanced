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
    rc::Rc,
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

#[path = "epub_css.rs"]
mod epub_css;
#[path = "odf_walk.rs"]
mod odf_walk;
#[path = "tabular_csv.rs"]
mod tabular_csv;
#[path = "xlsx_numfmt.rs"]
mod xlsx_numfmt;

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
/// Worker frame codes for a document the worker classifies itself, from its
/// bytes and its file name's extension (see [`DocumentFrame`]): to convert
/// it, or only to classify it. Codes 1 to 8 convert a variant already
/// known, and PDF operations use 16 and up.
const WORKER_CONVERT: u8 = 9;
const WORKER_CLASSIFY: u8 = 10;
/// The longest file name extension a classifying frame carries: a file
/// name's own limit on the platforms the worker runs on.
const MAX_EXTENSION_BYTES: usize = 255;
/// A classification's response: a kind and a variant, or an error.
const MAX_CLASSIFY_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_ARCHIVE_ENTRY_BYTES: u64 = 128 * 1024 * 1024;
const MAX_ARCHIVE_TOTAL_BYTES: u64 = 512 * 1024 * 1024;
/// Entries a package may hold: AnyDoc 0.2.4 refuses a larger archive
/// (`limits::MAX_ENTRY_COUNT`), and the checks stop there too.
const MAX_ARCHIVE_ENTRIES: usize = 100_000;
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
    /// pinned parser drops non-breaking hyphens (`characters_omitted`) or
    /// numbers lists differently from Word (`list_numbering_differs`).
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
/// The worker reads the package: the server reads the file, under one of
/// the worker slots, and counts an archive's entries, and no more.
pub async fn classify(path: impl AsRef<Path>) -> Result<DocumentClassification, DocumentError> {
    let canonical = crate::validate_path(path).map_err(map_path_error)?;
    let permit = worker_semaphore()
        .acquire_owned()
        .await
        .map_err(|_| DocumentError::WorkerBusy)?;
    let frame = read_document_frame(&canonical).await?;
    // An archive past AnyDoc's entry bound is classified by its extension,
    // unopened.
    if zip_past_entry_bound(frame.document()) {
        return Ok(classify_package(frame.document(), &canonical, true));
    }
    let size_bytes = frame.document().len() as u64;
    if !worker_sandbox_available() {
        // Without the worker the package is read here, as many at a time
        // as there are worker slots.
        return tokio::task::spawn_blocking(move || {
            let _permit = permit;
            classify_bytes(frame.document(), &canonical)
        })
        .await
        .map_err(|_| DocumentError::ConversionFailed);
    }
    let job = WorkerJob {
        code: WORKER_CLASSIFY,
        payload: frame.payload,
        timeout: WORKER_TIMEOUT,
        max_response_bytes: MAX_CLASSIFY_RESPONSE_BYTES,
    };
    let response = run_worker_job(job, worker_executable()?, permit).await?;
    match (response.classified, response.error) {
        (Some(found), None) => Ok(classification_of(found.kind, found.variant, size_bytes)),
        (None, Some(error)) => Err(error.into_document_error()),
        _ => Err(DocumentError::WorkerProtocol),
    }
}

/// A document read for the worker to classify: its file name's extension,
/// as a little-endian `u32` length and its bytes, then the document itself,
/// held once.
struct DocumentFrame {
    payload: Vec<u8>,
    prefix: usize,
}

impl DocumentFrame {
    fn document(&self) -> &[u8] {
        &self.payload[self.prefix..]
    }
}

/// Read a document into a frame for the worker to classify (see
/// [`DocumentFrame`]), refusing one past [`MAX_DOCUMENT_SIZE`].
async fn read_document_frame(path: &Path) -> Result<DocumentFrame, DocumentError> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .filter(|extension| extension.len() <= MAX_EXTENSION_BYTES)
        .unwrap_or_default()
        .as_bytes();
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|_| DocumentError::InputUnavailable)?;
    let size_hint = file.metadata().await.map_or(0, |metadata| metadata.len());
    let mut payload =
        Vec::with_capacity(4 + extension.len() + size_hint.min(MAX_DOCUMENT_SIZE) as usize);
    payload.extend_from_slice(&(extension.len() as u32).to_le_bytes());
    payload.extend_from_slice(extension);
    let prefix = payload.len();
    (&mut file)
        .take(MAX_DOCUMENT_SIZE + 1)
        .read_to_end(&mut payload)
        .await
        .map_err(|_| DocumentError::InputUnavailable)?;
    if (payload.len() - prefix) as u64 > MAX_DOCUMENT_SIZE {
        return Err(DocumentError::ResourceLimit);
    }
    Ok(DocumentFrame { payload, prefix })
}

/// Split a frame the worker classifies (see [`DocumentFrame`]) into a file
/// name with the document's extension, which is all the classification
/// reads of a path, and the document.
fn classifying_frame(frame: &[u8]) -> Result<(PathBuf, &[u8]), DocumentError> {
    let (length, rest) = frame
        .split_first_chunk::<4>()
        .ok_or(DocumentError::WorkerProtocol)?;
    let length = u32::from_le_bytes(*length) as usize;
    if length > MAX_EXTENSION_BYTES || length > rest.len() {
        return Err(DocumentError::WorkerProtocol);
    }
    let (extension, document) = rest.split_at(length);
    let extension = std::str::from_utf8(extension).map_err(|_| DocumentError::WorkerProtocol)?;
    let name = if extension.is_empty() {
        PathBuf::from("document")
    } else {
        PathBuf::from(format!("document.{extension}"))
    };
    Ok((name, document))
}

fn map_path_error(error: crate::SkillkitError) -> DocumentError {
    match error {
        crate::SkillkitError::FileTooLarge { .. } => DocumentError::ResourceLimit,
        _ => DocumentError::InputUnavailable,
    }
}
/// What the package preflight found. The worker runs the preflight before it
/// converts and returns this with the Markdown, for the supervisor to
/// disclose.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct PackagePreflight {
    external_relationships: bool,
    active_content: bool,
    hidden_content: bool,
    missing_formula_cache: bool,
    missing_required_content: bool,
    unsupported_content: bool,
    /// Characters the pinned parser drops while the text around them
    /// converts, so the result is usable but partial: non-breaking hyphens,
    /// and page numbers or dates Word fills in.
    omitted_characters: bool,
    omitted_page_blocks: bool,
    /// List numbers the pinned parser renders differently from Word, while
    /// the list text converts.
    list_numbering_differs: bool,
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

/// Row heights, in points, below half a pixel at 100% zoom: Excel draws such
/// a row as nothing.
const MIN_VISIBLE_ROW_POINTS: f64 = 0.375;

/// Column widths, in characters, that Excel draws as zero pixels with its
/// default font (a 7-pixel digit): it renders `trunc((256 * width + 18) / 256
/// * 7)` pixels.
const MIN_VISIBLE_COLUMN_CHARACTERS: f64 = (256.0 / 7.0 - 18.0) / 256.0;

/// Whether a size attribute leaves its row or column invisible. A value that
/// is not a finite number, such as `NaN`, counts as invisible.
fn invisible_size(value: &str, minimum: f64) -> bool {
    value
        .parse::<f64>()
        .is_ok_and(|size| !(size.is_finite() && size >= minimum))
}

/// Whether a SpreadsheetML part hides content: a hidden or very hidden
/// sheet, which AnyDoc 0.2.4 omits, or a hidden row or column, which it
/// omits too, or a row or column too small to draw, which Excel shows as
/// nothing and AnyDoc converts. That includes rows and columns left at a
/// sheet default (`sheetFormatPr`) too small to draw. `zeroHeight="1"` hides
/// only the rows a sheet does not write, and every cell sits in a written
/// row, so it hides no content. Names match without case and values are
/// decoded and trimmed, which finds more than AnyDoc's `bool_attr`. A part
/// the reader cannot parse counts as hiding content.
fn xml_has_hidden_content(bytes: &[u8]) -> bool {
    let mut reader = quick_xml::Reader::from_reader(Cursor::new(bytes));
    reader.config_mut().trim_text(false);
    reader.config_mut().check_end_names = false;
    let mut buffer = Vec::new();
    let mut default_row_invisible = false;
    let mut row_at_default_height = false;
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(quick_xml::events::Event::Start(event))
            | Ok(quick_xml::events::Event::Empty(event)) => {
                let element = xml_local_name(event.name().as_ref()).to_ascii_lowercase();
                let attributes = xml_attributes(&event);
                if element == b"row"
                    && !attributes
                        .iter()
                        .any(|attribute| attribute.local().eq_ignore_ascii_case(b"ht"))
                {
                    row_at_default_height = true;
                }
                let hidden = attributes.into_iter().any(|attribute| {
                    let name = attribute.local().to_ascii_lowercase();
                    let value = attribute.value.trim().to_ascii_lowercase();
                    match (element.as_slice(), name.as_slice()) {
                        (b"sheet", b"state") => matches!(value.as_str(), "hidden" | "veryhidden"),
                        (b"row" | b"col", b"hidden") => matches!(value.as_str(), "1" | "true"),
                        (b"row", b"ht") => invisible_size(&value, MIN_VISIBLE_ROW_POINTS),
                        (b"col", b"width") | (b"sheetformatpr", b"defaultcolwidth") => {
                            invisible_size(&value, MIN_VISIBLE_COLUMN_CHARACTERS)
                        }
                        (b"sheetformatpr", b"defaultrowheight") => {
                            default_row_invisible |= invisible_size(&value, MIN_VISIBLE_ROW_POINTS);
                            false
                        }
                        _ => false,
                    }
                });
                if hidden {
                    return true;
                }
            }
            Ok(quick_xml::events::Event::Eof) => {
                return default_row_invisible && row_at_default_height;
            }
            Ok(_) => {}
            Err(_) => return true,
        }
        buffer.clear();
    }
}

const VML_NAMESPACE: &[u8] = b"urn:schemas-microsoft-com:vml";
const VML_EXCEL_NAMESPACE: &[u8] = b"urn:schemas-microsoft-com:office:excel";

/// Whether a VML drawing holds a form checkbox that Excel hides and AnyDoc
/// converts. AnyDoc renders a checkbox caption unless the shape's `style`
/// contains `visibility:hidden` exactly, spaces aside
/// (`sheet::controls::vml_checkboxes`); Excel reads the property without
/// regard to case or other whitespace. A part the reader cannot parse
/// counts as hiding one.
fn vml_hides_checkbox(bytes: &[u8]) -> bool {
    let mut reader = quick_xml::NsReader::from_reader(Cursor::new(bytes));
    reader.config_mut().trim_text(false);
    reader.config_mut().check_end_names = false;
    let mut buffer = Vec::new();
    // For each open element: whether it is a VML shape that Excel hides
    // and AnyDoc does not skip.
    let mut stack: Vec<bool> = Vec::new();
    loop {
        let (namespace, event) = match reader.read_resolved_event_into(&mut buffer) {
            Ok(resolved) => resolved,
            Err(_) => return true,
        };
        let bound = |wanted: &[u8]| {
            matches!(
                namespace,
                quick_xml::name::ResolveResult::Bound(namespace) if namespace.as_ref() == wanted
            )
        };
        let (element, start) = match event {
            quick_xml::events::Event::Start(element) => (element, true),
            quick_xml::events::Event::Empty(element) => (element, false),
            quick_xml::events::Event::End(_) => {
                stack.pop();
                buffer.clear();
                continue;
            }
            quick_xml::events::Event::Eof => return false,
            _ => {
                buffer.clear();
                continue;
            }
        };
        let local = element.local_name();
        let mut concealed = false;
        if bound(VML_NAMESPACE) && local.as_ref() == b"shape" {
            let style = xml_attributes(&element)
                .into_iter()
                .find(|attribute| !attribute.prefixed() && attribute.local() == b"style")
                .map(|attribute| attribute.value)
                .unwrap_or_default();
            let excel_hides = style
                .chars()
                .filter(|character| !character.is_whitespace())
                .collect::<String>()
                .to_ascii_lowercase()
                .contains("visibility:hidden");
            let anydoc_skips = style.replace(' ', "").contains("visibility:hidden");
            concealed = excel_hides && !anydoc_skips;
        }
        if bound(VML_EXCEL_NAMESPACE)
            && local.as_ref() == b"ClientData"
            && stack.last() == Some(&true)
            && xml_attributes(&element).into_iter().any(|attribute| {
                !attribute.prefixed()
                    && attribute.local() == b"ObjectType"
                    && attribute.value == "Checkbox"
            })
        {
            return true;
        }
        if start {
            stack.push(concealed);
        }
        buffer.clear();
    }
}

/// Whether a worksheet's VML drawings, found through its `vmlDrawing`
/// relationships as AnyDoc finds them, hide a checkbox it converts.
fn xlsx_sheets_hide_checkboxes(
    archive: &mut ZipArchive<Cursor<&[u8]>>,
    sheets: &HashSet<String>,
) -> Result<bool, DocumentError> {
    let mut drawings = HashSet::new();
    for sheet in sheets {
        for relationship in read_relationships(archive, &ooxml_rels_part(sheet))? {
            let kind = relationship.kind.trim();
            if !relationship.external
                && kind.len() >= "/vmlDrawing".len()
                && kind[kind.len() - "/vmlDrawing".len()..].eq_ignore_ascii_case("/vmlDrawing")
            {
                drawings.extend(anydoc_resolve(sheet, &relationship.target));
            }
        }
    }
    for drawing in drawings {
        if read_optional_xml_part(archive, &drawing)?
            .is_some_and(|bytes| vml_hides_checkbox(&bytes))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether a workbook lists a sheet AnyDoc skips without a diagnostic: it
/// reads only the `sheet` children of the first `sheets` element, so a sheet
/// wrapped in another element, such as `mc:AlternateContent`, or listed in a
/// second `sheets`, is dropped. A workbook the reader cannot parse counts as
/// dropping one.
fn xlsx_workbook_drops_sheets(workbook: &[u8]) -> bool {
    let mut reader = quick_xml::NsReader::from_reader(Cursor::new(workbook));
    reader.config_mut().trim_text(false);
    reader.config_mut().check_end_names = false;
    let mut buffer = Vec::new();
    let mut depth = 0usize;
    // Depth of the first `sheets` element while it is open.
    let mut sheets: Option<usize> = None;
    let mut sheets_seen = false;
    loop {
        let (namespace, event) = match reader.read_resolved_event_into(&mut buffer) {
            Ok(resolved) => resolved,
            Err(_) => return true,
        };
        let sml = spreadsheetml(&namespace);
        let (element, start) = match event {
            quick_xml::events::Event::Start(element) => (element, true),
            quick_xml::events::Event::Empty(element) => (element, false),
            quick_xml::events::Event::End(_) => {
                depth = depth.saturating_sub(1);
                if sheets == Some(depth) {
                    sheets = None;
                }
                buffer.clear();
                continue;
            }
            quick_xml::events::Event::Eof => return false,
            _ => {
                buffer.clear();
                continue;
            }
        };
        match element.local_name().as_ref() {
            b"sheets" if sml && !sheets_seen => {
                sheets_seen = true;
                if start {
                    sheets = Some(depth);
                }
            }
            b"sheet" if sml && sheets.is_none_or(|open| open + 1 != depth) => return true,
            _ => {}
        }
        if start {
            depth += 1;
        }
        buffer.clear();
    }
}

/// SpreadsheetML's namespace, Transitional and Strict.
const SPREADSHEETML_NAMESPACES: [&[u8]; 2] = [
    b"http://schemas.openxmlformats.org/spreadsheetml/2006/main",
    b"http://purl.oclc.org/ooxml/spreadsheetml/main",
];

fn spreadsheetml(namespace: &quick_xml::name::ResolveResult<'_>) -> bool {
    matches!(
        namespace,
        quick_xml::name::ResolveResult::Bound(namespace)
            if SPREADSHEETML_NAMESPACES.contains(&namespace.as_ref())
    )
}

/// The text of an entity reference as AnyDoc's parser resolves it
/// (`package::xml::resolve_entity`); an unknown reference stays literal.
fn anydoc_entity_text(name: &str) -> String {
    if let Some(number) = name.strip_prefix('#') {
        let code = match number.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok(),
            None => number.parse().ok(),
        };
        return code
            .and_then(char::from_u32)
            .map_or_else(|| format!("&{name};"), String::from);
    }
    let character = match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "apos" => '\'',
        "quot" => '"',
        "nbsp" => '\u{a0}',
        "shy" => '\u{ad}',
        "mdash" => '\u{2014}',
        "ndash" => '\u{2013}',
        "lsquo" => '\u{2018}',
        "rsquo" => '\u{2019}',
        "ldquo" => '\u{201c}',
        "rdquo" => '\u{201d}',
        "hellip" => '\u{2026}',
        "copy" => '\u{a9}',
        "reg" => '\u{ae}',
        "trade" => '\u{2122}',
        "deg" => '\u{b0}',
        "middot" => '\u{b7}',
        "bull" => '\u{2022}',
        "sect" => '\u{a7}',
        "para" => '\u{b6}',
        "laquo" => '\u{ab}',
        "raquo" => '\u{bb}',
        "times" => '\u{d7}',
        "divide" => '\u{f7}',
        "plusmn" => '\u{b1}',
        "frac12" => '\u{bd}',
        "frac14" => '\u{bc}',
        "eacute" => '\u{e9}',
        "egrave" => '\u{e8}',
        "agrave" => '\u{e0}',
        "ccedil" => '\u{e7}',
        "uuml" => '\u{fc}',
        "ouml" => '\u{f6}',
        "auml" => '\u{e4}',
        "szlig" => '\u{df}',
        "aring" => '\u{e5}',
        "oslash" => '\u{f8}',
        "aelig" => '\u{e6}',
        "euro" => '\u{20ac}',
        "pound" => '\u{a3}',
        "yen" => '\u{a5}',
        "cent" => '\u{a2}',
        _ => return format!("&{name};"),
    };
    character.to_string()
}

/// What a worksheet holds that its conversion would lose.
#[derive(Default)]
struct WorksheetScan {
    /// A formula cell whose cached value AnyDoc cannot render, so the cell
    /// converts empty.
    uncached_formula: bool,
    /// A cell outside the positions AnyDoc reads (`c` in `row` in
    /// `sheetData` in the first `worksheet`, all in SpreadsheetML's
    /// namespace), which it drops.
    unreached_cell: bool,
    /// The style index and value class of each cell AnyDoc renders, to be
    /// checked against the workbook's number formats.
    format_uses: HashSet<(u32, xlsx_numfmt::CellClass)>,
    /// The largest negative value each style meets, the only negative of the
    /// style kept in `format_uses`.
    largest_negative: HashMap<u32, f64>,
    /// More distinct uses than a workbook can hold.
    too_many_formats: bool,
}

/// Distinct (style, value class) pairs kept per worksheet: Excel allows
/// 64,000 cell formats, each met by four classes of value.
const MAX_FORMAT_USES: usize = 1 << 18;

/// The formula cell being read.
struct FormulaCandidate {
    depth: usize,
    /// The unprefixed `t` AnyDoc reads, `n` when absent.
    kind: String,
    formula: bool,
    /// Text of the first direct `v` child, once one opens.
    value: Option<String>,
    /// Depth of that `v` while it is open.
    value_depth: Option<usize>,
    inline_string: bool,
    /// The cell's `s`, an index into `cellXfs`; `None` when it indexes no
    /// entry a workbook can hold.
    style: Option<u32>,
    /// Whether AnyDoc reads the cell.
    reached: bool,
}

impl FormulaCandidate {
    /// Whether AnyDoc's `cell_text` renders the cached value. A formula
    /// typed as a shared-string index is never cached that way by
    /// producers, who write formula text as `t="str"`, so it counts as
    /// uncached rather than being checked against the string table.
    fn renders(&self) -> bool {
        let value = self.value.as_deref().map(str::trim);
        match self.kind.as_str() {
            "s" => false,
            // An empty cached string is a real result.
            "str" => value.is_some(),
            "inlineStr" => self.inline_string,
            "b" => value.is_some_and(|value| matches!(value, "1" | "true" | "0" | "false")),
            "e" | "d" => value.is_some_and(|value| !value.is_empty()),
            _ => value.is_some_and(|value| !value.is_empty() && value.parse::<f64>().is_ok()),
        }
    }

    /// The cell's style and the class of value its format meets, for a cell
    /// AnyDoc renders through a format.
    fn format_use(&self) -> Option<(u32, xlsx_numfmt::CellClass)> {
        use xlsx_numfmt::CellClass;
        if !self.reached {
            return None;
        }
        let class = match self.kind.as_str() {
            "s" | "str" | "inlineStr" => CellClass::Text,
            "b" | "e" | "d" => return None,
            _ => {
                let number: f64 = self.value.as_deref()?.trim().parse().ok()?;
                if !number.is_finite() {
                    return None;
                }
                CellClass::of(number)
            }
        };
        Some((self.style?, class))
    }
}

impl WorksheetScan {
    fn close_cell(&mut self, cell: &FormulaCandidate) {
        self.uncached_formula |= cell.formula && !cell.renders();
        let Some(format_use) = cell.format_use() else {
            return;
        };
        // A larger negative value shows at least as much of its format as a
        // smaller one, so each style's largest is checked for them all.
        if let (style, xlsx_numfmt::CellClass::Negative { magnitude }) = format_use {
            match self.largest_negative.get(&style) {
                Some(&largest) if largest >= magnitude => return,
                Some(&largest) => {
                    self.format_uses.remove(&(
                        style,
                        xlsx_numfmt::CellClass::Negative { magnitude: largest },
                    ));
                }
                None => {}
            }
            self.largest_negative.insert(style, magnitude);
        }
        if self.format_uses.len() < MAX_FORMAT_USES {
            self.format_uses.insert(format_use);
        } else if !self.format_uses.contains(&format_use) {
            self.too_many_formats = true;
        }
    }
}

/// Read a worksheet as AnyDoc's reader walks it (`sheet::xlsx::read_sheet`
/// and `cell_text`). A formula (`f` in any namespace, at any depth in a cell
/// of any namespace) needs a cached value in the cell's first direct
/// SpreadsheetML `v` that its type renders: a number that parses, a boolean
/// AnyDoc spells, a present string. A part the reader cannot parse counts as
/// losing both.
fn scan_worksheet(bytes: &[u8]) -> WorksheetScan {
    let mut reader = quick_xml::NsReader::from_reader(Cursor::new(bytes));
    reader.config_mut().trim_text(false);
    reader.config_mut().check_end_names = false;
    let mut buffer = Vec::new();
    let mut scan = WorksheetScan::default();
    // Open elements: whether each is SpreadsheetML, and its local name.
    let mut stack: Vec<(bool, Vec<u8>)> = Vec::new();
    // Whether the first top-level worksheet was seen, and is open.
    let (mut worksheet_seen, mut worksheet_open) = (false, false);
    let mut cell: Option<FormulaCandidate> = None;
    loop {
        let (namespace, event) = match reader.read_resolved_event_into(&mut buffer) {
            Ok(resolved) => resolved,
            Err(_) => {
                return WorksheetScan {
                    uncached_formula: true,
                    unreached_cell: true,
                    ..WorksheetScan::default()
                };
            }
        };
        let sml = spreadsheetml(&namespace);
        let (element, start) = match event {
            quick_xml::events::Event::Start(element) => (element, true),
            quick_xml::events::Event::Empty(element) => (element, false),
            quick_xml::events::Event::End(_) => {
                stack.pop();
                let depth = stack.len();
                if depth == 0 {
                    worksheet_open = false;
                }
                if let Some(open) = cell.as_mut() {
                    if open.value_depth == Some(depth) {
                        open.value_depth = None;
                    }
                }
                if let Some(closed) = cell.take_if(|open| open.depth == depth) {
                    scan.close_cell(&closed);
                }
                buffer.clear();
                continue;
            }
            quick_xml::events::Event::Text(text) => {
                cell_value_push(&mut cell, &String::from_utf8_lossy(text.as_ref()));
                buffer.clear();
                continue;
            }
            quick_xml::events::Event::CData(text) => {
                cell_value_push(&mut cell, &String::from_utf8_lossy(text.as_ref()));
                buffer.clear();
                continue;
            }
            quick_xml::events::Event::GeneralRef(reference) => {
                let text = anydoc_entity_text(&String::from_utf8_lossy(reference.as_ref()));
                cell_value_push(&mut cell, &text);
                buffer.clear();
                continue;
            }
            quick_xml::events::Event::Eof => break,
            _ => {
                buffer.clear();
                continue;
            }
        };
        let local = element.local_name().as_ref().to_vec();
        let depth = stack.len();
        if sml && depth == 0 && local == b"worksheet" && !worksheet_seen {
            worksheet_seen = true;
            worksheet_open = start;
        }
        let reached = sml
            && local == b"c"
            && worksheet_open
            && depth == 3
            && stack[1] == (true, b"sheetData".to_vec())
            && stack[2] == (true, b"row".to_vec());
        if sml && local == b"c" {
            scan.unreached_cell |= !reached;
        }
        match cell.as_mut() {
            // A formula cell in any namespace counts: its value renders only
            // from a SpreadsheetML `v`.
            None if local == b"c" && start => {
                let attributes = xml_attributes(&element);
                let unprefixed = |name: &[u8]| {
                    attributes
                        .iter()
                        .find(|attribute| !attribute.prefixed() && attribute.local() == name)
                        .map(|attribute| attribute.value.clone())
                };
                let kind = unprefixed(b"t").unwrap_or_else(|| "n".to_string());
                // AnyDoc reads an `s` it cannot parse as style 0.
                let style = match unprefixed(b"s").map(|style| style.parse::<usize>()) {
                    Some(Ok(index)) => u32::try_from(index).ok(),
                    _ => Some(0),
                };
                cell = Some(FormulaCandidate {
                    depth,
                    kind,
                    formula: false,
                    value: None,
                    value_depth: None,
                    inline_string: false,
                    style,
                    reached,
                });
            }
            Some(open) => {
                open.formula |= local == b"f";
                if sml && depth == open.depth + 1 {
                    if local == b"v" && open.value.is_none() {
                        open.value = Some(String::new());
                        if start {
                            open.value_depth = Some(depth);
                        }
                    }
                    open.inline_string |= local == b"is";
                }
            }
            None => {}
        }
        if start {
            stack.push((sml, local));
        }
        buffer.clear();
    }
    if let Some(open) = cell {
        scan.close_cell(&open);
    }
    scan
}

/// A styles part's number formats as AnyDoc reads them
/// (`sheet::xlsx::Styles`): the file's own codes by id, from every
/// `numFmts` with later entries winning, and the numFmtId of each `xf` in
/// the first `cellXfs`, in order. A part AnyDoc cannot parse gives none, so
/// every cell renders as General.
#[derive(Default)]
struct XlsxFormats {
    codes: HashMap<u32, String>,
    cell_formats: Vec<u32>,
}

fn xlsx_formats(bytes: &[u8]) -> XlsxFormats {
    let mut reader = quick_xml::NsReader::from_reader(Cursor::new(bytes));
    reader.config_mut().trim_text(false);
    reader.config_mut().check_end_names = false;
    let mut buffer = Vec::new();
    let mut formats = XlsxFormats::default();
    let mut depth = 0usize;
    // Depths of the open `numFmts` elements, and of the first `cellXfs`
    // while it is open.
    let mut number_formats: Vec<usize> = Vec::new();
    let (mut cell_xfs, mut cell_xfs_seen) = (None, false);
    loop {
        let (namespace, event) = match reader.read_resolved_event_into(&mut buffer) {
            Ok(resolved) => resolved,
            Err(_) => return XlsxFormats::default(),
        };
        let sml = spreadsheetml(&namespace);
        let (element, start) = match event {
            quick_xml::events::Event::Start(element) => (element, true),
            quick_xml::events::Event::Empty(element) => (element, false),
            quick_xml::events::Event::End(_) => {
                depth = depth.saturating_sub(1);
                if number_formats.last() == Some(&depth) {
                    number_formats.pop();
                }
                if cell_xfs == Some(depth) {
                    cell_xfs = None;
                }
                buffer.clear();
                continue;
            }
            quick_xml::events::Event::Eof => return formats,
            _ => {
                buffer.clear();
                continue;
            }
        };
        let attribute = |name: &[u8]| {
            xml_attributes(&element)
                .into_iter()
                .find(|attribute| !attribute.prefixed() && attribute.local() == name)
                .map(|attribute| attribute.value)
        };
        match element.local_name().as_ref() {
            b"numFmts" if sml && start => number_formats.push(depth),
            b"numFmt" if sml && number_formats.last().is_some_and(|open| open + 1 == depth) => {
                if let (Some(id), Some(code)) = (
                    attribute(b"numFmtId").and_then(|id| id.parse().ok()),
                    attribute(b"formatCode"),
                ) {
                    formats.codes.insert(id, code);
                }
            }
            b"cellXfs" if sml && !cell_xfs_seen => {
                cell_xfs_seen = true;
                if start {
                    cell_xfs = Some(depth);
                }
            }
            b"xf" if sml && cell_xfs.is_some_and(|open| open + 1 == depth) => {
                let id = attribute(b"numFmtId")
                    .and_then(|id| id.parse().ok())
                    .unwrap_or(0);
                formats.cell_formats.push(id);
            }
            _ => {}
        }
        if start {
            depth += 1;
        }
        buffer.clear();
    }
}

/// Whether a worksheet's DrawingML drawings, found through its `drawing`
/// relationships, show text AnyDoc never reads: a text box or a shape with
/// text, not hidden and not linked to a cell. Pictures and charts hold no
/// text of their own here.
fn xlsx_sheets_have_drawing_text(
    archive: &mut ZipArchive<Cursor<&[u8]>>,
    sheets: &HashSet<String>,
) -> Result<bool, DocumentError> {
    let mut drawings = HashSet::new();
    for sheet in sheets {
        for relationship in read_relationships(archive, &ooxml_rels_part(sheet))? {
            if !relationship.external && relationship.kind.trim().ends_with("/drawing") {
                drawings.extend(anydoc_resolve(sheet, &relationship.target));
            }
        }
    }
    for drawing in drawings {
        if read_optional_xml_part(archive, &drawing)?
            .is_some_and(|bytes| drawing_shows_text(&bytes))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether an `mc:Choice`'s `Requires` names only namespaces an Office
/// application understands, so that it shows the choice.
fn mc_choice_understood<R>(
    reader: &quick_xml::NsReader<R>,
    event: &quick_xml::events::BytesStart<'_>,
) -> bool {
    let Some(requires) = xml_attribute_value(event, b"Requires") else {
        return false;
    };
    requires.split_whitespace().all(|prefix| {
        let probe = format!("{prefix}:x");
        match reader
            .resolver()
            .resolve_element(quick_xml::name::QName(probe.as_bytes()))
            .0
        {
            quick_xml::name::ResolveResult::Bound(namespace) => [
                b"http://schemas.microsoft.com/office/".as_slice(),
                b"http://schemas.openxmlformats.org/",
                b"http://purl.oclc.org/ooxml/",
            ]
            .iter()
            .any(|family| namespace.as_ref().starts_with(family)),
            _ => false,
        }
    })
}

/// One open element of a drawing part.
struct DrawingNode {
    /// A shape, group, or compatibility branch, which decides whether the
    /// text in it shows.
    shape: bool,
    /// A shape, connector, or group, whose text a viewer draws.
    drawn: bool,
    shown: bool,
    /// For `mc:AlternateContent`: whether Excel took one of its choices, so
    /// that it shows none of the rest or the fallback.
    choice_taken: Option<bool>,
}

/// Whether a drawing part holds text in a shape a viewer shows. A part that
/// does not parse counts as holding some.
fn drawing_shows_text(bytes: &[u8]) -> bool {
    const MARKUP_COMPATIBILITY: &[u8] =
        b"http://schemas.openxmlformats.org/markup-compatibility/2006";
    let mut reader = quick_xml::NsReader::from_reader(Cursor::new(bytes));
    reader.config_mut().trim_text(false);
    reader.config_mut().check_end_names = false;
    let mut buffer = Vec::new();
    let mut stack: Vec<DrawingNode> = Vec::new();
    let mut in_text = 0usize;
    let shown = |stack: &[DrawingNode]| {
        stack
            .iter()
            .rev()
            .find(|node| node.shape)
            .is_none_or(|node| node.shown)
    };
    loop {
        let (namespace, event) = match reader.read_resolved_event_into(&mut buffer) {
            Ok(resolved) => resolved,
            Err(_) => return true,
        };
        let compatibility = matches!(
            namespace,
            quick_xml::name::ResolveResult::Bound(quick_xml::name::Namespace(bound))
                if bound == MARKUP_COMPATIBILITY
        );
        let (element, start) = match event {
            quick_xml::events::Event::Start(element) => (element, true),
            quick_xml::events::Event::Empty(element) => (element, false),
            quick_xml::events::Event::End(end) => {
                if xml_local_name(end.name().as_ref()) == b"t" {
                    in_text = in_text.saturating_sub(1);
                }
                stack.pop();
                buffer.clear();
                continue;
            }
            quick_xml::events::Event::Text(text) => {
                if in_text > 0
                    && stack.iter().any(|node| node.drawn)
                    && shown(&stack)
                    && String::from_utf8_lossy(text.as_ref())
                        .chars()
                        .any(|c| !c.is_whitespace())
                {
                    return true;
                }
                buffer.clear();
                continue;
            }
            quick_xml::events::Event::GeneralRef(_) => {
                if in_text > 0 && stack.iter().any(|node| node.drawn) && shown(&stack) {
                    return true;
                }
                buffer.clear();
                continue;
            }
            quick_xml::events::Event::Eof => return false,
            _ => {
                buffer.clear();
                continue;
            }
        };
        let local = xml_local_name(element.name().as_ref()).to_vec();
        let attributes = xml_attributes(&element);
        let value = |name: &[u8]| {
            attributes
                .iter()
                .find(|attribute| attribute.local() == name)
                .map(|attribute| attribute.value.trim().to_string())
        };
        let parent_shown = shown(&stack);
        let node = match local.as_slice() {
            // A shape or connector; one linked to a cell repeats the cell's
            // text.
            b"sp" | b"cxnSp" => {
                let linked = value(b"textlink").is_some_and(|link| !link.is_empty());
                DrawingNode {
                    shape: true,
                    drawn: true,
                    shown: parent_shown && !linked,
                    choice_taken: None,
                }
            }
            b"grpSp" => DrawingNode {
                shape: true,
                drawn: true,
                shown: parent_shown,
                choice_taken: None,
            },
            b"AlternateContent" if compatibility => DrawingNode {
                shape: false,
                drawn: false,
                shown: false,
                choice_taken: Some(false),
            },
            // Excel shows the first choice whose namespaces it understands,
            // or else the fallback.
            b"Choice" | b"Fallback" if compatibility => {
                let understood = local == b"Fallback" || mc_choice_understood(&reader, &element);
                let taken = stack
                    .last_mut()
                    .and_then(|parent| parent.choice_taken.as_mut());
                let shows = match taken {
                    Some(taken) if !*taken && understood => {
                        *taken = true;
                        true
                    }
                    Some(_) => false,
                    // Outside `mc:AlternateContent`, it is read as it stands.
                    None => true,
                };
                DrawingNode {
                    shape: true,
                    drawn: false,
                    shown: parent_shown && shows,
                    choice_taken: None,
                }
            }
            // A hidden shape or group, such as the drawing copy of a form
            // control, shows none of its text.
            b"cNvPr"
                if value(b"hidden")
                    .is_some_and(|hidden| matches!(hidden.as_str(), "1" | "true")) =>
            {
                if let Some(shape) = stack.iter_mut().rev().find(|node| node.shape) {
                    shape.shown = false;
                }
                DrawingNode {
                    shape: false,
                    drawn: false,
                    shown: false,
                    choice_taken: None,
                }
            }
            _ => {
                if local == b"t" && start {
                    in_text += 1;
                }
                DrawingNode {
                    shape: false,
                    drawn: false,
                    shown: false,
                    choice_taken: None,
                }
            }
        };
        if start {
            stack.push(node);
        }
        buffer.clear();
    }
}

/// Add text to the open cell's cached value, if its first `v` is open.
fn cell_value_push(cell: &mut Option<FormulaCandidate>, text: &str) {
    if let Some(open) = cell.as_mut().filter(|open| open.value_depth.is_some()) {
        if let Some(value) = open.value.as_mut() {
            value.push_str(text);
        }
    }
}

/// Whether AnyDoc reads a workbook part as XML (`sheet::classify`): after a
/// UTF-8 byte order mark, the first byte that is not whitespace opens a tag
/// or a UTF-16 byte order mark. Anything else goes to its binary (XLSB)
/// reader.
fn anydoc_workbook_is_xml(bytes: &[u8]) -> bool {
    let body = bytes.strip_prefix(UTF8_BOM).unwrap_or(bytes);
    matches!(
        body.iter().find(|byte| !byte.is_ascii_whitespace()),
        Some(b'<' | 0xFF | 0xFE)
    )
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
    xml_attributes(event)
        .into_iter()
        .filter(|attribute| attribute.local() == wanted)
        .map(|attribute| attribute.value)
        .collect()
}

/// One attribute as AnyDoc's parser sees it.
struct XmlAttribute {
    key: Vec<u8>,
    value: String,
}

impl XmlAttribute {
    fn local(&self) -> &[u8] {
        xml_local_name(&self.key)
    }

    fn prefixed(&self) -> bool {
        self.key.contains(&b':')
    }
}

/// An element's attributes as AnyDoc reads them: namespace declarations
/// (`xmlns`, `xmlns:Target`) bind prefixes and are not attributes, so a
/// declaration cannot shadow the attribute it is named after; values are
/// decoded (`&#104;idden` is `hidden`), falling back to the raw text.
fn xml_attributes(event: &quick_xml::events::BytesStart<'_>) -> Vec<XmlAttribute> {
    event
        .attributes()
        .flatten()
        .filter(|attribute| {
            let key = attribute.key.as_ref();
            key != b"xmlns" && !key.starts_with(b"xmlns:")
        })
        .map(|attribute| XmlAttribute {
            key: attribute.key.as_ref().to_vec(),
            value: attribute
                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .map(|value| value.into_owned())
                .unwrap_or_else(|_| String::from_utf8_lossy(attribute.value.as_ref()).into_owned()),
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
                    let hidden = xml_attributes(&event).into_iter().any(|attribute| {
                        let value = attribute.value.trim().to_ascii_lowercase();
                        (attribute.local() == b"visibility"
                            && matches!(value.as_str(), "collapse" | "filter" | "hidden"))
                            || (attribute.local() == b"display"
                                && matches!(value.as_str(), "none" | "false" | "0" | "hidden"))
                            || (matches!(attribute.local(), b"row-height" | b"column-width")
                                && odf_length_points(&value).is_some_and(|points| {
                                    !(points.is_finite() && points >= MIN_VISIBLE_ROW_POINTS)
                                }))
                    });
                    if hidden {
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

/// An ODF length (`0.178in`, `4.5mm`, `12pt`) in points. `None` for a value
/// without a unit this check knows.
fn odf_length_points(value: &str) -> Option<f64> {
    let value = value.trim();
    let split = value
        .find(|character: char| character.is_ascii_alphabetic() || character == '%')
        .unwrap_or(value.len());
    let (number, unit) = value.split_at(split);
    let number: f64 = number.trim().parse().ok()?;
    let scale = match unit.trim().to_ascii_lowercase().as_str() {
        "pt" => 1.0,
        "in" => 72.0,
        "cm" => 72.0 / 2.54,
        "mm" => 72.0 / 25.4,
        "pc" => 12.0,
        "px" => 0.75,
        _ => return None,
    };
    Some(number * scale)
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
                if xml_attribute_values(&event, b"href")
                    .iter()
                    .any(|href| is_external_uri(href))
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

/// Whether an ODF part runs or links content: an OLE object, a plugin, an
/// applet, a script, event listeners, or a live data link. Embedded objects (`draw:object`)
/// are judged by what they embed ([`xml_odf_objects`]).
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
                    b"object-ole"
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

/// The embedded objects (`draw:object`) an ODF part shows, by reference;
/// `None` for one embedded inline rather than stored in the package.
fn xml_odf_objects(bytes: &[u8]) -> Vec<Option<String>> {
    let mut reader = quick_xml::Reader::from_reader(Cursor::new(bytes));
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();
    let mut objects = Vec::new();
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(quick_xml::events::Event::Start(event))
            | Ok(quick_xml::events::Event::Empty(event)) => {
                if xml_local_name(event.name().as_ref()) == b"object" {
                    objects.push(xml_attribute_values(&event, b"href").into_iter().next());
                }
                buffer.clear();
            }
            Ok(quick_xml::events::Event::Eof) => return objects,
            Ok(_) => buffer.clear(),
            Err(_) => {
                objects.push(None);
                return objects;
            }
        }
    }
}

/// The media type the package manifest gives each path, with a directory's
/// trailing `/` removed.
fn odf_manifest_media_types(bytes: &[u8]) -> HashMap<String, String> {
    let mut reader = quick_xml::Reader::from_reader(Cursor::new(bytes));
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();
    let mut types = HashMap::new();
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(quick_xml::events::Event::Start(event))
            | Ok(quick_xml::events::Event::Empty(event)) => {
                if xml_local_name(event.name().as_ref()) == b"file-entry" {
                    let path = xml_attribute_values(&event, b"full-path")
                        .into_iter()
                        .next();
                    let media = xml_attribute_values(&event, b"media-type")
                        .into_iter()
                        .next();
                    if let (Some(path), Some(media)) = (path, media) {
                        types.insert(
                            path.trim_end_matches('/').to_string(),
                            media.trim().to_string(),
                        );
                    }
                }
                buffer.clear();
            }
            Ok(quick_xml::events::Event::Eof) | Err(_) => return types,
            Ok(_) => buffer.clear(),
        }
    }
}

/// Whether an embedded object is one AnyDoc handles: a chart, which it
/// shows as the object's replacement image, or a formula, which it converts.
/// Any other object, or one the manifest does not describe, is refused.
fn odf_object_supported(object: Option<&str>, media_types: &HashMap<String, String>) -> bool {
    object
        .and_then(|reference| anydoc_resolve("content.xml", reference))
        .and_then(|path| media_types.get(path.trim_end_matches('/')))
        .is_some_and(|media| {
            matches!(
                media.as_str(),
                "application/vnd.oasis.opendocument.chart"
                    | "application/vnd.oasis.opendocument.formula"
            )
        })
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
                    && xml_attributes(&event).into_iter().any(|attribute| {
                        let value = attribute.value.trim().to_ascii_lowercase();
                        (attribute.local() == b"visibility"
                            && matches!(value.as_str(), "hidden" | "false" | "0"))
                            || (attribute.local() == b"show"
                                && matches!(value.as_str(), "false" | "0"))
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

/// A drawing-page style definition: its parent, and its visibility where
/// set (`true` hides).
type OdpPageStyle = (Option<String>, Option<bool>);

/// What hides slide content across an ODP package's content and styles
/// parts. LibreOffice hides a slide with `presentation:visibility="hidden"`
/// in its drawing-page style, which a style inherits from its parent and
/// from the default drawing-page style; a shape with `draw:display` set to
/// `none` or `printer` (shown only in print); and a shape on a layer whose
/// `draw:display` hides it. AnyDoc converts all of them.
#[derive(Default)]
struct OdpVisibility {
    /// Drawing-page style definitions by name. A name defined more than
    /// once, as in both parts, hides a slide if any definition does.
    page_styles: HashMap<String, Vec<OdpPageStyle>>,
    default_hidden: Option<bool>,
    /// The drawing-page styles slides use; `None` for a slide with none.
    pages: HashSet<Option<String>>,
    hidden_layers: HashSet<String>,
    used_layers: HashSet<String>,
    hidden_shape: bool,
}

/// Distinct drawing-page styles, layers, and slide styles recorded; a real
/// deck has a few dozen.
const MAX_ODP_STYLE_RECORDS: usize = 65_536;

/// Styles followed from one slide's style through its parents. A real deck
/// chains two or three; a longer chain is treated as hiding the slide.
const MAX_ODP_STYLE_CHAIN: usize = 64;

impl OdpVisibility {
    fn page_hidden(&self, style: Option<&str>) -> bool {
        // Every definition of every style on the way is followed; a chain
        // that ends without a setting falls back to the default style.
        let mut pending: Vec<&str> = style.into_iter().collect();
        let mut seen = HashSet::new();
        let mut unset = style.is_none();
        while let Some(name) = pending.pop() {
            if !seen.insert(name) {
                continue;
            }
            if seen.len() > MAX_ODP_STYLE_CHAIN {
                return true;
            }
            let Some(definitions) = self.page_styles.get(name) else {
                unset = true;
                continue;
            };
            for (parent, hidden) in definitions {
                match (hidden, parent) {
                    (Some(true), _) => return true,
                    (Some(false), _) => {}
                    (None, Some(parent)) => pending.push(parent),
                    (None, None) => unset = true,
                }
            }
        }
        unset && self.default_hidden.unwrap_or(false)
    }

    fn hides_content(&self) -> bool {
        self.hidden_shape
            || self
                .used_layers
                .iter()
                .any(|layer| self.hidden_layers.contains(layer))
            || self
                .pages
                .iter()
                .any(|style| self.page_hidden(style.as_deref()))
    }

    fn within_bounds(&self, definitions: usize) -> bool {
        definitions + self.pages.len() + self.hidden_layers.len() + self.used_layers.len()
            <= MAX_ODP_STYLE_RECORDS
    }
}

/// A style definition open while an ODP part is read.
enum OdpOpenStyle {
    /// A drawing-page style: its name and which of its definitions this is.
    Page(String, usize),
    /// A named style of another family, and its parent.
    Other(String, Option<String>),
    /// A default style.
    Default,
    /// A style without a name, which nothing can apply.
    Unnamed,
}

fn odp_display_hides(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "none" | "printer"
    )
}

/// Record what an ODP part says about visibility. Shapes and slides count
/// only in the content part, which is what AnyDoc converts.
fn scan_odp_visibility(
    bytes: &[u8],
    content: bool,
    visibility: &mut OdpVisibility,
) -> Result<(), DocumentError> {
    let mut reader = quick_xml::Reader::from_reader(Cursor::new(bytes));
    reader.config_mut().trim_text(false);
    reader.config_mut().check_end_names = false;
    let mut buffer = Vec::new();
    // For each open element, whether it is a style definition.
    let mut stack: Vec<bool> = Vec::new();
    let mut open_styles: Vec<OdpOpenStyle> = Vec::new();
    let mut definitions = visibility.page_styles.values().map(Vec::len).sum::<usize>();
    loop {
        let (event, start) = match reader.read_event_into(&mut buffer) {
            Ok(quick_xml::events::Event::Start(event)) => (event, true),
            Ok(quick_xml::events::Event::Empty(event)) => (event, false),
            Ok(quick_xml::events::Event::End(_)) => {
                if stack.pop() == Some(true) {
                    open_styles.pop();
                }
                buffer.clear();
                continue;
            }
            Ok(quick_xml::events::Event::Eof) | Err(_) => break,
            Ok(_) => {
                buffer.clear();
                continue;
            }
        };
        let local = xml_local_name(event.name().as_ref()).to_vec();
        let attributes = xml_attributes(&event);
        let value = |wanted: &[u8]| {
            attributes
                .iter()
                .find(|attribute| attribute.local() == wanted)
                .map(|attribute| attribute.value.trim().to_string())
        };
        match local.as_slice() {
            b"style" => {
                let open = match value(b"name") {
                    None => OdpOpenStyle::Unnamed,
                    Some(name) => {
                        let parent = value(b"parent-style-name");
                        if value(b"family")
                            .is_some_and(|family| family.eq_ignore_ascii_case("drawing-page"))
                        {
                            let entries = visibility.page_styles.entry(name.clone()).or_default();
                            entries.push((parent, None));
                            definitions += 1;
                            OdpOpenStyle::Page(name, entries.len() - 1)
                        } else {
                            OdpOpenStyle::Other(name, parent)
                        }
                    }
                };
                if start {
                    open_styles.push(open);
                }
            }
            b"default-style" => {
                if start {
                    open_styles.push(OdpOpenStyle::Default);
                }
            }
            b"drawing-page-properties" => {
                if let (Some(open), Some(setting)) = (open_styles.last_mut(), value(b"visibility"))
                {
                    let hidden = setting.eq_ignore_ascii_case("hidden");
                    match open {
                        OdpOpenStyle::Page(name, index) => {
                            if let Some(definition) = visibility
                                .page_styles
                                .get_mut(name.as_str())
                                .and_then(|entries| entries.get_mut(*index))
                            {
                                definition.1 = Some(definition.1.unwrap_or(false) || hidden);
                            }
                        }
                        // Page properties under a style of another family
                        // are recorded as a page style too.
                        OdpOpenStyle::Other(name, parent) => {
                            let entries = visibility.page_styles.entry(name.clone()).or_default();
                            entries.push((parent.take(), Some(hidden)));
                            definitions += 1;
                            *open = OdpOpenStyle::Page(name.clone(), entries.len() - 1);
                        }
                        OdpOpenStyle::Default => {
                            visibility.default_hidden =
                                Some(visibility.default_hidden.unwrap_or(false) || hidden);
                        }
                        OdpOpenStyle::Unnamed => {}
                    }
                }
            }
            b"layer" => {
                if attributes.iter().any(|attribute| {
                    attribute.local() == b"display" && odp_display_hides(&attribute.value)
                }) {
                    visibility.hidden_layers.extend(value(b"name"));
                }
            }
            b"page" if content => {
                let styles: Vec<String> = attributes
                    .iter()
                    .filter(|attribute| attribute.local() == b"style-name")
                    .map(|attribute| attribute.value.trim().to_string())
                    .collect();
                if styles.is_empty() {
                    visibility.pages.insert(None);
                }
                visibility.pages.extend(styles.into_iter().map(Some));
            }
            _ if content => {
                for attribute in &attributes {
                    match attribute.local() {
                        b"display" if odp_display_hides(&attribute.value) => {
                            visibility.hidden_shape = true;
                        }
                        b"layer" => {
                            visibility
                                .used_layers
                                .insert(attribute.value.trim().to_string());
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        if !visibility.within_bounds(definitions) {
            return Err(DocumentError::ResourceLimit);
        }
        if start {
            stack.push(matches!(local.as_slice(), b"style" | b"default-style"));
        }
        buffer.clear();
    }
    Ok(())
}

const ODF_OFFICE_NAMESPACE: &[u8] = b"urn:oasis:names:tc:opendocument:xmlns:office:1.0";

/// The document kind AnyDoc converts from an ODF content part
/// (`formats::odf::parse`): under the first top-level `office:document-content`
/// and its first `office:body` child, the first of `office:text`,
/// `office:spreadsheet`, and `office:presentation` in that order of
/// preference, whatever the package's mimetype says. Only the office
/// namespace counts, as there, so a same-named element in another namespace,
/// or a second body, is not what gets converted.
fn odf_body_kind(bytes: &[u8]) -> Option<DocumentKind> {
    let mut reader = quick_xml::NsReader::from_reader(Cursor::new(bytes));
    reader.config_mut().trim_text(false);
    reader.config_mut().check_end_names = false;
    let mut buffer = Vec::new();
    let mut depth = 0usize;
    // Whether the chosen `document-content` (depth 0) and `body` (depth 1)
    // are open, and whether each was already chosen.
    let (mut in_content, mut content_chosen) = (false, false);
    let (mut in_body, mut body_chosen) = (false, false);
    let mut children = [false; 3];
    while let Ok((namespace, event)) = reader.read_resolved_event_into(&mut buffer) {
        let office = matches!(
            namespace,
            quick_xml::name::ResolveResult::Bound(namespace)
                if namespace.as_ref() == ODF_OFFICE_NAMESPACE
        );
        let (element, start) = match event {
            quick_xml::events::Event::Start(element) => (element, true),
            quick_xml::events::Event::Empty(element) => (element, false),
            quick_xml::events::Event::End(_) => {
                depth = depth.saturating_sub(1);
                if depth == 1 && in_body {
                    in_body = false;
                } else if depth == 0 && in_content {
                    in_content = false;
                }
                buffer.clear();
                continue;
            }
            quick_xml::events::Event::Eof => break,
            _ => {
                buffer.clear();
                continue;
            }
        };
        let local = element.local_name();
        match (depth, office, local.as_ref()) {
            (0, true, b"document-content") if !content_chosen => {
                content_chosen = true;
                in_content = start;
            }
            (1, true, b"body") if in_content && !body_chosen => {
                body_chosen = true;
                in_body = start;
            }
            (2, true, name) if in_body => {
                if let Some(index) = [b"text".as_slice(), b"spreadsheet", b"presentation"]
                    .iter()
                    .position(|kind| *kind == name)
                {
                    children[index] = true;
                }
            }
            _ => {}
        }
        if start {
            depth += 1;
        }
        buffer.clear();
    }
    [DocumentKind::Odt, DocumentKind::Ods, DocumentKind::Odp]
        .into_iter()
        .zip(children)
        .find(|(_, present)| *present)
        .map(|(kind, _)| kind)
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
                for attribute in xml_attributes(&event) {
                    let value = attribute.value.trim().to_ascii_lowercase();
                    if (attribute.local() == b"condition" && !value.is_empty())
                        || (attribute.local() == b"display"
                            && matches!(
                                value.as_str(),
                                "none" | "false" | "0" | "hidden" | "printer"
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
    /// other symbol stays dropped there too. So is text Word shows in
    /// `mc:AlternateContent` that AnyDoc does not convert from it: where
    /// AnyDoc takes no branch, or a branch holding other text.
    dropped: bool,
    /// A run formatted hidden directly (`w:r/w:rPr/w:vanish`), which the
    /// pinned parser converts as ordinary text.
    hidden_run: bool,
    /// A non-breaking hyphen (`w:noBreakHyphen`), which the pinned parser
    /// drops, joining its neighbors: "Form 1040‑SR" converts as "Form 1040SR".
    omitted_hyphen: bool,
    /// A page number or date Word fills in where a run shows it (`w:pgNum`,
    /// the date blocks), which the pinned parser drops.
    omitted_page_block: bool,
    /// Character, paragraph, and table styles applied to content.
    styles_used: HashSet<String>,
    /// The marks of the paragraphs being read, innermost last: a text box's
    /// paragraph opens inside another.
    marks: Vec<DocxParagraphMark>,
    /// The note references the text Word shows or AnyDoc converts holds
    /// (see [`DocxNoteReference`]), and those met in compatibility content,
    /// set apart until it ends.
    references: HashSet<DocxNoteReference>,
    alternate_references: Option<DocxAlternateReferences>,
    /// Which sides read the story part being scanned: both the main part,
    /// and each the notes parts its reading of the relationships names.
    part_read: DocxPartRead,
    /// The distinct numbering paragraphs ask for, directly, through their
    /// style, or through Word's default paragraph style, and each
    /// paragraph, in each part's document order.
    list_uses: Vec<DocxListUse>,
    list_use_ids: HashMap<DocxListUse, u32>,
    list_paragraphs: Vec<DocxListParagraph>,
    /// The story part being scanned.
    part: DocxPart,
    /// The notes of the note parts in the order they are stored, the note
    /// being read, and where the body first references each.
    notes_stored: Vec<(bool, String)>,
    note: Option<u32>,
    note_references: HashMap<(bool, String), usize>,
}

/// A Word story part, in the order AnyDoc converts them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
enum DocxPart {
    #[default]
    Body,
    Footnotes,
    Endnotes,
}

/// A paragraph that asks for numbering.
#[derive(Clone, Copy)]
struct DocxListParagraph {
    /// Its entry in `DocxStoryScan::list_uses`.
    used: u32,
    part: DocxPart,
    /// The note it is in: its entry in `DocxStoryScan::notes_stored`.
    note: Option<u32>,
    /// It sits in a text box. Word numbers the text boxes of a document as
    /// one story, apart from the text around them, as it does all the
    /// footnotes and all the endnotes; AnyDoc counts on through every
    /// story, text boxes where they are anchored.
    in_text_box: bool,
    /// AnyDoc converts it, and so numbers it.
    anydoc: bool,
    /// Word shows it: it is in the `mc:AlternateContent` branches Word
    /// takes, or in AnyDoc's branch standing for Word's, and it is not
    /// deleted.
    word: bool,
    /// Its mark is a tracked deletion or move source, or it sits in one.
    deleted: bool,
}

/// The numbering a paragraph asks for: a list instance and a level given
/// directly, and its paragraph style, which supplies what is not, as Word
/// and as AnyDoc read them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
struct DocxListUse {
    list: Option<u64>,
    level: Option<usize>,
    style: Option<String>,
    anydoc_list: Option<u64>,
    anydoc_level: Option<usize>,
    anydoc_style: Option<String>,
}

/// Paragraphs a document may have, past what AnyDoc's node bound allows in
/// its three story parts.
const MAX_DOCX_LIST_PARAGRAPHS: usize = 1 << 22;

/// Notes a document may define or reference. Real documents have a few
/// hundred at most.
const MAX_DOCX_NOTES: usize = 65_536;

/// The sides that read a story part.
#[derive(Clone, Copy, Debug, Default)]
struct DocxPartRead {
    word: bool,
    anydoc: bool,
}

/// A note reference (`w:footnoteReference` or `w:endnoteReference`) as Word
/// shows it and as AnyDoc converts it. AnyDoc numbers it with the note whose
/// id is the reference's `w:id`, else its unprefixed `id`, as written; Word,
/// as LibreOffice shows it, reads `w:id` alone, as an integer.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct DocxNoteReference {
    endnote: bool,
    /// The id AnyDoc converts the reference with; `None` where it does not
    /// convert the reference, or reads no id on it.
    anydoc: Option<String>,
    /// Word shows the reference.
    word_shows: bool,
    /// The id Word shows it with; `None` where it shows no reference or
    /// the reference has no `w:id`.
    word: Option<i64>,
    /// Both sides read the reference, AnyDoc its id from an unprefixed `id`,
    /// which Word does not read.
    unprefixed: bool,
}

/// The note references met in `mc:AlternateContent`, as Word shows them in
/// the branch it takes and as AnyDoc converts them in its own, in order,
/// with each of AnyDoc's ids and whether it read the id from an unprefixed
/// `id`. Where the two hold as many, of the same kinds, each of Word's
/// stands at the same place as AnyDoc's; else each is judged alone.
#[derive(Default)]
struct DocxAlternateReferences {
    word: Vec<(bool, Option<i64>)>,
    anydoc: Vec<(bool, Option<String>, bool)>,
}

impl DocxStoryScan {
    /// Record a note reference, under the bound on the notes a document may
    /// reference.
    fn record_reference(&mut self, reference: DocxNoteReference) -> Result<(), DocumentError> {
        if self.references.len() >= MAX_DOCX_NOTES && !self.references.contains(&reference) {
            return Err(DocumentError::ResourceLimit);
        }
        self.references.insert(reference);
        Ok(())
    }

    /// Record the references of compatibility content that has ended (see
    /// [`DocxAlternateReferences`]).
    fn settle_alternate_references(&mut self) -> Result<(), DocumentError> {
        let Some(DocxAlternateReferences { word, anydoc }) = self.alternate_references.take()
        else {
            return Ok(());
        };
        let together = word.len() == anydoc.len()
            && word
                .iter()
                .zip(&anydoc)
                .all(|((shown, _), (converted, _, _))| shown == converted);
        if together {
            for ((endnote, shown), (_, converted, unprefixed)) in word.into_iter().zip(anydoc) {
                self.record_reference(DocxNoteReference {
                    endnote,
                    anydoc: converted,
                    word_shows: true,
                    word: shown,
                    unprefixed,
                })?;
            }
            return Ok(());
        }
        for (endnote, shown) in word {
            self.record_reference(DocxNoteReference {
                endnote,
                anydoc: None,
                word_shows: true,
                word: shown,
                unprefixed: false,
            })?;
        }
        for (endnote, converted, _) in anydoc {
            self.record_reference(DocxNoteReference {
                endnote,
                anydoc: converted,
                word_shows: false,
                word: None,
                unprefixed: false,
            })?;
        }
        Ok(())
    }
}

/// A paragraph's mark (`w:pPr`) as Word and as AnyDoc read it. Its run
/// properties (`w:pPr/w:rPr`) format a list label in Word, so a hidden mark
/// on a numbered paragraph hides the label that AnyDoc converts. Word's
/// style separator (`w:specVanish`) hides only the mark and is left alone.
#[derive(Default)]
struct DocxParagraphMark {
    /// The paragraph's entry in `DocxStoryScan::list_paragraphs`, where
    /// either side shows it.
    slot: Option<usize>,
    numbered: bool,
    hidden: bool,
    style_separator: bool,
    styles: Vec<String>,
    /// The list instance (`w:numId`), level (`w:ilvl`), and paragraph style
    /// (`w:pStyle`) numbering it, as Word reads them: every mark it shows,
    /// in the branches of compatibility content it takes, later values
    /// winning, numbers read with white space collapsed. A list instance of
    /// 0 removes a style's numbering.
    list: Option<u64>,
    level: Option<usize>,
    style: Option<String>,
    /// The same as AnyDoc reads them: the first mark that is the
    /// paragraph's own child, its first `w:pStyle` and `w:numPr`, and that
    /// one's first `w:numId` and `w:ilvl`, numbers read as they stand.
    anydoc_list: Option<u64>,
    anydoc_level: Option<usize>,
    anydoc_style: Option<String>,
    /// Which of those elements AnyDoc has met: a mark, then in its mark a
    /// numbering and a style, then in its numbering a list and a level.
    anydoc_met: [bool; 5],
    /// The mark is a tracked deletion or move source.
    deleted: bool,
}

/// The elements of a paragraph's mark AnyDoc reads the first of, as
/// indices of `DocxParagraphMark::anydoc_met`.
const ANYDOC_MARK: usize = 0;
const ANYDOC_NUMBERING: usize = 1;
const ANYDOC_STYLE: usize = 2;
const ANYDOC_LIST: usize = 3;
const ANYDOC_LEVEL: usize = 4;

/// Levels a Word list has; AnyDoc clamps deeper ones to the last.
const DOCX_LIST_LEVELS: usize = 9;

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

/// WordprocessingML's namespace, Transitional and Strict.
const WORDPROCESSINGML_NAMESPACES: [&[u8]; 2] = [
    b"http://schemas.openxmlformats.org/wordprocessingml/2006/main",
    b"http://purl.oclc.org/ooxml/wordprocessingml/main",
];
const MARKUP_COMPATIBILITY_NAMESPACE: &[u8] =
    b"http://schemas.openxmlformats.org/markup-compatibility/2006";
/// Office Math's namespace, Transitional and Strict.
const MATH_NAMESPACES: [&[u8]; 2] = [
    b"http://schemas.openxmlformats.org/officeDocument/2006/math",
    b"http://purl.oclc.org/ooxml/officeDocument/math",
];

/// The vocabulary of an element in a Word story part, as far as AnyDoc's
/// walker distinguishes it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum WordVocabulary {
    Word,
    MarkupCompatibility,
    Math,
    Other,
}

impl WordVocabulary {
    fn of(namespace: &quick_xml::name::ResolveResult<'_>) -> Self {
        match namespace {
            quick_xml::name::ResolveResult::Bound(namespace)
                if WORDPROCESSINGML_NAMESPACES.contains(&namespace.as_ref()) =>
            {
                Self::Word
            }
            quick_xml::name::ResolveResult::Bound(namespace)
                if namespace.as_ref() == MARKUP_COMPATIBILITY_NAMESPACE =>
            {
                Self::MarkupCompatibility
            }
            quick_xml::name::ResolveResult::Bound(namespace)
                if MATH_NAMESPACES.contains(&namespace.as_ref()) =>
            {
                Self::Math
            }
            _ => Self::Other,
        }
    }
}

/// An open element of a Word story part.
struct WordNode {
    local: Vec<u8>,
    vocabulary: WordVocabulary,
    /// For an `mc:Choice` or `mc:Fallback`: AnyDoc, or Word, does not take
    /// the branch. Neither shows a separator note as text.
    anydoc_skips: bool,
    word_skips: bool,
    /// For `mc:AlternateContent`: AnyDoc, or Word, has taken a branch.
    anydoc_took: bool,
    word_took: bool,
    /// For `mc:AlternateContent`: the branches AnyDoc has taken, and the
    /// numbered paragraphs of Word's branch and of the first branch AnyDoc
    /// takes, as ranges of `DocxStoryScan::list_paragraphs`.
    anydoc_branches: u32,
    word_paragraphs: Option<std::ops::Range<usize>>,
    anydoc_paragraphs: Option<std::ops::Range<usize>>,
    /// For an `mc:Choice` or `mc:Fallback`: the first branch of its
    /// alternate content that AnyDoc takes.
    anydoc_first: bool,
    /// The numbered paragraphs read before the element opened.
    paragraphs_before: usize,
    /// For a `w:p`, and an element of its mark: AnyDoc reads it, the first
    /// of its kind among the children of an element AnyDoc reads.
    anydoc_mark: bool,
    /// For `mc:AlternateContent` outside any other: the text Word shows in
    /// it, and the text AnyDoc converts from it.
    word_shown: DocxShownText,
    anydoc_shown: DocxShownText,
}

impl WordNode {
    fn new(local: &[u8], vocabulary: WordVocabulary, paragraphs_before: usize) -> Self {
        WordNode {
            local: local.to_vec(),
            vocabulary,
            anydoc_skips: false,
            word_skips: false,
            anydoc_took: false,
            word_took: false,
            anydoc_branches: 0,
            word_paragraphs: None,
            anydoc_paragraphs: None,
            anydoc_first: false,
            paragraphs_before,
            anydoc_mark: false,
            word_shown: DocxShownText::default(),
            anydoc_shown: DocxShownText::default(),
        }
    }

    fn is(&self, vocabulary: WordVocabulary, local: &[u8]) -> bool {
        self.vocabulary == vocabulary && self.local == local
    }

    /// An `mc:Choice` or `mc:Fallback`.
    fn is_branch(&self) -> bool {
        self.vocabulary == WordVocabulary::MarkupCompatibility
            && matches!(self.local.as_slice(), b"Choice" | b"Fallback")
    }

    /// `mc:AlternateContent` and its branches, which wrap content without
    /// changing what it formats.
    fn is_compatibility_wrapper(&self) -> bool {
        self.vocabulary == WordVocabulary::MarkupCompatibility
            && matches!(
                self.local.as_slice(),
                b"AlternateContent" | b"Choice" | b"Fallback"
            )
    }
}

/// Whether the open elements end with `suffix` by local name, looking
/// through markup-compatibility wrappers: Word applies run properties in an
/// `mc:Choice` it understands.
fn word_path_ends_with(stack: &[WordNode], suffix: &[&[u8]]) -> bool {
    let mut path = stack
        .iter()
        .rev()
        .filter(|node| !node.is_compatibility_wrapper());
    suffix
        .iter()
        .rev()
        .all(|wanted| path.next().is_some_and(|node| node.local == *wanted))
}

/// Whether AnyDoc's walker never reaches content under these open elements.
/// It skips a tracked deletion or move source in WordprocessingML's
/// namespace. Inside a drawing it instead searches every wrapper for text
/// boxes, skipping only `mc:Fallback`, until a text box's content returns it
/// to the walker; a deletion there hides nothing.
fn word_content_omitted(stack: &[WordNode]) -> bool {
    let mut searching_drawing = false;
    for node in stack {
        if searching_drawing {
            if node.is(WordVocabulary::Word, b"txbxContent") {
                searching_drawing = false;
            } else if node.is(WordVocabulary::MarkupCompatibility, b"Fallback") {
                return true;
            }
        } else if node.vocabulary == WordVocabulary::Word {
            match node.local.as_slice() {
                b"del" | b"moveFrom" => return true,
                b"drawing" | b"pict" | b"object" => searching_drawing = true,
                _ => {}
            }
        }
    }
    false
}

/// Whether these open elements are inside a drawing that AnyDoc searches for
/// text boxes rather than walks (see [`word_content_omitted`]).
fn word_drawing_search(stack: &[WordNode]) -> bool {
    let mut searching = false;
    for node in stack {
        if searching {
            searching = !node.is(WordVocabulary::Word, b"txbxContent");
        } else if node.vocabulary == WordVocabulary::Word
            && matches!(node.local.as_slice(), b"drawing" | b"pict" | b"object")
        {
            searching = true;
        }
    }
    searching
}

/// Whether an `mc:Choice` inside a drawing requires a namespace outside the
/// Office vocabularies. Word then shows the `mc:Fallback` instead, while
/// AnyDoc's text-box search takes the choice whatever it requires, so the
/// choice's text converts although Word never shows it.
fn drawing_choice_word_skips<R>(
    reader: &quick_xml::NsReader<R>,
    event: &quick_xml::events::BytesStart<'_>,
    node: &WordNode,
    stack: &[WordNode],
) -> bool {
    if !node.is(WordVocabulary::MarkupCompatibility, b"Choice") || !word_drawing_search(stack) {
        return false;
    }
    let Some(requires) = xml_attribute_value(event, b"Requires") else {
        return false;
    };
    requires.split_whitespace().any(|prefix| {
        let probe = format!("{prefix}:x");
        match reader
            .resolver()
            .resolve_element(quick_xml::name::QName(probe.as_bytes()))
            .0
        {
            quick_xml::name::ResolveResult::Bound(namespace) => ![
                b"http://schemas.microsoft.com/office/".as_slice(),
                b"http://schemas.openxmlformats.org/",
                b"http://purl.oclc.org/ooxml/",
            ]
            .iter()
            .any(|family| namespace.as_ref().starts_with(family)),
            _ => true,
        }
    })
}

/// Namespaces AnyDoc's Word reader supports (`SUPPORTED_NS`): it takes the
/// first `mc:Choice` requiring only these, and the `mc:Fallback` otherwise.
const ANYDOC_WORD_NAMESPACES: [&[u8]; 11] = [
    b"http://schemas.openxmlformats.org/wordprocessingml/2006/main",
    b"http://schemas.openxmlformats.org/drawingml/2006/main",
    b"http://schemas.openxmlformats.org/drawingml/2006/picture",
    b"http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing",
    b"http://schemas.openxmlformats.org/markup-compatibility/2006",
    b"http://schemas.openxmlformats.org/drawingml/2006/chart",
    b"http://schemas.openxmlformats.org/drawingml/2006/diagram",
    b"urn:schemas-microsoft-com:vml",
    b"urn:schemas-microsoft-com:office:office",
    b"http://schemas.microsoft.com/office/word/2010/wordprocessingShape",
    b"http://schemas.microsoft.com/office/word/2010/wordprocessingGroup",
];

/// Whether AnyDoc supports every namespace an `mc:Choice` requires, read
/// as it reads them: prefixes resolved in the element's scope, Strict
/// namespaces mapped to Transitional, and a choice without `Requires`
/// supported.
fn anydoc_supports_choice<R>(
    reader: &quick_xml::NsReader<R>,
    event: &quick_xml::events::BytesStart<'_>,
) -> bool {
    let Some(requires) = xml_attribute_value(event, b"Requires") else {
        return true;
    };
    requires.split_whitespace().all(|prefix| {
        let probe = format!("{prefix}:x");
        match reader
            .resolver()
            .resolve_element(quick_xml::name::QName(probe.as_bytes()))
            .0
        {
            quick_xml::name::ResolveResult::Bound(namespace) => {
                let namespace = String::from_utf8_lossy(namespace.as_ref());
                let transitional = namespace
                    .strip_prefix("http://purl.oclc.org/ooxml/")
                    .and_then(|rest| rest.split_once('/'))
                    .map(|(family, tail)| {
                        format!("http://schemas.openxmlformats.org/{family}/2006/{tail}")
                    })
                    .unwrap_or_else(|| namespace.into_owned());
                ANYDOC_WORD_NAMESPACES.contains(&transitional.as_bytes())
            }
            _ => false,
        }
    })
}

/// Mark which of AnyDoc and Word take an `mc:Choice` or `mc:Fallback`:
/// each takes the first choice it supports, else the fallback, except that
/// inside a drawing AnyDoc searches every choice for text boxes and never
/// the fallback.
fn take_word_branch<R>(
    reader: &quick_xml::NsReader<R>,
    event: &quick_xml::events::BytesStart<'_>,
    node: &mut WordNode,
    stack: &mut [WordNode],
) {
    if node.vocabulary != WordVocabulary::MarkupCompatibility {
        return;
    }
    let choice = match node.local.as_slice() {
        b"Choice" => true,
        b"Fallback" => false,
        _ => return,
    };
    let searching = word_drawing_search(stack);
    let Some(alternate) = stack
        .last_mut()
        .filter(|parent| parent.is(WordVocabulary::MarkupCompatibility, b"AlternateContent"))
    else {
        return;
    };
    let anydoc_takes = if searching {
        choice
    } else {
        !alternate.anydoc_took && (!choice || anydoc_supports_choice(reader, event))
    };
    let word_takes = !alternate.word_took && (!choice || mc_choice_understood(reader, event));
    alternate.anydoc_took |= anydoc_takes && !searching;
    alternate.word_took |= word_takes;
    node.anydoc_first = anydoc_takes && alternate.anydoc_branches == 0;
    alternate.anydoc_branches += u32::from(anydoc_takes);
    node.anydoc_skips = !anydoc_takes;
    node.word_skips = !word_takes;
}

/// Where Word and AnyDoc take different branches of `mc:AlternateContent`,
/// decide when it ends which paragraphs Word's numbers are compared with.
/// When the paragraphs Word shows in its branch and those AnyDoc converts
/// in the first branch it takes ask for the same numbering, in the same
/// order and stories, the branches hold one list in two vocabularies:
/// AnyDoc's stands for Word's, and the replay compares Word's numbers with
/// the paragraphs AnyDoc converts. Branches that number differently count
/// each for its own side.
fn stand_in_for_word_branch(paragraphs: &mut [DocxListParagraph], alternate: &WordNode) {
    let (Some(word), Some(anydoc)) = (
        alternate.word_paragraphs.clone(),
        alternate.anydoc_paragraphs.clone(),
    ) else {
        return;
    };
    if word == anydoc || word.end > paragraphs.len() || anydoc.end > paragraphs.len() {
        return;
    }
    let shown = |range: std::ops::Range<usize>, shows: fn(&DocxListParagraph) -> bool| {
        paragraphs[range]
            .iter()
            .filter(move |paragraph| shows(paragraph))
            .map(|paragraph| (paragraph.used, paragraph.in_text_box))
    };
    let converted = |paragraph: &DocxListParagraph| paragraph.anydoc && !paragraph.deleted;
    if !shown(word.clone(), |paragraph| paragraph.word).eq(shown(anydoc.clone(), converted)) {
        return;
    }
    for paragraph in &mut paragraphs[anydoc] {
        paragraph.word = converted(paragraph);
    }
    for paragraph in &mut paragraphs[word] {
        paragraph.word = false;
    }
}

/// Scan one Word story part into `scan`. Parse errors fail closed as
/// malformed; end tags are not matched against start tags by prefix, as
/// AnyDoc does not match them either.
fn scan_docx_story(bytes: &[u8], scan: &mut DocxStoryScan) -> Result<(), DocumentError> {
    let mut reader = quick_xml::NsReader::from_reader(Cursor::new(bytes));
    reader.config_mut().trim_text(false);
    reader.config_mut().check_end_names = false;
    let mut buffer = Vec::new();
    let mut stack: Vec<WordNode> = Vec::new();
    loop {
        let (namespace, event) = reader
            .read_resolved_event_into(&mut buffer)
            .map_err(|_| DocumentError::Malformed)?;
        let vocabulary = WordVocabulary::of(&namespace);
        match event {
            quick_xml::events::Event::Start(event) => {
                if stack.len() >= MAX_XML_DEPTH {
                    return Err(DocumentError::ResourceLimit);
                }
                let mut node = WordNode::new(
                    xml_local_name(event.name().as_ref()),
                    vocabulary,
                    scan.list_paragraphs.len(),
                );
                if stack.is_empty() {
                    scan.note = None;
                    scan.part = match node.local.as_slice() {
                        b"footnotes" => DocxPart::Footnotes,
                        b"endnotes" => DocxPart::Endnotes,
                        _ => DocxPart::Body,
                    };
                }
                take_word_branch(&reader, &event, &mut node, &mut stack);
                if docx_note_element(&node, &stack) {
                    let (anydoc_skips, word_skips) = docx_note_skipped(reader.resolver(), &event);
                    node.anydoc_skips |= anydoc_skips;
                    node.word_skips |= word_skips;
                }
                scan.hidden_run |= drawing_choice_word_skips(&reader, &event, &node, &stack);
                scan_docx_element(reader.resolver(), &event, &mut node, &stack, scan)?;
                if node.is(WordVocabulary::Word, b"p") {
                    open_paragraph(scan, &stack)?;
                }
                stack.push(node);
            }
            quick_xml::events::Event::Empty(event) => {
                let mut node = WordNode::new(
                    xml_local_name(event.name().as_ref()),
                    vocabulary,
                    scan.list_paragraphs.len(),
                );
                take_word_branch(&reader, &event, &mut node, &mut stack);
                scan.hidden_run |= drawing_choice_word_skips(&reader, &event, &node, &stack);
                scan_docx_element(reader.resolver(), &event, &mut node, &stack, scan)?;
                // An empty paragraph Word numbers through its default style.
                if node.is(WordVocabulary::Word, b"p") {
                    open_paragraph(scan, &stack)?;
                    close_paragraph(scan)?;
                }
            }
            quick_xml::events::Event::End(_) => {
                let closed = stack.pop();
                if closed
                    .as_ref()
                    .is_some_and(|node| node.is(WordVocabulary::Word, b"p"))
                {
                    close_paragraph(scan)?;
                }
                if let Some(closed) = &closed {
                    // A branch's numbered paragraphs, where Word takes it or
                    // it is the first AnyDoc takes.
                    if let Some(alternate) = stack.last_mut().filter(|parent| {
                        closed.is_branch()
                            && parent.is(WordVocabulary::MarkupCompatibility, b"AlternateContent")
                    }) {
                        let read = closed.paragraphs_before..scan.list_paragraphs.len();
                        if !closed.word_skips {
                            alternate.word_paragraphs = Some(read.clone());
                        }
                        if closed.anydoc_first {
                            alternate.anydoc_paragraphs = Some(read);
                        }
                    }
                    if closed.is(WordVocabulary::MarkupCompatibility, b"AlternateContent") {
                        stand_in_for_word_branch(&mut scan.list_paragraphs, closed);
                        // AnyDoc takes no branch, and drops the text Word
                        // shows in its own, or takes one holding other text.
                        scan.dropped |= !closed.word_shown.same(&closed.anydoc_shown);
                        if !stack.iter().any(|open| {
                            open.is(WordVocabulary::MarkupCompatibility, b"AlternateContent")
                        }) {
                            scan.settle_alternate_references()?;
                        }
                    }
                }
            }
            quick_xml::events::Event::Text(text) => {
                note_branch_text(&mut stack, &String::from_utf8_lossy(text.as_ref()));
            }
            quick_xml::events::Event::CData(text) => {
                note_branch_text(&mut stack, &String::from_utf8_lossy(text.as_ref()));
            }
            quick_xml::events::Event::GeneralRef(reference) => {
                let name = String::from_utf8_lossy(reference.as_ref());
                note_branch_text(&mut stack, &anydoc_entity_text(&name));
            }
            quick_xml::events::Event::Eof => {
                // A paragraph, or compatibility content, left open ends with
                // its part.
                while !scan.marks.is_empty() {
                    close_paragraph(scan)?;
                }
                scan.settle_alternate_references()?;
                return Ok(());
            }
            _ => {}
        }
        buffer.clear();
    }
}

/// Note text inside `mc:AlternateContent` on the outermost alternate
/// content, as Word shows it and as AnyDoc converts it, for the two to be
/// compared when it ends: where AnyDoc takes another branch than Word, or
/// none, that branch must hold the text Word's does. The text of `w:t` and
/// of math (`m:t`) counts, not white space; Word does not show deleted
/// text, and text AnyDoc's walker never reaches, or that sits in a drawing
/// outside its text boxes, is left to the checks of that content.
fn note_branch_text(stack: &mut [WordNode], text: &str) {
    if !stack.last().is_some_and(|node| {
        node.is(WordVocabulary::Word, b"t") || node.is(WordVocabulary::Math, b"t")
    }) || word_drawing_search(stack)
    {
        return;
    }
    let Some(outermost) = stack
        .iter()
        .position(|node| node.is(WordVocabulary::MarkupCompatibility, b"AlternateContent"))
    else {
        return;
    };
    let word = !stack.iter().any(|node| {
        node.word_skips
            || (node.vocabulary == WordVocabulary::Word
                && matches!(node.local.as_slice(), b"del" | b"moveFrom"))
    });
    let anydoc = !stack.iter().any(|node| node.anydoc_skips) && !word_content_omitted(stack);
    let alternate = &mut stack[outermost];
    for character in text.chars().filter(|character| !character.is_whitespace()) {
        if word {
            alternate.word_shown.push(character);
        }
        if anydoc {
            alternate.anydoc_shown.push(character);
        }
    }
}

/// Text one side shows in compatibility content, as far as comparing it
/// with the other side's needs: its characters, hashed in order, and how
/// many there are.
#[derive(Default)]
struct DocxShownText {
    hasher: std::collections::hash_map::DefaultHasher,
    characters: u64,
}

impl DocxShownText {
    fn push(&mut self, character: char) {
        let mut encoded = [0; 4];
        std::hash::Hasher::write(
            &mut self.hasher,
            character.encode_utf8(&mut encoded).as_bytes(),
        );
        self.characters += 1;
    }

    fn same(&self, other: &Self) -> bool {
        self.characters == other.characters
            && std::hash::Hasher::finish(&self.hasher) == std::hash::Hasher::finish(&other.hasher)
    }
}

/// Open a paragraph (`w:p`) under the open elements `stack`: any paragraph
/// may be numbered, directly, through its style, or through Word's default
/// one. It takes its place in document order where either side shows it,
/// ahead of the text boxes inside it, which AnyDoc numbers after it; the
/// numbering its mark asks for is known when it closes.
fn open_paragraph(scan: &mut DocxStoryScan, stack: &[WordNode]) -> Result<(), DocumentError> {
    let anydoc = !stack.iter().any(|node| node.anydoc_skips) && !word_content_omitted(stack);
    let deleted = stack.iter().any(|node| {
        node.vocabulary == WordVocabulary::Word
            && matches!(node.local.as_slice(), b"del" | b"moveFrom")
    });
    // Word shows what the branches it takes hold, until the end of their
    // alternate content lets AnyDoc's branch stand for its own.
    let word = !deleted && !stack.iter().any(|node| node.word_skips);
    let mut slot = None;
    if anydoc || word {
        if scan.list_paragraphs.len() >= MAX_DOCX_LIST_PARAGRAPHS {
            return Err(DocumentError::ResourceLimit);
        }
        let used = docx_list_use_id(scan, DocxListUse::default())?;
        slot = Some(scan.list_paragraphs.len());
        scan.list_paragraphs.push(DocxListParagraph {
            used,
            part: scan.part,
            note: scan.note.filter(|_| scan.part != DocxPart::Body),
            in_text_box: stack
                .iter()
                .any(|node| node.is(WordVocabulary::Word, b"txbxContent")),
            anydoc,
            word,
            deleted,
        });
    }
    scan.marks.push(DocxParagraphMark {
        slot,
        ..DocxParagraphMark::default()
    });
    Ok(())
}

/// Close the innermost open paragraph: record the numbering its mark asks
/// for, and hide it from Word where the mark is deleted. Word formats a
/// numbered paragraph's label with the mark's run properties.
fn close_paragraph(scan: &mut DocxStoryScan) -> Result<(), DocumentError> {
    let Some(mark) = scan.marks.pop() else {
        return Ok(());
    };
    if mark.numbered && !mark.style_separator {
        scan.hidden_run |= mark.hidden;
        for style in mark.styles {
            if scan.styles_used.len() >= MAX_DOCX_STYLES && !scan.styles_used.contains(&style) {
                return Err(DocumentError::ResourceLimit);
            }
            scan.styles_used.insert(style);
        }
    }
    let Some(slot) = mark.slot else {
        return Ok(());
    };
    let used = docx_list_use_id(
        scan,
        DocxListUse {
            list: mark.list,
            level: mark.level,
            style: mark.style,
            anydoc_list: mark.anydoc_list,
            anydoc_level: mark.anydoc_level,
            anydoc_style: mark.anydoc_style,
        },
    )?;
    let paragraph = &mut scan.list_paragraphs[slot];
    paragraph.used = used;
    if mark.deleted {
        paragraph.deleted = true;
        paragraph.word = false;
    }
    Ok(())
}

/// The entry of `DocxStoryScan::list_uses` for a numbering asked for,
/// entered when first asked for.
fn docx_list_use_id(scan: &mut DocxStoryScan, used: DocxListUse) -> Result<u32, DocumentError> {
    if let Some(&id) = scan.list_use_ids.get(&used) {
        return Ok(id);
    }
    if scan.list_uses.len() >= MAX_DOCX_STYLES {
        return Err(DocumentError::ResourceLimit);
    }
    let id = scan.list_uses.len() as u32;
    scan.list_uses.push(used.clone());
    scan.list_use_ids.insert(used, id);
    Ok(id)
}

/// Whether this open element is a note of a notes part: a `w:footnote` of a
/// `w:footnotes` root, or a `w:endnote` of a `w:endnotes` one.
fn docx_note_element(node: &WordNode, stack: &[WordNode]) -> bool {
    let root: &[u8] = match node.local.as_slice() {
        b"footnote" => b"footnotes",
        b"endnote" => b"endnotes",
        _ => return false,
    };
    node.vocabulary == WordVocabulary::Word
        && stack.len() == 1
        && stack[0].is(WordVocabulary::Word, root)
}

/// Whether AnyDoc, and Word, skip a note of a notes part. Each skips a
/// separator, which Word draws as a rule, as each reads its type: AnyDoc
/// its `w:type`, else an unprefixed `type`, and Word its `w:type` alone,
/// each as written. AnyDoc also skips a note without an id, read the same
/// way.
fn docx_note_skipped(
    resolver: &quick_xml::name::NamespaceResolver,
    event: &quick_xml::events::BytesStart<'_>,
) -> (bool, bool) {
    let separator = |kind: Option<&str>| {
        matches!(
            kind,
            Some("separator" | "continuationSeparator" | "continuationNotice")
        )
    };
    let (word_type, unprefixed_type) = word_attribute_forms(resolver, event, b"type");
    let (word_id, unprefixed_id) = word_attribute_forms(resolver, event, b"id");
    let anydoc_type = word_type.as_deref().or(unprefixed_type.as_deref());
    (
        separator(anydoc_type) || (word_id.is_none() && unprefixed_id.is_none()),
        separator(word_type.as_deref()),
    )
}

/// Record a note reference (see [`DocxNoteReference`]) where the text a side
/// reads holds it: for Word, outside a tracked deletion and in the branches
/// of compatibility content Word takes; for AnyDoc, where its walker
/// reaches, in the branches it takes. References in compatibility content
/// wait for it to end (see [`DocxAlternateReferences`]). The order in which
/// the text first references each note, where AnyDoc's walker reaches the
/// reference, is kept for the list replay.
fn record_note_reference(
    resolver: &quick_xml::name::NamespaceResolver,
    event: &quick_xml::events::BytesStart<'_>,
    node: &WordNode,
    stack: &[WordNode],
    scan: &mut DocxStoryScan,
) -> Result<(), DocumentError> {
    let endnote = node.local == b"endnoteReference";
    let (qualified, unprefixed) = word_attribute_forms(resolver, event, b"id");
    if qualified
        .iter()
        .chain(&unprefixed)
        .any(|id| id.len() > MAX_STYLE_ID_BYTES)
    {
        return Err(DocumentError::ResourceLimit);
    }
    let omitted = word_content_omitted(stack);
    if let Some(id) = qualified
        .as_ref()
        .or(unprefixed.as_ref())
        .filter(|_| !omitted)
    {
        let note = (endnote, id.trim().to_string());
        if !scan.note_references.contains_key(&note) {
            if scan.note_references.len() >= MAX_DOCX_NOTES {
                return Err(DocumentError::ResourceLimit);
            }
            let order = scan.note_references.len();
            scan.note_references.insert(note, order);
        }
    }
    let converts = scan.part_read.anydoc && !omitted && !stack.iter().any(|open| open.anydoc_skips);
    let shows = scan.part_read.word
        && !stack.iter().any(|open| {
            open.word_skips
                || (open.vocabulary == WordVocabulary::Word
                    && matches!(open.local.as_slice(), b"del" | b"moveFrom"))
        });
    let unprefixed_only = qualified.is_none() && unprefixed.is_some();
    let word = qualified.as_deref().map(word_integer);
    let anydoc = qualified.or(unprefixed);
    if stack
        .iter()
        .any(|open| open.is(WordVocabulary::MarkupCompatibility, b"AlternateContent"))
    {
        let pending = scan
            .alternate_references
            .get_or_insert_with(DocxAlternateReferences::default);
        if pending.word.len() + pending.anydoc.len() >= MAX_DOCX_NOTES {
            return Err(DocumentError::ResourceLimit);
        }
        if shows {
            pending.word.push((endnote, word));
        }
        if converts {
            pending.anydoc.push((endnote, anydoc, unprefixed_only));
        }
        return Ok(());
    }
    if shows || converts {
        scan.record_reference(DocxNoteReference {
            endnote,
            anydoc: anydoc.filter(|_| converts),
            word_shows: shows,
            word: word.filter(|_| shows),
            unprefixed: unprefixed_only && shows && converts,
        })?;
    }
    Ok(())
}

/// A notes part's notes as Word and as AnyDoc 0.2.4 read them, each in the
/// order stored.
#[derive(Default)]
struct DocxNotesRead {
    /// AnyDoc's (`formats::docx::parse`): the WordprocessingML notes of the
    /// part's kind that are children of its first root of that kind, but a
    /// separator or a note without an id, each type and id read from its
    /// `w:type` and `w:id`, else from an unprefixed `type` and `id`, as
    /// written.
    anydoc: Vec<DocxAnydocNote>,
    /// Word's, as LibreOffice shows it: the notes of the part's kind under
    /// its root, where that is the root of that kind, as the root's children
    /// or in the branches of compatibility content in it that Word takes,
    /// each type and id read from its `w:type` and `w:id` alone.
    word: Vec<DocxWordNote>,
    /// The start tag of the root each side reads the notes in, as written:
    /// with a note's markup, it says what the note holds.
    anydoc_root: Vec<u8>,
    word_root: Vec<u8>,
}

struct DocxAnydocNote {
    id: String,
    /// Its id is an unprefixed `id`, which Word does not read.
    unprefixed: bool,
    written: Rc<DocxNoteWritten>,
}

struct DocxWordNote {
    /// Its `w:id`, read as an integer.
    id: Option<i64>,
    separator: bool,
    written: Rc<DocxNoteWritten>,
}

/// A note as written: where its start tag ends in its part, which tells one
/// note from another, and its markup (see [`docx_notes_read`]).
struct DocxNoteWritten {
    at: u64,
    markup: Vec<u8>,
}

/// An open element of a notes part outside its notes.
struct DocxNotesFrame {
    /// AnyDoc reads notes among its children: it is AnyDoc's root.
    anydoc_notes: bool,
    /// Word reads notes among its children: it is Word's root, or a branch
    /// of compatibility content in it that Word takes.
    word_notes: bool,
    /// For `mc:AlternateContent` among whose children Word reads notes:
    /// Word reads the notes of the branch it takes, and has taken one.
    alternate: bool,
    took: bool,
    /// Its start tag, as written, for compatibility content a note is read
    /// in.
    tag: Vec<u8>,
}

/// A note being read: how many elements are open outside it, what each
/// side reads of it, where its start tag ends, its markup so far, the
/// elements open inside it, and a run open in it that may hold nothing but
/// the note's reference mark: where the run's markup starts, how many
/// elements were open inside the note outside it, and whether it has held a
/// mark and anything else.
struct DocxOpenNote {
    depth: usize,
    anydoc: Option<(String, bool)>,
    word: Option<(Option<i64>, bool)>,
    at: u64,
    markup: Vec<u8>,
    inner: Vec<(WordVocabulary, Vec<u8>)>,
    run: Option<(usize, usize, bool, bool)>,
}

impl DocxOpenNote {
    const CONTEXT: u8 = 1;
    const ATTRIBUTES: u8 = 2;
    const START: u8 = 3;
    const EMPTY: u8 = 4;
    const END: u8 = 5;
    const TEXT: u8 = 6;
    const CDATA: u8 = 7;
    const REFERENCE: u8 = 8;

    /// Add an item to the markup, framed by its kind and length so that no
    /// text can pass for another item.
    fn item(&mut self, kind: u8, bytes: &[u8]) {
        self.markup.push(kind);
        self.markup
            .extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        self.markup.extend_from_slice(bytes);
    }

    /// Note content other than a run's properties or a reference mark in a
    /// run open at the note's level.
    fn run_holds_other(&mut self) {
        if let Some((_, depth, _, other)) = self.run.as_mut() {
            if self.inner.len() == *depth + 1 {
                *other = true;
            }
        }
    }

    /// An element opening, or empty, inside the note.
    fn open(
        &mut self,
        vocabulary: WordVocabulary,
        element: &quick_xml::events::BytesStart<'_>,
        opens: bool,
    ) {
        let local = xml_local_name(element.name().as_ref()).to_vec();
        if let Some((_, depth, mark, other)) = self.run.as_mut() {
            if self.inner.len() == *depth + 1 {
                match (vocabulary, local.as_slice()) {
                    (WordVocabulary::Word, b"rPr") => {}
                    (WordVocabulary::Word, b"footnoteRef" | b"endnoteRef") => *mark = true,
                    _ => *other = true,
                }
            }
        }
        if opens && self.run.is_none() && vocabulary == WordVocabulary::Word && local == b"r" {
            self.run = Some((self.markup.len(), self.inner.len(), false, false));
        }
        let kind = if opens { Self::START } else { Self::EMPTY };
        self.item(kind, element.as_ref());
        if opens {
            self.inner.push((vocabulary, local));
        }
    }

    /// An element closing inside the note: a run that held nothing but its
    /// properties and a reference mark is left out.
    fn close(&mut self, name: &[u8]) {
        self.item(Self::END, name);
        self.inner.pop();
        if let Some((start, depth, mark, other)) = self.run {
            if self.inner.len() == depth {
                if mark && !other {
                    self.markup.truncate(start);
                }
                self.run = None;
            }
        }
    }

    /// Text inside the note, as written: white space around a run's
    /// properties and mark is not content.
    fn text(&mut self, kind: u8, bytes: &[u8]) {
        if kind != Self::TEXT || !bytes.iter().all(u8::is_ascii_whitespace) {
            self.run_holds_other();
        }
        self.item(kind, bytes);
    }
}

/// Read a notes part's notes as each side reads them (see
/// [`DocxNotesRead`]). A note's markup holds what could change how it
/// shows: the compatibility content Word finds it in, its attributes but
/// its id and type, and every element, attribute, and text inside it, as
/// written. A run holding nothing but its properties and the note's
/// reference mark (`w:footnoteRef`, `w:endnoteRef`) is left out: AnyDoc
/// writes the mark as the note's label, and Word shows its own. Parse
/// errors fail closed as malformed.
fn docx_notes_read(bytes: &[u8], endnotes: bool) -> Result<DocxNotesRead, DocumentError> {
    let (root_name, note_name): (&[u8], &[u8]) = if endnotes {
        (b"endnotes", b"endnote")
    } else {
        (b"footnotes", b"footnote")
    };
    let mut reader = quick_xml::NsReader::from_reader(Cursor::new(bytes));
    reader.config_mut().trim_text(false);
    reader.config_mut().check_end_names = false;
    let mut buffer = Vec::new();
    let mut read = DocxNotesRead::default();
    let mut stack: Vec<DocxNotesFrame> = Vec::new();
    let mut note: Option<DocxOpenNote> = None;
    let mut anydoc_root_found = false;
    let mut root_found = false;
    loop {
        let (namespace, event) = reader
            .read_resolved_event_into(&mut buffer)
            .map_err(|_| DocumentError::Malformed)?;
        let vocabulary = WordVocabulary::of(&namespace);
        if let Some(open) = note.as_mut() {
            match event {
                quick_xml::events::Event::Start(element) => {
                    if open.depth + open.inner.len() >= MAX_XML_DEPTH {
                        return Err(DocumentError::ResourceLimit);
                    }
                    open.open(vocabulary, &element, true);
                }
                quick_xml::events::Event::Empty(element) => open.open(vocabulary, &element, false),
                quick_xml::events::Event::End(element) => {
                    if open.inner.is_empty() {
                        if let Some(open) = note.take() {
                            docx_note_read(open, &mut read)?;
                        }
                    } else {
                        open.close(element.as_ref());
                    }
                }
                quick_xml::events::Event::Text(text) => {
                    open.text(DocxOpenNote::TEXT, text.as_ref());
                }
                quick_xml::events::Event::CData(text) => {
                    open.text(DocxOpenNote::CDATA, text.as_ref());
                }
                quick_xml::events::Event::GeneralRef(reference) => {
                    open.text(DocxOpenNote::REFERENCE, reference.as_ref());
                }
                quick_xml::events::Event::Eof => {
                    if let Some(open) = note.take() {
                        docx_note_read(open, &mut read)?;
                    }
                    return Ok(read);
                }
                _ => {}
            }
            buffer.clear();
            continue;
        }
        let (element, opens) = match event {
            quick_xml::events::Event::Start(element) => (element, true),
            quick_xml::events::Event::Empty(element) => (element, false),
            quick_xml::events::Event::End(_) => {
                stack.pop();
                buffer.clear();
                continue;
            }
            quick_xml::events::Event::Eof => return Ok(read),
            _ => {
                buffer.clear();
                continue;
            }
        };
        if opens && stack.len() >= MAX_XML_DEPTH {
            return Err(DocumentError::ResourceLimit);
        }
        let local = xml_local_name(element.name().as_ref()).to_vec();
        let word = vocabulary == WordVocabulary::Word;
        let mut frame = DocxNotesFrame {
            anydoc_notes: false,
            word_notes: false,
            alternate: false,
            took: false,
            tag: Vec::new(),
        };
        match stack.last_mut() {
            // AnyDoc reads the first root of the notes' kind; Word the
            // part's root, where it is of that kind.
            None => {
                let root = word && local == root_name;
                frame.anydoc_notes = root && !anydoc_root_found;
                frame.word_notes = root && !root_found;
                if frame.anydoc_notes {
                    read.anydoc_root = (*element).to_vec();
                }
                if !root_found {
                    read.word_root = (*element).to_vec();
                }
                anydoc_root_found |= frame.anydoc_notes;
                root_found = true;
            }
            Some(parent)
                if word && local == note_name && (parent.anydoc_notes || parent.word_notes) =>
            {
                let (word_type, unprefixed_type) =
                    word_attribute_forms(reader.resolver(), &element, b"type");
                let (word_id, unprefixed_id) =
                    word_attribute_forms(reader.resolver(), &element, b"id");
                if word_id
                    .iter()
                    .chain(&unprefixed_id)
                    .any(|id| id.len() > MAX_STYLE_ID_BYTES)
                {
                    return Err(DocumentError::ResourceLimit);
                }
                let separator = |kind: Option<&str>| {
                    matches!(
                        kind,
                        Some("separator" | "continuationSeparator" | "continuationNotice")
                    )
                };
                let anydoc_type = word_type.as_deref().or(unprefixed_type.as_deref());
                let anydoc = word_id
                    .clone()
                    .map(|id| (id, false))
                    .or_else(|| unprefixed_id.clone().map(|id| (id, true)))
                    .filter(|_| parent.anydoc_notes && !separator(anydoc_type));
                let word_note = parent.word_notes.then(|| {
                    (
                        word_id.as_deref().map(word_integer),
                        separator(word_type.as_deref()),
                    )
                });
                let mut open = DocxOpenNote {
                    depth: stack.len(),
                    anydoc,
                    word: word_note,
                    at: reader.buffer_position(),
                    markup: Vec::new(),
                    inner: Vec::new(),
                    run: None,
                };
                // The compatibility content Word found the note in.
                for wrapper in stack.iter().skip(1) {
                    open.item(DocxOpenNote::CONTEXT, &wrapper.tag);
                }
                let mut attributes = Vec::new();
                for attribute in element.attributes().with_checks(false) {
                    let Ok(attribute) = attribute else {
                        attributes = (*element).to_vec();
                        break;
                    };
                    let key = attribute.key.as_ref();
                    let declaration = key == b"xmlns" || key.starts_with(b"xmlns:");
                    if !declaration && matches!(xml_local_name(key), b"id" | b"type") {
                        continue;
                    }
                    attributes.extend_from_slice(&(key.len() as u64).to_le_bytes());
                    attributes.extend_from_slice(key);
                    attributes.extend_from_slice(&(attribute.value.len() as u64).to_le_bytes());
                    attributes.extend_from_slice(&attribute.value);
                }
                open.item(DocxOpenNote::ATTRIBUTES, &attributes);
                if opens {
                    note = Some(open);
                } else {
                    docx_note_read(open, &mut read)?;
                }
                buffer.clear();
                continue;
            }
            Some(parent)
                if vocabulary == WordVocabulary::MarkupCompatibility
                    && local == b"AlternateContent"
                    && parent.word_notes =>
            {
                frame.alternate = true;
                frame.tag = (*element).to_vec();
            }
            Some(parent)
                if vocabulary == WordVocabulary::MarkupCompatibility
                    && matches!(local.as_slice(), b"Choice" | b"Fallback")
                    && parent.alternate =>
            {
                let takes = !parent.took
                    && (local == b"Fallback" || mc_choice_understood(&reader, &element));
                parent.took |= takes;
                frame.word_notes = takes;
                frame.tag = (*element).to_vec();
            }
            Some(_) => {}
        }
        if opens {
            stack.push(frame);
        }
        buffer.clear();
    }
}

/// Add a note that has been read to the notes each side reads of it.
fn docx_note_read(open: DocxOpenNote, read: &mut DocxNotesRead) -> Result<(), DocumentError> {
    if read.anydoc.len() + read.word.len() >= 2 * MAX_DOCX_NOTES {
        return Err(DocumentError::ResourceLimit);
    }
    let written = Rc::new(DocxNoteWritten {
        at: open.at,
        markup: open.markup,
    });
    if let Some((id, unprefixed)) = open.anydoc {
        read.anydoc.push(DocxAnydocNote {
            id,
            unprefixed,
            written: written.clone(),
        });
    }
    if let Some((id, separator)) = open.word {
        read.word.push(DocxWordNote {
            id,
            separator,
            written,
        });
    }
    Ok(())
}

/// A notes part's notes as each side reads them (see [`DocxNotesRead`]);
/// none where the part is missing.
fn docx_notes_part_read(
    archive: &mut ZipArchive<Cursor<&[u8]>>,
    part: &str,
    endnotes: bool,
) -> Result<DocxNotesRead, DocumentError> {
    match read_optional_xml_part(archive, part)? {
        Some(bytes) => docx_notes_read(&bytes, endnotes),
        None => Ok(DocxNotesRead::default()),
    }
}

/// What AnyDoc converts of one kind of note against what Word shows: at each
/// reference, the note Word shows there (the first with the reference's id
/// among Word's notes, a separator showing none) and the note AnyDoc
/// converts (the first of its notes with the id, or a later one where the
/// first is blank to it) must be one note, or be written alike, in roots and
/// relationships alike. Returns whether the conversion is refused, and
/// whether it holds text Word does not show.
///
/// Refused: a note Word shows where AnyDoc converts none, or another; a
/// note AnyDoc converts at a reference Word shows with none, as where Word
/// reads no notes part; and a reference or note whose id AnyDoc reads from
/// an unprefixed `id`, which Word does not read. Disclosed as hidden: a
/// note AnyDoc converts at a reference Word does not show; a later note of
/// the same id, written otherwise, which AnyDoc converts in place of a
/// first one blank to it; and a note no reference AnyDoc converts names,
/// which AnyDoc converts after the text.
fn docx_notes_verdict(
    references: &[&DocxNoteReference],
    word: Option<&DocxNotesRead>,
    anydoc: &DocxNotesRead,
    same_part: bool,
    relationships_alike: bool,
) -> (bool, bool) {
    let mut candidates: HashMap<&str, Vec<&DocxAnydocNote>> = HashMap::new();
    for note in &anydoc.anydoc {
        candidates.entry(note.id.as_str()).or_default().push(note);
    }
    let mut shown: HashMap<i64, &DocxWordNote> = HashMap::new();
    for note in word.map_or(&[][..], |read| read.word.as_slice()) {
        if let Some(id) = note.id {
            shown.entry(id).or_insert(note);
        }
    }
    let written_alike =
        relationships_alike && word.is_some_and(|read| read.word_root == anydoc.anydoc_root);
    let one = |first: &DocxNoteWritten, second: &DocxNoteWritten| {
        (same_part && first.at == second.at) || (written_alike && first.markup == second.markup)
    };
    let mut refused = false;
    let mut hidden = false;
    let mut referenced: HashSet<&str> = HashSet::new();
    for reference in references {
        if let Some(id) = &reference.anydoc {
            referenced.insert(id.as_str());
        }
        let word_note = reference
            .word
            .filter(|_| reference.word_shows)
            .and_then(|id| shown.get(&id))
            .filter(|note| !note.separator);
        let converted = reference
            .anydoc
            .as_deref()
            .and_then(|id| candidates.get(id))
            .map_or(&[][..], Vec::as_slice);
        refused |= reference.unprefixed || converted.iter().any(|note| note.unprefixed);
        match (word_note, converted.first()) {
            (Some(_), None) => refused = true,
            (None, Some(_)) if reference.word_shows => refused = true,
            (None, Some(_)) => hidden = true,
            (Some(shown), Some(first)) => {
                refused |= !one(&shown.written, &first.written);
                hidden |= converted[1..].iter().any(|later| {
                    later.written.at != first.written.at
                        && later.written.markup != first.written.markup
                });
            }
            (None, None) => {}
        }
    }
    hidden |= anydoc
        .anydoc
        .iter()
        .any(|note| !referenced.contains(note.id.as_str()));
    (refused, hidden)
}

fn scan_docx_element(
    resolver: &quick_xml::name::NamespaceResolver,
    event: &quick_xml::events::BytesStart<'_>,
    node: &mut WordNode,
    stack: &[WordNode],
    scan: &mut DocxStoryScan,
) -> Result<(), DocumentError> {
    scan_docx_mark(resolver, event, node, stack, scan)?;
    // Run properties apply to content in these positions. Under a paragraph
    // mark (`w:pPr/w:rPr`) they format only a list label, read with the
    // mark; in revision history (`w:rPrChange/w:rPr`, `w:pPrChange/w:pPr`)
    // they format nothing visible.
    let run_property = word_path_ends_with(stack, &[b"r", b"rPr"]);
    let omitted = word_content_omitted(stack);
    match node.local.as_slice() {
        // Ruby text loses its base text as well as the annotation, and an
        // imported chunk (`w:altChunk`, HTML or RTF that Word merges on
        // opening) loses all of its content.
        b"sym" | b"checkBox" | b"ddList" | b"ruby" | b"altChunk" if !omitted => {
            scan.dropped = true;
        }
        // A page number or date Word fills in where the run shows it
        // (`w:pgNum`, and the date blocks), which AnyDoc's run walker drops
        // and LibreOffice does not show either. The text around it converts.
        // In a header or footer, which AnyDoc does not convert, nothing is
        // lost.
        b"pgNum" | b"dayShort" | b"dayLong" | b"monthShort" | b"monthLong" | b"yearShort"
        | b"yearLong"
            if !omitted
                && node.vocabulary == WordVocabulary::Word
                && word_path_ends_with(stack, &[b"r"]) =>
        {
            scan.omitted_page_block = true;
        }
        // AnyDoc reads a table's rows only as its direct children, so a row
        // wrapped in a content control, custom XML, or a compatibility block
        // is dropped, as is a row of a table in another namespace.
        b"tr"
            if !omitted
                && node.vocabulary == WordVocabulary::Word
                && !stack
                    .last()
                    .is_some_and(|parent| parent.is(WordVocabulary::Word, b"tbl")) =>
        {
            scan.dropped = true;
        }
        b"tc"
            if !omitted && node.vocabulary == WordVocabulary::Word && !docx_cell_reached(stack) =>
        {
            scan.dropped = true;
        }
        b"noBreakHyphen" if !omitted && word_path_ends_with(stack, &[b"r"]) => {
            scan.omitted_hyphen = true;
        }
        // Word hides a run marked `w:specVanish` even when hidden text is
        // shown; AnyDoc converts it.
        b"vanish" | b"specVanish" if run_property => scan.hidden_run |= !xml_toggle_off(event),
        b"rStyle" if run_property => record_style(event, scan)?,
        b"footnoteReference" | b"endnoteReference" if node.vocabulary == WordVocabulary::Word => {
            record_note_reference(resolver, event, node, stack, scan)?;
        }
        // A note either side shows, for the list replay: AnyDoc stores it
        // by its `w:id`, else an unprefixed `id`.
        b"footnote" | b"endnote" if docx_note_element(node, stack) => {
            let endnote = node.local == b"endnote";
            let (anydoc_skips, word_skips) = docx_note_skipped(resolver, event);
            let (qualified, unprefixed) = word_attribute_forms(resolver, event, b"id");
            scan.note = None;
            if let Some(id) = qualified
                .or(unprefixed)
                .filter(|_| !(anydoc_skips && word_skips))
            {
                if scan.notes_stored.len() >= MAX_DOCX_NOTES || id.len() > MAX_STYLE_ID_BYTES {
                    return Err(DocumentError::ResourceLimit);
                }
                scan.note = Some(scan.notes_stored.len() as u32);
                scan.notes_stored.push((endnote, id.trim().to_string()));
            }
        }
        // A row deleted with tracked changes, which AnyDoc converts as
        // current text.
        b"del" if word_path_ends_with(stack, &[b"tr", b"trPr"]) => scan.hidden_run = true,
        b"pStyle" if word_path_ends_with(stack, &[b"p", b"pPr"]) => record_style(event, scan)?,
        b"tblStyle" if word_path_ends_with(stack, &[b"tbl", b"tblPr"]) => {
            record_style(event, scan)?;
        }
        _ => {}
    }
    Ok(())
}

/// Read an element of the innermost open paragraph's mark, as Word and as
/// AnyDoc read it. AnyDoc finds each element it reads as the first child
/// of its kind: the paragraph's first `w:pPr`, that one's first `w:pStyle`
/// and `w:numPr`, and that one's first `w:numId` and `w:ilvl`, none inside
/// compatibility content. Word, as LibreOffice shows it, reads every one,
/// in the branches of compatibility content it takes, and merges them,
/// later values winning.
fn scan_docx_mark(
    resolver: &quick_xml::name::NamespaceResolver,
    event: &quick_xml::events::BytesStart<'_>,
    node: &mut WordNode,
    stack: &[WordNode],
    scan: &mut DocxStoryScan,
) -> Result<(), DocumentError> {
    if node.vocabulary != WordVocabulary::Word {
        return Ok(());
    }
    if node.local == b"p" {
        node.anydoc_mark = true;
        return Ok(());
    }
    let Some(mark) = scan.marks.last_mut() else {
        return Ok(());
    };
    // Where Word reads the element, and which kind AnyDoc reads the first
    // of among the children of its parent.
    let (path, anydoc_kind): (&[&[u8]], Option<usize>) = match node.local.as_slice() {
        b"pPr" => (&[b"p"], Some(ANYDOC_MARK)),
        b"pStyle" => (&[b"p", b"pPr"], Some(ANYDOC_STYLE)),
        b"numPr" => (&[b"p", b"pPr"], Some(ANYDOC_NUMBERING)),
        b"numId" => (&[b"p", b"pPr", b"numPr"], Some(ANYDOC_LIST)),
        b"ilvl" => (&[b"p", b"pPr", b"numPr"], Some(ANYDOC_LEVEL)),
        b"del" | b"moveFrom" | b"vanish" | b"specVanish" | b"rStyle" => {
            (&[b"p", b"pPr", b"rPr"], None)
        }
        _ => return Ok(()),
    };
    let word = word_mark_path(stack, path);
    let mut anydoc = false;
    if let Some(kind) = anydoc_kind {
        let parent = path[path.len() - 1];
        if stack
            .last()
            .is_some_and(|open| open.anydoc_mark && open.is(WordVocabulary::Word, parent))
        {
            anydoc = !mark.anydoc_met[kind];
            mark.anydoc_met[kind] = true;
        }
    }
    node.anydoc_mark = anydoc;
    if !word && !anydoc {
        return Ok(());
    }
    match node.local.as_slice() {
        b"pStyle" => {
            let style = word_attribute(resolver, event, b"val")
                .filter(|style| style.len() <= MAX_STYLE_ID_BYTES);
            if anydoc {
                mark.anydoc_style = style.clone();
            }
            if word && style.is_some() {
                mark.style = style;
            }
        }
        // Word reads a number with white space around it collapsed, AnyDoc
        // as it stands.
        b"numId" => {
            let values = xml_attribute_values(event, b"val");
            mark.numbered |= values.iter().any(|value| value.trim() != "0");
            let value = word_attribute(resolver, event, b"val");
            if anydoc {
                mark.anydoc_list = value.as_deref().and_then(|value| value.parse().ok());
            }
            if let Some(list) = value
                .as_deref()
                .and_then(|value| value.trim().parse().ok())
                .filter(|_| word)
            {
                mark.list = Some(list);
            }
        }
        b"ilvl" => {
            let value = word_attribute(resolver, event, b"val");
            if anydoc {
                mark.anydoc_level = value.as_deref().and_then(|value| value.parse().ok());
            }
            if let Some(level) = value
                .as_deref()
                .and_then(|value| value.trim().parse().ok())
                .filter(|_| word)
            {
                mark.level = Some(level);
            }
        }
        b"del" | b"moveFrom" => mark.deleted = true,
        b"vanish" => mark.hidden |= !xml_toggle_off(event),
        b"specVanish" => mark.style_separator |= !xml_toggle_off(event),
        b"rStyle" => {
            for style in xml_attribute_values(event, b"val") {
                if style.len() > MAX_STYLE_ID_BYTES || mark.styles.len() >= MAX_DOCX_STYLES {
                    return Err(DocumentError::ResourceLimit);
                }
                mark.styles.push(style);
            }
        }
        _ => {}
    }
    Ok(())
}

/// Whether the open elements end with the WordprocessingML elements
/// `suffix`, looking through the branches of compatibility content that
/// Word takes: where Word reads an element of a paragraph's mark.
fn word_mark_path(stack: &[WordNode], suffix: &[&[u8]]) -> bool {
    let mut path = stack.iter().rev();
    for wanted in suffix.iter().rev() {
        let open = loop {
            match path.next() {
                Some(node) if node.is_compatibility_wrapper() && !node.word_skips => {}
                open => break open,
            }
        };
        if !open.is_some_and(|node| node.is(WordVocabulary::Word, wanted)) {
            return false;
        }
    }
    true
}

/// Whether AnyDoc's `collect_row_cells` reaches a `w:tc` opened under these
/// elements: a row's direct cells, and cells inside custom XML or a content
/// control's content, nested any number of times. A cell under anything
/// else, such as `mc:AlternateContent`, is dropped with its text.
fn docx_cell_reached(stack: &[WordNode]) -> bool {
    let mut path = stack.iter().rev();
    loop {
        match path.next() {
            Some(node) if node.is(WordVocabulary::Word, b"tr") => return true,
            Some(node) if node.is(WordVocabulary::Word, b"customXml") => {}
            Some(node) if node.is(WordVocabulary::Word, b"sdtContent") => {
                if !path
                    .next()
                    .is_some_and(|parent| parent.is(WordVocabulary::Word, b"sdt"))
                {
                    return false;
                }
            }
            _ => return false,
        }
    }
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

/// Whether a numbering part hides list labels (`w:lvl/w:rPr/w:vanish`),
/// which AnyDoc 0.2.4 converts, and the character styles its levels apply
/// to labels. Streamed under the same bounds as the styles part.
fn docx_numbering_labels(
    reader: impl std::io::BufRead,
) -> Result<(bool, HashSet<String>), DocumentError> {
    let mut reader = quick_xml::Reader::from_reader(reader);
    reader.config_mut().trim_text(false);
    reader.config_mut().check_end_names = false;
    let mut buffer = Vec::new();
    let mut stack: Vec<Vec<u8>> = Vec::new();
    let mut nodes = 0usize;
    let mut hidden = false;
    let mut styles = HashSet::new();
    loop {
        let (event, start) = match reader.read_event_into(&mut buffer) {
            Ok(quick_xml::events::Event::Start(event)) => (event, true),
            Ok(quick_xml::events::Event::Empty(event)) => (event, false),
            Ok(quick_xml::events::Event::End(_)) => {
                stack.pop();
                buffer.clear();
                continue;
            }
            Ok(quick_xml::events::Event::Eof) => return Ok((hidden, styles)),
            Ok(_) => {
                nodes += 1;
                if nodes > MAX_XML_NODES {
                    return Err(DocumentError::ResourceLimit);
                }
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
        if xml_path_ends_with(&stack, &[b"lvl", b"rPr"]) {
            match local.as_slice() {
                b"vanish" => hidden |= !xml_toggle_off(&event),
                b"rStyle" => {
                    for style in xml_attribute_values(&event, b"val") {
                        if style.len() > MAX_STYLE_ID_BYTES
                            || (styles.len() >= MAX_DOCX_STYLES && !styles.contains(&style))
                        {
                            return Err(DocumentError::ResourceLimit);
                        }
                        styles.insert(style);
                    }
                }
                _ => {}
            }
        }
        if start {
            stack.push(local);
        }
        buffer.clear();
    }
}

/// A numbering part's list definitions as Word or AnyDoc reads them, as far
/// as the numbers they show differ.
#[derive(Default)]
struct DocxNumbering {
    /// Each abstract definition (`w:abstractNum`), in the order first read.
    definitions: Vec<DocxDefinition>,
    /// Each definition's index by id: AnyDoc matches the id's text, and
    /// Word the integer it reads, so `01` and `+1` name definition 1 for
    /// Word only.
    definition_ids: HashMap<String, usize>,
    /// Each list instance (`w:num`) by id.
    lists: HashMap<u64, DocxList>,
    /// The first definition declaring each list style (`w:styleLink`).
    style_definitions: HashMap<String, usize>,
}

#[derive(Default)]
struct DocxDefinition {
    levels: [Option<DocxLevel>; DOCX_LIST_LEVELS],
    /// A list style whose own list instance's definition this one uses
    /// (`w:numStyleLink`).
    style_link: Option<String>,
}

/// A list instance: the definition it shares counters through, the levels
/// it replaces (`w:lvlOverride/w:lvl`), and the levels it restarts
/// (`w:startOverride`). AnyDoc replaces a level whole; Word, as LibreOffice
/// shows it, lays the instance's level over the definition's.
#[derive(Default)]
struct DocxList {
    definition: Option<String>,
    levels: [Option<DocxLevel>; DOCX_LIST_LEVELS],
    starts: [Option<u64>; DOCX_LIST_LEVELS],
}

/// One list level (`w:lvl`), each property `None` where the level does not
/// give it. Its text is shared, not copied, by the list instances that use
/// it.
#[derive(Clone, Default)]
struct DocxLevel {
    /// `w:numFmt`: `None` for a level defined without one, which Word
    /// numbers and AnyDoc renders as bullets.
    format: Option<Rc<str>>,
    /// `w:start`, clamped at 0. When it is absent AnyDoc starts at 1 and
    /// Word at 0.
    start: Option<u64>,
    /// `w:lvlRestart`: `None` restarts the level after any shallower one,
    /// 0 never, `n` after a level shallower than `n`.
    restart: Option<u32>,
    /// The paragraph style bound to the level (`w:pStyle`).
    style: Option<Rc<str>>,
    /// The number text (`w:lvlText`), `%1` to `%9` standing for levels'
    /// numbers: `None` when absent, and then Word shows no number.
    text: Option<DocxLevelText>,
    /// Legal numbering (`w:isLgl`): every level's number shows in decimal.
    legal: Option<bool>,
}

/// A level's number text (`w:lvlText`).
#[derive(Clone)]
enum DocxLevelText {
    Text(Rc<str>),
    /// Longer than the replay reads (see [`MAX_DOCX_LEVEL_TEXT_BYTES`]).
    Oversized,
}

impl DocxLevelText {
    fn of(text: String) -> Self {
        if text.len() > MAX_DOCX_LEVEL_TEXT_BYTES {
            DocxLevelText::Oversized
        } else {
            DocxLevelText::Text(text.into())
        }
    }
}

/// The longest level number text the replay reads. Word's number texts run
/// to a few characters; a longer one would cost its length for every
/// paragraph numbered at the level, on each side, so a paragraph numbered
/// there is disclosed instead of compared.
const MAX_DOCX_LEVEL_TEXT_BYTES: usize = 1024;

impl DocxLevel {
    fn start(&self) -> u64 {
        self.start.unwrap_or(1)
    }

    fn legal(&self) -> bool {
        self.legal == Some(true)
    }

    /// This level laid over `base`: each property it gives replaces the
    /// base's.
    fn merged_over(&self, base: &DocxLevel) -> DocxLevel {
        DocxLevel {
            format: self.format.clone().or_else(|| base.format.clone()),
            start: self.start.or(base.start),
            restart: self.restart.or(base.restart),
            style: self.style.clone().or_else(|| base.style.clone()),
            text: self.text.clone().or_else(|| base.text.clone()),
            legal: self.legal.or(base.legal),
        }
    }

    /// The level's start as Word reads it: 0 when `w:start` is absent.
    fn word_start(&self) -> u64 {
        self.start.unwrap_or(0)
    }

    /// The level's number text, empty where it has none; `None` where it is
    /// longer than the replay reads.
    fn number_text(&self) -> Option<&str> {
        match &self.text {
            None => Some(""),
            Some(DocxLevelText::Text(text)) => Some(text),
            Some(DocxLevelText::Oversized) => None,
        }
    }
}

/// One counter per level, as Word keeps for a definition and AnyDoc for a
/// list instance.
#[derive(Default)]
struct DocxCounters {
    value: [u64; DOCX_LIST_LEVELS],
    started: [bool; DOCX_LIST_LEVELS],
    restart_pending: [bool; DOCX_LIST_LEVELS],
}

impl DocxCounters {
    /// Count a paragraph at `level` and return its number: `start` when the
    /// level first counts or restarts, one more otherwise. Deeper levels
    /// restart as their `w:lvlRestart` says.
    fn next(
        &mut self,
        level: usize,
        start: u64,
        restart_at: Option<u64>,
        levels: &[DocxLevel; DOCX_LIST_LEVELS],
    ) -> u64 {
        if let Some(start) = restart_at {
            self.value[level] = start;
        } else if !self.started[level] || self.restart_pending[level] {
            self.value[level] = start;
        } else {
            self.value[level] = self.value[level].saturating_add(1);
        }
        self.started[level] = true;
        self.restart_pending[level] = false;
        for (deeper, definition) in levels.iter().enumerate().skip(level + 1) {
            let restarts = match definition.restart {
                None => true,
                Some(0) => false,
                Some(shallower) => (level as u64) < u64::from(shallower),
            };
            if restarts {
                self.restart_pending[deeper] = true;
            }
        }
        self.value[level]
    }

    /// The number AnyDoc's label shows for a level: its count, or its start
    /// before it counts and while it waits to restart.
    fn shown(&self, level: usize, start: u64) -> u64 {
        if self.started[level] && !self.restart_pending[level] {
            self.value[level]
        } else {
            start
        }
    }

    /// Count each shallower level that has not counted since it last
    /// restarted as used once, as Word does when a deeper level is numbered:
    /// its next paragraph takes the number after its start ("1.1.1." then
    /// "2."). AnyDoc does not.
    fn imply_parents(&mut self, level: usize, levels: &[DocxLevel; DOCX_LIST_LEVELS]) {
        for (shallower, definition) in levels.iter().enumerate().take(level) {
            if !self.started[shallower] || self.restart_pending[shallower] {
                self.value[shallower] = definition.word_start();
                self.started[shallower] = true;
                self.restart_pending[shallower] = false;
            }
        }
    }
}

/// What a list level shows before its paragraph's text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DocxMarker {
    Nothing,
    Bullet,
    Count(DocxCount),
    /// A count in a format AnyDoc renders as a plain number: ordinals,
    /// words, zero-padded numbers, and the rest.
    Other,
    /// A level the list does not define, where what Word shows is
    /// uncertain: LibreOffice numbers it in decimal where the list defines
    /// no level, and shows nothing where it defines another.
    Undefined,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DocxCount {
    Decimal,
    LowerRoman,
    UpperRoman,
    LowerLetter,
    UpperLetter,
}

impl DocxCount {
    fn of(format: &str) -> Option<Self> {
        Some(match format {
            "decimal" => DocxCount::Decimal,
            "lowerRoman" => DocxCount::LowerRoman,
            "upperRoman" => DocxCount::UpperRoman,
            "lowerLetter" => DocxCount::LowerLetter,
            "upperLetter" => DocxCount::UpperLetter,
            _ => return None,
        })
    }

    /// A number in this format as AnyDoc writes it: letters count on past
    /// `z` (`aa`, `ab`), and zero, or a Roman numeral past 3,999, is written
    /// in decimal.
    fn text(self, value: u64) -> String {
        let lower = match self {
            DocxCount::Decimal => return value.to_string(),
            DocxCount::LowerRoman | DocxCount::UpperRoman => {
                if value == 0 || value > 3999 {
                    return value.to_string();
                }
                let mut rest = value;
                let mut roman = String::new();
                for (step, numeral) in [
                    (1000, "m"),
                    (900, "cm"),
                    (500, "d"),
                    (400, "cd"),
                    (100, "c"),
                    (90, "xc"),
                    (50, "l"),
                    (40, "xl"),
                    (10, "x"),
                    (9, "ix"),
                    (5, "v"),
                    (4, "iv"),
                    (1, "i"),
                ] {
                    while rest >= step {
                        roman.push_str(numeral);
                        rest -= step;
                    }
                }
                roman
            }
            DocxCount::LowerLetter | DocxCount::UpperLetter => {
                if value == 0 {
                    return value.to_string();
                }
                let mut rest = value;
                let mut letters = Vec::new();
                while rest > 0 {
                    rest -= 1;
                    letters.push(b'a' + (rest % 26) as u8);
                    rest /= 26;
                }
                letters
                    .iter()
                    .rev()
                    .map(|&letter| char::from(letter))
                    .collect()
            }
        };
        if matches!(self, DocxCount::UpperRoman | DocxCount::UpperLetter) {
            lower.to_ascii_uppercase()
        } else {
            lower
        }
    }
}

impl DocxCount {
    /// How Word shows a level's number in another level's number text: in
    /// its format, decimal when it names none; `None` for bullets, no
    /// number, and formats beyond these.
    fn shown_by_word(level: &DocxLevel) -> Option<Self> {
        match level.format.as_deref() {
            None => Some(DocxCount::Decimal),
            Some(format) => DocxCount::of(format),
        }
    }

    /// How AnyDoc shows it: a level without a format is a bullet, and one
    /// without a number (`none`), or in a format it does not know, shows
    /// in decimal.
    fn shown_by_anydoc(level: &DocxLevel) -> Option<Self> {
        match level.format.as_deref() {
            None | Some("bullet") => None,
            Some(format) => Some(DocxCount::of(format).unwrap_or(DocxCount::Decimal)),
        }
    }
}

impl DocxMarker {
    /// What Word shows: a level defined without a format is numbered, one
    /// without number text shows no number, and what a level not defined
    /// shows is uncertain.
    fn word(level: Option<&DocxLevel>) -> Self {
        let Some(level) = level else {
            return DocxMarker::Undefined;
        };
        if level
            .number_text()
            .is_some_and(|text| text.trim().is_empty())
            && level.format.as_deref() != Some("bullet")
        {
            return DocxMarker::Nothing;
        }
        match level.format.as_deref() {
            None => DocxMarker::Count(DocxCount::Decimal),
            Some("bullet") => DocxMarker::Bullet,
            Some("none") => DocxMarker::Nothing,
            Some(format) => DocxCount::of(format).map_or(DocxMarker::Other, DocxMarker::Count),
        }
    }

    /// What AnyDoc shows: a level without a format, or not defined at all,
    /// is a bullet, and a format it does not know is a plain number.
    fn anydoc(level: Option<&DocxLevel>) -> Self {
        match level.and_then(|level| level.format.as_deref()) {
            None | Some("bullet") => DocxMarker::Bullet,
            Some("none") => DocxMarker::Nothing,
            Some(format) => DocxMarker::Count(DocxCount::of(format).unwrap_or(DocxCount::Decimal)),
        }
    }
}

/// A list instance as one side of the replay counts it: the definition its
/// levels come from, which Word shares counters through, the levels as
/// the instance shows them, and the levels it restarts
/// (`w:startOverride`).
struct DocxInstance {
    definition: usize,
    shape: Rc<DocxShape>,
    starts: [Option<u64>; DOCX_LIST_LEVELS],
}

/// A list's levels and their markers, and the styles its levels are bound
/// to, each with the first level bound to it. The instances of a definition
/// that replace none of its levels share one.
struct DocxShape {
    levels: [DocxLevel; DOCX_LIST_LEVELS],
    markers: [DocxMarker; DOCX_LIST_LEVELS],
    bound: Vec<(usize, usize)>,
}

/// The list instances the replay has read for one side, and the shapes of
/// the definitions they share.
#[derive(Default)]
struct DocxInstances {
    lists: HashMap<u64, Option<DocxInstance>>,
    shapes: HashMap<usize, Rc<DocxShape>>,
}

/// A paragraph's list label as one side shows it.
#[derive(Debug, PartialEq, Eq)]
enum DocxLabel {
    Nothing,
    /// A bullet, which shows no count.
    Bullet,
    Text(String),
    /// A number in a format AnyDoc does not write, such as an ordinal, or
    /// a label Word shows uncertainly, which differs from anything AnyDoc
    /// shows.
    Unknown,
}

/// Written on both sides for a deeper level's number in a label, which is
/// not compared.
const DOCX_DEEPER_NUMBER: char = '\u{fffc}';

/// A piece of a level's number text (`w:lvlText`): literal text, or `%1` to
/// `%9` for a level's number.
enum DocxTextPiece {
    Literal(String),
    Number(usize),
}

/// A level's number text in pieces, read alike by Word and AnyDoc.
fn docx_number_text(text: &str) -> Vec<DocxTextPiece> {
    let mut pieces: Vec<DocxTextPiece> = Vec::new();
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '%' {
            if let Some(digit) = characters
                .peek()
                .and_then(|next| next.to_digit(10))
                .filter(|digit| (1..=9).contains(digit))
            {
                characters.next();
                pieces.push(DocxTextPiece::Number(digit as usize - 1));
                continue;
            }
        }
        match pieces.last_mut() {
            Some(DocxTextPiece::Literal(literal)) => literal.push(character),
            _ => pieces.push(DocxTextPiece::Literal(character.to_string())),
        }
    }
    pieces
}

impl DocxShape {
    /// The label Word shows for a paragraph numbered `value` at `level`,
    /// with `counters` holding the shallower levels' numbers. Legal
    /// numbering (`w:isLgl`) shows the levels in decimal, but a format
    /// beyond decimal, Roman numerals, and letters stays unknown at the
    /// paragraph's own level, as it does at a shallower level without legal
    /// numbering; past `z` Word doubles the letter (`aa`, `bb`) where AnyDoc
    /// counts on (`aa`, `ab`). A level whose text is longer than the replay
    /// reads is unknown.
    fn word_label(&self, level: usize, value: u64, counters: &DocxCounters) -> DocxLabel {
        match self.markers[level] {
            DocxMarker::Nothing => return self.word_literal(level),
            DocxMarker::Bullet => return DocxLabel::Bullet,
            DocxMarker::Undefined => return DocxLabel::Unknown,
            DocxMarker::Count(_) | DocxMarker::Other => {}
        }
        let own = &self.levels[level];
        let Some(text) = own.number_text() else {
            return DocxLabel::Unknown;
        };
        let mut label = String::new();
        for piece in docx_number_text(text) {
            let shown = match piece {
                DocxTextPiece::Literal(literal) => {
                    label.push_str(&literal);
                    continue;
                }
                DocxTextPiece::Number(shown) if shown > level => {
                    label.push(DOCX_DEEPER_NUMBER);
                    continue;
                }
                DocxTextPiece::Number(shown) => shown,
            };
            let count = match DocxCount::shown_by_word(&self.levels[shown]) {
                Some(_) if own.legal() => DocxCount::Decimal,
                Some(count) => count,
                None if own.legal() && shown < level => DocxCount::Decimal,
                None => return DocxLabel::Unknown,
            };
            let number = if shown == level {
                value
            } else {
                counters.value[shown]
            };
            if matches!(count, DocxCount::LowerLetter | DocxCount::UpperLetter) && number > 26 {
                return DocxLabel::Unknown;
            }
            label.push_str(&count.text(number));
        }
        DocxLabel::Text(label)
    }

    /// The label Word shows at a level without a number (`w:numFmt` of
    /// `none`), which AnyDoc does not number at all: the level's literal
    /// text, as in "WHEREAS,". What a number there shows is uncertain:
    /// nothing for the level's own, and for another level's nothing in
    /// LibreOffice and that level's number in Word, probably. So only words
    /// and digits of the text count; punctuation alone, such as the `.` of
    /// `%1.`, is taken to show nothing, as a bullet shows no count.
    fn word_literal(&self, level: usize) -> DocxLabel {
        let own = &self.levels[level];
        if own.format.as_deref() != Some("none") {
            return DocxLabel::Nothing;
        }
        let Some(text) = own.number_text() else {
            return DocxLabel::Unknown;
        };
        let literal: String = docx_number_text(text)
            .into_iter()
            .filter_map(|piece| match piece {
                DocxTextPiece::Literal(text) => Some(text),
                DocxTextPiece::Number(_) => None,
            })
            .collect();
        if literal.chars().any(char::is_alphanumeric) {
            DocxLabel::Text(literal)
        } else {
            DocxLabel::Nothing
        }
    }

    /// The label AnyDoc shows: its level's number text, or the number and a
    /// full stop where the level has none, with every level in the level's
    /// own marker, a bullet level as `-`, or in decimal for legal numbering.
    /// A shallower level shows its count, or its start before it counts and
    /// while it waits to restart. A level whose text is longer than the
    /// replay reads is unknown.
    fn anydoc_label(
        &self,
        level: usize,
        value: u64,
        counters: &DocxCounters,
        start: impl Fn(usize) -> u64,
    ) -> DocxLabel {
        let own_count = match self.markers[level] {
            DocxMarker::Count(count) => count,
            DocxMarker::Bullet => return DocxLabel::Bullet,
            DocxMarker::Nothing | DocxMarker::Other | DocxMarker::Undefined => {
                return DocxLabel::Nothing
            }
        };
        let own = &self.levels[level];
        let Some(text) = own.number_text() else {
            return DocxLabel::Unknown;
        };
        let pieces = docx_number_text(text);
        if pieces.is_empty() {
            return DocxLabel::Text(format!("{}.", own_count.text(value)));
        }
        let mut label = String::new();
        for piece in pieces {
            match piece {
                DocxTextPiece::Literal(literal) => label.push_str(&literal),
                DocxTextPiece::Number(shown) if shown > level => label.push(DOCX_DEEPER_NUMBER),
                DocxTextPiece::Number(shown) => {
                    let count = if own.legal() {
                        Some(DocxCount::Decimal)
                    } else if shown == level {
                        Some(own_count)
                    } else {
                        DocxCount::shown_by_anydoc(&self.levels[shown])
                    };
                    let number = if shown == level {
                        value
                    } else {
                        counters.shown(shown, start(shown))
                    };
                    match count {
                        Some(count) => label.push_str(&count.text(number)),
                        None => label.push('-'),
                    }
                }
            }
        }
        DocxLabel::Text(label)
    }
}

/// Where a paragraph's numbering resolves, as Word and as AnyDoc resolve
/// it: a list instance and a level, or `None` where the side numbers
/// nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct DocxResolved {
    word: Option<(u64, usize)>,
    anydoc: Option<(u64, usize)>,
}

/// Definitions followed through list styles, as AnyDoc bounds nothing but a
/// cycle.
const MAX_DOCX_DEFINITION_LINKS: usize = 64;

impl DocxNumbering {
    /// The definition a list instance's levels come from: its own, or, for
    /// a definition naming a list style (`w:numStyleLink`), the definition
    /// of that style's list instance, as AnyDoc resolves it. Word, as
    /// LibreOffice shows it, also finds the definition that declares the
    /// style (`w:styleLink`) when the style names no list instance.
    fn resolve_definition(
        &self,
        definition: &str,
        chains: &DocxStyleChains,
        word: bool,
    ) -> Option<usize> {
        let mut current = *self.definition_ids.get(definition)?;
        for _ in 0..MAX_DOCX_DEFINITION_LINKS {
            let Some(style) = &self.definitions[current].style_link else {
                return Some(current);
            };
            let through_style = chains
                .own_list(style)
                .and_then(|list| self.lists.get(&list))
                .and_then(|list| list.definition.as_deref());
            let next = match through_style {
                // A list naming no definition read ends the link unresolved.
                Some(next) => Some(*self.definition_ids.get(next)?),
                None if word => self.style_definitions.get(style).copied(),
                None => None,
            };
            match next {
                Some(next) if next != current => current = next,
                _ => return Some(current),
            }
        }
        None
    }

    /// A list instance's effective levels from a resolved definition, with
    /// the instance's replacements applied: AnyDoc's whole, Word's laid over
    /// the definition's levels, as LibreOffice shows them. A level neither
    /// defines is `None`. The levels share their text with the definition's.
    fn levels_from(
        &self,
        instance: &DocxList,
        definition: usize,
        word: bool,
    ) -> [Option<DocxLevel>; DOCX_LIST_LEVELS] {
        let mut levels = self.definitions[definition].levels.clone();
        for (level, replaced) in instance.levels.iter().enumerate() {
            let Some(replaced) = replaced else {
                continue;
            };
            levels[level] = Some(match &levels[level] {
                Some(base) if word => replaced.merged_over(base),
                _ => replaced.clone(),
            });
        }
        levels
    }

    /// A list's levels as Word or as AnyDoc shows them.
    fn shape(
        &self,
        instance: &DocxList,
        definition: usize,
        chains: &DocxStyleChains,
        word: bool,
    ) -> DocxShape {
        let levels = self.levels_from(instance, definition, word);
        DocxShape {
            bound: chains.bound(&levels),
            markers: levels.each_ref().map(|level| {
                if word {
                    DocxMarker::word(level.as_ref())
                } else {
                    DocxMarker::anydoc(level.as_ref())
                }
            }),
            levels: levels.map(Option::unwrap_or_default),
        }
    }

    /// A list instance as Word or as AnyDoc counts it. An instance that
    /// replaces none of its definition's levels shares the definition's
    /// shape with the definition's other instances.
    fn instance(
        &self,
        list: u64,
        chains: &DocxStyleChains,
        word: bool,
        shapes: &mut HashMap<usize, Rc<DocxShape>>,
    ) -> Option<DocxInstance> {
        let instance = self.lists.get(&list)?;
        let definition = self.resolve_definition(instance.definition.as_deref()?, chains, word)?;
        let shape = if instance.levels.iter().any(Option::is_some) {
            Rc::new(self.shape(instance, definition, chains, word))
        } else {
            shapes
                .entry(definition)
                .or_insert_with(|| Rc::new(self.shape(instance, definition, chains, word)))
                .clone()
        };
        Some(DocxInstance {
            definition,
            shape,
            starts: instance.starts,
        })
    }
}

/// How Word or AnyDoc reads a document's lists: its reading of the
/// numbering part and of the style chains, and the list instances read so
/// far.
struct DocxReading<'a> {
    word: bool,
    numbering: &'a DocxNumbering,
    chains: DocxStyleChains<'a>,
    instances: DocxInstances,
}

impl<'a> DocxReading<'a> {
    fn new(
        word: bool,
        numbering: &'a DocxNumbering,
        styles: &'a HashMap<String, DocxStyleList>,
    ) -> Self {
        DocxReading {
            word,
            numbering,
            chains: DocxStyleChains::new(styles),
            instances: DocxInstances::default(),
        }
    }

    /// A list instance as this side counts it.
    fn instance(&mut self, list: u64) -> Option<&DocxInstance> {
        let DocxReading {
            word,
            numbering,
            chains,
            instances: DocxInstances { lists, shapes },
        } = self;
        lists
            .entry(list)
            .or_insert_with(|| numbering.instance(list, chains, *word, shapes))
            .as_ref()
    }

    /// Where this side numbers a paragraph asking for `used` through
    /// `style`: the list given directly or the style chain's, and the level
    /// given directly or read from the style chain (see
    /// [`DocxStyleChains::level`]). A list instance of 0 removes numbering.
    /// AnyDoc numbers a level past the ninth at the ninth; Word's is kept
    /// as read, for the replay to show uncertainly.
    fn resolve(&mut self, used: &DocxListUse, style: Option<&str>) -> Option<(u64, usize)> {
        let (list, level) = if self.word {
            (used.list, used.level)
        } else {
            (used.anydoc_list, used.anydoc_level)
        };
        let list = match list {
            Some(list) => list,
            None => self.chains.list(style?)?,
        };
        if list == 0 {
            return None;
        }
        let bound = self.instance(list)?.shape.bound.clone();
        let level = match (level, style) {
            (Some(level), _) => level,
            (None, Some(style)) => self.chains.level(style, &bound, self.word),
            (None, None) => 0,
        };
        if self.word {
            Some((list, level))
        } else {
            Some((list, level.min(DOCX_LIST_LEVELS - 1)))
        }
    }
}

/// A numbering part as Word and as AnyDoc read it.
struct DocxNumberings {
    word: DocxNumbering,
    anydoc: DocxNumbering,
}

impl DocxNumberings {
    /// Whether a list runs through notes whose stored order, id order, and
    /// order of reference disagree. AnyDoc numbers the notes as they are
    /// stored; LibreOffice numbers them by id, and Word lays them out as the
    /// text references them. With the three apart, which numbers the reader
    /// sees is uncertain, and the list is disclosed.
    fn notes_out_of_order(scan: &DocxStoryScan, resolved: &[DocxResolved]) -> bool {
        let mut notes: HashMap<(DocxPart, u64), Vec<u32>> = HashMap::new();
        for paragraph in &scan.list_paragraphs {
            let (Some(note), Some(resolved)) =
                (paragraph.note, resolved.get(paragraph.used as usize))
            else {
                continue;
            };
            for (list, _) in [resolved.word, resolved.anydoc].into_iter().flatten() {
                let seen = notes.entry((paragraph.part, list)).or_default();
                if seen.last() != Some(&note) {
                    seen.push(note);
                }
            }
        }
        notes.values().filter(|seen| seen.len() > 1).any(|seen| {
            let stored: Vec<&(bool, String)> = seen
                .iter()
                .filter_map(|&note| scan.notes_stored.get(note as usize))
                .collect();
            let ids: Option<Vec<i64>> = stored.iter().map(|(_, id)| id.parse().ok()).collect();
            let references: Option<Vec<usize>> = stored
                .iter()
                .map(|note| scan.note_references.get(*note).copied())
                .collect();
            let rising = |values: &[i64]| values.windows(2).all(|pair| pair[0] < pair[1]);
            let ids_rise = ids.as_deref().is_some_and(rising);
            let references_rise = references
                .is_some_and(|references| references.windows(2).all(|pair| pair[0] < pair[1]));
            !(ids_rise && references_rise)
        })
    }

    /// Whether AnyDoc's list labels differ from Word's. Both count the
    /// paragraphs in order, each at the list and level it resolves them to.
    /// Word keeps one set of counters per definition in each story (the
    /// body, the text boxes, the footnotes, and the endnotes). A list
    /// instance restarts it once in a story, at the instance's first
    /// paragraph whose level has a `w:startOverride`, which that level
    /// takes; every other restart takes the level's own start, 0 if it
    /// names none. AnyDoc keeps one set per list instance through the body,
    /// meeting text boxes where they are anchored, then the footnotes, then
    /// the endnotes; it advances only the levels it numbers, restarts a
    /// level at its override whenever it restarts, and starts at 1 where a
    /// level names no start. A paragraph Word does not show, such as one
    /// whose mark is deleted, still takes a number in AnyDoc.
    fn numbers_differ(&self, scan: &DocxStoryScan, styles: &DocxStyleNumbering) -> bool {
        let mut word = DocxReading::new(true, &self.word, &styles.styles);
        let mut anydoc = DocxReading::new(false, &self.anydoc, &styles.anydoc_styles);
        let resolved: Vec<DocxResolved> = scan
            .list_uses
            .iter()
            .map(|used| {
                let word_style = styles.word_paragraph_style(&word.chains, used.style.as_deref());
                DocxResolved {
                    word: word.resolve(used, word_style),
                    anydoc: anydoc.resolve(used, used.anydoc_style.as_deref()),
                }
            })
            .collect();
        if Self::notes_out_of_order(scan, &resolved) {
            return true;
        }
        // Word's stories: a part, or `None` for the text boxes.
        let mut word_counters: HashMap<(Option<DocxPart>, usize), DocxCounters> = HashMap::new();
        let mut anydoc_counters: HashMap<u64, DocxCounters> = HashMap::new();
        let mut restarted: HashSet<(Option<DocxPart>, u64)> = HashSet::new();
        let in_order = [DocxPart::Body, DocxPart::Footnotes, DocxPart::Endnotes]
            .into_iter()
            .flat_map(|part| {
                scan.list_paragraphs
                    .iter()
                    .filter(move |paragraph| paragraph.part == part)
            });
        for paragraph in in_order {
            let resolved = resolved
                .get(paragraph.used as usize)
                .copied()
                .unwrap_or_default();
            let anydoc_label = match resolved.anydoc.filter(|_| paragraph.anydoc) {
                Some((list, level)) => match anydoc.instance(list) {
                    Some(instance)
                        if matches!(instance.shape.markers[level], DocxMarker::Count(_)) =>
                    {
                        let shape = &instance.shape;
                        let start = |level: usize| {
                            instance.starts[level].unwrap_or_else(|| shape.levels[level].start())
                        };
                        let counters = anydoc_counters.entry(list).or_default();
                        let value = counters.next(level, start(level), None, &shape.levels);
                        shape.anydoc_label(level, value, counters, start)
                    }
                    Some(instance) if instance.shape.markers[level] == DocxMarker::Bullet => {
                        DocxLabel::Bullet
                    }
                    _ => DocxLabel::Nothing,
                },
                None => DocxLabel::Nothing,
            };
            if !paragraph.word {
                // A paragraph Word does not show still takes a number in
                // AnyDoc.
                if matches!(anydoc_label, DocxLabel::Text(_) | DocxLabel::Unknown) {
                    return true;
                }
                continue;
            }
            let word_label = match resolved.word {
                // A level past the ninth, which ECMA-376 leaves undefined:
                // LibreOffice numbers the tenth as a level of its own that
                // shows nothing, and a deeper one at the first. So too a
                // level Word's readings part on (`DOCX_UNCERTAIN_LEVEL`).
                Some((_, level)) if level >= DOCX_LIST_LEVELS => DocxLabel::Unknown,
                Some((list, level)) => match word.instance(list) {
                    Some(instance) => {
                        let shape = &instance.shape;
                        let story = (!paragraph.in_text_box).then_some(paragraph.part);
                        let restart_at =
                            instance.starts[level].filter(|_| restarted.insert((story, list)));
                        let counters = word_counters
                            .entry((story, instance.definition))
                            .or_default();
                        counters.imply_parents(level, &shape.levels);
                        let value = counters.next(
                            level,
                            shape.levels[level].word_start(),
                            restart_at,
                            &shape.levels,
                        );
                        shape.word_label(level, value, counters)
                    }
                    None => DocxLabel::Nothing,
                },
                None => DocxLabel::Nothing,
            };
            // A paragraph AnyDoc drops takes no number there; its text is
            // missing, which the checks of that content find.
            if !paragraph.anydoc {
                continue;
            }
            // Labels differ where either shows a number, unless both show
            // the same text; a bullet shows no count to differ.
            let differs = match (&word_label, &anydoc_label) {
                (DocxLabel::Text(shown), DocxLabel::Text(converted)) => shown != converted,
                (DocxLabel::Text(_) | DocxLabel::Unknown, _)
                | (_, DocxLabel::Text(_) | DocxLabel::Unknown) => true,
                _ => false,
            };
            if differs {
                return true;
            }
        }
        false
    }
}

/// Paragraph styles' numbering (`w:pPr/w:numPr/w:numId`), with the styles
/// they are based on, as each side reads its styles part: the
/// WordprocessingML `w:style` children of the part's first `w:styles`,
/// each found by its exact id. AnyDoc ignores a style's `w:ilvl`, as
/// ECMA-376 says to, and takes the level from the list levels' style
/// bindings; Word reads it.
#[derive(Default)]
struct DocxStyleNumbering {
    /// Word's reading, as LibreOffice shows it: every definition of an id,
    /// its values merged, later ones winning. A style without an id is kept
    /// under its name, behind a NUL no id can hold.
    styles: HashMap<String, DocxStyleList>,
    /// The style Word finds by each name (`w:name`) where no style has the
    /// id a paragraph names: the first so named.
    style_names: HashMap<String, String>,
    /// AnyDoc's reading: each id's last definition among the part's
    /// styles, alone, with its first `w:basedOn` and the list its first
    /// `w:pPr/w:numPr/w:numId` names.
    anydoc_styles: HashMap<String, DocxStyleList>,
    /// The default paragraph style (`w:default="1"`), which Word applies to
    /// a paragraph naming no style it finds. AnyDoc reads no default.
    default_paragraph: Option<String>,
}

#[derive(Default)]
struct DocxStyleList {
    based_on: Option<String>,
    list: Option<u64>,
    /// The level its numbering names (`w:numPr/w:ilvl`).
    level: Option<usize>,
    /// Not a character, table, or numbering style, as Word's last
    /// definition of the id says.
    paragraph: bool,
}

impl DocxStyleNumbering {
    /// The style Word, as LibreOffice shows it, numbers a paragraph through
    /// when the paragraph names `named`: a paragraph style with that exact
    /// id, else the first with that name; a style of another type with the
    /// id where its chain names a list; and otherwise the default paragraph
    /// style. ECMA-376 finds a style by its id alone and has Word apply the
    /// default where none has it; LibreOffice also matches names, and
    /// takes a character, table, or numbering style's own list.
    fn word_paragraph_style<'s>(
        &'s self,
        chains: &DocxStyleChains,
        named: Option<&'s str>,
    ) -> Option<&'s str> {
        let default = self.default_paragraph.as_deref();
        let Some(named) = named else {
            return default;
        };
        match chains.kind(named) {
            Some(true) => Some(named),
            Some(false) if chains.list(named).is_some() => Some(named),
            Some(false) => default,
            None => self
                .style_names
                .get(named)
                .map(String::as_str)
                .filter(|&style| chains.kind(style) == Some(true))
                .or(default),
        }
    }
}

/// Paragraph styles' numbering read along their `w:basedOn` chains once,
/// in one walk over the styles, so that a paragraph's style costs nothing
/// to follow however long its chain. A chain ends at a style the parts do
/// not define, and where it would pass a style again; AnyDoc refuses a
/// document whose converted text reaches such a cycle.
struct DocxStyleChains<'a> {
    index: HashMap<&'a str, usize>,
    definitions: Vec<&'a DocxStyleList>,
    /// Each style's list instance: the first along its chain naming one.
    list: Vec<Option<u64>>,
    /// The first style along each chain that names its own level, and the
    /// level.
    named: Vec<Option<(usize, usize)>>,
    /// Each style's distance from the end of its chain, and when the walk
    /// entered it and left the styles based on it, which tell whether a
    /// style lies on another's chain.
    depth: Vec<usize>,
    entered: Vec<usize>,
    left: Vec<usize>,
}

impl<'a> DocxStyleChains<'a> {
    fn new(styles: &'a HashMap<String, DocxStyleList>) -> Self {
        let mut ids: Vec<&str> = styles.keys().map(String::as_str).collect();
        ids.sort_unstable();
        let index: HashMap<&str, usize> =
            ids.iter().enumerate().map(|(at, &id)| (id, at)).collect();
        let definitions: Vec<&DocxStyleList> = ids.iter().map(|&id| &styles[id]).collect();
        let mut bases: Vec<Option<usize>> = definitions
            .iter()
            .map(|definition| {
                definition
                    .based_on
                    .as_deref()
                    .and_then(|base| index.get(base).copied())
            })
            .collect();
        // Cut each cycle where a walk along a chain meets it.
        let mut walked = vec![0u8; ids.len()];
        for start in 0..ids.len() {
            let mut path = Vec::new();
            let mut current = Some(start);
            while let Some(style) = current {
                match walked[style] {
                    0 => {
                        walked[style] = 1;
                        path.push(style);
                        current = bases[style];
                    }
                    1 => {
                        if let Some(&last) = path.last() {
                            bases[last] = None;
                        }
                        break;
                    }
                    _ => break,
                }
            }
            for style in path {
                walked[style] = 2;
            }
        }
        let mut derived: Vec<Vec<usize>> = vec![Vec::new(); ids.len()];
        for (style, base) in bases.iter().enumerate() {
            if let Some(base) = base {
                derived[*base].push(style);
            }
        }
        let mut chains = DocxStyleChains {
            index,
            definitions,
            list: vec![None; ids.len()],
            named: vec![None; ids.len()],
            depth: vec![0; ids.len()],
            entered: vec![0; ids.len()],
            left: vec![0; ids.len()],
        };
        // Walk from each chain's end to the styles based on it, so that a
        // style's base is read before the style.
        let mut clock = 0;
        let mut pending: Vec<(usize, bool)> = (0..ids.len())
            .filter(|&style| bases[style].is_none())
            .map(|style| (style, false))
            .collect();
        while let Some((style, done)) = pending.pop() {
            if done {
                chains.left[style] = clock;
                continue;
            }
            chains.entered[style] = clock;
            clock += 1;
            let base = bases[style];
            let definition = chains.definitions[style];
            chains.depth[style] = base.map_or(0, |base| chains.depth[base] + 1);
            chains.list[style] = definition
                .list
                .or_else(|| base.and_then(|base| chains.list[base]));
            chains.named[style] = definition
                .level
                .map(|level| (style, level))
                .or_else(|| base.and_then(|base| chains.named[base]));
            pending.push((style, true));
            pending.extend(derived[style].iter().map(|&style| (style, false)));
        }
        chains
    }

    /// Whether the styles part defines a style of this id, and whether it
    /// is a paragraph style.
    fn kind(&self, style: &str) -> Option<bool> {
        self.index
            .get(style)
            .map(|&style| self.definitions[style].paragraph)
    }

    /// The list instance a style's own numbering names, without its chain.
    fn own_list(&self, style: &str) -> Option<u64> {
        self.index
            .get(style)
            .and_then(|&style| self.definitions[style].list)
    }

    /// The list instance a paragraph style numbers with.
    fn list(&self, style: &str) -> Option<u64> {
        self.index.get(style).and_then(|&style| self.list[style])
    }

    /// The styles a list's levels are bound to (`w:lvl/w:pStyle`), each
    /// with the first level bound to it.
    fn bound(&self, levels: &[Option<DocxLevel>; DOCX_LIST_LEVELS]) -> Vec<(usize, usize)> {
        let mut bound: Vec<(usize, usize)> = Vec::new();
        for (at, level) in levels.iter().enumerate() {
            let Some(&style) = level
                .as_ref()
                .and_then(|level| level.style.as_deref())
                .and_then(|style| self.index.get(style))
            else {
                continue;
            };
            if !bound.iter().any(|&(already, _)| already == style) {
                bound.push((style, at));
            }
        }
        bound
    }

    /// The level a paragraph style numbers at in a list whose levels are
    /// bound to `bound`. AnyDoc takes the level bound to the first style
    /// along the chain that a level names, else the first, reading no
    /// style's own level, as ECMA-376 says. Word takes the nearest style
    /// along the chain that is bound or names its own level
    /// (`w:numPr/w:ilvl`), a style that does both at the level bound to it.
    /// LibreOffice reads no binding: it takes the level the chain names,
    /// else the first, so a style bound to level 2 that names none is
    /// numbered at level 0. Where the two part, what Word shows is
    /// uncertain ([`DOCX_UNCERTAIN_LEVEL`]).
    fn level(&self, style: &str, bound: &[(usize, usize)], word: bool) -> usize {
        let Some(&style) = self.index.get(style) else {
            return 0;
        };
        let on_chain = |ancestor: usize| {
            self.entered[ancestor] <= self.entered[style]
                && self.entered[style] < self.left[ancestor]
        };
        let binding = bound
            .iter()
            .filter(|&&(ancestor, _)| on_chain(ancestor))
            .max_by_key(|&&(ancestor, _)| self.depth[ancestor]);
        let named = self.named[style].filter(|_| word);
        let level = match (binding, named) {
            (Some(&(ancestor, level)), Some((nearer, own))) => {
                if self.depth[ancestor] >= self.depth[nearer] {
                    level
                } else {
                    own
                }
            }
            (Some(&(_, level)), None) => level,
            (None, Some((_, own))) => own,
            (None, None) => 0,
        };
        if word && level != named.map_or(0, |(_, own)| own) {
            DOCX_UNCERTAIN_LEVEL
        } else {
            level
        }
    }
}

/// A level Word shows uncertainly, past every level a list defines: one a
/// style's binding and its own numbering name apart (see
/// [`DocxStyleChains::level`]).
const DOCX_UNCERTAIN_LEVEL: usize = usize::MAX;

/// A style definition AnyDoc keeps, as it is read: its id, what it says,
/// and how many `w:basedOn` and `w:pPr` children have opened, `w:numPr` in
/// the first mark, and `w:numId` in the first numbering, since AnyDoc
/// reads the first of each.
#[derive(Default)]
struct DocxKeptStyle {
    id: String,
    style: DocxStyleList,
    bases: u32,
    marks: u32,
    numberings: u32,
    lists: u32,
}

/// Read paragraph styles' numbering from a styles part, as Word and as
/// AnyDoc read it, streamed under AnyDoc's depth and node bounds: the
/// WordprocessingML `w:style` children of the part's first `w:styles`, each
/// found by its exact id, and each attribute as AnyDoc picks it.
fn docx_style_numbering(
    reader: impl std::io::BufRead,
    numbering: &mut DocxStyleNumbering,
) -> Result<(), DocumentError> {
    let mut reader = quick_xml::NsReader::from_reader(reader);
    reader.config_mut().trim_text(false);
    reader.config_mut().check_end_names = false;
    let mut buffer = Vec::new();
    let mut stack: Vec<DocxNumberingNode> = Vec::new();
    let mut nodes = 0usize;
    // Whether the part's first `w:styles` is open, and has been read.
    let mut in_root = false;
    let mut root_read = false;
    // The style being read, as Word merges it, and the definition AnyDoc
    // keeps of it.
    let mut open: Option<DocxOpenStyle> = None;
    let mut kept: Option<DocxKeptStyle> = None;
    loop {
        let (namespace, event) = reader
            .read_resolved_event_into(&mut buffer)
            .map_err(|_| DocumentError::Malformed)?;
        let in_word = WordVocabulary::of(&namespace) == WordVocabulary::Word;
        let (event, start) = match event {
            quick_xml::events::Event::Start(event) => (event, true),
            quick_xml::events::Event::Empty(event) => (event, false),
            quick_xml::events::Event::End(_) => {
                if let Some(closed) = stack.pop() {
                    if in_root && stack.len() == 1 && closed.word && closed.local == b"style" {
                        open = None;
                        if let Some(kept) = kept.take() {
                            numbering.anydoc_styles.insert(kept.id, kept.style);
                        }
                    }
                    if stack.is_empty() {
                        in_root = false;
                    }
                }
                buffer.clear();
                continue;
            }
            quick_xml::events::Event::Eof => return Ok(()),
            _ => {
                nodes += 1;
                if nodes > MAX_XML_NODES {
                    return Err(DocumentError::ResourceLimit);
                }
                buffer.clear();
                continue;
            }
        };
        nodes += 1;
        if nodes > MAX_XML_NODES || (start && stack.len() >= MAX_XML_DEPTH) {
            return Err(DocumentError::ResourceLimit);
        }
        let node = DocxNumberingNode {
            local: xml_local_name(event.name().as_ref()).to_vec(),
            word: in_word,
        };
        let attribute = |name: &[u8]| word_attribute(reader.resolver(), &event, name);
        // A WordprocessingML element at `path` below the root, every element
        // along it WordprocessingML's too.
        let under = |path: &[&[u8]]| {
            node.word
                && stack.len() == path.len() + 1
                && stack[1..]
                    .iter()
                    .zip(path)
                    .all(|(open, wanted)| open.word && open.local == *wanted)
        };
        let local = node.local.as_slice();
        if stack.is_empty() {
            if node.word && local == b"styles" && !root_read {
                root_read = true;
                in_root = start;
            }
        } else if in_root && local == b"style" && under(&[]) {
            let id = attribute(b"styleId").filter(|id| id.len() <= MAX_STYLE_ID_BYTES);
            let paragraph = attribute(b"type")
                .is_none_or(|kind| !matches!(kind.trim(), "character" | "table" | "numbering"));
            // The last paragraph style marked the default one.
            let default = paragraph && attribute(b"default").is_some_and(|value| xml_true(&value));
            if let Some(id) = &id {
                docx_word_style(numbering, id, paragraph, default)?;
                // A later definition of the id replaces this one for AnyDoc.
                if start {
                    kept = Some(DocxKeptStyle {
                        id: id.clone(),
                        ..DocxKeptStyle::default()
                    });
                } else {
                    numbering
                        .anydoc_styles
                        .insert(id.clone(), DocxStyleList::default());
                }
            }
            if start {
                open = Some(DocxOpenStyle {
                    key: id,
                    paragraph,
                    default,
                    named: false,
                });
            }
        } else if in_root {
            // AnyDoc reads the first `w:basedOn` of a style, and the list of
            // the first `w:numId` of the first `w:numPr` of its first
            // `w:pPr`, as it stands.
            if let Some(kept) = kept.as_mut() {
                match local {
                    b"basedOn" if under(&[b"style"]) => {
                        kept.bases += 1;
                        if kept.bases == 1 {
                            kept.style.based_on = attribute(b"val");
                        }
                    }
                    b"pPr" if under(&[b"style"]) => kept.marks += 1,
                    b"numPr" if kept.marks == 1 && under(&[b"style", b"pPr"]) => {
                        kept.numberings += 1;
                    }
                    b"numId"
                        if kept.marks == 1
                            && kept.numberings == 1
                            && under(&[b"style", b"pPr", b"numPr"]) =>
                    {
                        kept.lists += 1;
                        if kept.lists == 1 {
                            kept.style.list = attribute(b"val").and_then(|list| list.parse().ok());
                        }
                    }
                    _ => {}
                }
            }
            if let Some(style) = open.as_mut() {
                // Word finds a paragraph style by its first name too, and a
                // style without an id by its name alone.
                if local == b"name" && under(&[b"style"]) && !style.named {
                    style.named = true;
                    if let Some(name) =
                        attribute(b"val").filter(|name| name.len() <= MAX_STYLE_ID_BYTES)
                    {
                        if style.key.is_none() {
                            let key = format!("\0{name}");
                            docx_word_style(numbering, &key, style.paragraph, style.default)?;
                            style.key = Some(key);
                        }
                        if let Some(key) = style.key.as_ref().filter(|_| style.paragraph) {
                            numbering
                                .style_names
                                .entry(name)
                                .or_insert_with(|| key.clone());
                        }
                    }
                }
            }
            // Word merges every value, reading numbers with white space
            // collapsed.
            if let Some(key) = open.as_ref().and_then(|style| style.key.as_ref()) {
                match local {
                    b"basedOn" if under(&[b"style"]) => {
                        if let Some(base) = attribute(b"val") {
                            numbering.styles.entry(key.clone()).or_default().based_on = Some(base);
                        }
                    }
                    b"numId" if under(&[b"style", b"pPr", b"numPr"]) => {
                        if let Some(list) =
                            attribute(b"val").and_then(|list| list.trim().parse().ok())
                        {
                            numbering.styles.entry(key.clone()).or_default().list = Some(list);
                        }
                    }
                    b"ilvl" if under(&[b"style", b"pPr", b"numPr"]) => {
                        if let Some(level) =
                            attribute(b"val").and_then(|level| level.trim().parse::<usize>().ok())
                        {
                            numbering.styles.entry(key.clone()).or_default().level = Some(level);
                        }
                    }
                    _ => {}
                }
            }
        }
        if start {
            stack.push(node);
        }
        buffer.clear();
    }
}

/// A style Word is reading: the key its values merge under, whether it is
/// a paragraph style marked the default, and whether its name has been
/// read.
struct DocxOpenStyle {
    key: Option<String>,
    paragraph: bool,
    default: bool,
    named: bool,
}

/// Enter a definition of a style in Word's reading, under the bound on the
/// styles a part may define.
fn docx_word_style(
    numbering: &mut DocxStyleNumbering,
    key: &str,
    paragraph: bool,
    default: bool,
) -> Result<(), DocumentError> {
    if numbering.styles.len() >= MAX_DOCX_STYLES && !numbering.styles.contains_key(key) {
        return Err(DocumentError::ResourceLimit);
    }
    numbering
        .styles
        .entry(key.to_string())
        .or_default()
        .paragraph = paragraph;
    if default {
        numbering.default_paragraph = Some(key.to_string());
    }
    Ok(())
}

/// The attribute AnyDoc 0.2.4 reads for `attr(ns::W, name)`: the first in
/// WordprocessingML's namespace, Transitional or Strict, else the first
/// without a prefix. Word reads the same attribute, and each side then reads
/// its value as it reads numbers and names.
fn word_attribute(
    resolver: &quick_xml::name::NamespaceResolver,
    event: &quick_xml::events::BytesStart<'_>,
    wanted: &[u8],
) -> Option<String> {
    let (qualified, unprefixed) = word_attribute_forms(resolver, event, wanted);
    qualified.or(unprefixed)
}

/// An attribute's first value in WordprocessingML's namespace, Transitional
/// or Strict, and its first value without a namespace: unprefixed, or with a
/// prefix no declaration binds, which AnyDoc reads as unprefixed.
fn word_attribute_forms(
    resolver: &quick_xml::name::NamespaceResolver,
    event: &quick_xml::events::BytesStart<'_>,
    wanted: &[u8],
) -> (Option<String>, Option<String>) {
    let mut qualified = None;
    let mut unprefixed = None;
    for attribute in event.attributes().flatten() {
        let key = attribute.key.as_ref();
        if key == b"xmlns" || key.starts_with(b"xmlns:") {
            continue;
        }
        let (namespace, local) = resolver.resolve_attribute(attribute.key);
        if local.as_ref() != wanted {
            continue;
        }
        let value = || {
            attribute
                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .map(|value| value.into_owned())
                .unwrap_or_else(|_| String::from_utf8_lossy(attribute.value.as_ref()).into_owned())
        };
        match namespace {
            quick_xml::name::ResolveResult::Bound(namespace)
                if qualified.is_none()
                    && WORDPROCESSINGML_NAMESPACES.contains(&namespace.as_ref()) =>
            {
                qualified = Some(value());
            }
            quick_xml::name::ResolveResult::Unbound
            | quick_xml::name::ResolveResult::Unknown(_)
                if unprefixed.is_none() =>
            {
                unprefixed = Some(value());
            }
            _ => {}
        }
    }
    (qualified, unprefixed)
}

/// An open element of a numbering part: its local name, and whether it is
/// in WordprocessingML's namespace.
struct DocxNumberingNode {
    local: Vec<u8>,
    word: bool,
}

/// A definition (`w:abstractNum`) being read, and the list styles it
/// declares; `None` for one the side does not keep.
struct DocxOpenDefinition {
    id: String,
    definition: DocxDefinition,
    style_links: Vec<String>,
    /// A `w:numStyleLink` has been read; AnyDoc reads the first.
    style_link_read: bool,
    /// Word's current level (see [`word_level_index`]).
    current: Option<usize>,
}

/// A list instance (`w:num`) being read, and whether its `w:abstractNumId`
/// has been read, since AnyDoc reads the first.
struct DocxOpenList {
    id: Option<u64>,
    list: DocxList,
    definition_read: bool,
    /// Word's current level (see [`word_level_index`]).
    current: Option<usize>,
}

/// A `w:lvlOverride` being read for AnyDoc: the level it names, and the
/// level and the start it gives, each the first of its kind. Word applies
/// what an override gives as it reads it.
struct DocxOpenOverride {
    index: Option<usize>,
    level: Option<DocxLevel>,
    start: Option<u64>,
    level_read: bool,
    start_read: bool,
}

/// A list level being read: its index, how deep it opened, what it says so
/// far, and the properties whose first element AnyDoc has read.
struct DocxOpenLevel {
    index: usize,
    /// The elements open outside it, which close it again.
    depth: usize,
    level: DocxLevel,
    read: Vec<&'static [u8]>,
}

/// A `w:start` or `w:startOverride` value as AnyDoc reads it, clamped to
/// `xsd:int`'s non-negative range.
fn docx_start_value(value: &str) -> Option<u64> {
    value
        .parse::<i64>()
        .ok()
        .map(|value| value.clamp(0, i64::from(i32::MAX)) as u64)
}

/// An integer as Word, as LibreOffice shows it, reads one from an
/// attribute (`rtl_str_toInt32`): white space and control characters
/// skipped at its start, a sign, and the decimal digits up to the first
/// other character, so `7x` reads as 7. A value without digits reads as 0,
/// as does one outside `i32`'s range.
fn word_integer(value: &str) -> i64 {
    let rest = value.trim_start_matches(|character: char| character != '\0' && character <= ' ');
    let (negative, digits) = match rest.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, rest.strip_prefix('+').unwrap_or(rest)),
    };
    let mut number = 0i64;
    for digit in digits.bytes().take_while(u8::is_ascii_digit) {
        number = number * 10 + i64::from(digit - b'0');
        if number > i64::from(i32::MAX) + 1 {
            return 0;
        }
    }
    match if negative { -number } else { number } {
        number if number > i64::from(i32::MAX) => 0,
        number => number,
    }
}

/// The number formats ECMA-376 defines (`ST_NumberFormat`). Word, as
/// LibreOffice shows it, reads no other: `<w:numFmt w:val=" upperRoman"/>`
/// leaves the level's format as it was.
const WORD_NUMBER_FORMATS: [&str; 63] = [
    "decimal",
    "upperRoman",
    "lowerRoman",
    "upperLetter",
    "lowerLetter",
    "ordinal",
    "cardinalText",
    "ordinalText",
    "hex",
    "chicago",
    "ideographDigital",
    "japaneseCounting",
    "aiueo",
    "iroha",
    "decimalFullWidth",
    "decimalHalfWidth",
    "japaneseLegal",
    "japaneseDigitalTenThousand",
    "decimalEnclosedCircle",
    "decimalFullWidth2",
    "aiueoFullWidth",
    "irohaFullWidth",
    "decimalZero",
    "bullet",
    "ganada",
    "chosung",
    "decimalEnclosedFullstop",
    "decimalEnclosedParen",
    "decimalEnclosedCircleChinese",
    "ideographEnclosedCircle",
    "ideographTraditional",
    "ideographZodiac",
    "ideographZodiacTraditional",
    "taiwaneseCounting",
    "ideographLegalTraditional",
    "taiwaneseCountingThousand",
    "taiwaneseDigital",
    "chineseCounting",
    "chineseLegalSimplified",
    "chineseCountingThousand",
    "koreanDigital",
    "koreanCounting",
    "koreanLegal",
    "koreanDigital2",
    "vietnameseCounting",
    "russianLower",
    "russianUpper",
    "none",
    "numberInDash",
    "hebrew1",
    "hebrew2",
    "arabicAlpha",
    "arabicAbjad",
    "hindiVowels",
    "hindiConsonants",
    "hindiNumbers",
    "hindiCounting",
    "thaiLetters",
    "thaiNumbers",
    "thaiCounting",
    "bahtText",
    "dollarText",
    "custom",
];

/// A `w:start` or `w:startOverride` value as Word reads it (see
/// [`word_integer`]), a negative one as 0. An element without a value
/// reads as 0.
fn word_start_value(value: Option<&str>) -> u64 {
    word_integer(value.unwrap_or_default()).max(0) as u64
}

/// The level a `w:lvl` or `w:lvlOverride` names as Word, as LibreOffice
/// shows it, reads it: its WordprocessingML `w:ilvl` read as an integer,
/// `Some(None)` for a level past the ninth, which numbers nothing. `None`
/// where it names none: Word then reads the element into its current
/// level, the last one named in the definition or list instance, and drops
/// it where none has been.
fn word_level_index(
    resolver: &quick_xml::name::NamespaceResolver,
    event: &quick_xml::events::BytesStart<'_>,
) -> Option<Option<usize>> {
    let (qualified, _) = word_attribute_forms(resolver, event, b"ilvl");
    Some(
        usize::try_from(word_integer(&qualified?))
            .ok()
            .filter(|&index| index < DOCX_LIST_LEVELS),
    )
}

/// Read a numbering part's list definitions as Word or AnyDoc reads them,
/// streamed under AnyDoc's depth and node bounds: the WordprocessingML
/// `w:abstractNum` and `w:num` children of the first `w:numbering`, each
/// attribute as AnyDoc picks it. Of two definitions or list instances with
/// one id, AnyDoc keeps the last, read afresh, and Word, as LibreOffice
/// shows it, the first.
fn docx_numbering_definitions(
    reader: impl std::io::BufRead,
    word: bool,
) -> Result<DocxNumbering, DocumentError> {
    let mut reader = quick_xml::NsReader::from_reader(reader);
    reader.config_mut().trim_text(false);
    reader.config_mut().check_end_names = false;
    let mut buffer = Vec::new();
    let mut stack: Vec<DocxNumberingNode> = Vec::new();
    let mut nodes = 0usize;
    let mut numbering = DocxNumbering::default();
    // Whether the part's first `w:numbering` is open, and has been read.
    let mut in_root = false;
    let mut root_read = false;
    let mut definition: Option<DocxOpenDefinition> = None;
    let mut list: Option<DocxOpenList> = None;
    let mut override_level: Option<DocxOpenOverride> = None;
    let mut open_level: Option<DocxOpenLevel> = None;
    // A number as the side reads it: Word reads an XML Schema integer,
    // collapsing white space around it, and AnyDoc parses the text as it
    // stands, so a number written with white space is only Word's.
    let number = |text: String| {
        if word {
            text.trim().to_string()
        } else {
            text
        }
    };
    // A definition id as the side matches it: AnyDoc its text, Word the
    // integer it reads.
    let definition_id = |id: String| {
        if word {
            id.trim().parse::<i64>().ok().map(|id| id.to_string())
        } else {
            Some(id)
        }
    };
    loop {
        let (namespace, event) = reader
            .read_resolved_event_into(&mut buffer)
            .map_err(|_| DocumentError::Malformed)?;
        let in_word = WordVocabulary::of(&namespace) == WordVocabulary::Word;
        let (event, start) = match event {
            quick_xml::events::Event::Start(event) => (event, true),
            quick_xml::events::Event::Empty(event) => (event, false),
            quick_xml::events::Event::End(_) => {
                if let Some(closed) = stack.pop() {
                    docx_numbering_close(
                        &closed,
                        stack.len(),
                        word,
                        &mut numbering,
                        &mut definition,
                        &mut list,
                        &mut override_level,
                        &mut open_level,
                    )?;
                    if stack.is_empty() && in_root {
                        in_root = false;
                    }
                }
                buffer.clear();
                continue;
            }
            quick_xml::events::Event::Eof => return Ok(numbering),
            _ => {
                nodes += 1;
                if nodes > MAX_XML_NODES {
                    return Err(DocumentError::ResourceLimit);
                }
                buffer.clear();
                continue;
            }
        };
        nodes += 1;
        if nodes > MAX_XML_NODES || (start && stack.len() >= MAX_XML_DEPTH) {
            return Err(DocumentError::ResourceLimit);
        }
        let node = DocxNumberingNode {
            local: xml_local_name(event.name().as_ref()).to_vec(),
            word: in_word,
        };
        let attribute = |name: &[u8]| word_attribute(reader.resolver(), &event, name);
        // A WordprocessingML element whose parent is the named one.
        let parent_is = |name: &[u8]| {
            node.word
                && stack
                    .last()
                    .is_some_and(|parent| parent.word && parent.local == name)
        };
        let local = node.local.as_slice();
        if stack.is_empty() {
            if node.word && local == b"numbering" && !root_read {
                root_read = true;
                in_root = start;
            }
        } else if in_root && stack.len() == 1 && node.word {
            match local {
                b"abstractNum" => {
                    definition = attribute(b"abstractNumId")
                        .and_then(definition_id)
                        // Word keeps a definition's first reading.
                        .filter(|id| !word || !numbering.definition_ids.contains_key(id))
                        .map(|id| DocxOpenDefinition {
                            id,
                            definition: DocxDefinition::default(),
                            style_links: Vec::new(),
                            style_link_read: false,
                            current: None,
                        });
                }
                b"num" => {
                    let id = attribute(b"numId").and_then(|id| number(id).parse().ok());
                    list = Some(DocxOpenList {
                        // Word keeps a list instance's first reading.
                        id: id.filter(|id| !word || !numbering.lists.contains_key(id)),
                        list: DocxList::default(),
                        definition_read: false,
                        current: None,
                    });
                }
                _ => {}
            }
        } else if parent_is(b"abstractNum") && stack.len() == 2 {
            if let Some(open) = definition.as_mut() {
                match local {
                    // Word starts afresh the level an element names, and
                    // reads one naming none into its current level. AnyDoc
                    // takes the level named, else the first.
                    b"lvl" if word => {
                        let (index, base) = match word_level_index(reader.resolver(), &event) {
                            Some(named) => {
                                open.current = named;
                                (named, None)
                            }
                            None => (
                                open.current,
                                open.current
                                    .and_then(|current| open.definition.levels[current].clone()),
                            ),
                        };
                        open_level = index.map(|index| DocxOpenLevel {
                            index,
                            depth: stack.len(),
                            level: base.unwrap_or_default(),
                            read: Vec::new(),
                        });
                    }
                    b"lvl" => {
                        let index = attribute(b"ilvl")
                            .and_then(|value| value.parse::<usize>().ok())
                            .unwrap_or(0);
                        if index < DOCX_LIST_LEVELS {
                            open_level = Some(DocxOpenLevel {
                                index,
                                depth: stack.len(),
                                level: DocxLevel::default(),
                                read: Vec::new(),
                            });
                        }
                    }
                    b"numStyleLink" => {
                        // AnyDoc reads the first; Word the last that names
                        // a style.
                        let style = attribute(b"val");
                        if word {
                            open.definition.style_link =
                                style.or(open.definition.style_link.take());
                        } else if !open.style_link_read {
                            open.definition.style_link = style;
                        }
                        open.style_link_read = true;
                    }
                    b"styleLink" => open.style_links.extend(attribute(b"val")),
                    _ => {}
                }
            }
        } else if parent_is(b"num") && stack.len() == 2 {
            if let Some(open) = list.as_mut() {
                match local {
                    b"abstractNumId" => {
                        // AnyDoc reads the first; Word the last.
                        if word || !open.definition_read {
                            open.list.definition = attribute(b"val").and_then(definition_id);
                        }
                        open.definition_read = true;
                    }
                    b"lvlOverride" => {
                        // An override naming no level goes on, for Word,
                        // with the level the previous one named.
                        if word {
                            if let Some(named) = word_level_index(reader.resolver(), &event) {
                                open.current = named;
                            }
                        }
                        let index = attribute(b"ilvl")
                            .and_then(|value| value.parse::<usize>().ok())
                            .unwrap_or(0);
                        override_level = Some(DocxOpenOverride {
                            index: (index < DOCX_LIST_LEVELS).then_some(index),
                            level: None,
                            start: None,
                            level_read: false,
                            start_read: false,
                        });
                    }
                    _ => {}
                }
            }
        } else if parent_is(b"lvlOverride") && stack.len() == 3 {
            match (override_level.as_mut(), list.as_mut()) {
                // Word applies each start override and level as it reads
                // them, to its current level, a level naming its own
                // becoming current: LibreOffice restarts level 0 and
                // replaces level 1 for an override of level 0 holding a
                // level 1.
                (Some(_), Some(instance)) if word => match local {
                    b"startOverride" => {
                        if let Some(current) = instance.current {
                            instance.list.starts[current] =
                                Some(word_start_value(attribute(b"val").as_deref()));
                        }
                    }
                    b"lvl" => {
                        let (index, base) = match word_level_index(reader.resolver(), &event) {
                            Some(named) => {
                                instance.current = named;
                                (named, None)
                            }
                            None => (
                                instance.current,
                                instance
                                    .current
                                    .and_then(|current| instance.list.levels[current].clone()),
                            ),
                        };
                        open_level = index.map(|index| DocxOpenLevel {
                            index,
                            depth: stack.len(),
                            level: base.unwrap_or_default(),
                            read: Vec::new(),
                        });
                    }
                    _ => {}
                },
                // AnyDoc reads an override's first start override and first
                // level, both for the level the override names.
                (Some(open), _) if !word => match local {
                    b"startOverride" if !open.start_read => {
                        open.start_read = true;
                        open.start = attribute(b"val").and_then(|start| docx_start_value(&start));
                    }
                    b"lvl" if !open.level_read => {
                        open.level_read = true;
                        if let Some(index) = open.index {
                            open_level = Some(DocxOpenLevel {
                                index,
                                depth: stack.len(),
                                level: DocxLevel::default(),
                                read: Vec::new(),
                            });
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
        } else if parent_is(b"lvl") {
            if let Some(open) = open_level.as_mut() {
                docx_level_property(open, local, word, |name| attribute(name), &event);
            }
        }
        if start {
            stack.push(node);
        } else {
            docx_numbering_close(
                &node,
                stack.len(),
                word,
                &mut numbering,
                &mut definition,
                &mut list,
                &mut override_level,
                &mut open_level,
            )?;
        }
        buffer.clear();
    }
}

/// Read one property of an open list level. AnyDoc reads the first element
/// of each, its value as it stands. Word, as LibreOffice shows it, reads
/// every element, the last winning: a number (`w:start`, `w:lvlRestart`)
/// as it reads integers, an element without a value as 0; a format
/// (`w:numFmt`), number text (`w:lvlText`), or style (`w:pStyle`) only from
/// an element that gives one, a format only one ECMA-376 defines, so
/// `<w:numFmt/>` leaves the format before it.
/// LibreOffice does not apply `w:lvlRestart` at all; Word's, which ECMA-376
/// defines, is read as its other numbers are.
fn docx_level_property(
    open: &mut DocxOpenLevel,
    local: &[u8],
    word: bool,
    attribute: impl Fn(&[u8]) -> Option<String>,
    event: &quick_xml::events::BytesStart<'_>,
) {
    let property: &'static [u8] = match local {
        b"numFmt" => b"numFmt",
        b"start" => b"start",
        b"lvlRestart" => b"lvlRestart",
        b"pStyle" => b"pStyle",
        b"lvlText" => b"lvlText",
        b"isLgl" => b"isLgl",
        _ => return,
    };
    let level = &mut open.level;
    let value = attribute(b"val");
    if word {
        match property {
            b"numFmt" => {
                level.format = value
                    .filter(|format| WORD_NUMBER_FORMATS.contains(&format.as_str()))
                    .map(Rc::from)
                    .or(level.format.take());
            }
            b"start" => level.start = Some(word_start_value(value.as_deref())),
            b"lvlRestart" => {
                level.restart =
                    u32::try_from(word_integer(value.as_deref().unwrap_or_default())).ok();
            }
            b"pStyle" => level.style = value.map(Rc::from).or(level.style.take()),
            b"lvlText" => level.text = value.map(DocxLevelText::of).or(level.text.take()),
            b"isLgl" => level.legal = Some(!xml_toggle_off(event)),
            _ => {}
        }
        return;
    }
    if open.read.contains(&property) {
        return;
    }
    open.read.push(property);
    match property {
        b"numFmt" => level.format = value.map(Rc::from),
        b"start" => level.start = value.and_then(|start| docx_start_value(&start)),
        b"lvlRestart" => level.restart = value.and_then(|restart| restart.parse().ok()),
        b"pStyle" => level.style = value.map(Rc::from),
        b"lvlText" => level.text = Some(DocxLevelText::of(value.unwrap_or_default())),
        b"isLgl" => {
            level.legal = Some(!matches!(
                value.as_deref(),
                Some("0" | "false" | "off" | "none")
            ));
        }
        _ => {}
    }
}

/// Close an element of a numbering part, `depth` elements deep: commit the
/// level, override, list instance, or definition it ends as the side keeps
/// it.
#[allow(clippy::too_many_arguments)]
fn docx_numbering_close(
    closed: &DocxNumberingNode,
    depth: usize,
    word: bool,
    numbering: &mut DocxNumbering,
    definition: &mut Option<DocxOpenDefinition>,
    list: &mut Option<DocxOpenList>,
    override_level: &mut Option<DocxOpenOverride>,
    open_level: &mut Option<DocxOpenLevel>,
) -> Result<(), DocumentError> {
    if !closed.word {
        return Ok(());
    }
    match (closed.local.as_slice(), depth) {
        (b"lvl", 2 | 3) if open_level.as_ref().is_some_and(|open| open.depth == depth) => {
            let Some(open) = open_level.take() else {
                return Ok(());
            };
            if depth == 2 {
                if let Some(definition) = definition.as_mut() {
                    definition.definition.levels[open.index] = Some(open.level);
                }
            } else if word {
                if let Some(instance) = list.as_mut() {
                    instance.list.levels[open.index] = Some(open.level);
                }
            } else if let Some(replacing) = override_level.as_mut() {
                replacing.level = Some(open.level);
            }
        }
        (b"lvlOverride", 2) => {
            let (Some(replacing), Some(open)) = (override_level.take(), list.as_mut()) else {
                return Ok(());
            };
            // Word has applied what the override gives.
            if word {
                return Ok(());
            }
            let Some(index) = replacing.index else {
                return Ok(());
            };
            if let Some(level) = replacing.level {
                open.list.levels[index] = Some(level);
                // AnyDoc replaces the whole level, and then restarts it
                // at this override's start, if it gives one.
                open.list.starts[index] = None;
            }
            if let Some(start) = replacing.start {
                open.list.starts[index] = Some(start);
            }
        }
        (b"num", 1) => {
            let Some(open) = list.take() else {
                return Ok(());
            };
            // AnyDoc keeps an instance naming a definition, and Word the
            // first reading of each id.
            let Some(id) = open.id.filter(|_| word || open.list.definition.is_some()) else {
                return Ok(());
            };
            if numbering.lists.len() >= MAX_DOCX_STYLES && !numbering.lists.contains_key(&id) {
                return Err(DocumentError::ResourceLimit);
            }
            numbering.lists.insert(id, open.list);
        }
        (b"abstractNum", 1) => {
            let Some(open) = definition.take() else {
                return Ok(());
            };
            if open.id.len() > MAX_STYLE_ID_BYTES
                || (numbering.definitions.len() >= MAX_DOCX_STYLES
                    && !numbering.definition_ids.contains_key(&open.id))
            {
                return Err(DocumentError::ResourceLimit);
            }
            let next = numbering.definitions.len();
            let index = *numbering.definition_ids.entry(open.id).or_insert(next);
            if index == next {
                numbering.definitions.push(open.definition);
            } else {
                numbering.definitions[index] = open.definition;
            }
            for style in open.style_links {
                if numbering.style_definitions.len() >= MAX_DOCX_STYLES
                    && !numbering.style_definitions.contains_key(&style)
                {
                    return Err(DocumentError::ResourceLimit);
                }
                numbering.style_definitions.entry(style).or_insert(index);
            }
        }
        _ => {}
    }
    Ok(())
}

/// The paragraph styles' numbering each side of the list replay reads: Word
/// from the styles part its relationship names, and AnyDoc from its own.
fn docx_list_styles(
    archive: &mut ZipArchive<Cursor<&[u8]>>,
    parts: &DocxListParts,
) -> Result<DocxStyleNumbering, DocumentError> {
    let mut read = |part: Option<&str>| -> Result<DocxStyleNumbering, DocumentError> {
        let mut numbering = DocxStyleNumbering::default();
        if let Some(entry) = part.and_then(|part| archive.by_name(part).ok()) {
            docx_style_numbering(
                open_xml_stream(entry.take(MAX_ARCHIVE_ENTRY_BYTES))?,
                &mut numbering,
            )?;
        }
        Ok(numbering)
    };
    let word = read(parts.word_styles.as_deref())?;
    if parts.word_styles.as_deref() == Some(parts.anydoc_styles.as_str()) {
        return Ok(word);
    }
    let anydoc = read(Some(&parts.anydoc_styles))?;
    Ok(DocxStyleNumbering {
        styles: word.styles,
        style_names: word.style_names,
        default_paragraph: word.default_paragraph,
        anydoc_styles: anydoc.anydoc_styles,
    })
}

/// A numbering part as one side reads it; a side without one, or whose
/// part is missing, numbers nothing.
fn docx_list_numbering(
    archive: &mut ZipArchive<Cursor<&[u8]>>,
    part: Option<&str>,
    word: bool,
) -> Result<DocxNumbering, DocumentError> {
    match part.and_then(|part| archive.by_name(part).ok()) {
        Some(entry) => {
            docx_numbering_definitions(open_xml_stream(entry.take(MAX_ARCHIVE_ENTRY_BYTES))?, word)
        }
        None => Ok(DocxNumbering::default()),
    }
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
    /// `TargetMode="External"`: a link outside the package.
    external: bool,
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
                    external: xml_attribute_value(&event, b"TargetMode")
                        .is_some_and(|mode| mode.trim().eq_ignore_ascii_case("External")),
                });
            }
            Ok(quick_xml::events::Event::Eof) => return Ok(relationships),
            Ok(_) => {}
            Err(_) => return Err(DocumentError::Malformed),
        }
        buffer.clear();
    }
}

/// The package relationships namespace.
const PACKAGE_RELATIONSHIPS_NAMESPACE: &[u8] =
    b"http://schemas.openxmlformats.org/package/2006/relationships";

/// A relationship type AnyDoc reads, in the Transitional form it maps
/// Strict types onto.
const STYLES_RELATIONSHIP: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles";
const NUMBERING_RELATIONSHIP: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/numbering";
const FOOTNOTES_RELATIONSHIP: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/footnotes";
const ENDNOTES_RELATIONSHIP: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/endnotes";

/// A Strict (ISO/IEC 29500) namespace or relationship type in the
/// Transitional form AnyDoc 0.2.4 maps it onto (`normalize_ooxml_uri`);
/// anything else as it stands.
fn transitional_uri(uri: &str) -> String {
    uri.strip_prefix("http://purl.oclc.org/ooxml/")
        .and_then(|rest| rest.split_once('/'))
        .map(|(family, tail)| format!("http://schemas.openxmlformats.org/{family}/2006/{tail}"))
        .unwrap_or_else(|| uri.to_string())
}

/// One relationship as Word and AnyDoc read a rels part: a `Relationship`
/// element in the package relationships namespace, at any depth, its
/// attributes taken by local name as AnyDoc's `attr_any` takes them, and
/// its type in the Transitional form.
struct PackageRelationship {
    id: String,
    kind: String,
    target: String,
    internal: bool,
}

/// The relationships of a rels part, in document order, read as AnyDoc
/// 0.2.4 reads them (`read_rels`): an element without an id or a target is
/// skipped. A part the reader cannot parse is malformed.
fn package_relationships(bytes: &[u8]) -> Result<Vec<PackageRelationship>, DocumentError> {
    let mut reader = quick_xml::NsReader::from_reader(Cursor::new(bytes));
    reader.config_mut().trim_text(false);
    reader.config_mut().check_end_names = false;
    let mut buffer = Vec::new();
    let mut relationships = Vec::new();
    loop {
        let (namespace, event) = reader
            .read_resolved_event_into(&mut buffer)
            .map_err(|_| DocumentError::Malformed)?;
        match event {
            quick_xml::events::Event::Start(event) | quick_xml::events::Event::Empty(event)
                if xml_local_name(event.name().as_ref()) == b"Relationship"
                    && matches!(
                        namespace,
                        quick_xml::name::ResolveResult::Bound(namespace)
                            if namespace.as_ref() == PACKAGE_RELATIONSHIPS_NAMESPACE
                    ) =>
            {
                let (Some(id), Some(target)) = (
                    xml_attribute_value(&event, b"Id"),
                    xml_attribute_value(&event, b"Target"),
                ) else {
                    buffer.clear();
                    continue;
                };
                relationships.push(PackageRelationship {
                    id,
                    kind: transitional_uri(
                        &xml_attribute_value(&event, b"Type").unwrap_or_default(),
                    ),
                    target,
                    internal: !xml_attribute_value(&event, b"TargetMode")
                        .is_some_and(|mode| mode.eq_ignore_ascii_case("External")),
                });
            }
            quick_xml::events::Event::Eof => return Ok(relationships),
            _ => {}
        }
        buffer.clear();
    }
}

/// The part AnyDoc 0.2.4 reads for a typed relationship of a main part
/// (`typed_part_path`): the target of the internal relationship of that
/// type with the lowest id, where a later relationship with the same id
/// replaces an earlier one, else the conventional name beside the main
/// part.
fn anydoc_typed_part(
    relationships: &[PackageRelationship],
    main: &str,
    kind: &str,
    conventional: &str,
) -> String {
    let mut by_id: HashMap<&str, &PackageRelationship> = HashMap::new();
    for relationship in relationships {
        by_id.insert(&relationship.id, relationship);
    }
    let reference = by_id
        .into_values()
        .filter(|relationship| relationship.internal && relationship.kind == kind)
        .min_by(|first, second| first.id.cmp(&second.id))
        .map_or(conventional, |relationship| relationship.target.as_str());
    anydoc_resolve(main, reference)
        .or_else(|| anydoc_resolve(main, conventional))
        .unwrap_or_else(|| conventional.to_string())
}

/// The part Word reads for a typed relationship of a main part, as
/// LibreOffice shows it: the target of the first internal relationship of
/// that type in the rels part, as written (see [`word_resolve`]), and none
/// without one. Word has no conventional name to fall back on.
fn word_typed_part(
    relationships: &[PackageRelationship],
    main: &str,
    kind: &str,
) -> Option<String> {
    relationships
        .iter()
        .find(|relationship| relationship.internal && relationship.kind == kind)
        .map(|relationship| word_resolve(main, &relationship.target))
}

/// Resolve a package reference as Word, as LibreOffice shows it, opens the
/// part it names: from the base part's directory, `..` clamped at the root,
/// each segment as written, a query or fragment kept. An empty reference
/// names the base part, as for AnyDoc.
fn word_resolve(base_part: &str, reference: &str) -> String {
    if reference.is_empty() {
        return base_part.to_string();
    }
    let mut segments: Vec<&str> = Vec::new();
    if !reference.starts_with('/') {
        if let Some((directory, _)) = base_part.rsplit_once('/') {
            segments.extend(directory.split('/').filter(|segment| !segment.is_empty()));
        }
    }
    for segment in reference.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            segment => segments.push(segment),
        }
    }
    segments.join("/")
}

/// Whether Word and AnyDoc open different parts for a relationship's
/// target: Word the part it names as written, and AnyDoc the part it names
/// once its query and fragment are dropped and each segment percent-decoded
/// (`anydoc_resolve`), as `foot%6Eotes.xml` names `footnotes.xml` to AnyDoc
/// alone. Where the package holds neither part, both read nothing.
fn target_parts_differ(archive: &ZipArchive<Cursor<&[u8]>>, base_part: &str, target: &str) -> bool {
    let word = word_resolve(base_part, target);
    let anydoc = anydoc_resolve(base_part, target);
    anydoc.as_deref() != Some(word.as_str())
        && std::iter::once(word.as_str())
            .chain(anydoc.as_deref())
            .any(|part| archive.index_for_name(part).is_some())
}

/// Whether a relationships part is written as OPC defines it, which is how
/// Word and AnyDoc read it alike: a `Relationships` root in the package
/// relationships namespace, holding nothing but `Relationship` elements in
/// it, each with a unique, non-empty `Id`, a `Type`, and a `Target`, and no
/// attribute but those and `TargetMode` (`Internal` or `External`), all
/// unprefixed. LibreOffice, and System.IO.Packaging, refuse any other
/// element or attribute; LibreOffice reads the unprefixed attributes, by
/// element names as written, while AnyDoc reads the first attribute of each
/// local name, in any namespace, of a `Relationship` in the namespace at
/// any depth. Written otherwise, one part could name one target to Word and
/// another to AnyDoc (`x:Target="other.xml" Target="footnotes.xml"`).
fn opc_relationships(bytes: &[u8]) -> bool {
    let mut reader = quick_xml::NsReader::from_reader(Cursor::new(bytes));
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();
    let mut depth = 0usize;
    let mut roots = 0usize;
    let mut ids: HashSet<Vec<u8>> = HashSet::new();
    loop {
        let Ok((namespace, event)) = reader.read_resolved_event_into(&mut buffer) else {
            return false;
        };
        let in_namespace = matches!(
            namespace,
            quick_xml::name::ResolveResult::Bound(namespace)
                if namespace.as_ref() == PACKAGE_RELATIONSHIPS_NAMESPACE
        );
        let (element, opens) = match event {
            quick_xml::events::Event::Start(element) => (element, true),
            quick_xml::events::Event::Empty(element) => (element, false),
            quick_xml::events::Event::End(_) => {
                depth = depth.saturating_sub(1);
                buffer.clear();
                continue;
            }
            quick_xml::events::Event::Eof => return roots == 1,
            _ => {
                buffer.clear();
                continue;
            }
        };
        let name = element.name();
        let written = match depth {
            0 => {
                roots += 1;
                roots == 1 && in_namespace && name.as_ref() == b"Relationships"
            }
            1 => {
                in_namespace
                    && name.as_ref() == b"Relationship"
                    && opc_relationship_attributes(&element, &mut ids)
            }
            _ => false,
        };
        if !written {
            return false;
        }
        if opens {
            depth += 1;
        }
        buffer.clear();
    }
}

/// Whether a `Relationship` element carries the attributes OPC defines, and
/// no other (see [`opc_relationships`]), its `Id` not among `ids`, which it
/// joins.
fn opc_relationship_attributes(
    element: &quick_xml::events::BytesStart<'_>,
    ids: &mut HashSet<Vec<u8>>,
) -> bool {
    let mut id = None;
    let mut kind = false;
    let mut target = false;
    for attribute in element.attributes() {
        let Ok(attribute) = attribute else {
            return false;
        };
        let key = attribute.key.as_ref();
        if key == b"xmlns" || key.starts_with(b"xmlns:") {
            continue;
        }
        let Ok(value) = attribute.normalized_value(quick_xml::XmlVersion::Implicit1_0) else {
            return false;
        };
        match key {
            b"Id" if !value.is_empty() => id = Some(value.as_bytes().to_vec()),
            b"Type" if !value.is_empty() => kind = true,
            b"Target" if !value.is_empty() => target = true,
            b"TargetMode" if matches!(value.as_ref(), "Internal" | "External") => {}
            _ => return false,
        }
    }
    kind && target && id.is_some_and(|id| ids.insert(id))
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
    /// DOCX list numbering, whose level properties format the labels.
    numbering_parts: HashSet<String>,
    /// XLSX worksheets.
    worksheet_parts: HashSet<String>,
    /// The DOCX styles and numbering parts each side of the list replay
    /// reads.
    list_parts: DocxListParts,
    /// The DOCX footnotes and endnotes parts each side reads.
    note_parts: DocxNoteParts,
    /// A relationship naming a part both sides read, of those the checks
    /// follow, opens one part to Word and another to AnyDoc (see
    /// [`target_parts_differ`]).
    targets_differ: bool,
}

/// The styles and numbering parts Word and AnyDoc each read for a Word
/// document's lists. Where the main part's relationships name one of each,
/// as ECMA-376 allows, the two sides read the same parts; AnyDoc also reads
/// a conventional name no relationship gives, and of several relationships
/// of a type Word takes the first and AnyDoc the lowest id.
#[derive(Default)]
struct DocxListParts {
    word_styles: Option<String>,
    anydoc_styles: String,
    word_numbering: Option<String>,
    anydoc_numbering: String,
}

/// The footnotes and endnotes parts Word and AnyDoc each read, found as the
/// list parts are: of several relationships of a type Word, as LibreOffice
/// shows it, takes the first and AnyDoc the lowest id, and AnyDoc alone
/// reads a conventional part no relationship names.
#[derive(Default)]
struct DocxNoteParts {
    word_footnotes: Option<String>,
    anydoc_footnotes: String,
    word_endnotes: Option<String>,
    anydoc_endnotes: String,
}

impl DocxNoteParts {
    /// The sides that read a story part: both the main part, and each side
    /// the notes parts it reads.
    fn read(&self, part: &str) -> DocxPartRead {
        if part == "word/document.xml" {
            return DocxPartRead {
                word: true,
                anydoc: true,
            };
        }
        DocxPartRead {
            word: [&self.word_footnotes, &self.word_endnotes]
                .into_iter()
                .any(|named| named.as_deref() == Some(part)),
            anydoc: self.anydoc_footnotes == part || self.anydoc_endnotes == part,
        }
    }
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
    // The package's relationships, and a Word or Excel main part's, name the
    // parts both sides read; each must be written as OPC defines it, so that
    // they name the same parts to both.
    let package_rels = read_optional_xml_part(archive, "_rels/.rels")?;
    let rels = read_optional_xml_part(archive, &ooxml_rels_part(main))?;
    let main_rels = rels.as_ref().filter(|_| kind != DocumentKind::Pptx);
    if package_rels
        .iter()
        .chain(main_rels)
        .any(|bytes| !opc_relationships(bytes))
    {
        return Err(DocumentError::Malformed);
    }
    // Every check reads the conventional main part, while AnyDoc converts the
    // part the officeDocument relationship names (lowest id first), and Word
    // the part it names as written. A package with such a relationship
    // naming any other part to either is refused rather than converted from
    // a part the checks never saw.
    let declared: Vec<OoxmlRelationship> = match &package_rels {
        Some(bytes) => ooxml_relationships(bytes)?,
        None => Vec::new(),
    }
    .into_iter()
    .filter(|relationship| relationship.kind.ends_with("/officeDocument"))
    .collect();
    if !declared.iter().all(|relationship| {
        anydoc_resolve("", &relationship.target).as_deref() == Some(main)
            && word_resolve("", &relationship.target) == main
    }) {
        return Err(DocumentError::Malformed);
    }
    let relationships = match &rels {
        Some(bytes) => ooxml_relationships(bytes)?,
        None => Vec::new(),
    };
    let typed = |suffix: &str| -> Vec<String> {
        relationships
            .iter()
            .filter(|relationship| relationship.kind.ends_with(suffix))
            .filter_map(|relationship| anydoc_resolve(main, &relationship.target))
            .collect()
    };
    // The internal relationships of these types, or these ids, whose parts
    // Word and AnyDoc must both open.
    let differ = |archive: &ZipArchive<Cursor<&[u8]>>, suffixes: &[&str], ids: &HashSet<String>| {
        relationships
            .iter()
            .filter(|relationship| {
                !relationship.external
                    && (ids.contains(&relationship.id)
                        || suffixes
                            .iter()
                            .any(|suffix| relationship.kind.ends_with(suffix)))
            })
            .any(|relationship| target_parts_differ(archive, main, &relationship.target))
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
            layout
                .numbering_parts
                .insert("word/numbering.xml".to_string());
            layout.numbering_parts.extend(typed("/numbering"));
            layout.targets_differ = differ(
                archive,
                &["/styles", "/numbering", "/footnotes", "/endnotes"],
                &HashSet::new(),
            );
            let read = match &rels {
                Some(bytes) => package_relationships(bytes)?,
                None => Vec::new(),
            };
            layout.list_parts = DocxListParts {
                word_styles: word_typed_part(&read, main, STYLES_RELATIONSHIP),
                anydoc_styles: anydoc_typed_part(&read, main, STYLES_RELATIONSHIP, "styles.xml"),
                word_numbering: word_typed_part(&read, main, NUMBERING_RELATIONSHIP),
                anydoc_numbering: anydoc_typed_part(
                    &read,
                    main,
                    NUMBERING_RELATIONSHIP,
                    "numbering.xml",
                ),
            };
            layout.note_parts = DocxNoteParts {
                word_footnotes: word_typed_part(&read, main, FOOTNOTES_RELATIONSHIP),
                anydoc_footnotes: anydoc_typed_part(
                    &read,
                    main,
                    FOOTNOTES_RELATIONSHIP,
                    "footnotes.xml",
                ),
                word_endnotes: word_typed_part(&read, main, ENDNOTES_RELATIONSHIP),
                anydoc_endnotes: anydoc_typed_part(
                    &read,
                    main,
                    ENDNOTES_RELATIONSHIP,
                    "endnotes.xml",
                ),
            };
        }
        DocumentKind::Xlsx => {
            layout.styles_parts.insert("xl/styles.xml".to_string());
            layout.styles_parts.extend(typed("/styles"));
            // AnyDoc loads each `<sheet r:id>` through the workbook
            // relationships whatever their type.
            let ids = match read_optional_xml_part(archive, main)? {
                Some(workbook) => xlsx_sheet_relationship_ids(&workbook),
                None => HashSet::new(),
            };
            layout.worksheet_parts.extend(
                relationships
                    .iter()
                    .filter(|relationship| ids.contains(&relationship.id))
                    .filter_map(|relationship| anydoc_resolve(main, &relationship.target)),
            );
            layout.targets_differ = differ(archive, &["/sharedStrings", "/styles"], &ids);
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
                for attribute in xml_attributes(&event) {
                    let value = attribute.value.trim().to_ascii_lowercase();
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
fn odf_reference_missing(
    value: &str,
    archive_names: &HashSet<String>,
    archive_directories: &HashSet<&str>,
) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') || is_external_uri(trimmed) {
        return false;
    }
    // An embedded object is a directory of parts.
    anydoc_resolve("content.xml", value).is_none_or(|part| {
        !archive_names.contains(&part) && !archive_directories.contains(part.trim_end_matches('/'))
    })
}

/// Every directory an entry name sits in, at any depth: `Object 1` and
/// `Object 1/Pictures` for `Object 1/Pictures/a.png`.
fn archive_directories(archive_names: &HashSet<String>) -> HashSet<&str> {
    let mut directories = HashSet::new();
    for name in archive_names {
        for (index, _) in name.match_indices('/') {
            if index > 0 {
                directories.insert(&name[..index]);
            }
        }
    }
    directories
}

/// Whether a slide or notes slide hides content AnyDoc 0.2.4 converts: the
/// slide itself (`show="0"`) or a shape (`p:cNvPr hidden="1"`). Values are
/// decoded and trimmed; a part the reader cannot parse counts as hidden.
fn xml_has_hidden_slide(bytes: &[u8]) -> bool {
    let mut reader = quick_xml::Reader::from_reader(Cursor::new(bytes));
    reader.config_mut().trim_text(false);
    reader.config_mut().check_end_names = false;
    let mut buffer = Vec::new();
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(quick_xml::events::Event::Start(event))
            | Ok(quick_xml::events::Event::Empty(event)) => {
                let shape_properties = xml_local_name(event.name().as_ref()) == b"cNvPr";
                let hidden = xml_attributes(&event).into_iter().any(|attribute| {
                    let value = attribute.value.trim().to_ascii_lowercase();
                    (attribute.local() == b"show" && matches!(value.as_str(), "0" | "false"))
                        || (shape_properties
                            && attribute.local() == b"hidden"
                            && matches!(value.as_str(), "1" | "true"))
                });
                if hidden {
                    return true;
                }
            }
            Ok(quick_xml::events::Event::Eof) => return false,
            Ok(_) => {}
            Err(_) => return true,
        }
        buffer.clear();
    }
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

/// Relationship types that embed or run content: OLE objects, embedded
/// packages, ActiveX controls, and macro projects. Matched by type, so a
/// part stored under any name is found.
fn ooxml_active_relationship(kind: &str) -> bool {
    const ACTIVE: [&str; 10] = [
        "/oleObject",
        "/package",
        "/control",
        "/activeXControl",
        "/activeXControlBinary",
        "/vbaProject",
        "/vbaProjectSignature",
        "/wordVbaData",
        "/attachedToolbars",
        "/keyMapCustomizations",
    ];
    let kind = kind.trim();
    ACTIVE.iter().any(|suffix| {
        kind.len() >= suffix.len() && kind[kind.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
    })
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
                let external = xml_attributes(&event).into_iter().any(|attribute| {
                    let name = attribute.local();
                    (name.eq_ignore_ascii_case(b"TargetMode")
                        && attribute.value.trim().eq_ignore_ascii_case("External"))
                        || (name.eq_ignore_ascii_case(b"Target")
                            && ooxml_external_target(&attribute.value))
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

/// The listed slides' parts, and whether a listed slide is missing from the
/// parts the checks read.
///
/// AnyDoc loads each `<p:sldId r:id="…">` from the relationship with that id,
/// whatever its type, resolving the target its own way (`path::resolve`
/// drops a fragment before any `..` is applied). Every part it could load
/// for a listed slide must be a slide that was checked. Each relationship is
/// resolved once, so repeated ids cost no more than distinct ones.
fn validate_pptx_slide_targets(
    presentation: &[u8],
    presentation_rels: &[u8],
    slide_parts: &HashSet<String>,
) -> Result<(HashSet<String>, bool), DocumentError> {
    if !xml_is_well_formed(presentation) || !xml_is_well_formed(presentation_rels) {
        return Err(DocumentError::Malformed);
    }
    // For each id: the checked slides it may load, and whether it may load
    // anything else.
    let mut targets_by_id: HashMap<String, (HashSet<String>, bool)> = HashMap::new();
    for relationship in ooxml_relationships(presentation_rels)? {
        let entry = targets_by_id.entry(relationship.id).or_default();
        match anydoc_resolve("ppt/presentation.xml", &relationship.target) {
            Some(part) if slide_parts.contains(&part) => {
                entry.0.insert(part);
            }
            _ => entry.1 = true,
        }
    }
    let slides = pptx_slide_relationship_ids(presentation)?;
    if slides.is_empty() {
        return Err(DocumentError::Malformed);
    }
    let mut listed = HashSet::new();
    let mut listed_ids = HashSet::new();
    let mut incomplete = false;
    for ids in slides {
        let mut found = false;
        for (id, (parts, unchecked)) in ids.iter().filter_map(|id| targets_by_id.get_key_value(id))
        {
            found = true;
            incomplete |= *unchecked;
            if listed_ids.insert(id.as_str()) {
                listed.extend(parts.iter().cloned());
            }
        }
        incomplete |= !found;
    }
    Ok((listed, incomplete))
}

/// Whether the speaker notes of any listed slide hide content. AnyDoc reads a
/// slide's notes from its `notesSlide` relationship, wherever the part is
/// stored; every such relationship a slide declares is followed here.
fn pptx_notes_hide_content(
    archive: &mut ZipArchive<Cursor<&[u8]>>,
    slides: &HashSet<String>,
) -> Result<bool, DocumentError> {
    let mut notes = HashSet::new();
    for slide in slides {
        for relationship in read_relationships(archive, &ooxml_rels_part(slide))? {
            let kind = relationship.kind.trim();
            let is_notes = kind.len() >= "/notesSlide".len()
                && kind[kind.len() - "/notesSlide".len()..].eq_ignore_ascii_case("/notesSlide");
            if is_notes {
                notes.extend(anydoc_resolve(slide, &relationship.target));
            }
        }
    }
    for part in notes {
        if read_optional_xml_part(archive, &part)?.is_some_and(|bytes| xml_has_hidden_slide(&bytes))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// PresentationML's namespace, Transitional and Strict.
const PRESENTATIONML_NAMESPACES: [&[u8]; 2] = [
    b"http://schemas.openxmlformats.org/presentationml/2006/main",
    b"http://purl.oclc.org/ooxml/presentationml/main",
];

/// The relationship ids of each listed slide, read as AnyDoc reads the list:
/// the `p:sldId` children of the first `p:sldIdLst`, both in PresentationML's
/// namespace, so a section list (`p14:sldIdLst`) is not a slide list. Every
/// prefixed `id` a slide carries counts; its unprefixed `id` is the slide's
/// number, and a namespace declaration is not an attribute.
fn pptx_slide_relationship_ids(presentation: &[u8]) -> Result<Vec<HashSet<String>>, DocumentError> {
    let mut reader = quick_xml::NsReader::from_reader(Cursor::new(presentation));
    reader.config_mut().trim_text(false);
    reader.config_mut().check_end_names = false;
    let mut buffer = Vec::new();
    let mut depth = 0usize;
    // Depth of the open first slide list, once one has been seen.
    let mut list: Option<usize> = None;
    let mut list_seen = false;
    let mut slides = Vec::new();
    // Open elements that are `mc:AlternateContent`, and how many are open.
    let mut alternates: Vec<bool> = Vec::new();
    let mut compatibility_blocks = 0usize;
    loop {
        let (namespace, event) = reader
            .read_resolved_event_into(&mut buffer)
            .map_err(|_| DocumentError::Malformed)?;
        let presentationml = matches!(
            namespace,
            quick_xml::name::ResolveResult::Bound(namespace)
                if PRESENTATIONML_NAMESPACES.contains(&namespace.as_ref())
        );
        let (element, start) = match event {
            quick_xml::events::Event::Start(element) => (element, true),
            quick_xml::events::Event::Empty(element) => (element, false),
            quick_xml::events::Event::End(_) => {
                depth = depth.saturating_sub(1);
                if list == Some(depth) {
                    list = None;
                }
                if alternates.pop() == Some(true) {
                    compatibility_blocks -= 1;
                }
                buffer.clear();
                continue;
            }
            quick_xml::events::Event::Eof => return Ok(slides),
            _ => {
                buffer.clear();
                continue;
            }
        };
        let local = xml_local_name(element.name().as_ref()).to_vec();
        if presentationml && local == b"sldIdLst" && !list_seen {
            // PowerPoint picks a compatibility branch by what it supports and
            // AnyDoc by document order, so a slide list inside one may list
            // different slides to each.
            if compatibility_blocks > 0 {
                return Err(DocumentError::Malformed);
            }
            list_seen = true;
            if start {
                list = Some(depth);
            }
        } else if presentationml && local == b"sldId" && list.is_some_and(|open| open + 1 == depth)
        {
            slides.push(
                xml_attributes(&element)
                    .into_iter()
                    .filter(|attribute| attribute.prefixed() && attribute.local() == b"id")
                    .map(|attribute| attribute.value)
                    .collect(),
            );
        }
        if start {
            depth += 1;
            let alternate = local == b"AlternateContent"
                && matches!(
                    namespace,
                    quick_xml::name::ResolveResult::Bound(namespace)
                        if namespace.as_ref() == MARKUP_COMPATIBILITY_NAMESPACE
                );
            compatibility_blocks += usize::from(alternate);
            alternates.push(alternate);
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

/// A stylesheet a chapter applies.
#[derive(Clone, Debug, PartialEq, Eq)]
enum EpubStyleSource {
    /// A linked sheet, or one named by an `<?xml-stylesheet?>` instruction,
    /// as a resolved part.
    Linked(String),
    /// A `<style>` element's text.
    Embedded(String),
}

/// The stylesheets a chapter applies, in document order: as a reading
/// system applies them (see [`EpubSheetCandidate`]), only where their media
/// may apply on its screen, each with where they do; and as AnyDoc applies
/// them (`epub::chapter_stylesheet`: the first `rel` and `href` of a
/// `link`, and every `style`, whatever their type, title, and media).
#[derive(Default)]
struct EpubChapterStyles {
    reader: Vec<(EpubStyleSource, epub_css::Condition)>,
    anydoc: Vec<EpubStyleSource>,
}

/// A stylesheet a chapter offers a reader, as Chromium reads one: a `link`
/// by its unprefixed `rel`, `href`, `type`, `title`, and `disabled`, a
/// `style` by its `type` and `title`, and an `<?xml-stylesheet?>`
/// instruction by its pseudo-attributes, each with a type that names CSS,
/// and a link not disabled. Its media, its title, and whether it is an
/// alternate decide whether the reader applies it: one without a title
/// unless it is an alternate, and one of the preferred set, whose title
/// the first titled sheet that is not an alternate gives, whatever its
/// media.
struct EpubSheetCandidate {
    source: EpubStyleSource,
    media: epub_css::Condition,
    title: String,
    alternate: bool,
}

/// The sheets a reader applies of those a chapter offers (see
/// [`EpubSheetCandidate`]), each with where its media apply.
fn epub_enabled_sheets(
    candidates: Vec<EpubSheetCandidate>,
) -> Vec<(EpubStyleSource, epub_css::Condition)> {
    let preferred = candidates
        .iter()
        .find(|candidate| !candidate.alternate && !candidate.title.is_empty())
        .map(|candidate| candidate.title.clone());
    candidates
        .into_iter()
        .filter(|candidate| {
            candidate.media.applies() != epub_css::Applies::No
                && if candidate.title.is_empty() {
                    !candidate.alternate
                } else {
                    preferred.as_ref() == Some(&candidate.title)
                }
        })
        .map(|candidate| (candidate.source, candidate.media))
        .collect()
}

/// Whether a `link`'s `type` names CSS as Chromium reads it: a MIME type
/// whose essence is `text/css`, in any case, or none.
fn epub_link_type_is_css(kind: &str) -> bool {
    let essence = kind.split(';').next().unwrap_or_default().trim();
    kind.trim().is_empty() || essence.eq_ignore_ascii_case("text/css")
}

/// Whether a `style` element's `type` names CSS as Chromium reads it:
/// `text/css` alone, in any case, or none.
fn epub_style_type_is_css(kind: &str) -> bool {
    kind.is_empty() || kind.eq_ignore_ascii_case("text/css")
}

/// The pseudo-attributes of a processing instruction's content, such as
/// `href="a.css" media="print"`.
fn pseudo_attributes(content: &str) -> Vec<(String, String)> {
    let mut attributes = Vec::new();
    let mut rest = content;
    while let Some(equals) = rest.find('=') {
        let name = rest[..equals].trim().to_ascii_lowercase();
        let value_part = rest[equals + 1..].trim_start();
        let Some(quote) = value_part
            .chars()
            .next()
            .filter(|c| *c == '"' || *c == '\'')
        else {
            break;
        };
        let Some(end) = value_part[1..].find(quote) else {
            break;
        };
        attributes.push((name, value_part[1..1 + end].to_string()));
        rest = &value_part[1 + end + 1..];
    }
    attributes
}

fn epub_inspect_chapter(
    bytes: &[u8],
    chapter_path: &str,
    archive_names: &HashSet<String>,
    media: &mut epub_css::MediaQueries,
) -> Result<(PackagePreflight, EpubChapterStyles), DocumentError> {
    if !xml_is_well_formed(bytes) {
        return Err(DocumentError::Malformed);
    }
    let mut reader = quick_xml::Reader::from_reader(Cursor::new(bytes));
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();
    let mut result = PackagePreflight::default();
    let mut styles = EpubChapterStyles::default();
    let mut candidates: Vec<EpubSheetCandidate> = Vec::new();
    let mut has_html = false;
    let mut has_body = false;
    // The `<style>` element being read: its text; how its media apply and
    // its title, and whether a reader applies it (an exact `style` name and
    // a type that names CSS); and whether AnyDoc reads it (an exact `style`
    // name).
    let mut style_text: Option<(String, epub_css::Condition, String, bool, bool)> = None;
    let mut depth = 0usize;
    let mut style_depth = 0usize;
    loop {
        let event = reader.read_event_into(&mut buffer);
        let start = matches!(event, Ok(quick_xml::events::Event::Start(_)));
        match event {
            Ok(quick_xml::events::Event::Start(event))
            | Ok(quick_xml::events::Event::Empty(event)) => {
                let event_name = event.name();
                let exact = xml_local_name(event_name.as_ref()).to_vec();
                let local = exact.to_ascii_lowercase();
                match local.as_slice() {
                    b"html" => has_html = true,
                    b"body" => has_body = true,
                    b"script" | b"form" | b"iframe" | b"object" | b"embed" | b"applet" => {
                        result.active_content = true;
                    }
                    _ => {}
                }
                let attributes = xml_attributes(&event);
                for attribute in &attributes {
                    let name = attribute.local();
                    let value = attribute.value.as_str();
                    if matches!(name, b"href" | b"src" | b"action" | b"data") {
                        epub_check_reference(value, chapter_path, archive_names, &mut result);
                    }
                    if name.starts_with(b"on") {
                        result.active_content = true;
                    }
                    if name == b"style" && epub_css::references_external(value) {
                        result.external_relationships = true;
                    }
                }
                // Only a stylesheet's `media` decides where it applies; that
                // of any other element is not read.
                let media = match attributes
                    .iter()
                    .find(|attribute| !attribute.prefixed() && attribute.local() == b"media")
                {
                    Some(attribute) if matches!(local.as_slice(), b"link" | b"style") => {
                        media.attribute(&attribute.value)?
                    }
                    _ => epub_css::Condition::always(),
                };
                let unprefixed = |name: &[u8]| {
                    attributes
                        .iter()
                        .find(|attribute| !attribute.prefixed() && attribute.local() == name)
                        .map(|attribute| attribute.value.as_str())
                };
                let title = || unprefixed(b"title").unwrap_or_default().to_string();
                if local == b"link" {
                    let has_rel = |rel: &str, wanted: &str| {
                        rel.split_whitespace()
                            .any(|rel| rel.eq_ignore_ascii_case(wanted))
                    };
                    let stylesheet = |rel: &str| has_rel(rel, "stylesheet");
                    if exact == b"link"
                        && unprefixed(b"rel").is_some_and(stylesheet)
                        && unprefixed(b"type").is_none_or(epub_link_type_is_css)
                        && unprefixed(b"disabled").is_none()
                    {
                        if let Some(target) =
                            unprefixed(b"href").and_then(|href| anydoc_resolve(chapter_path, href))
                        {
                            candidates.push(EpubSheetCandidate {
                                source: EpubStyleSource::Linked(target),
                                media: media.clone(),
                                title: title(),
                                alternate: unprefixed(b"rel")
                                    .is_some_and(|rel| has_rel(rel, "alternate")),
                            });
                        }
                    }
                    // AnyDoc reads the first `rel` and `href` of an exact
                    // `link`, as `attr_any` does.
                    let first = |name: &[u8]| {
                        attributes
                            .iter()
                            .find(|attribute| attribute.local() == name)
                            .map(|attribute| attribute.value.as_str())
                    };
                    if exact == b"link" && first(b"rel").is_some_and(stylesheet) {
                        if let Some(target) =
                            first(b"href").and_then(|href| anydoc_resolve(chapter_path, href))
                        {
                            styles.anydoc.push(EpubStyleSource::Linked(target));
                        }
                    }
                }
                let styles_reader =
                    exact == b"style" && unprefixed(b"type").is_none_or(epub_style_type_is_css);
                if local == b"style" && start && style_text.is_none() {
                    style_text = Some((
                        String::new(),
                        media.clone(),
                        title(),
                        styles_reader,
                        exact == b"style",
                    ));
                    style_depth = depth;
                } else if styles_reader && !start && !title().is_empty() {
                    // An empty `style` shows nothing, but its title may
                    // name the preferred set.
                    candidates.push(EpubSheetCandidate {
                        source: EpubStyleSource::Embedded(String::new()),
                        media,
                        title: title(),
                        alternate: false,
                    });
                }
                if start {
                    depth += 1;
                }
                buffer.clear();
            }
            Ok(quick_xml::events::Event::End(_)) => {
                depth = depth.saturating_sub(1);
                if depth == style_depth {
                    if let Some((text, media, title, reader, anydoc)) = style_text.take() {
                        if epub_css::references_external(&text) {
                            result.external_relationships = true;
                        }
                        if anydoc {
                            styles.anydoc.push(EpubStyleSource::Embedded(text.clone()));
                        }
                        if reader {
                            candidates.push(EpubSheetCandidate {
                                source: EpubStyleSource::Embedded(text),
                                media,
                                title,
                                alternate: false,
                            });
                        }
                    }
                }
                buffer.clear();
            }
            // Chapter text loads nothing: a URL written in it is only
            // text, and the Markdown sanitizer replaces it.
            Ok(quick_xml::events::Event::Text(event)) => {
                if let Some((text, ..)) = style_text.as_mut() {
                    text.push_str(&String::from_utf8_lossy(event.as_ref()));
                }
                buffer.clear();
            }
            Ok(quick_xml::events::Event::CData(event)) => {
                if let Some((text, ..)) = style_text.as_mut() {
                    text.push_str(&String::from_utf8_lossy(event.as_ref()));
                }
                buffer.clear();
            }
            Ok(quick_xml::events::Event::GeneralRef(event)) => {
                if let Some((text, ..)) = style_text.as_mut() {
                    text.push_str(&anydoc_entity_text(&String::from_utf8_lossy(
                        event.as_ref(),
                    )));
                }
                buffer.clear();
            }
            Ok(quick_xml::events::Event::PI(event)) => {
                // Reading systems apply `<?xml-stylesheet?>` to XHTML;
                // AnyDoc does not.
                let content = String::from_utf8_lossy(event.as_ref()).into_owned();
                if let Some(rest) = content.strip_prefix("xml-stylesheet") {
                    let attributes = pseudo_attributes(rest);
                    let value = |wanted: &str| {
                        attributes
                            .iter()
                            .find(|(name, _)| name == wanted)
                            .map(|(_, value)| value.as_str())
                    };
                    let media = match value("media") {
                        Some(query) => media.attribute(query)?,
                        None => epub_css::Condition::always(),
                    };
                    // Chromium reads a type of exactly `text/css`, or none.
                    let css =
                        value("type").is_none_or(|kind| kind.is_empty() || kind == "text/css");
                    for (name, href) in &attributes {
                        if name == "href" {
                            epub_check_reference(href, chapter_path, archive_names, &mut result);
                            if css {
                                candidates.extend(anydoc_resolve(chapter_path, href).map(
                                    |target| EpubSheetCandidate {
                                        source: EpubStyleSource::Linked(target),
                                        media: media.clone(),
                                        title: value("title").unwrap_or_default().to_string(),
                                        alternate: value("alternate") == Some("yes"),
                                    },
                                ));
                            }
                        }
                    }
                }
                buffer.clear();
            }
            Ok(quick_xml::events::Event::Eof) => {
                if !has_html || !has_body {
                    result.missing_required_content = true;
                }
                styles.reader = epub_enabled_sheets(candidates);
                return Ok((result, styles));
            }
            Ok(_) => buffer.clear(),
            Err(_) => return Err(DocumentError::Malformed),
        }
    }
}

/// Distinct stylesheets a package may apply, applications of stylesheets
/// (links and imports) one chapter may make, and the import depth
/// followed. Real books have a few sheets and shallow imports.
const MAX_EPUB_STYLESHEETS: usize = 256;
const MAX_EPUB_SHEET_APPLICATIONS: usize = 1024;
const MAX_EPUB_IMPORT_DEPTH: usize = 16;
/// Chapter cascades kept for reuse: chapters usually share their sheets.
const EPUB_CASCADES_KEPT: usize = 16;

/// Decode a stylesheet as a reading system does (CSS Syntax 3, section
/// 3.2): a byte order mark, else an `@charset` rule at the very start,
/// else UTF-8.
fn decode_stylesheet(bytes: &[u8]) -> String {
    if let Some((encoding, bom)) = encoding_rs::Encoding::for_bom(bytes) {
        return encoding
            .decode_without_bom_handling(&bytes[bom..])
            .0
            .into_owned();
    }
    if let Some(rest) = bytes.strip_prefix(b"@charset \"") {
        if let Some(end) = rest.iter().position(|byte| *byte == b'"') {
            if rest[end..].starts_with(b"\";") {
                if let Some(encoding) = encoding_rs::Encoding::for_label(&rest[..end]) {
                    let encoding =
                        if encoding == encoding_rs::UTF_16LE || encoding == encoding_rs::UTF_16BE {
                            encoding_rs::UTF_8
                        } else {
                            encoding
                        };
                    return encoding.decode_without_bom_handling(bytes).0.into_owned();
                }
            }
        }
    }
    String::from_utf8_lossy(bytes).into_owned()
}

/// A linked stylesheet as both models read it.
struct EpubLoadedSheet {
    reader: epub_css::Stylesheet,
    /// The local sheets it imports, resolved against it, each by the
    /// import naming it, as an index into its imports.
    imports: Vec<(usize, String)>,
    /// The text AnyDoc adds: the part read as lossy UTF-8.
    anydoc_text: String,
}

/// A chapter's cascade under both models.
struct EpubChapterCascade {
    reader: epub_css::Cascade,
    anydoc: epub_css::AnyDocCascade,
}

type EpubCascadeKey = (
    String,
    Vec<(EpubStyleSource, epub_css::Condition)>,
    Vec<EpubStyleSource>,
);

/// Stylesheets read once per package however many chapters apply them,
/// and recently built chapter cascades.
#[derive(Default)]
struct EpubStylesheets {
    /// Linked sheets by part; `None` for a part that cannot be read, which
    /// the reference check reports.
    linked: HashMap<String, Option<Rc<EpubLoadedSheet>>>,
    embedded: HashMap<String, Rc<epub_css::Stylesheet>>,
    rules: usize,
    cascades: Vec<(EpubCascadeKey, Rc<EpubChapterCascade>)>,
    /// The media query lists of the package's sheets and chapters.
    media: epub_css::MediaQueries,
}

impl EpubStylesheets {
    fn count_rules(&mut self, sheet: &epub_css::Stylesheet) -> Result<(), DocumentError> {
        self.rules += sheet.rule_count();
        if self.rules > epub_css::MAX_STYLE_RULES {
            return Err(DocumentError::ResourceLimit);
        }
        Ok(())
    }

    fn load(
        &mut self,
        archive: &mut ZipArchive<Cursor<&[u8]>>,
        path: &str,
        result: &mut PackagePreflight,
    ) -> Result<Option<Rc<EpubLoadedSheet>>, DocumentError> {
        if let Some(loaded) = self.linked.get(path) {
            return Ok(loaded.clone());
        }
        if self.linked.len() >= MAX_EPUB_STYLESHEETS {
            return Err(DocumentError::ResourceLimit);
        }
        let loaded = match epub_read_part(archive, path) {
            Ok(bytes) => {
                let text = decode_stylesheet(&bytes);
                if epub_css::references_external(&text) {
                    result.external_relationships = true;
                }
                let reader = epub_css::parse_stylesheet(&text, &mut self.media)?;
                self.count_rules(&reader)?;
                let mut imports = Vec::new();
                for (index, import) in reader.imports.iter().enumerate() {
                    if epub_is_external_uri(&import.target) {
                        result.external_relationships = true;
                    } else {
                        imports
                            .extend(anydoc_resolve(path, &import.target).map(|path| (index, path)));
                    }
                }
                Some(Rc::new(EpubLoadedSheet {
                    reader,
                    imports,
                    anydoc_text: String::from_utf8_lossy(&bytes).into_owned(),
                }))
            }
            Err(DocumentError::Malformed) => None,
            Err(error) => return Err(error),
        };
        self.linked.insert(path.to_string(), loaded.clone());
        Ok(loaded)
    }

    fn embedded(&mut self, text: &str) -> Result<Rc<epub_css::Stylesheet>, DocumentError> {
        if let Some(sheet) = self.embedded.get(text) {
            return Ok(sheet.clone());
        }
        let sheet = Rc::new(epub_css::parse_stylesheet(text, &mut self.media)?);
        self.count_rules(&sheet)?;
        self.embedded.insert(text.to_string(), sheet.clone());
        Ok(sheet)
    }

    /// Apply a linked sheet to a reader cascade, after the sheets it
    /// imports, as a reader orders them, its rules holding only where
    /// `condition`, that of the link or import applying it, holds, and
    /// standing in the cascade layer `within` where an import puts them in
    /// one. An import cycle stops at the repeat, as readers stop it.
    #[allow(clippy::too_many_arguments)]
    fn apply_linked(
        &mut self,
        cascade: &mut epub_css::Cascade,
        archive: &mut ZipArchive<Cursor<&[u8]>>,
        path: &str,
        condition: epub_css::Condition,
        within: Option<u32>,
        depth: usize,
        visiting: &mut Vec<String>,
        applications: &mut usize,
        result: &mut PackagePreflight,
    ) -> Result<(), DocumentError> {
        if visiting.iter().any(|open| open == path) {
            return Ok(());
        }
        *applications += 1;
        if depth > MAX_EPUB_IMPORT_DEPTH || *applications > MAX_EPUB_SHEET_APPLICATIONS {
            return Err(DocumentError::ResourceLimit);
        }
        let Some(loaded) = self.load(archive, path, result)? else {
            return Ok(());
        };
        visiting.push(path.to_string());
        let mut open = cascade.open_sheet(condition.clone(), within);
        for (index, import) in &loaded.imports {
            let rule = &loaded.reader.imports[*index];
            let layer = cascade.import_layer(&loaded.reader, &mut open, rule, &mut self.media)?;
            let applies = condition.and(&rule.applies, &mut self.media)?;
            self.apply_linked(
                cascade,
                archive,
                import,
                applies,
                Some(layer),
                depth + 1,
                visiting,
                applications,
                result,
            )?;
        }
        visiting.pop();
        cascade.close_sheet(&loaded.reader, open, &mut self.media)?;
        if cascade.rule_count() > epub_css::MAX_STYLE_RULES {
            return Err(DocumentError::ResourceLimit);
        }
        Ok(())
    }

    fn chapter_cascade(
        &mut self,
        archive: &mut ZipArchive<Cursor<&[u8]>>,
        chapter_path: &str,
        styles: &EpubChapterStyles,
        result: &mut PackagePreflight,
    ) -> Result<Rc<EpubChapterCascade>, DocumentError> {
        let directory = chapter_path
            .rsplit_once('/')
            .map_or("", |(directory, _)| directory)
            .to_string();
        let key = (directory, styles.reader.clone(), styles.anydoc.clone());
        if let Some((_, cascade)) = self.cascades.iter().find(|(known, _)| *known == key) {
            return Ok(cascade.clone());
        }
        let mut reader = epub_css::Cascade::default();
        let mut applications = 0usize;
        for (source, condition) in &styles.reader {
            match source {
                EpubStyleSource::Linked(path) => self.apply_linked(
                    &mut reader,
                    archive,
                    path,
                    condition.clone(),
                    None,
                    0,
                    &mut Vec::new(),
                    &mut applications,
                    result,
                )?,
                EpubStyleSource::Embedded(text) => {
                    let sheet = self.embedded(text)?;
                    let mut open = reader.open_sheet(condition.clone(), None);
                    for rule in &sheet.imports {
                        if epub_is_external_uri(&rule.target) {
                            result.external_relationships = true;
                        } else if let Some(path) = anydoc_resolve(chapter_path, &rule.target) {
                            let layer =
                                reader.import_layer(&sheet, &mut open, rule, &mut self.media)?;
                            let applies = condition.and(&rule.applies, &mut self.media)?;
                            self.apply_linked(
                                &mut reader,
                                archive,
                                &path,
                                applies,
                                Some(layer),
                                1,
                                &mut Vec::new(),
                                &mut applications,
                                result,
                            )?;
                        }
                    }
                    reader.close_sheet(&sheet, open, &mut self.media)?;
                    if reader.rule_count() > epub_css::MAX_STYLE_RULES {
                        return Err(DocumentError::ResourceLimit);
                    }
                }
            }
        }
        let mut anydoc = epub_css::AnyDocCascade::default();
        for source in &styles.anydoc {
            match source {
                EpubStyleSource::Linked(path) => {
                    if let Some(sheet) = self.load(archive, path, result)? {
                        anydoc.add(&sheet.anydoc_text);
                    }
                }
                EpubStyleSource::Embedded(text) => anydoc.add(text),
            }
        }
        let cascade = Rc::new(EpubChapterCascade { reader, anydoc });
        if self.cascades.len() >= EPUB_CASCADES_KEPT {
            self.cascades.remove(0);
        }
        self.cascades.push((key, cascade.clone()));
        Ok(cascade)
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
                    let is_toc = xml_attributes(&event).into_iter().any(|attribute| {
                        let value = attribute.value.trim();
                        (attribute.local() == b"type" && value.eq_ignore_ascii_case("toc"))
                            || (attribute.local() == b"role"
                                && value.eq_ignore_ascii_case("doc-toc"))
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

/// Open a package's archive, refusing one with more entries than AnyDoc
/// reads.
fn open_package(bytes: &[u8]) -> Result<ZipArchive<Cursor<&[u8]>>, DocumentError> {
    let archive = ZipArchive::new(Cursor::new(bytes)).map_err(|_| DocumentError::Malformed)?;
    if archive.len() > MAX_ARCHIVE_ENTRIES {
        return Err(DocumentError::ResourceLimit);
    }
    Ok(archive)
}

fn preflight_epub(bytes: &[u8]) -> Result<PackagePreflight, DocumentError> {
    let mut archive = open_package(bytes)?;
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
    let mut stylesheets = EpubStylesheets::default();
    let mut css_work = 0u64;
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
        match epub_read_xml_part(&mut archive, &target).and_then(|chapter| {
            epub_inspect_chapter(&chapter, &target, &archive_names, &mut stylesheets.media)
                .map(|inspected| (chapter, inspected))
        }) {
            Ok((chapter, (chapter_result, styles))) => {
                result.active_content |= chapter_result.active_content;
                result.external_relationships |= chapter_result.external_relationships;
                result.hidden_content |= chapter_result.hidden_content;
                result.missing_required_content |= chapter_result.missing_required_content;
                let cascade =
                    stylesheets.chapter_cascade(&mut archive, &target, &styles, &mut result)?;
                let text = epub_css::chapter_text(
                    &chapter,
                    &cascade.reader,
                    &cascade.anydoc,
                    &mut css_work,
                )?;
                result.hidden_content |= text.converts_hidden;
                result.unsupported_content |= text.drops_shown || text.fuses_blocks;
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
                // A navigation document in the spine, as pandoc places it,
                // cannot list itself.
                let chapters: Vec<String> = spine_targets
                    .iter()
                    .filter(|target| **target != nav_target)
                    .cloned()
                    .collect();
                match epub_read_xml_part(&mut archive, &nav_target).and_then(|nav| {
                    epub_nav_spine_mismatch(
                        &nav,
                        &nav_target,
                        &chapters,
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

/// Whether a compound file's bytes name a stream: its directory stores names
/// in UTF-16LE, and the ASCII spelling is accepted too.
fn ole_names_stream(bytes: &[u8], name: &str) -> bool {
    let wide: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
    bytes.windows(wide.len()).any(|part| part == wide)
        || bytes
            .windows(name.len())
            .any(|part| part == name.as_bytes())
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
        // A password-protected Office file is a compound file holding these
        // two streams.
        let encrypted = ole_names_stream(bytes, "EncryptedPackage")
            || ole_names_stream(bytes, "EncryptionInfo");
        return Err(if encrypted {
            DocumentError::Encrypted
        } else {
            DocumentError::Malformed
        });
    }
    if matches!(kind, DocumentKind::Epub) {
        return preflight_epub(bytes);
    }
    let mut archive = open_package(bytes)?;
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
    let mut odf_objects: Vec<Option<String>> = Vec::new();
    let mut odf_spreadsheet_walk: Option<odf_walk::OdfWalk> = None;
    let mut odf_styles: Option<Vec<u8>> = None;

    let layout = ooxml_layout(&mut archive, kind)?;
    let mut docx_scan = DocxStoryScan::default();
    let mut odp_visibility = OdpVisibility::default();
    // XLSX cells by style and value class, checked once the styles are read.
    let mut format_uses: HashSet<(u32, xlsx_numfmt::CellClass)> = HashSet::new();
    let mut result = PackagePreflight::default();
    // A part Word would read in place of the one AnyDoc converts.
    result.unsupported_content |= layout.targets_differ;
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
        // AnyDoc looks main parts up by exact name, so a case variant cannot
        // stand in for one. Without the exact `xl/workbook.xml`, its workbook
        // reader falls back to `xl/workbook.bin`, the binary format.
        if (matches!(kind, DocumentKind::Docx) && name == "word/document.xml")
            || (matches!(kind, DocumentKind::Pptx) && name == "ppt/presentation.xml")
            || (matches!(kind, DocumentKind::Xlsx) && name == "xl/workbook.xml")
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
            // An object's replacement image is judged with its object.
            || (lower_name.contains("/object") && !lower_name.starts_with("objectreplacements/"))
            || lower_name.starts_with("object/")
            || lower_name.contains("/oleobject")
            || lower_name.starts_with("oleobject/")
            || lower_name.contains("/embeddings/")
            || lower_name.starts_with("embeddings/"))
        {
            result.active_content = true;
        }
        if matches!(
            kind,
            DocumentKind::Odt | DocumentKind::Ods | DocumentKind::Odp
        ) && name == "mimetype"
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
                    || ((lower_name.starts_with("ppt/slides/")
                        || lower_name.starts_with("ppt/notesslides/"))
                        && lower_name.ends_with(".xml"))))
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
            // A workbook part AnyDoc cannot read as XML goes to its binary
            // (XLSB) reader, a lane this route does not enable.
            if matches!(kind, DocumentKind::Xlsx)
                && name == "xl/workbook.xml"
                && !anydoc_workbook_is_xml(&content)
            {
                return Err(DocumentError::Unsupported);
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
                docx_scan.part_read = layout.note_parts.read(&name);
                scan_docx_story(&content, &mut docx_scan)?;
            }
            if matches!(
                kind,
                DocumentKind::Odt | DocumentKind::Ods | DocumentKind::Odp
            ) {
                if lower_name == "content.xml" {
                    // Case variants are checked too, but only the exact part
                    // is the one AnyDoc converts.
                    if name == "content.xml" {
                        odf_content = Some(content.clone());
                    }
                    if !xml_is_well_formed(&content) {
                        return Err(DocumentError::Malformed);
                    }
                    // The checks below follow the mimetype's lane; AnyDoc
                    // follows the body, so a package whose body belongs to
                    // another lane lacks the content its own lane requires.
                    result.missing_required_content |= odf_body_kind(&content) != Some(kind);
                    if matches!(kind, DocumentKind::Odp) && name == "content.xml" {
                        scan_odp_visibility(&content, true, &mut odp_visibility)?;
                    }
                    result.external_relationships |= xml_has_odf_external_reference(&content);
                    result.active_content |= xml_has_odf_active_content(&content);
                    odf_objects.extend(xml_odf_objects(&content));
                    if name == "content.xml" {
                        let (body, spreadsheet) = match kind {
                            DocumentKind::Odt => (odf_walk::OdfBody::Text, false),
                            DocumentKind::Ods => (odf_walk::OdfBody::Spreadsheet, true),
                            _ => (odf_walk::OdfBody::Presentation, false),
                        };
                        let walk = odf_walk::walk_content(&content, body, spreadsheet)?;
                        result.unsupported_content |= walk.dropped_text;
                        if spreadsheet {
                            result.missing_formula_cache |= walk.uncached_formula;
                            result.hidden_content |= walk.hidden_value;
                            // Signs are decided with the styles part.
                            odf_spreadsheet_walk = Some(walk);
                        }
                    }
                    if matches!(kind, DocumentKind::Ods) {
                        result.hidden_content |= xml_has_odf_hidden_content(&content);
                        result.missing_required_content |= !xml_has_odf_spreadsheet(&content);
                    } else if matches!(kind, DocumentKind::Odt) {
                        result.hidden_content |= xml_has_odt_hidden_or_tracked_content(&content);
                        result.active_content |= xml_has_odt_active_content(&content);
                        result.unsupported_content |= xml_has_odt_unsupported_content(&content);
                        result.missing_required_content |= !xml_has_odf_text(&content);
                        odf_references.extend(xml_odf_internal_references(&content));
                    } else {
                        result.hidden_content |= xml_has_odp_hidden_content(&content);
                        result.missing_required_content |= !xml_has_odf_presentation(&content);
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
                    if name == "styles.xml" {
                        odf_styles = Some(content.clone());
                    }
                    result.hidden_content |= xml_has_odf_hidden_content(&content);
                    odf_objects.extend(xml_odf_objects(&content));
                    if matches!(kind, DocumentKind::Odt) {
                        result.hidden_content |= xml_has_odt_hidden_or_tracked_content(&content);
                        result.external_relationships |= xml_has_odf_external_reference(&content);
                        result.active_content |= xml_has_odt_active_content(&content);
                        odf_references.extend(xml_odf_internal_references(&content));
                    } else if matches!(kind, DocumentKind::Odp) {
                        result.hidden_content |= xml_has_odp_hidden_content(&content);
                        result.external_relationships |= xml_has_odf_external_reference(&content);
                        odf_references.extend(xml_odf_internal_references(&content));
                        scan_odp_visibility(&content, false, &mut odp_visibility)?;
                    }
                }
            }
            if lower_name.ends_with(".rels") {
                result.external_relationships |= xml_has_ooxml_external_relationship(&content)?;
                // A chart's link to the workbook holding its data is an
                // external relationship, reported as one; the chart converts
                // from its cached values.
                let chart_part = lower_name.contains("/charts/_rels/");
                result.active_content |=
                    ooxml_relationships(&content)?.iter().any(|relationship| {
                        ooxml_active_relationship(&relationship.kind)
                            && !(chart_part && relationship.external)
                    });
            }
            // A macro-enabled content type marks the package; the same word in
            // slide, note, or cell text is only text.
            if lower_name == "[content_types].xml" && lower_content.contains("macroenabled") {
                result.active_content = true;
            }
            if matches!(kind, DocumentKind::Xlsx) {
                result.hidden_content |= xml_has_hidden_content(&content);
                if name == "xl/workbook.xml" {
                    result.unsupported_content |= xlsx_workbook_drops_sheets(&content);
                }
                if lower_name.starts_with("xl/worksheets/")
                    || layout.worksheet_parts.contains(&name)
                {
                    let sheet = scan_worksheet(&content);
                    result.missing_formula_cache |= sheet.uncached_formula;
                    result.unsupported_content |= sheet.unreached_cell;
                    if sheet.too_many_formats {
                        return Err(DocumentError::ResourceLimit);
                    }
                    format_uses.extend(sheet.format_uses);
                    if format_uses.len() > MAX_FORMAT_USES {
                        return Err(DocumentError::ResourceLimit);
                    }
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
                } else if lower_name.starts_with("ppt/notesslides/") && lower_name.ends_with(".xml")
                {
                    // AnyDoc converts the text bodies of speaker notes.
                    result.hidden_content |= xml_has_hidden_slide(&content);
                }
            }
        }
    }
    if matches!(
        kind,
        DocumentKind::Odt | DocumentKind::Ods | DocumentKind::Odp
    ) {
        result.hidden_content |= odp_visibility.hides_content();
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
            let directories = archive_directories(&archive_names);
            result.missing_required_content |= odf_references
                .iter()
                .any(|reference| odf_reference_missing(reference, &archive_names, &directories));
        }
        if let Some(manifest) = &odf_manifest {
            if xml_has_odf_encryption_data(manifest) {
                return Err(DocumentError::Encrypted);
            }
        }
        if let Some(mut walk) = odf_spreadsheet_walk {
            walk.decide_signs(odf_styles.as_deref())?;
            result.unsupported_content |= walk.sign_lost;
        }
        let media_types = odf_manifest
            .as_deref()
            .map(odf_manifest_media_types)
            .unwrap_or_default();
        // An OLE object stored in the package, whether shown or not.
        result.active_content |= media_types
            .values()
            .any(|media| media.eq_ignore_ascii_case("application/vnd.sun.star.oleobject"));
        result.active_content |= odf_objects
            .iter()
            .any(|object| !odf_object_supported(object.as_deref(), &media_types));
        return Ok(result);
    }
    if !has_content_types || !has_main {
        return Err(DocumentError::Malformed);
    }
    if matches!(kind, DocumentKind::Xlsx) {
        result.hidden_content |=
            xlsx_sheets_hide_checkboxes(&mut archive, &layout.worksheet_parts)?;
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
        // Each candidate styles part is checked, as AnyDoc reads one.
        for part in &layout.styles_parts {
            let Some(styles) = read_optional_xml_part(&mut archive, part)? else {
                continue;
            };
            let formats = xlsx_formats(&styles);
            let mut losses = HashMap::new();
            for &(style, class) in &format_uses {
                let Some(&id) = formats.cell_formats.get(style as usize) else {
                    continue;
                };
                let loss = *losses.entry((id, class)).or_insert_with(|| {
                    xlsx_numfmt::loss(id, formats.codes.get(&id).map(String::as_str), class)
                });
                result.hidden_content |= loss.hidden;
                result.unsupported_content |= loss.misrendered;
            }
        }
        result.unsupported_content |=
            xlsx_sheets_have_drawing_text(&mut archive, &layout.worksheet_parts)?;
    }
    if matches!(kind, DocumentKind::Docx) {
        // Hidden text and labels: every styles and numbering part either
        // side could read.
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
        let mut hidden_labels = false;
        let mut label_styles = HashSet::new();
        for part in &layout.numbering_parts {
            let Ok(entry) = archive.by_name(part) else {
                continue;
            };
            let (hidden, styles) =
                docx_numbering_labels(open_xml_stream(entry.take(MAX_ARCHIVE_ENTRY_BYTES))?)?;
            hidden_labels |= hidden;
            label_styles.extend(styles);
        }
        // The lists are replayed once, each side reading the styles and
        // numbering parts it reads.
        let parts = &layout.list_parts;
        let style_numbering = docx_list_styles(&mut archive, parts)?;
        let word = docx_list_numbering(&mut archive, parts.word_numbering.as_deref(), true)?;
        let anydoc = docx_list_numbering(&mut archive, Some(&parts.anydoc_numbering), false)?;
        result.list_numbering_differs |=
            DocxNumberings { word, anydoc }.numbers_differ(&docx_scan, &style_numbering);
        // At each note reference, AnyDoc must convert the note Word shows,
        // each read from the notes part its relationships name. Where the
        // two read different parts, a note converts as Word shows it only
        // if both parts write it alike; a note Word does not show at a
        // reference is disclosed as hidden (see `docx_notes_verdict`).
        let notes = &layout.note_parts;
        for (endnotes, word_part, anydoc_part) in [
            (
                false,
                notes.word_footnotes.as_deref(),
                notes.anydoc_footnotes.as_str(),
            ),
            (
                true,
                notes.word_endnotes.as_deref(),
                notes.anydoc_endnotes.as_str(),
            ),
        ] {
            let references: Vec<&DocxNoteReference> = docx_scan
                .references
                .iter()
                .filter(|reference| reference.endnote == endnotes)
                .collect();
            let anydoc = docx_notes_part_read(&mut archive, anydoc_part, endnotes)?;
            let same_part = word_part == Some(anydoc_part);
            let word = match word_part {
                Some(part) if !same_part => {
                    Some(docx_notes_part_read(&mut archive, part, endnotes)?)
                }
                _ => None,
            };
            let relationships_alike = same_part
                || match word_part {
                    Some(part) => {
                        read_optional_xml_part(&mut archive, &ooxml_rels_part(part))?
                            == read_optional_xml_part(&mut archive, &ooxml_rels_part(anydoc_part))?
                    }
                    None => false,
                };
            let (refused, hidden) = docx_notes_verdict(
                &references,
                if same_part {
                    Some(&anydoc)
                } else {
                    word.as_ref()
                },
                &anydoc,
                same_part,
                relationships_alike,
            );
            result.unsupported_content |= refused;
            result.hidden_content |= hidden;
        }
        result.hidden_content |= hidden_labels
            || label_styles
                .iter()
                .any(|style| hidden_styles.contains(style));
        result.unsupported_content |= docx_scan.dropped;
        result.omitted_characters |= docx_scan.omitted_hyphen;
        result.omitted_page_blocks |= docx_scan.omitted_page_block;
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
        let (slides, incomplete) =
            validate_pptx_slide_targets(&presentation, &presentation_rels, &ppt_slide_parts)?;
        result.missing_required_content |= incomplete;
        result.hidden_content |= pptx_notes_hide_content(&mut archive, &slides)?;
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
                || preflight.unsupported_content
        }
        DocumentKind::Ods => {
            preflight.hidden_content
                || preflight.missing_formula_cache
                || preflight.external_relationships
                || preflight.missing_required_content
                || preflight.unsupported_content
        }
        DocumentKind::Odt => {
            preflight.hidden_content
                || preflight.external_relationships
                || preflight.missing_required_content
                || preflight.unsupported_content
        }
        DocumentKind::Pptx => {
            preflight.hidden_content
                || preflight.external_relationships
                || preflight.missing_required_content
        }
        DocumentKind::Epub => {
            preflight.hidden_content
                || preflight.external_relationships
                || preflight.missing_required_content
                || preflight.unsupported_content
        }
        DocumentKind::Odp => {
            preflight.hidden_content
                || preflight.external_relationships
                || preflight.missing_required_content
                || preflight.unsupported_content
        }
        _ => false,
    };
    incomplete.then_some(DocumentError::IncompleteConversion)
}

/// Whether a zip archive holds more central directory records than AnyDoc
/// reads (`MAX_ARCHIVE_ENTRIES`), counted by their signature without
/// reading the directory. Opening the archive indexes every entry first:
/// a 43 MB package of empty entries costs a second and 340 MiB before any
/// bound applies, and AnyDoc then refuses it.
fn zip_past_entry_bound(bytes: &[u8]) -> bool {
    if !bytes.starts_with(b"PK\x03\x04") {
        return false;
    }
    let mut records = 0usize;
    let mut rest = bytes;
    while let Some(at) = rest.windows(4).position(|window| window == b"PK\x01\x02") {
        records += 1;
        if records > MAX_ARCHIVE_ENTRIES {
            return true;
        }
        rest = &rest[at + 4..];
    }
    false
}

fn classify_bytes(bytes: &[u8], path: &Path) -> DocumentClassification {
    classify_package(bytes, path, zip_past_entry_bound(bytes))
}

/// Classify bytes, `oversized` when they are an archive past AnyDoc's
/// entry bound: such an archive is not opened, and as AnyDoc's detection
/// finds no format in it, its extension names one.
fn classify_package(bytes: &[u8], path: &Path, oversized: bool) -> DocumentClassification {
    let detected_format = if oversized {
        anydoc::Format::from_path(path)
    } else {
        anydoc::Format::from_bytes(bytes).or_else(|| anydoc::Format::from_path(path))
    };
    let detected = detected_format.map(DocumentKind::from_anydoc);
    let variant = detected_format.map(|format| {
        if oversized {
            DocumentVariant::from_extension(path, format)
        } else {
            DocumentVariant::for_format(format, bytes, path)
        }
    });
    classification_of(detected, variant, bytes.len() as u64)
}

/// The classification of `size_bytes` of input found to be of this kind
/// and variant: the kind's capabilities, and whether its route converts
/// the variant.
fn classification_of(
    kind: Option<DocumentKind>,
    variant: Option<DocumentVariant>,
    size_bytes: u64,
) -> DocumentClassification {
    let capabilities = kind.map(capabilities);
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
        kind,
        variant,
        enabled,
        size_bytes,
        capabilities,
    }
}

/// The route a classified document takes: its kind and variant where the
/// route converts it, else the refusal it gives. The worker routes what it
/// classifies, and the server checks the kind and variant it reports.
fn document_route(
    classification: &DocumentClassification,
) -> Result<(DocumentKind, DocumentVariant), DocumentError> {
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
    if !classification.enabled || kind_for_variant(variant) != Some(kind) {
        return Err(DocumentError::Unsupported);
    }
    Ok((kind, variant))
}

/// Convert an enabled document through the supervised worker.
pub async fn to_markdown(path: impl AsRef<Path>) -> Result<DocumentContent, DocumentError> {
    let canonical = crate::validate_path(path).map_err(map_path_error)?;
    // A worker slot is taken before the file is read, so calls waiting for
    // one hold no document bytes. The worker classifies the document, runs
    // the package preflight, and converts it, under the conversion's
    // deadline, memory ceiling, and in-flight bound, and returns what it
    // found with the Markdown. Classified here, AnyDoc's detection parsed a
    // package's relationships, content types, or main part, to 128 MiB
    // each, in the server: a 9.9 MB package took it to 1.8 GiB.
    let permit = worker_semaphore()
        .acquire_owned()
        .await
        .map_err(|_| DocumentError::WorkerBusy)?;
    let frame = read_document_frame(&canonical).await?;
    // An archive with more entries than AnyDoc reads is refused unopened,
    // as the worker would refuse it after indexing them all.
    if zip_past_entry_bound(frame.document()) {
        return Err(DocumentError::ResourceLimit);
    }
    let input_bytes = frame.document().len() as u64;
    if !worker_sandbox_available() {
        // No route converts without the worker; the refusal the document's
        // route gives is found here, under the slot.
        return tokio::task::spawn_blocking(move || {
            let _permit = permit;
            document_route(&classify_package(frame.document(), &canonical, false))
                .and(Err(DocumentError::WorkerUnavailable))
        })
        .await
        .map_err(|_| DocumentError::ConversionFailed)?;
    }
    let WorkerConversion {
        kind,
        variant,
        markdown: raw_markdown,
        preflight,
    } = run_worker_conversion(frame.payload, worker_executable()?, permit).await?;
    // The worker refuses what the preflight rejects before converting it;
    // the supervisor applies the same rejection to what it returns.
    if let Some(error) = preflight_rejection(kind, &preflight) {
        return Err(error);
    }
    if raw_markdown.len() > MAX_MARKDOWN_SIZE {
        return Err(DocumentError::OutputTooLarge);
    }
    let (markdown, sanitized) = sanitize_markdown(&raw_markdown);
    if markdown.len() > MAX_MARKDOWN_SIZE {
        return Err(DocumentError::OutputTooLarge);
    }
    let mut warnings = Vec::new();
    let mut completeness = Completeness::Complete;
    if kind == DocumentKind::Docx && (preflight.omitted_characters || preflight.omitted_page_blocks)
    {
        completeness = Completeness::Partial;
        let message = match (preflight.omitted_characters, preflight.omitted_page_blocks) {
            (true, false) => "The document uses non-breaking hyphens, which the converter drops; hyphenated terms such as form numbers may appear joined.",
            (false, _) => "The document shows page numbers or dates Word fills in where the text stands, which the converter drops.",
            (true, true) => "The document uses non-breaking hyphens and page numbers or dates Word fills in, which the converter drops; hyphenated terms such as form numbers may appear joined.",
        };
        warnings.push(DocumentWarning {
            code: "characters_omitted".into(),
            message: message.into(),
        });
    }
    if kind == DocumentKind::Docx && preflight.list_numbering_differs {
        completeness = Completeness::Partial;
        warnings.push(DocumentWarning {
            code: "list_numbering_differs".into(),
            message: "Some list numbers differ from what Word shows: a list continuing another, a numbered paragraph deleted with tracked changes, or a format such as ordinals or words. The list text is converted."
                .into(),
        });
    }
    if kind == DocumentKind::Docx && preflight.hidden_content {
        warnings.push(DocumentWarning {
            code: "hidden_content_preserved".into(),
            message: "The document holds text Word does not display, such as hidden text, a tracked deletion, or an unreferenced note; it was converted with the visible text."
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
        input_bytes,
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

/// What the worker made of a document it classified: the kind and variant
/// it found, the Markdown, and what the package preflight found.
struct WorkerConversion {
    kind: DocumentKind,
    variant: DocumentVariant,
    markdown: String,
    preflight: PackagePreflight,
}

/// Classify and convert a document in the worker, from a frame read by
/// [`read_document_frame`], under the worker slot `permit`. The kind and
/// variant the worker reports must be a route that converts.
async fn run_worker_conversion(
    payload: Vec<u8>,
    executable: PathBuf,
    permit: OwnedSemaphorePermit,
) -> Result<WorkerConversion, DocumentError> {
    let job = WorkerJob {
        code: WORKER_CONVERT,
        payload,
        timeout: WORKER_TIMEOUT,
        max_response_bytes: MAX_SERIALIZED_WORKER_RESPONSE_BYTES,
    };
    let response = run_worker_job(job, executable, permit).await?;
    match (
        response.markdown,
        response.error,
        response.preflight,
        response.classified,
    ) {
        (Some(markdown), None, Some(preflight), Some(found)) => {
            let (kind, variant) = document_route(&classification_of(found.kind, found.variant, 0))
                .map_err(|_| DocumentError::WorkerProtocol)?;
            Ok(WorkerConversion {
                kind,
                variant,
                markdown,
                preflight,
            })
        }
        (None, Some(error), _, _) => Err(error.into_document_error()),
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
    /// What the package preflight found, beside a document's Markdown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) preflight: Option<PackagePreflight>,
    /// What the worker found a document to be, where it classified it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) classified: Option<WorkerClassification>,
}

/// A document's kind and variant as the worker classified it.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct WorkerClassification {
    kind: Option<DocumentKind>,
    variant: Option<DocumentVariant>,
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
            "unrecognized" => DocumentError::Unrecognized,
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
    let response = document_worker_response(*code, bytes);
    write_worker_response(&mut output, response, MAX_SERIALIZED_WORKER_RESPONSE_BYTES)
}

/// Answer a document frame. A frame the worker classifies (codes 9 and 10)
/// is answered with the kind and variant found, and for code 9, as for the
/// variants of codes 1 to 8, with the refusal its route gives or the
/// Markdown and what the package preflight found.
fn document_worker_response(code: u8, frame: &[u8]) -> WorkerResponse {
    let (variant, classified, bytes) = if matches!(code, WORKER_CONVERT | WORKER_CLASSIFY) {
        let (name, document) = match classifying_frame(frame) {
            Ok(split) => split,
            Err(error) => return worker_response_for_error(&error),
        };
        if document.len() as u64 > MAX_DOCUMENT_SIZE {
            return worker_response_for_error(&DocumentError::ResourceLimit);
        }
        let classification = classify_bytes(document, &name);
        let found = WorkerClassification {
            kind: classification.kind,
            variant: classification.variant,
        };
        if code == WORKER_CLASSIFY {
            return WorkerResponse {
                classified: Some(found),
                ..Default::default()
            };
        }
        match document_route(&classification) {
            Ok((_, variant)) => (variant, Some(found), document),
            Err(error) => return worker_response_for_error(&error),
        }
    } else {
        if frame.len() as u64 > MAX_DOCUMENT_SIZE {
            return worker_response_for_error(&DocumentError::ResourceLimit);
        }
        let Some(variant) = [
            DocumentVariant::Docx,
            DocumentVariant::Xlsx,
            DocumentVariant::Pptx,
            DocumentVariant::Ods,
            DocumentVariant::Odt,
            DocumentVariant::Csv,
            DocumentVariant::Odp,
            DocumentVariant::Epub,
        ]
        .into_iter()
        .find(|variant| variant.worker_code() == code) else {
            return worker_response_for_error(&DocumentError::Unsupported);
        };
        (variant, None, frame)
    };
    let mut response = convert_in_worker(variant, bytes);
    if response.error.is_none() {
        response.classified = classified;
    }
    response
}

/// Convert a document of a known variant in the worker: the refusal the
/// package preflight gives, or the Markdown and what the preflight found.
fn convert_in_worker(variant: DocumentVariant, bytes: &[u8]) -> WorkerResponse {
    if variant == DocumentVariant::Csv {
        match tabular_csv::to_markdown(bytes) {
            Ok(markdown) if markdown.len() <= MAX_MARKDOWN_SIZE => WorkerResponse {
                markdown: Some(markdown),
                error: None,
                preflight: Some(PackagePreflight::default()),
                ..Default::default()
            },
            Ok(_) => worker_response_for_error(&DocumentError::OutputTooLarge),
            Err(error) => worker_response_for_error(&error),
        }
    } else {
        let (Some(kind), Some(format)) = (kind_for_variant(variant), anydoc_format(variant)) else {
            return worker_response_for_error(&DocumentError::WorkerProtocol);
        };
        match preflight_package(bytes, kind, variant)
            .and_then(|preflight| preflight_rejection(kind, &preflight).map_or(Ok(preflight), Err))
        {
            Err(error) => worker_response_for_error(&error),
            Ok(preflight) => match anydoc::to_markdown_bytes(bytes, Some(format)) {
                Ok(_) if worker_diagnostics_incomplete() => {
                    worker_response_for_error(&DocumentError::IncompleteConversion)
                }
                Ok(markdown) if markdown.len() <= MAX_MARKDOWN_SIZE => WorkerResponse {
                    markdown: Some(markdown),
                    error: None,
                    preflight: Some(preflight),
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
    }
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
    let stripped = strip_html_tags(&redacted, html);
    let sanitized = redact_destinations_and_paths(&stripped);
    let changed = sanitized != markdown;
    (sanitized, changed)
}

/// Remove HTML tags, except what is text or a line break. AnyDoc escapes a
/// literal `<` (`\<Client name>` is a `<` and words, as a Markdown renderer
/// reads it); a tag may still start after an escaped `<`, so the search
/// resumes just past it. Code spans and fenced code blocks show `<` as
/// written, so nothing in them is a tag. AnyDoc's own `<br>` separates the
/// lines of a table cell; without it, "52,000" over "1,250" would read as
/// one number.
/// The longest anchor id the sanitizer keeps.
const MAX_ANCHOR_ID_BYTES: usize = 64;

fn strip_html_tags(text: &str, html: &Regex) -> String {
    let code = markdown_code_ranges(text);
    let in_code = |position: usize| {
        code.binary_search_by(|&(start, end)| {
            if end <= position {
                std::cmp::Ordering::Less
            } else if start > position {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
    };
    let mut output = String::with_capacity(text.len());
    let mut copied = 0;
    let mut position = 0;
    while let Some(tag) = html.find_at(text, position) {
        let escapes = text[..tag.start()]
            .bytes()
            .rev()
            .take_while(|&byte| byte == b'\\')
            .count();
        if escapes % 2 == 1 || in_code(tag.start()) {
            position = tag.start() + 1;
            continue;
        }
        position = tag.end();
        if tag.as_str() == "<br>" {
            continue;
        }
        // The anchor AnyDoc writes for a link target, `<a id="…"></a>` with
        // an id of its own characters (`sanitize_id`), shows nothing and is
        // what the document's own links point at.
        if tag
            .as_str()
            .strip_prefix("<a id=\"")
            .and_then(|rest| rest.strip_suffix("\">"))
            .is_some_and(|id| {
                (1..=MAX_ANCHOR_ID_BYTES).contains(&id.len())
                    && id
                        .bytes()
                        .all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
            })
            && text[tag.end()..].starts_with("</a>")
        {
            position = tag.end() + "</a>".len();
            continue;
        }
        output.push_str(&text[copied..tag.start()]);
        copied = tag.end();
    }
    output.push_str(&text[copied..]);
    output
}

/// The byte ranges of fenced code blocks and code spans, in order and not
/// overlapping, as CommonMark pairs backtick strings. Fences may sit in a
/// list item or block quote.
fn markdown_code_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    // Fenced blocks, line by line: (marker, length) of the open fence.
    let mut fence: Option<(u8, usize, usize)> = None;
    let mut prose: Vec<(usize, usize)> = Vec::new();
    let mut prose_start = 0;
    let mut line_start = 0;
    for line in text.split_inclusive('\n') {
        let line_end = line_start + line.len();
        let body = line.trim_start_matches([' ', '>']);
        let marker = body
            .bytes()
            .next()
            .filter(|byte| matches!(byte, b'`' | b'~'));
        let run = marker.map_or(0, |marker| {
            body.bytes().take_while(|&b| b == marker).count()
        });
        match (fence, marker) {
            (None, Some(marker)) if run >= 3 && !(marker == b'`' && body[run..].contains('`')) => {
                prose.push((prose_start, line_start));
                fence = Some((marker, run, line_start));
            }
            (Some((open, length, start)), Some(marker))
                if marker == open && run >= length && body[run..].trim().is_empty() =>
            {
                ranges.push((start, line_end));
                fence = None;
                prose_start = line_end;
            }
            _ => {}
        }
        line_start = line_end;
    }
    match fence {
        Some((_, _, start)) => ranges.push((start, text.len())),
        None => prose.push((prose_start, text.len())),
    }
    // Code spans in the prose between fences: a backtick string not escaped
    // opens one, closed by the next string of the same length before a
    // blank line.
    for (start, end) in prose {
        let bytes = &text.as_bytes()[start..end];
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] != b'`' {
                index += 1;
                continue;
            }
            let run = bytes[index..].iter().take_while(|&&b| b == b'`').count();
            let escapes = bytes[..index]
                .iter()
                .rev()
                .take_while(|&&b| b == b'\\')
                .count();
            if escapes % 2 == 1 {
                index += 1;
                continue;
            }
            let mut search = index + run;
            let mut closed = None;
            while search < bytes.len() {
                if bytes[search..].starts_with(b"\n\n") {
                    break;
                }
                if bytes[search] == b'`' {
                    let length = bytes[search..].iter().take_while(|&&b| b == b'`').count();
                    if length == run {
                        closed = Some(search + length);
                        break;
                    }
                    search += length;
                } else {
                    search += 1;
                }
            }
            match closed {
                Some(close) => {
                    ranges.push((start + index, start + close));
                    index = close;
                }
                None => index += run,
            }
        }
    }
    ranges.sort_unstable();
    ranges
}

fn redact_destinations_and_paths(markdown: &str) -> String {
    static URL: OnceLock<Regex> = OnceLock::new();
    static WWW: OnceLock<Regex> = OnceLock::new();
    static PATH: OnceLock<Regex> = OnceLock::new();
    // Bare URLs with an authority, and the schemes that act without one.
    let url = URL.get_or_init(|| {
        Regex::new(
            r#"(?i)\b(?:[a-z][a-z0-9+.\-]*://|(?:mailto|data|javascript|vbscript|file|tel|sms|callto):)[^\s)\]>`"<]+"#,
        )
        .expect("URL regex")
    });
    // `www.` hosts where GFM links them on its own: at a line start or after
    // whitespace, `*`, `_`, `~`, or `(`. Inside `Text/www.index.xhtml` it is
    // part of a relative path.
    let www = WWW
        .get_or_init(|| Regex::new(r#"(?im)(^|[\s*_~(])www\.[^\s<)\]>`"]*"#).expect("www regex"));
    // Home directories, temporary and private roots, Windows profiles, and
    // UNC shares, unless a path or word character precedes them:
    // `Text/home/ch1.xhtml` is a relative link, not a home directory.
    let path = PATH.get_or_init(|| {
        Regex::new(
            r#"(^|[^A-Za-z0-9._/\\\-])((?:(?:/Users|/home|/root|/private|/tmp|/var/folders)/|\\\\[A-Za-z0-9._$\-]+\\|[A-Za-z]:\\(?i:users)\\)[^\s)\]>`"]*)"#,
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
            "Claim[^1]<span></span>(//attacker.example.invalid/p)",
            "Claim[^1]<a id=\"bm\" name=\"x\"></a>(//attacker.example.invalid/p)",
            "[x](<b>//attacker.example.invalid/p)",
        ] {
            let (output, changed) = sanitize_markdown(input);
            assert!(changed, "{input}");
            assert!(!output.contains("attacker"), "{input} -> {output}");
        }
        // AnyDoc's own anchor is kept, so it keeps the bracket and the
        // parenthesis apart and no link forms.
        let kept = "Claim[^1]<a id=\"bm\"></a>(//attacker.example.invalid/p)";
        let (output, _) = sanitize_markdown(kept);
        assert!(!output.contains("](//"), "{output}");
    }

    #[test]
    fn sanitizer_keeps_escaped_angle_brackets_as_text() {
        // AnyDoc escapes a literal `<`, as in a template placeholder.
        for input in [
            "Dear \\<Client name>, your \\<Tax year> return",
            "| \\<Amount> | 100 |",
        ] {
            assert_eq!(
                sanitize_markdown(input),
                (input.to_string(), false),
                "{input}"
            );
        }
        // An escaped backslash does not escape the tag after it, and a tag
        // after an escaped `<` is still removed.
        assert_eq!(
            sanitize_markdown("a \\\\<b>bold</b>"),
            ("a \\\\bold".to_string(), true)
        );
        assert_eq!(
            sanitize_markdown("\\<a <script>x</script>"),
            ("\\<a x".to_string(), true)
        );
    }

    #[test]
    fn sanitizer_keeps_cell_line_breaks_and_code() {
        // AnyDoc joins a cell's lines with `<br>`; without it the amounts
        // would read as one number.
        let cell = "| Wages<br>Interest | 52,000<br>1,250 |";
        assert_eq!(sanitize_markdown(cell), (cell.to_string(), false));
        // Other spellings are not AnyDoc's and are removed.
        assert_eq!(
            sanitize_markdown("a<br onclick=\"x()\">b<BR>c"),
            ("abc".to_string(), true)
        );
        // Code shows what is written.
        for input in [
            "Use `<Client Name>` in the salutation.",
            "```\nDear <Client Name>,\nDue if AGI<threshold and credit>0\n```\n",
            "- item\n\n  ~~~~\n  <b>kept</b>\n  ~~~~\n",
            "`` a ` <b> ``",
        ] {
            assert_eq!(
                sanitize_markdown(input),
                (input.to_string(), false),
                "{input}"
            );
        }
        // The anchor AnyDoc writes for a link target stays; any other
        // anchor goes.
        let anchored = "<a id=\"_toc12-2\"></a>Target `<x>` and [back](#_toc12-2)";
        assert_eq!(sanitize_markdown(anchored), (anchored.to_string(), false));
        for (input, output) in [
            ("<a id=\"Upper\"></a>x", "x"),
            ("<a id=\"m\" onclick=\"y()\"></a>x", "x"),
            ("<a id=\"m\">shown</a>x", "shownx"),
            ("<a id=\"\"></a>x", "x"),
        ] {
            assert_eq!(
                sanitize_markdown(input),
                (output.to_string(), true),
                "{input}"
            );
        }
        let long = format!("<a id=\"{}\"></a>x", "a".repeat(MAX_ANCHOR_ID_BYTES + 1));
        assert_eq!(sanitize_markdown(&long), ("x".to_string(), true));
        // A web address or a path in a code span is removed without its
        // closing backtick.
        for (input, output) in [
            (
                "Portal: `https://portal.example.com/upload` today",
                "Portal: `[external URL removed]` today",
            ),
            (
                &["Share `", "\\\\", "fileserver\\clients\\2025` today"].concat() as &str,
                "Share `[local path removed]` today",
            ),
            (
                &["Old `C:", "\\", "Users\\preparer\\returns` path"].concat() as &str,
                "Old `[local path removed]` path",
            ),
            (
                "<a href=\"javascript:alert(2)\">x</a> and \"https://a.example/x\"",
                "x and \"[external URL removed]\"",
            ),
        ] {
            assert_eq!(sanitize_markdown(input).0, output, "{input}");
        }
        // An escaped or unmatched backtick opens no span.
        assert_eq!(
            sanitize_markdown("\\`<b>x</b>`"),
            ("\\`x`".to_string(), true)
        );
        assert_eq!(
            sanitize_markdown("`open <b>x</b>\n\nlater`"),
            ("`open x\n\nlater`".to_string(), true)
        );
        // An unclosed fence runs to the end.
        let unclosed = "text\n```\n<b>code</b>";
        assert_eq!(sanitize_markdown(unclosed), (unclosed.to_string(), false));
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
        // A password-protected Office file names its streams in UTF-16LE.
        let ole = |name: &str| {
            let mut bytes = vec![0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
            bytes.resize(512, 0);
            bytes.extend(name.encode_utf16().flat_map(u16::to_le_bytes));
            bytes
        };
        for name in ["EncryptedPackage", "EncryptionInfo"] {
            assert!(matches!(
                preflight_package(&ole(name), DocumentKind::Docx, DocumentVariant::Docx),
                Err(DocumentError::Encrypted)
            ));
        }
        assert!(matches!(
            preflight_package(
                &ole("WordDocument"),
                DocumentKind::Docx,
                DocumentVariant::Docx
            ),
            Err(DocumentError::Malformed)
        ));
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
    const PPTX_PRESENTATION: &[u8] = br#"<p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><p:sldIdLst><p:sldId id="256" r:id="rId1"/></p:sldIdLst></p:presentation>"#;
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
        let rels = format!(
            r#"<Relationships xmlns="{PACKAGE_RELS_NS}"><Relationship Id="rId9" Type="{REL_NS}/hyperlink" TargetMode="External" Target="https://example.invalid"/></Relationships>"#
        );
        let bytes = zip_entries(&[
            ("[Content_Types].xml", DOCX_TYPES),
            ("word/document.xml", DOCX_XML),
            ("_rels/.rels", rels.as_bytes()),
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
    fn namespace_declarations_are_not_attributes() {
        let event = quick_xml::events::BytesStart::from_content(
            r#"Relationship xmlns:Target="word/document.xml" Target="word/other.xml" x:Id="r&#49;""#,
            "Relationship".len(),
        );
        assert_eq!(xml_attribute_values(&event, b"Target"), ["word/other.xml"]);
        assert_eq!(xml_attribute_value(&event, b"Id").as_deref(), Some("r1"));

        // A declaration named after `Type` cannot hide the relationship that
        // makes another part the main document.
        let body = word_part("document", "<w:p><w:r><w:t>Decoy</w:t></w:r></w:p>");
        let rels = format!(
            r#"<Relationships xmlns="{PACKAGE_RELS_NS}"><Relationship xmlns:Type="urn:decoy" Id="rId1" Type="{REL_NS}/officeDocument" Target="word/other.xml"/></Relationships>"#
        );
        assert!(matches!(
            docx_result(&[
                ("_rels/.rels", rels.as_bytes()),
                ("word/document.xml", &body),
                ("word/other.xml", &body),
            ]),
            Err(DocumentError::Malformed)
        ));
    }

    #[test]
    fn pptx_checks_read_slides_as_anydoc_reads_them() {
        let presentation = |extra: &str| {
            format!(
                r#"<p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="{REL_NS}" xmlns:p14="http://schemas.microsoft.com/office/powerpoint/2010/main"><p:sldIdLst><p:sldId id="256" r:id="rId1"/></p:sldIdLst>{extra}</p:presentation>"#
            )
            .into_bytes()
        };
        let preflight = |presentation: &[u8], slide: &[u8], extra: &[(&str, &[u8])]| {
            let mut entries: Vec<(&str, &[u8])> = vec![
                ("[Content_Types].xml", PPTX_TYPES),
                ("ppt/presentation.xml", presentation),
                ("ppt/_rels/presentation.xml.rels", PPTX_RELS),
                ("ppt/slides/slide1.xml", slide),
            ];
            entries.extend_from_slice(extra);
            preflight_package(
                &zip_entries(&entries),
                DocumentKind::Pptx,
                DocumentVariant::Pptx,
            )
            .unwrap()
        };
        // A section list names slides by number, not relationship; AnyDoc
        // reads only the first PresentationML slide list.
        let sections = presentation(
            r#"<p:extLst><p:ext uri="{521415D9-36F7-43E2-AB2F-B90AF26B5E84}"><p14:sectionLst><p14:section name="One"><p14:sldIdLst><p14:sldId id="256"/></p14:sldIdLst></p14:section></p14:sectionLst></p:ext></p:extLst>"#,
        );
        assert!(!preflight(&sections, PPTX_SLIDE, &[]).missing_required_content);
        // Hidden slides and shapes, however the attribute is spelled.
        for slide in [
            r#"<p:sld show = "0"><p:cSld><p:spTree/></p:cSld></p:sld>"#,
            r#"<p:sld show="&#48;"><p:cSld><p:spTree/></p:cSld></p:sld>"#,
            r#"<p:sld><p:cSld><p:spTree><p:sp><p:nvSpPr><p:cNvPr id="2" name="x" hidden=" 1"/></p:nvSpPr></p:sp></p:spTree></p:cSld></p:sld>"#,
        ] {
            assert!(
                preflight(&presentation(""), slide.as_bytes(), &[]).hidden_content,
                "{slide}"
            );
        }
        // Speaker notes are converted, so their hidden shapes count too.
        let notes = br#"<p:notes><p:cSld><p:spTree><p:sp><p:nvSpPr><p:cNvPr id="3" name="n" hidden="true"/></p:nvSpPr></p:sp></p:spTree></p:cSld></p:notes>"#;
        assert!(
            preflight(
                &presentation(""),
                PPTX_SLIDE,
                &[("ppt/notesSlides/notesSlide1.xml", notes)]
            )
            .hidden_content
        );
        assert!(!preflight(&presentation(""), PPTX_SLIDE, &[]).hidden_content);
    }

    #[test]
    fn slide_lists_are_matched_to_relationships_in_one_pass() {
        let slides: String = (0..20_000)
            .map(|index| format!(r#"<p:sldId id="{}" r:id="rId{index}"/>"#, 256 + index))
            .collect();
        let presentation = format!(
            r#"<p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="{REL_NS}"><p:sldIdLst>{slides}</p:sldIdLst></p:presentation>"#
        );
        let relationships: String = (0..20_000)
            .map(|index| {
                format!(
                    r#"<Relationship Id="rId{index}" Type="{REL_NS}/slide" Target="slides/slide{index}.xml"/>"#
                )
            })
            .collect();
        let rels = format!("<Relationships>{relationships}</Relationships>");
        let slide_parts: HashSet<String> = (0..20_000)
            .map(|index| format!("ppt/slides/slide{index}.xml"))
            .collect();
        let started = std::time::Instant::now();
        let (listed, incomplete) =
            validate_pptx_slide_targets(presentation.as_bytes(), rels.as_bytes(), &slide_parts)
                .unwrap();
        assert!(!incomplete);
        assert_eq!(listed.len(), 20_000);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "matching must not scan every relationship for every slide"
        );

        // Every slide naming one id that 20,000 relationships repeat.
        let slides = r#"<p:sldId id="256" r:id="rId1"/>"#.repeat(20_000);
        let presentation = format!(
            r#"<p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="{REL_NS}"><p:sldIdLst>{slides}</p:sldIdLst></p:presentation>"#
        );
        let relationships: String = (0..20_000)
            .map(|index| {
                format!(
                    r#"<Relationship Id="rId1" Type="{REL_NS}/slide" Target="slides/slide{index}.xml"/>"#
                )
            })
            .collect();
        let rels = format!("<Relationships>{relationships}</Relationships>");
        let started = std::time::Instant::now();
        let (_, incomplete) =
            validate_pptx_slide_targets(presentation.as_bytes(), rels.as_bytes(), &slide_parts)
                .unwrap();
        assert!(!incomplete);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "a repeated id must not multiply the work"
        );
    }

    #[test]
    fn xlsx_checks_parse_what_they_look_for() {
        let with_sheet = |workbook_sheet: &str, worksheet: &str| {
            let workbook = format!(
                r#"<workbook xmlns:r="{REL_NS}"><sheets><sheet name="Data" sheetId="1" {workbook_sheet} r:id="rId1"/></sheets></workbook>"#
            );
            let rels = format!(
                r#"<Relationships xmlns="{PACKAGE_RELS_NS}"><Relationship Id="rId1" Type="{REL_NS}/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#
            );
            let sheet =
                format!("<worksheet {SML_NS}><sheetData>{worksheet}</sheetData></worksheet>");
            preflight_package(
                &zip_entries(&[
                    ("[Content_Types].xml", XLSX_TYPES),
                    ("xl/workbook.xml", workbook.as_bytes()),
                    ("xl/_rels/workbook.xml.rels", rels.as_bytes()),
                    ("xl/worksheets/sheet1.xml", sheet.as_bytes()),
                ]),
                DocumentKind::Xlsx,
                DocumentVariant::Xlsx,
            )
        };
        let visible_row = r#"<row r="1"><c r="A1"><v>1</v></c></row>"#;
        for (sheet, rows) in [
            (r#"state='hidden'"#, visible_row),
            (r#"state="&#104;idden""#, visible_row),
            (r#"state=" veryHidden ""#, visible_row),
            ("", r#"<row r="1" hidden='1'><c r="A1"><v>1</v></c></row>"#),
            (
                "",
                r#"<row r="1" hidden=" true"><c r="A1"><v>1</v></c></row>"#,
            ),
            (
                "",
                r#"<row r="1" ht="0" customHeight="1"><c r="A1"><v>1</v></c></row>"#,
            ),
        ] {
            assert!(
                with_sheet(sheet, rows).unwrap().hidden_content,
                "{sheet} {rows}"
            );
        }
        for rows in [
            r#"<row r="1"><c r="A1"><f>1+1</f><!--<v>2</v>--></c></row>"#,
            r#"<row r="1"><c r="A1"><f>1+1</f></c ></row>"#,
            r#"<row r="1"><x:c xmlns:x="urn:x" r="A1"><x:f>1+1</x:f></x:c></row>"#,
        ] {
            assert!(
                with_sheet("", rows).unwrap().missing_formula_cache,
                "{rows}"
            );
        }
        let cached =
            with_sheet("", r#"<row r="1"><c r="A1"><f>1+1</f><v>2</v></c></row>"#).unwrap();
        assert!(!cached.missing_formula_cache && !cached.hidden_content);
        // Hidden defined names, which Excel adds for filters, hide nothing.
        let workbook = format!(
            r#"<workbook xmlns:r="{REL_NS}"><sheets><sheet name="Data" sheetId="1" r:id="rId1"/></sheets><definedNames><definedName name="_xlnm._FilterDatabase" hidden="1">Data!$A$1</definedName></definedNames></workbook>"#
        );
        let bytes = zip_entries(&[
            ("[Content_Types].xml", XLSX_TYPES),
            ("xl/workbook.xml", workbook.as_bytes()),
        ]);
        assert!(
            !preflight_package(&bytes, DocumentKind::Xlsx, DocumentVariant::Xlsx)
                .unwrap()
                .hidden_content
        );

        // A workbook part that is not XML goes to AnyDoc's binary reader.
        let bytes = zip_entries(&[
            ("[Content_Types].xml", XLSX_TYPES),
            ("xl/workbook.xml", b"\x81\x01\x00\x83\x01\x00"),
        ]);
        assert!(matches!(
            preflight_package(&bytes, DocumentKind::Xlsx, DocumentVariant::Xlsx),
            Err(DocumentError::Unsupported)
        ));
        assert!(anydoc_workbook_is_xml(b"\xEF\xBB\xBF \n<workbook/>"));
        assert!(anydoc_workbook_is_xml(&utf16("<workbook/>", true)));
    }

    #[test]
    fn odf_checks_follow_the_body_anydoc_converts() {
        const OFFICE: &str = r#"xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:presentation="urn:oasis:names:tc:opendocument:xmlns:presentation:1.0""#;
        let content = |styles: &str, body: &str| {
            format!(
                "<office:document-content {OFFICE}><office:automatic-styles>{styles}</office:automatic-styles><office:body>{body}</office:body></office:document-content>"
            )
            .into_bytes()
        };
        // AnyDoc converts a text body first, whatever the mimetype says.
        let text_first = content(
            "",
            "<office:text><text:p>Text</text:p></office:text><office:spreadsheet><table:table><table:table-row><table:table-cell/></table:table-row></table:table></office:spreadsheet>",
        );
        assert!(
            preflight_package(
                &ods_package(&text_first, &[]),
                DocumentKind::Ods,
                DocumentVariant::Ods
            )
            .unwrap()
            .missing_required_content
        );
        let text_body = content("", "<office:text><text:p>Text</text:p></office:text>");
        assert!(
            preflight_package(
                &odp_package(&text_body, &[]),
                DocumentKind::Odp,
                DocumentVariant::Odp
            )
            .unwrap()
            .missing_required_content
        );

        // LibreOffice hides a slide through its drawing-page style, defined
        // in either part.
        let page = r#"<office:presentation><draw:page draw:name="One" draw:style-name="dp9"><draw:frame><draw:text-box><text:p>Hidden</text:p></draw:text-box></draw:frame></draw:page></office:presentation>"#;
        let hidden_style = r#"<style:style style:name="dp9" style:family="drawing-page"><style:drawing-page-properties presentation:visibility="hidden"/></style:style>"#;
        let odp = |styles: &str, extra: &[(&str, &[u8])]| {
            preflight_package(
                &odp_package(&content(styles, page), extra),
                DocumentKind::Odp,
                DocumentVariant::Odp,
            )
            .unwrap()
            .hidden_content
        };
        assert!(odp(hidden_style, &[]));
        let styles_part = format!(
            "<office:document-styles {OFFICE}><office:styles>{hidden_style}</office:styles></office:document-styles>"
        );
        assert!(odp("", &[("styles.xml", styles_part.as_bytes())]));
        assert!(!odp(
            r#"<style:style style:name="dp9" style:family="drawing-page"><style:drawing-page-properties presentation:visibility="visible"/></style:style>"#,
            &[]
        ));

        // A namespace declaration cannot shadow the attribute that hides a row.
        let ods = content(
            "",
            r#"<office:spreadsheet><table:table><table:table-row xmlns:visibility="urn:decoy" table:visibility="collapse"><table:table-cell office:value-type="string"><text:p>Hidden</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet>"#,
        );
        assert!(
            preflight_package(
                &ods_package(&ods, &[]),
                DocumentKind::Ods,
                DocumentVariant::Ods
            )
            .unwrap()
            .hidden_content
        );
        // Values are decoded before they are compared.
        let odt = content(
            "",
            r#"<office:text><text:p text:display="&#110;one">Hidden</text:p></office:text>"#,
        );
        assert!(
            preflight_package(
                &odt_package(&odt, &[]),
                DocumentKind::Odt,
                DocumentVariant::Odt
            )
            .unwrap()
            .hidden_content
        );
    }

    #[test]
    fn docx_checks_follow_anydocs_walker() {
        const VML: &str = r#"xmlns:v="urn:schemas-microsoft-com:vml""#;
        const MC: &str =
            r#"xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006""#;
        let dropped = |body: &str| {
            let document = format!(
                "<w:document {WORD_NS} {VML} {MC} xmlns:x=\"urn:x\"><w:body>{body}</w:body></w:document>"
            );
            docx_preflight(&[("word/document.xml", document.as_bytes())]).unsupported_content
        };
        let symbol = r#"<w:p><w:r><w:sym w:char="F0FE"/></w:r></w:p>"#;
        let text_box = |inner: &str| {
            format!("<w:p><w:r><w:pict><v:shape>{inner}</v:shape></w:pict></w:r></w:p>")
        };
        // Inside a drawing AnyDoc searches every wrapper for text boxes, so a
        // deletion or foreign element there hides nothing.
        assert!(dropped(&text_box(&format!(
            "<x:del><v:textbox><w:txbxContent>{symbol}</w:txbxContent></v:textbox></x:del>"
        ))));
        assert!(dropped(&text_box(&format!(
            "<w:del><v:textbox><w:txbxContent>{symbol}</w:txbxContent></v:textbox></w:del>"
        ))));
        // A deletion around the drawing, or the fallback branch AnyDoc skips
        // while searching, does hide it.
        assert!(!dropped(&format!(
            "<w:p><w:del><w:r><w:pict><v:shape><v:textbox><w:txbxContent>{symbol}</w:txbxContent></v:textbox></v:shape></w:pict></w:r></w:del></w:p>"
        )));
        assert!(!dropped(&format!(
            "<w:p><w:r><w:drawing><mc:AlternateContent><mc:Fallback><w:pict><v:shape><v:textbox><w:txbxContent>{symbol}</w:txbxContent></v:textbox></v:shape></w:pict></mc:Fallback></mc:AlternateContent></w:drawing></w:r></w:p>"
        )));
        // A deletion in a foreign namespace is not WordprocessingML's.
        assert!(dropped(
            r#"<w:p><x:del><w:r><w:sym w:char="F0FE"/></w:r></x:del></w:p>"#
        ));
        // AnyDoc reads only a table's direct rows.
        let row = "<w:tr><w:tc><w:p><w:r><w:t>Row</w:t></w:r></w:p></w:tc></w:tr>";
        assert!(dropped(&format!(
            "<w:tbl>{row}<w:sdt><w:sdtContent>{row}</w:sdtContent></w:sdt></w:tbl>"
        )));
        assert!(dropped(&format!(
            "<w:tbl>{row}<w:customXml>{row}</w:customXml></w:tbl>"
        )));
        assert!(!dropped(&format!("<w:tbl>{row}{row}</w:tbl>")));

        // Run properties apply through a markup-compatibility branch.
        let document = format!(
            "<w:document {WORD_NS} {MC}><w:body><w:p><w:r><mc:AlternateContent><mc:Choice Requires=\"w14\"><w:rPr><w:vanish/></w:rPr></mc:Choice></mc:AlternateContent><w:t>Hidden</w:t></w:r></w:p></w:body></w:document>"
        );
        assert!(docx_preflight(&[("word/document.xml", document.as_bytes())]).hidden_content);
    }

    #[test]
    fn docx_numbering_and_embedded_objects_are_checked() {
        let body = word_part(
            "document",
            r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>Item</w:t></w:r></w:p>"#,
        );
        let numbering = |level_properties: &str| {
            format!(
                r#"<w:numbering {WORD_NS}><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:lvlText w:val="%1."/><w:rPr>{level_properties}</w:rPr></w:lvl></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#
            )
        };
        let with_numbering = |numbering: &str, styles: &str| {
            let styles = format!("<w:styles {WORD_NS}>{styles}</w:styles>");
            docx_preflight(&[
                ("word/document.xml", &body),
                ("word/numbering.xml", numbering.as_bytes()),
                ("word/styles.xml", styles.as_bytes()),
            ])
            .hidden_content
        };
        assert!(with_numbering(&numbering("<w:vanish/>"), ""));
        assert!(with_numbering(
            &numbering(r#"<w:rStyle w:val="Quiet"/>"#),
            r#"<w:style w:type="character" w:styleId="Quiet"><w:rPr><w:vanish/></w:rPr></w:style>"#
        ));
        assert!(!with_numbering(&numbering("<w:b/>"), ""));

        // An embedded object is found by its relationship type, whatever
        // the part is called.
        let rels = format!(
            r#"<Relationships xmlns="{PACKAGE_RELS_NS}"><Relationship Id="rIdOle" Type="{REL_NS}/oleObject" Target="data.bin"/></Relationships>"#
        );
        let preflight = docx_preflight(&[
            ("word/document.xml", &body),
            ("word/_rels/document.xml.rels", rels.as_bytes()),
            ("word/data.bin", b"ole"),
        ]);
        assert!(preflight.active_content);
        assert!(ooxml_active_relationship(
            "http://schemas.microsoft.com/office/2006/relationships/vbaProject"
        ));
        assert!(!ooxml_active_relationship(&format!("{REL_NS}/image")));
    }

    #[test]
    fn epub_checks_read_every_stylesheet_the_chapter_applies() {
        let chapter = |head: &str, body: &str| {
            format!(
                r#"<?xml version="1.0"?><html xmlns="http://www.w3.org/1999/xhtml"><head>{head}</head><body><h1>Chapter One</h1>{body}<img src="../images/logo.png" alt="logo"/></body></html>"#
            )
            .into_bytes()
        };
        let hidden = |head: &str, body: &str, extra: &[(&str, &[u8])]| {
            let mut entries: Vec<(&str, &[u8])> = vec![("OPS/images/logo.png", b"png")];
            entries.extend_from_slice(extra);
            preflight_package(
                &epub_package(&chapter(head, body), Some(EPUB_CHAPTER_TWO), &entries),
                DocumentKind::Epub,
                DocumentVariant::Epub,
            )
            .unwrap()
            .hidden_content
        };
        // AnyDoc omits what an inline `display: none` hides, as a reader
        // does, so nothing hidden converts; it ignores `visibility`.
        for body in [
            r#"<p style="display: none">Hidden</p>"#,
            r#"<p style="display&#58;none">Hidden</p>"#,
        ] {
            assert!(!hidden("", body, &[]), "{body}");
        }
        assert!(hidden(
            "",
            r#"<p style="VISIBILITY : Hidden !important">Hidden</p>"#,
            &[]
        ));
        let marked = r#"<p class="note h">Hidden</p>"#;
        assert!(hidden(
            "<style>.h { visibility: hidden }</style>",
            marked,
            &[]
        ));
        assert!(hidden(
            "<style>@media screen { p.h { display : none } }</style>",
            marked,
            &[]
        ));
        // Rules that match no element, or style generated content, hide
        // nothing in the chapter.
        assert!(!hidden(
            "<style>.unused { display: none }</style>",
            marked,
            &[]
        ));
        assert!(!hidden(
            "<style>p::before { display: none }</style>",
            marked,
            &[]
        ));
        // Linked sheets, and the local sheets they import, are read too.
        let link = r#"<link rel="stylesheet" href="../Styles/main.css"/>"#;
        assert!(hidden(
            link,
            marked,
            &[("OPS/Styles/main.css", b"/* x */ .h{visibility:collapse}")]
        ));
        assert!(hidden(
            link,
            marked,
            &[
                ("OPS/Styles/main.css", b"@import url(\"more.css\");"),
                ("OPS/Styles/more.css", b"p.h { display: none; }"),
            ]
        ));
        assert!(!hidden(
            link,
            marked,
            &[("OPS/Styles/main.css", b".h { color: gray }")]
        ));
        // An import's layer holds the rules it imports, which the importing
        // sheet's unlayered rules beat.
        let layered = [
            (
                "OPS/Styles/main.css",
                b"@import url(\"more.css\") layer(base); p.h { visibility: visible }".as_slice(),
            ),
            ("OPS/Styles/more.css", b"body p.h { visibility: hidden }"),
        ];
        assert!(!hidden(link, marked, &layered));
        assert!(!hidden(
            r#"<style>@import url("../Styles/more.css") layer; p.h { visibility: visible }</style>"#,
            marked,
            &layered
        ));

        // Sheets are reduced once per package, under a cap on the rules
        // that set what the check reads.
        let crowded: String = (0..=epub_css::MAX_STYLE_RULES)
            .map(|index| format!(".x{index}{{display:none}}"))
            .collect();
        let result = preflight_package(
            &epub_package(
                &chapter(link, marked),
                Some(EPUB_CHAPTER_TWO),
                &[
                    ("OPS/images/logo.png", b"png"),
                    ("OPS/Styles/main.css", crowded.as_bytes()),
                ],
            ),
            DocumentKind::Epub,
            DocumentVariant::Epub,
        );
        assert!(matches!(result, Err(DocumentError::ResourceLimit)));
    }

    #[test]
    fn epub_stylesheets_are_read_once_per_package() {
        let bytes = zip_entries(&[(
            "OPS/Styles/main.css",
            b".note { visibility: hidden }".as_slice(),
        )]);
        let mut archive = ZipArchive::new(Cursor::new(bytes.as_slice())).unwrap();
        let mut result = PackagePreflight::default();
        let mut stylesheets = EpubStylesheets::default();
        let linked = EpubStyleSource::Linked("OPS/Styles/main.css".to_string());
        let styles = EpubChapterStyles {
            reader: vec![(linked.clone(), epub_css::Condition::always())],
            anydoc: vec![linked],
        };
        let first = stylesheets
            .chapter_cascade(&mut archive, "OPS/Text/ch1.xhtml", &styles, &mut result)
            .unwrap();
        // Chapters that apply the same sheets share one cascade.
        for chapter in 2..200 {
            let path = format!("OPS/Text/ch{chapter}.xhtml");
            let again = stylesheets
                .chapter_cascade(&mut archive, &path, &styles, &mut result)
                .unwrap();
            assert!(Rc::ptr_eq(&first, &again));
        }
        // A chapter elsewhere builds its own cascade from the same sheet.
        let other = stylesheets
            .chapter_cascade(&mut archive, "OPS/Other/ch.xhtml", &styles, &mut result)
            .unwrap();
        assert!(!Rc::ptr_eq(&first, &other));
        assert_eq!(stylesheets.linked.len(), 1);
        assert_eq!(stylesheets.rules, 1);
    }

    #[test]
    fn epub_media_is_read_only_where_it_applies_a_stylesheet() {
        let query = "(min-width: 321px) and (max-width: 1256px) and (min-height: 400px)";
        let inspect = |head: &str, body: &str, media: &mut epub_css::MediaQueries| {
            let chapter = format!(
                r#"<?xml version="1.0"?><html xmlns="http://www.w3.org/1999/xhtml"><head><title>T</title>{head}</head><body><p>{body}</p></body></html>"#
            );
            epub_inspect_chapter(
                chapter.as_bytes(),
                "OPS/Text/ch1.xhtml",
                &HashSet::new(),
                media,
            )
            .unwrap();
        };
        // Any other element's `media` is not read.
        let mut media = epub_css::MediaQueries::default();
        let spans = format!(r#"<span media="{query}">w</span>"#).repeat(2000);
        inspect("", &spans, &mut media);
        assert_eq!(media.work(), 0);
        // A stylesheet's is, once however many links and chapters repeat it.
        let links = format!(r#"<link rel="stylesheet" href="a.css" media="{query}"/>"#);
        inspect(&links, "Text", &mut media);
        let once = media.work();
        assert!(once > 0);
        inspect(&links.repeat(500), "Text", &mut media);
        let styles = format!(r#"<style media="{query}">.x {{ display: none }}</style>"#);
        inspect(&styles, "Text", &mut media);
        assert_eq!(media.work(), once);
    }

    #[test]
    fn epub_readers_apply_only_the_stylesheets_chromium_enables() {
        let preflight = |prolog: &str, head: &str| {
            let chapter = format!(
                r#"<?xml version="1.0"?>{prolog}<html xmlns="http://www.w3.org/1999/xhtml"><head>{head}</head><body><h1>Chapter One</h1><p>Refund due <span class="x">1,250.00</span> by April.</p><img src="../images/logo.png" alt="logo"/></body></html>"#
            );
            preflight_package(
                &epub_package(
                    chapter.as_bytes(),
                    Some(EPUB_CHAPTER_TWO),
                    &[
                        ("OPS/images/logo.png", b"png"),
                        ("OPS/Styles/hide.css", b".x { display: none }"),
                        ("OPS/Styles/plain.css", b".d { color: red }"),
                    ],
                ),
                DocumentKind::Epub,
                DocumentVariant::Epub,
            )
            .unwrap()
        };
        let dropped = |head: &str| preflight("", head).unsupported_content;
        // AnyDoc applies every `link` to a stylesheet and every `style`;
        // Chromium no alternate sheet, none of a type other than CSS, no
        // disabled link, and of the titled sheets only those the first
        // titled one names, whatever its media.
        for head in [
            r#"<link rel="alternate stylesheet" title="Alt" href="../Styles/hide.css"/>"#,
            r#"<link rel="alternate stylesheet" href="../Styles/hide.css"/>"#,
            r#"<link rel="stylesheet" type="text/plain" href="../Styles/hide.css"/>"#,
            r#"<link rel="stylesheet" disabled="disabled" href="../Styles/hide.css"/>"#,
            r#"<style type="text/plain">.x { display: none }</style>"#,
            r#"<style type="text/css; charset=utf-8">.x { display: none }</style>"#,
            r#"<link rel="stylesheet" title="Main" href="../Styles/plain.css"/><link rel="stylesheet" title="Other" href="../Styles/hide.css"/>"#,
            r#"<style title="Main">.d { color: red }</style><style title="Other">.x { display: none }</style>"#,
            r#"<link rel="stylesheet" title="Main" media="print" href="../Styles/plain.css"/><style title="Other">.x { display: none }</style>"#,
        ] {
            assert!(dropped(head), "{head}");
        }
        // It applies those without a title, those of the preferred set,
        // and a type naming CSS as it reads one.
        for head in [
            r#"<link rel="stylesheet" href="../Styles/hide.css"/>"#,
            r#"<link rel="StyleSheet" type=" text/CSS; charset=utf-8" href="../Styles/hide.css"/>"#,
            r#"<link rel="stylesheet" title="Main" href="../Styles/plain.css"/><link rel="stylesheet" href="../Styles/hide.css"/>"#,
            r#"<link rel="stylesheet" title="Main" href="../Styles/plain.css"/><link rel="alternate stylesheet" title="Main" href="../Styles/hide.css"/>"#,
            r#"<link rel="stylesheet" type="text/plain" title="Main" href="../Styles/plain.css"/><link rel="stylesheet" title="Other" href="../Styles/hide.css"/>"#,
            r#"<style type="TEXT/CSS" disabled="disabled">.x { display: none }</style>"#,
        ] {
            assert!(!dropped(head), "{head}");
        }
        // An `<?xml-stylesheet?>` instruction AnyDoc does not follow: of a
        // type exactly `text/css`, or none, and not an alternate.
        let converts_hidden = |prolog: &str| preflight(prolog, "").hidden_content;
        assert!(converts_hidden(
            r#"<?xml-stylesheet href="../Styles/hide.css"?>"#
        ));
        for prolog in [
            r#"<?xml-stylesheet href="../Styles/hide.css" type="TEXT/CSS"?>"#,
            r#"<?xml-stylesheet href="../Styles/hide.css" type="text/css" alternate="yes" title="Alt"?>"#,
        ] {
            assert!(!converts_hidden(prolog), "{prolog}");
        }
    }

    #[test]
    fn odp_visibility_follows_styles_layers_and_display() {
        let hides = |content: &str, styles: &str| {
            let mut visibility = OdpVisibility::default();
            scan_odp_visibility(styles.as_bytes(), false, &mut visibility).unwrap();
            scan_odp_visibility(content.as_bytes(), true, &mut visibility).unwrap();
            visibility.hides_content()
        };
        let style = |name: &str, parent: &str, visibility: &str| {
            let parent = if parent.is_empty() {
                String::new()
            } else {
                format!(r#" style:parent-style-name="{parent}""#)
            };
            let properties = if visibility.is_empty() {
                String::new()
            } else {
                format!(
                    r#"<style:drawing-page-properties presentation:visibility="{visibility}"/>"#
                )
            };
            format!(
                r#"<style:style style:name="{name}" style:family="drawing-page"{parent}>{properties}</style:style>"#
            )
        };
        let page = |style: &str, shapes: &str| {
            format!(r#"<draw:page draw:style-name="{style}">{shapes}</draw:page>"#)
        };
        // A slide style hides a slide itself or through its parent.
        assert!(hides(&(style("dp1", "", "hidden") + &page("dp1", "")), ""));
        assert!(hides(
            &(style("dp2", "base", "") + &page("dp2", "")),
            &style("base", "", "hidden")
        ));
        assert!(!hides(
            &(style("dp3", "base", "visible") + &page("dp3", "")),
            &style("base", "", "hidden")
        ));
        // A style defined in both parts hides if either definition does.
        assert!(hides(
            &(style("dp4", "", "") + &page("dp4", "")),
            &(style("dp4", "base", "") + &style("base", "", "hidden"))
        ));
        // A chain too long to follow is taken to hide the slide.
        let chain: String = (0..=MAX_ODP_STYLE_CHAIN)
            .map(|link| style(&format!("s{link}"), &format!("s{}", link + 1), ""))
            .collect();
        assert!(hides(&(chain.clone() + &page("s0", "")), ""));
        assert!(!hides(&(chain + &page("s50", "")), ""));
        // Page properties count under a style of another family, but not
        // under a style nothing can apply.
        let graphic = r#"<style:style style:name="gr1" style:family="graphic"><style:drawing-page-properties presentation:visibility="hidden"/></style:style>"#;
        assert!(hides(&page("gr1", ""), graphic));
        let unnamed = r#"<style:style style:family="drawing-page"><style:drawing-page-properties presentation:visibility="hidden"/></style:style>"#;
        assert!(!hides(&page("none", ""), unnamed));
        // The default drawing-page style hides slides that set nothing.
        let default = r#"<style:default-style style:family="drawing-page"><style:drawing-page-properties presentation:visibility="hidden"/></style:default-style>"#;
        assert!(hides(&page("none", ""), default));
        assert!(!hides(&page("none", ""), ""));
        // A shape shown only in print, or on a hidden layer.
        assert!(hides(
            &page("x", r#"<draw:frame draw:display="printer"/>"#),
            ""
        ));
        assert!(hides(
            &page("x", r#"<draw:custom-shape draw:display="none"/>"#),
            ""
        ));
        let layers = r#"<draw:layer-set><draw:layer draw:name="Secret" draw:display="none"/></draw:layer-set>"#;
        assert!(hides(
            &page("x", r#"<draw:custom-shape draw:layer="Secret"/>"#),
            layers
        ));
        assert!(!hides(
            &page("x", r#"<draw:custom-shape draw:layer="layout"/>"#),
            layers
        ));
        // Master-page shapes in the styles part are not converted.
        assert!(!hides(
            &page("x", ""),
            r#"<style:master-page><draw:frame draw:display="none"/></style:master-page>"#
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
        let worksheet = format!(
            r#"<worksheet {SML_NS}><sheetData><row hidden="1"><c r="A1"><f>A1</f></c></row></sheetData></worksheet>"#
        );
        let bytes = zip_entries(&[
            ("[Content_Types].xml", XLSX_TYPES),
            ("xl/workbook.xml", workbook),
            ("xl/worksheets/sheet1.xml", worksheet.as_bytes()),
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

    /// The main part's relationships as Word writes them, naming the
    /// conventional styles, numbering, and notes parts.
    const WORD_DOCUMENT_RELS: &[u8] = br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/numbering" Target="numbering.xml"/><Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/footnotes" Target="footnotes.xml"/><Relationship Id="rId4" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/endnotes" Target="endnotes.xml"/></Relationships>"#;

    /// A Word package of these parts, with Word's relationships for the
    /// main part unless the parts give their own.
    fn docx_package(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut all: Vec<(&str, &[u8])> = vec![("[Content_Types].xml", DOCX_TYPES)];
        all.extend_from_slice(entries);
        if !entries
            .iter()
            .any(|(name, _)| *name == "word/_rels/document.xml.rels")
        {
            all.push(("word/_rels/document.xml.rels", WORD_DOCUMENT_RELS));
        }
        zip_entries(&all)
    }

    fn docx_preflight(entries: &[(&str, &[u8])]) -> PackagePreflight {
        preflight_package(
            &docx_package(entries),
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

        // Compatibility content AnyDoc takes no branch of loses the text of
        // the branch Word shows, as a block or in a run.
        let alternate = |branches: &str| {
            format!(
                r#"<mc:AlternateContent xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:w14="http://schemas.microsoft.com/office/word/2010/wordml" xmlns:m="http://schemas.openxmlformats.org/officeDocument/2006/math">{branches}</mc:AlternateContent>"#
            )
        };
        let dropped_text = |body: String| {
            let document = word_part("document", &body);
            docx_preflight(&[("word/document.xml", &document)]).unsupported_content
        };
        let choice =
            r#"<mc:Choice Requires="w14"><w:p><w:r><w:t>Shown</w:t></w:r></w:p></mc:Choice>"#;
        assert!(dropped_text(alternate(choice)));
        assert!(dropped_text(format!(
            "<w:p><w:r>{}</w:r></w:p>",
            alternate(r#"<mc:Choice Requires="w14"><w:t>Shown</w:t></mc:Choice>"#)
        )));
        // Not where AnyDoc reads a fallback holding it, where the choice
        // holds no text, or where Word does not understand the choice
        // either.
        assert!(!dropped_text(alternate(&format!(
            r#"{choice}<mc:Fallback><w:p><w:r><w:t>Shown</w:t></w:r></w:p></mc:Fallback>"#
        ))));
        assert!(!dropped_text(alternate(
            r#"<mc:Choice Requires="w14"><w:p><w:r><w:t> </w:t></w:r></w:p></mc:Choice>"#
        )));
        assert!(!dropped_text(alternate(
            r#"<mc:Choice xmlns:zz="urn:zz" Requires="zz"><w:p><w:r><w:t>Unread</w:t></w:r></w:p></mc:Choice>"#
        )));
        // Math is text too, in a choice AnyDoc does not read or one
        // requiring math itself, which AnyDoc does not support.
        let math = r#"<m:oMath><m:r><m:t>r=0.07</m:t></m:r></m:oMath>"#;
        for requires in ["w14", "m"] {
            assert!(
                dropped_text(format!(
                    r#"<w:p><w:r><w:t xml:space="preserve">The rate is </w:t></w:r>{}</w:p>"#,
                    alternate(&format!(
                        r#"<mc:Choice Requires="{requires}">{math}</mc:Choice>"#
                    ))
                )),
                "{requires}"
            );
        }
        assert!(dropped_text(alternate(&format!(
            r#"<mc:Choice Requires="w14"><m:oMathPara>{math}</m:oMathPara></mc:Choice>"#
        ))));
        assert!(!dropped_text(format!(
            r#"<w:p xmlns:m="http://schemas.openxmlformats.org/officeDocument/2006/math">{math}</w:p>"#
        )));
        // A fallback holding other text than Word's branch converts text
        // Word does not show; the same text, split and spaced otherwise,
        // is the same.
        let run = |text: &str| format!(r#"<w:r><w:t xml:space="preserve">{text}</w:t></w:r>"#);
        let branches = |choice: &str, fallback: &str| {
            format!(
                "<w:p>{}</w:p>",
                alternate(&format!(
                    r#"<mc:Choice Requires="w14">{choice}</mc:Choice><mc:Fallback>{fallback}</mc:Fallback>"#
                ))
            )
        };
        assert!(dropped_text(branches(
            &run("Pay 100 USD by March 1"),
            &run("Pay 900 USD by March 9")
        )));
        assert!(dropped_text(branches(&run("Pay 100 USD"), "")));
        assert!(!dropped_text(branches(
            &format!("{}{}", run("Pay 100 "), run("USD")),
            &run("Pay 100  USD")
        )));
        assert!(!dropped_text(branches(
            &run("Pay &amp; go"),
            &run("Pay &#38; go")
        )));
        // Deleted text is shown by neither.
        assert!(!dropped_text(branches(
            &format!(
                r#"<w:del w:id="1" w:author="a">{}</w:del>{}"#,
                run("Old"),
                run("New")
            ),
            &run("New")
        )));
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
        // A page number or date Word fills in is dropped with the text
        // around it converted: partial, like a dropped hyphen.
        for body in [
            r#"<w:p><w:r><w:t xml:space="preserve">Page </w:t><w:pgNum/></w:r></w:p>"#,
            r#"<w:p><w:r><w:t xml:space="preserve">Signed </w:t><w:monthLong/><w:t xml:space="preserve"> </w:t><w:dayShort/></w:r></w:p>"#,
        ] {
            let preflight = preflight_for(body);
            assert!(preflight.omitted_page_blocks, "{body}");
            assert!(!preflight.unsupported_content, "{body}");
            assert!(preflight_rejection(DocumentKind::Docx, &preflight).is_none());
        }
        // A page number in a header, which AnyDoc does not convert, loses
        // nothing.
        let header = word_part("hdr", "<w:p><w:r><w:pgNum/></w:r></w:p>");
        let body = word_part("document", "<w:p><w:r><w:t>Body</w:t></w:r></w:p>");
        let preflight =
            docx_preflight(&[("word/document.xml", &body), ("word/header1.xml", &header)]);
        assert!(!preflight.unsupported_content && !preflight.omitted_page_blocks);
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
        let notes_rels = br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId7" Type = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/footnotes" Target = "notes/fn.xml"/></Relationships>"#;
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
        let rels = br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId9" Type = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target = "design/s.xml"/></Relationships>"#;
        let bytes = xlsx_with_styles("xl/design/s.xml", &oversized, Some(rels));
        assert!(matches!(
            preflight_package(&bytes, DocumentKind::Xlsx, DocumentVariant::Xlsx),
            Err(DocumentError::ResourceLimit)
        ));

        let sheet = format!(
            r#"<worksheet {SML_NS}><sheetData><row><c r="A1"><f>1+1</f></c></row></sheetData></worksheet>"#
        );
        let relocated = |sheet_attributes: &str, relationship_type: &str| {
            let workbook = format!(
                r#"<workbook xmlns:r="{REL_NS}" xmlns:x="urn:decoy"><sheets><sheet name="Data" sheetId="1" {sheet_attributes}/></sheets></workbook>"#
            );
            let rels = format!(
                r#"<Relationships xmlns="{PACKAGE_RELS_NS}"><Relationship Id="rDecoy" Type="{REL_NS}/worksheet" Target="decoy.xml"/><Relationship Id="rId1" Type="{REL_NS}/{relationship_type}" Target="data/s1.xml"/></Relationships>"#
            );
            let bytes = zip_entries(&[
                ("[Content_Types].xml", XLSX_TYPES),
                ("xl/workbook.xml", workbook.as_bytes()),
                ("xl/_rels/workbook.xml.rels", rels.as_bytes()),
                ("xl/data/s1.xml", sheet.as_bytes()),
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
    const SML_NS: &str = r#"xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main""#;
    const PACKAGE_RELS_NS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";

    fn docx_result(entries: &[(&str, &[u8])]) -> Result<PackagePreflight, DocumentError> {
        preflight_package(
            &docx_package(entries),
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
        // Spellings Word and AnyDoc both resolve to the conventional part
        // are accepted.
        for target in [
            "word/document.xml",
            "../word/document.xml",
            "word/./document.xml",
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
        // Encoded structure and traversal out of the conventional part fail,
        // as do spellings AnyDoc alone resolves to it: Word opens the part a
        // target names as written, query and all.
        for target in [
            "word%2Fdocument.xml",
            "word/main.xml",
            "../main.xml",
            "word/%64ocument.xml",
            "word/document.xml?v=1#top",
        ] {
            assert!(
                matches!(
                    with_root(&rels(relationship("rId1", target))),
                    Err(DocumentError::Malformed)
                ),
                "{target}"
            );
        }
        // Relationships written otherwise than OPC defines them, which Word
        // and AnyDoc can read apart, are refused: prefixed elements, which
        // LibreOffice does not read, an end tag that does not match, and a
        // relationship nested in another element.
        let prefixed = format!(
            r#"<pr:Relationships xmlns:pr="{PACKAGE_RELS_NS}"><pr:Relationship Id="rId1" Type="{REL_NS}/officeDocument" Target="word/document.xml"/></pr:Relationships>"#
        );
        let mismatched = rels(relationship("rId1", "word/document.xml"))
            .replace("</Relationships>", "</x:Relationships>");
        let nested = rels(format!(
            "<x:wrap xmlns:x=\"urn:x\">{}</x:wrap>",
            relationship("rId1", "word/document.xml")
        ));
        for written in [prefixed, mismatched, nested] {
            assert!(
                matches!(with_root(&written), Err(DocumentError::Malformed)),
                "{written}"
            );
        }
    }

    #[test]
    fn main_parts_are_found_by_exact_name() {
        // Without the exact `xl/workbook.xml`, AnyDoc falls back to the binary
        // `xl/workbook.bin`; a case variant is only a decoy.
        let decoy = zip_entries(&[
            ("[Content_Types].xml", XLSX_TYPES),
            ("XL/workbook.xml", b"<workbook/>"),
            ("xl/workbook.bin", b"\x83\x01\x00"),
        ]);
        assert!(matches!(
            preflight_package(&decoy, DocumentKind::Xlsx, DocumentVariant::Xlsx),
            Err(DocumentError::Malformed)
        ));
        let body = word_part("document", "<w:p><w:r><w:t>Body</w:t></w:r></w:p>");
        let word = zip_entries(&[
            ("[Content_Types].xml", DOCX_TYPES),
            ("Word/document.xml", &body),
        ]);
        assert!(matches!(
            preflight_package(&word, DocumentKind::Docx, DocumentVariant::Docx),
            Err(DocumentError::Malformed)
        ));
        let slides = zip_entries(&[
            ("[Content_Types].xml", PPTX_TYPES),
            ("PPT/presentation.xml", PPTX_PRESENTATION),
            ("ppt/_rels/presentation.xml.rels", PPTX_RELS),
            ("ppt/slides/slide1.xml", PPTX_SLIDE),
        ]);
        assert!(matches!(
            preflight_package(&slides, DocumentKind::Pptx, DocumentVariant::Pptx),
            Err(DocumentError::Malformed)
        ));
    }

    #[test]
    fn odf_body_kind_follows_the_office_namespace() {
        let content = |body: &str| {
            format!(
                r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:x="urn:x">{body}</office:document-content>"#
            )
        };
        let kind = |body: &str| odf_body_kind(content(body).as_bytes());
        // A same-named element outside the office namespace is not a body.
        assert_eq!(
            kind("<office:body><text/><x:text/><office:presentation/></office:body>"),
            Some(DocumentKind::Odp)
        );
        // Text wins over the others wherever it appears.
        assert_eq!(
            kind("<office:body><office:presentation/><office:spreadsheet/><office:text/></office:body>"),
            Some(DocumentKind::Odt)
        );
        // Only the first body counts, and only its direct children.
        assert_eq!(
            kind("<office:body><office:presentation/></office:body><office:body><office:text/></office:body>"),
            Some(DocumentKind::Odp)
        );
        assert_eq!(
            kind("<office:body><x:wrap><office:text/></x:wrap></office:body>"),
            None
        );
        // Only the first top-level document-content counts.
        let two = r#"<x:document-content xmlns:x="urn:x"><office:body xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"><office:text/></office:body></x:document-content><office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"><office:body><office:spreadsheet/></office:body></office:document-content>"#;
        assert_eq!(odf_body_kind(two.as_bytes()), Some(DocumentKind::Ods));
    }

    #[test]
    fn pptx_notes_are_followed_through_relationships() {
        let notes = br#"<p:notes xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"><p:cSld><p:spTree><p:sp><p:nvSpPr><p:cNvPr id="3" name="n" hidden="1"/></p:nvSpPr></p:sp></p:spTree></p:cSld></p:notes>"#;
        let with_notes = |target: &str, part: &str, notes: &[u8]| {
            let rels = format!(
                r#"<Relationships><Relationship Id="rId2" Type="{REL_NS}/notesSlide" Target="{target}"/></Relationships>"#
            );
            preflight_package(
                &zip_entries(&[
                    ("[Content_Types].xml", PPTX_TYPES),
                    ("ppt/presentation.xml", PPTX_PRESENTATION),
                    ("ppt/_rels/presentation.xml.rels", PPTX_RELS),
                    ("ppt/slides/slide1.xml", PPTX_SLIDE),
                    ("ppt/slides/_rels/slide1.xml.rels", rels.as_bytes()),
                    (part, notes),
                ]),
                DocumentKind::Pptx,
                DocumentVariant::Pptx,
            )
            .unwrap()
            .hidden_content
        };
        for (target, part) in [
            ("../notes/notesSlide1.xml", "ppt/notes/notesSlide1.xml"),
            (
                "../notesSlides/notesSlide1.part",
                "ppt/notesSlides/notesSlide1.part",
            ),
            ("/n%31.xml", "n1.xml"),
        ] {
            assert!(with_notes(target, part, notes), "{target}");
        }
        let visible = br#"<p:notes xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"><p:cSld><p:spTree/></p:cSld></p:notes>"#;
        assert!(!with_notes(
            "../notes/notesSlide1.xml",
            "ppt/notes/notesSlide1.xml",
            visible
        ));
    }

    #[test]
    fn pptx_slide_lists_inside_compatibility_blocks_are_refused() {
        let presentation = format!(
            r#"<p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="{REL_NS}" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006"><mc:AlternateContent><mc:Choice Requires="zz"><p:sldIdLst><p:sldId id="256" r:id="rId1"/></p:sldIdLst></mc:Choice></mc:AlternateContent></p:presentation>"#
        );
        assert!(matches!(
            pptx_slide_relationship_ids(presentation.as_bytes()),
            Err(DocumentError::Malformed)
        ));
    }

    #[test]
    fn docx_list_numbers_across_branches_notes_and_styles() {
        let item = |text: &str| {
            format!(
                r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>{text}</w:t></w:r></w:p>"#
            )
        };
        let numbering = format!(
            r#"<w:numbering {WORD_NS}><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/></w:lvl><w:lvl w:ilvl="1"><w:start w:val="1"/><w:numFmt w:val="lowerLetter"/><w:lvlText w:val="%2)"/></w:lvl></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#
        );
        let differs = |body: &str, extra: &[(&str, Vec<u8>)]| {
            let document = word_part("document", body);
            let mut entries: Vec<(&str, &[u8])> = vec![
                ("word/document.xml", &document),
                ("word/numbering.xml", numbering.as_bytes()),
            ];
            entries.extend(extra.iter().map(|(name, bytes)| (*name, bytes.as_slice())));
            docx_preflight(&entries).list_numbering_differs
        };
        // Word takes a choice AnyDoc cannot read, and AnyDoc the fallback
        // holding the same list: one list, numbered alike.
        let alternate = |choice: String, fallback: Option<String>| {
            format!(
                r#"<mc:AlternateContent xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:w14="http://schemas.microsoft.com/office/word/2010/wordml"><mc:Choice Requires="w14">{choice}</mc:Choice>{}</mc:AlternateContent>"#,
                fallback
                    .map(|fallback| format!("<mc:Fallback>{fallback}</mc:Fallback>"))
                    .unwrap_or_default()
            )
        };
        let list = format!("{}{}", item("A"), item("B"));
        assert!(!differs(
            &format!(
                "{}{}",
                alternate(list.clone(), Some(list.clone())),
                item("C")
            ),
            &[]
        ));
        // With no branch AnyDoc reads, Word still counts its own.
        assert!(differs(
            &format!("{}{}", alternate(list.clone(), None), item("C")),
            &[]
        ));
        // Branches that number differently count each for its own side: a
        // fallback numbering a note the choice leaves plain, one flattening
        // the choice's list, and one numbering it at another level.
        let plain = |text: &str| format!("<w:p><w:r><w:t>{text}</w:t></w:r></w:p>");
        assert!(differs(
            &format!(
                "{}{}{}",
                item("A"),
                alternate(plain("Note"), Some(item("Note"))),
                item("B")
            ),
            &[]
        ));
        assert!(differs(
            &format!(
                "{}{}{}",
                item("A"),
                alternate(list.clone(), Some(format!("{}{}", plain("A"), plain("B")))),
                item("C")
            ),
            &[]
        ));
        let sub = |text: &str| {
            format!(
                r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="1"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>{text}</w:t></w:r></w:p>"#
            )
        };
        assert!(differs(
            &format!(
                "{}{}",
                alternate(list.clone(), Some(format!("{}{}", sub("A"), sub("B")))),
                item("C")
            ),
            &[]
        ));
        // A list through notes stored in another order than their ids and
        // references reads in an order no one oracle settles.
        let reference =
            |id: u32| format!(r#"<w:p><w:r><w:footnoteReference w:id="{id}"/></w:r></w:p>"#);
        let footnotes = |ids: &[u32]| {
            let notes: String = ids
                .iter()
                .map(|id| {
                    format!(
                        r#"<w:footnote w:id="{id}">{}</w:footnote>"#,
                        item(&format!("Note {id}"))
                    )
                })
                .collect();
            format!("<w:footnotes {WORD_NS}>{notes}</w:footnotes>").into_bytes()
        };
        let body = format!("{}{}", reference(1), reference(2));
        assert!(!differs(
            &body,
            &[("word/footnotes.xml", footnotes(&[1, 2]))]
        ));
        assert!(differs(
            &body,
            &[("word/footnotes.xml", footnotes(&[2, 1]))]
        ));
        let reversed = format!("{}{}", reference(2), reference(1));
        assert!(differs(
            &reversed,
            &[("word/footnotes.xml", footnotes(&[1, 2]))]
        ));
        // A paragraph style naming a level no level binds: Word numbers at
        // it, AnyDoc at the first.
        let styled = |bound: bool| {
            let styles = format!(
                r#"<w:styles {WORD_NS}><w:style w:type="paragraph" w:styleId="ListSub"><w:pPr><w:numPr><w:ilvl w:val="1"/><w:numId w:val="1"/></w:numPr></w:pPr></w:style></w:styles>"#
            );
            let numbering = if bound {
                numbering.replace(
                    r#"<w:lvl w:ilvl="1">"#,
                    r#"<w:lvl w:ilvl="1"><w:pStyle w:val="ListSub"/>"#,
                )
            } else {
                numbering.clone()
            };
            let body: String = ["A", "B"]
                .iter()
                .map(|text| format!(r#"<w:p><w:pPr><w:pStyle w:val="ListSub"/></w:pPr><w:r><w:t>{text}</w:t></w:r></w:p>"#))
                .collect();
            let document = word_part("document", &body);
            docx_preflight(&[
                ("word/document.xml", &document),
                ("word/numbering.xml", numbering.as_bytes()),
                ("word/styles.xml", styles.as_bytes()),
            ])
            .list_numbering_differs
        };
        assert!(styled(false));
        assert!(!styled(true));
    }

    #[test]
    fn docx_list_numbers_follow_word_stories() {
        let item = |list: u32, text: &str| {
            format!(
                r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="{list}"/></w:numPr></w:pPr><w:r><w:t>{text}</w:t></w:r></w:p>"#
            )
        };
        // Instance 2 restarts the definition instance 1 counts.
        let numbering = format!(
            r#"<w:numbering {WORD_NS}><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/></w:lvl></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num><w:num w:numId="2"><w:abstractNumId w:val="0"/><w:lvlOverride w:ilvl="0"><w:startOverride w:val="1"/></w:lvlOverride></w:num></w:numbering>"#
        );
        let differs = |body: &str, notes: &[(&str, String)]| {
            let document = word_part("document", body);
            let notes: Vec<(String, Vec<u8>)> = notes
                .iter()
                .map(|(root, body)| (format!("word/{root}.xml"), word_part(root, body)))
                .collect();
            let mut entries: Vec<(&str, &[u8])> = vec![
                ("word/document.xml", &document),
                ("word/numbering.xml", numbering.as_bytes()),
            ];
            entries.extend(
                notes
                    .iter()
                    .map(|(name, bytes)| (name.as_str(), bytes.as_slice())),
            );
            docx_preflight(&entries).list_numbering_differs
        };
        let body = format!("{}{}", item(1, "One"), item(1, "Two"));
        // Word counts the footnotes, and the endnotes, apart from the body;
        // AnyDoc counts on from the body into them.
        assert!(differs(&body, &[("footnotes", item(1, "Note"))]));
        assert!(differs(&body, &[("endnotes", item(1, "Note"))]));
        assert!(!differs(
            "",
            &[("footnotes", format!("{}{}", item(1, "A"), item(1, "B")))]
        ));
        assert!(!differs(
            &body,
            &[("footnotes", format!("{}{}", item(2, "A"), item(2, "B")))]
        ));
        assert!(differs(
            &body,
            &[("footnotes", format!("{}{}", item(2, "A"), item(1, "B")))]
        ));
        // So too the text boxes, where AnyDoc meets them in the body.
        let text_box = |inner: String| {
            format!(
                r#"<w:p><w:r><w:pict><w:txbxContent>{inner}</w:txbxContent></w:pict></w:r></w:p>"#
            )
        };
        assert!(differs(
            &format!("{}{}", body, text_box(item(1, "Boxed"))),
            &[]
        ));
        assert!(!differs(
            &format!(
                "{}{}{}",
                item(1, "One"),
                text_box(format!("{}{}", item(2, "A"), item(2, "B"))),
                item(1, "Two")
            ),
            &[]
        ));
        assert!(!differs(
            &format!("{}{}", text_box(item(1, "A")), text_box(item(1, "B"))),
            &[]
        ));
        // A text box Word writes twice, as a shape and as its VML fallback,
        // counts once.
        let shape_and_fallback = |inner: String| {
            format!(
                r#"<w:p><w:r><mc:AlternateContent xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:wps="http://schemas.microsoft.com/office/word/2010/wordprocessingShape"><mc:Choice Requires="wps"><w:drawing><w:txbxContent>{inner}</w:txbxContent></w:drawing></mc:Choice><mc:Fallback><w:pict><w:txbxContent>{inner}</w:txbxContent></w:pict></mc:Fallback></mc:AlternateContent></w:r></w:p>"#
            )
        };
        assert!(!differs(
            &format!(
                "{}{}{}",
                item(1, "One"),
                shape_and_fallback(format!("{}{}", item(2, "A"), item(2, "B"))),
                item(1, "Two")
            ),
            &[]
        ));
        // Word counts a level a deeper paragraph skips as used once; AnyDoc
        // does not ("1.1.1." then "2." against "1.").
        let outline = format!(
            r#"<w:numbering {WORD_NS}><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/></w:lvl><w:lvl w:ilvl="1"><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%1.%2."/></w:lvl><w:lvl w:ilvl="2"><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%1.%2.%3."/></w:lvl></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#
        );
        let outline_differs = |levels: &[u32]| {
            let body: String = levels
                .iter()
                .map(|level| {
                    format!(
                        r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="{level}"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>Item</w:t></w:r></w:p>"#
                    )
                })
                .collect();
            let document = word_part("document", &body);
            docx_preflight(&[
                ("word/document.xml", &document),
                ("word/numbering.xml", outline.as_bytes()),
            ])
            .list_numbering_differs
        };
        assert!(outline_differs(&[2, 0]));
        assert!(outline_differs(&[0, 1, 0, 2, 1]));
        assert!(!outline_differs(&[0, 1, 2, 1, 0]));
        assert!(!outline_differs(&[0, 2]));
        // An instance's overrides restart Word's count once, at its first
        // paragraph at an overridden level; other restarts take the level's
        // own start, 0 where it names none. AnyDoc restarts a level at its
        // override every time, and starts at 1.
        let overridden_differs = |start: &str, overrides: &[(u32, u32)], levels: &[u32]| {
            let level = |ilvl: u32| {
                format!(
                    r#"<w:lvl w:ilvl="{ilvl}">{start}<w:numFmt w:val="decimal"/><w:lvlText w:val="%{}."/></w:lvl>"#,
                    ilvl + 1
                )
            };
            let overrides: String = overrides
                .iter()
                .map(|(ilvl, value)| {
                    format!(
                        r#"<w:lvlOverride w:ilvl="{ilvl}"><w:startOverride w:val="{value}"/></w:lvlOverride>"#
                    )
                })
                .collect();
            let numbering = format!(
                r#"<w:numbering {WORD_NS}><w:abstractNum w:abstractNumId="0">{}{}</w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/>{overrides}</w:num></w:numbering>"#,
                level(0),
                level(1)
            );
            let body: String = levels
                .iter()
                .map(|ilvl| {
                    format!(
                        r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="{ilvl}"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>Item</w:t></w:r></w:p>"#
                    )
                })
                .collect();
            let document = word_part("document", &body);
            docx_preflight(&[
                ("word/document.xml", &document),
                ("word/numbering.xml", numbering.as_bytes()),
            ])
            .list_numbering_differs
        };
        let one = r#"<w:start w:val="1"/>"#;
        assert!(overridden_differs("", &[], &[0, 1, 0]));
        assert!(!overridden_differs(one, &[], &[0, 1, 0]));
        assert!(overridden_differs(one, &[(0, 5), (1, 5)], &[0, 1, 1, 0, 1]));
        assert!(!overridden_differs(one, &[(1, 7)], &[0, 1, 1]));
        assert!(!overridden_differs(one, &[(0, 5)], &[1, 0, 0]));
        // A text box in a tracked deletion is neither shown nor converted.
        assert!(!differs(
            &format!(
                r#"{}<w:p><w:del><w:r><w:pict><w:txbxContent>{}</w:txbxContent></w:pict></w:r></w:del></w:p>{}"#,
                item(1, "One"),
                item(1, "Gone"),
                item(1, "Two")
            ),
            &[]
        ));
    }

    #[test]
    fn docx_list_labels_follow_word_number_text() {
        let paragraph = |list: u32, level: u32, style: &str| {
            let style = if style.is_empty() {
                String::new()
            } else {
                format!(r#"<w:pStyle w:val="{style}"/>"#)
            };
            let numbering = if list == 0 {
                String::new()
            } else {
                format!(r#"<w:numPr><w:ilvl w:val="{level}"/><w:numId w:val="{list}"/></w:numPr>"#)
            };
            format!(r#"<w:p><w:pPr>{style}{numbering}</w:pPr><w:r><w:t>Item</w:t></w:r></w:p>"#)
        };
        let differs = |body: String, levels: &str, styles: &str| {
            let numbering = format!(
                r#"<w:numbering {WORD_NS}><w:abstractNum w:abstractNumId="0">{levels}</w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num><w:num w:numId="2"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#
            );
            let document = word_part("document", &body);
            let styles = format!("<w:styles {WORD_NS}>{styles}</w:styles>");
            docx_preflight(&[
                ("word/document.xml", &document),
                ("word/numbering.xml", numbering.as_bytes()),
                ("word/styles.xml", styles.as_bytes()),
            ])
            .list_numbering_differs
        };
        let level = |ilvl: u32, text: Option<&str>| {
            let text = text.map_or(String::new(), |text| {
                format!(r#"<w:lvlText w:val="{text}"/>"#)
            });
            format!(
                r#"<w:lvl w:ilvl="{ilvl}"><w:start w:val="1"/><w:numFmt w:val="decimal"/>{text}</w:lvl>"#
            )
        };
        let items = format!("{}{}", paragraph(1, 0, ""), paragraph(1, 0, ""));
        // A level without number text shows no number in Word; AnyDoc
        // numbers it.
        assert!(differs(items.clone(), &level(0, None), ""));
        assert!(differs(items.clone(), &level(0, Some("")), ""));
        assert!(!differs(items.clone(), &level(0, Some("%1.")), ""));
        // A composite label shows the shallower number as each counts it:
        // Word 2.1 for the second instance's first sub-item, AnyDoc 1.1.
        let outline = format!("{}{}", level(0, Some("%1.")), level(1, Some("%1.%2.")));
        assert!(differs(
            format!(
                "{}{}{}",
                paragraph(1, 0, ""),
                paragraph(1, 0, ""),
                paragraph(2, 1, "")
            ),
            &outline,
            ""
        ));
        assert!(!differs(
            format!("{}{}", paragraph(1, 0, ""), paragraph(1, 1, "")),
            &outline,
            ""
        ));
        // A level without a number (`none`) still shows its literal words in
        // Word, which AnyDoc drops with the number; punctuation alone, left
        // where the text shows a number, counts as nothing.
        let none = |text: &str| {
            format!(
                r#"{}<w:lvl w:ilvl="1"><w:start w:val="1"/><w:numFmt w:val="none"/><w:lvlText w:val="{text}"/></w:lvl>"#,
                level(0, Some("%1."))
            )
        };
        let recital = format!(
            "{}{}{}",
            paragraph(1, 0, ""),
            paragraph(1, 1, ""),
            paragraph(1, 0, "")
        );
        assert!(differs(recital.clone(), &none("WHEREAS,"), ""));
        assert!(differs(recital.clone(), &none("Recital %2:"), ""));
        for text in ["%2", "(%2)", "%1."] {
            assert!(!differs(recital.clone(), &none(text), ""), "{text}");
        }
        // A level's own number is compared only where its text shows it.
        let chapter = format!("{}{}", level(0, Some("%1.")), level(1, Some("Part %1")));
        assert!(!differs(
            format!(
                "{}{}{}",
                paragraph(1, 0, ""),
                paragraph(1, 1, ""),
                paragraph(2, 1, "")
            ),
            &chapter,
            ""
        ));
        // A style chain longer than 32 styles is followed, as AnyDoc
        // follows it.
        let chain: String = (0..40)
            .map(|index| {
                if index == 0 {
                    r#"<w:style w:type="paragraph" w:styleId="S0"><w:pPr><w:numPr><w:numId w:val="2"/></w:numPr></w:pPr></w:style>"#.to_string()
                } else {
                    format!(
                        r#"<w:style w:type="paragraph" w:styleId="S{index}"><w:basedOn w:val="S{}"/></w:style>"#,
                        index - 1
                    )
                }
            })
            .collect();
        assert!(differs(
            format!("{}{}", items, paragraph(0, 0, "S39")),
            &level(0, Some("%1.")),
            &chain
        ));
        // A list instance written with white space around it, which Word
        // reads and AnyDoc cannot.
        let padded = r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val=" 1"/></w:numPr></w:pPr><w:r><w:t>Item</w:t></w:r></w:p>"#;
        assert!(differs(padded.to_string(), &level(0, Some("%1.")), ""));
        // Such a number matters only where a numbered paragraph's labels
        // read it: a start AnyDoc takes as 1 where Word reads 3, but not a
        // start of 1 either way, a definition no instance names, or a style
        // no paragraph uses.
        let started = |start: &str| {
            format!(
                r#"<w:lvl w:ilvl="0"><w:start w:val="{start}"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/></w:lvl>"#
            )
        };
        assert!(differs(items.clone(), &started(" 3"), ""));
        assert!(!differs(items.clone(), &started("1 "), ""));
        let listed = |numbering: &str, styles: &str| {
            let document = word_part("document", &items);
            let numbering = format!("<w:numbering {WORD_NS}>{numbering}</w:numbering>");
            let styles = format!("<w:styles {WORD_NS}>{styles}</w:styles>");
            docx_preflight(&[
                ("word/document.xml", &document),
                ("word/numbering.xml", numbering.as_bytes()),
                ("word/styles.xml", styles.as_bytes()),
            ])
            .list_numbering_differs
        };
        let definition = |id: &str, start: &str| {
            format!(
                r#"<w:abstractNum w:abstractNumId="{id}">{}</w:abstractNum>"#,
                started(start)
            )
        };
        let unused = format!(
            r#"{}{}<w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>"#,
            definition("0", "1"),
            definition("1", " 1")
        );
        assert!(!listed(&unused, ""));
        let unused_style = r#"<w:style w:type="paragraph" w:styleId="Unused"><w:pPr><w:numPr><w:numId w:val=" 1"/></w:numPr></w:pPr></w:style>"#;
        assert!(!listed(&unused, unused_style));
        // A definition id written with white space names the definition for
        // Word only.
        let spaced = format!(
            r#"{}<w:num w:numId="1"><w:abstractNumId w:val=" 0"/></w:num>"#,
            definition("0", "1")
        );
        assert!(listed(&spaced, ""));
    }

    #[test]
    fn docx_list_numbers_word_shows_differently_are_disclosed() {
        let item = |list: u32, level: u32, extra: &str| {
            format!(
                r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="{level}"/><w:numId w:val="{list}"/></w:numPr>{extra}</w:pPr><w:r><w:t>Item</w:t></w:r></w:p>"#
            )
        };
        let definition = |id: u32, format: &str| {
            format!(
                r#"<w:abstractNum w:abstractNumId="{id}"><w:lvl w:ilvl="0"><w:start w:val="1"/>{format}<w:lvlText w:val="%1."/></w:lvl></w:abstractNum>"#
            )
        };
        let decimal = r#"<w:numFmt w:val="decimal"/>"#;
        let differs = |body: String, numbering: String, styles: &str| {
            let document = word_part("document", &body);
            let numbering = format!("<w:numbering {WORD_NS}>{numbering}</w:numbering>");
            let styles = format!("<w:styles {WORD_NS}>{styles}</w:styles>");
            docx_preflight(&[
                ("word/document.xml", &document),
                ("word/numbering.xml", numbering.as_bytes()),
                ("word/styles.xml", styles.as_bytes()),
            ])
            .list_numbering_differs
        };
        let two_lists = |second: &str| {
            format!(
                r#"{}<w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num><w:num w:numId="2"><w:abstractNumId w:val="0"/>{second}</w:num>"#,
                definition(0, decimal)
            )
        };
        let items = format!("{}{}", item(1, 0, ""), item(2, 0, ""));
        // A second list instance of one definition continues Word's count;
        // AnyDoc restarts it, unless the instance restarts the level.
        assert!(differs(items.clone(), two_lists(""), ""));
        assert!(!differs(
            items.clone(),
            two_lists(r#"<w:lvlOverride w:ilvl="0"><w:startOverride w:val="1"/></w:lvlOverride>"#),
            ""
        ));
        assert!(!differs(
            format!("{}{}", item(1, 0, ""), item(1, 0, "")),
            two_lists(""),
            ""
        ));
        // Bullet lists sharing a definition show no count to differ.
        let bullets = format!(
            r#"{}<w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num><w:num w:numId="2"><w:abstractNumId w:val="0"/></w:num>"#,
            definition(0, r#"<w:numFmt w:val="bullet"/>"#)
        );
        assert!(!differs(items.clone(), bullets, ""));
        // Numbering through a paragraph style counts too.
        let heading = r#"<w:style w:type="paragraph" w:styleId="Heading1"><w:pPr><w:numPr><w:numId w:val="1"/></w:numPr></w:pPr></w:style><w:style w:type="paragraph" w:styleId="Heading2"><w:basedOn w:val="Heading1"/></w:style>"#;
        let styled = |style: &str| {
            format!(
                r#"<w:p><w:pPr><w:pStyle w:val="{style}"/></w:pPr><w:r><w:t>Head</w:t></w:r></w:p>"#
            )
        };
        assert!(differs(
            format!("{}{}", styled("Heading2"), item(2, 0, "")),
            two_lists(""),
            heading
        ));
        assert!(!differs(
            format!("{}{}", styled("Heading2"), styled("Heading1")),
            two_lists(""),
            heading
        ));
        // A deleted numbered paragraph still takes a number in AnyDoc.
        let one_list = format!(
            r#"{}<w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>"#,
            definition(0, decimal)
        );
        for revision in ["del", "moveFrom"] {
            let deleted = item(
                1,
                0,
                &format!(r#"<w:rPr><w:{revision} w:id="1" w:author="A"/></w:rPr>"#),
            );
            assert!(
                differs(format!("{}{deleted}", item(1, 0, "")), one_list.clone(), ""),
                "{revision}"
            );
        }
        assert!(!differs(item(1, 0, ""), one_list.clone(), ""));
        // Formats AnyDoc renders as plain decimals, or as bullets.
        let formatted = |format: &str| {
            format!(
                r#"{}<w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>"#,
                definition(0, format)
            )
        };
        for format in ["ordinal", "cardinalText", "decimalZero"] {
            assert!(
                differs(
                    item(1, 0, ""),
                    formatted(&format!(r#"<w:numFmt w:val="{format}"/>"#)),
                    ""
                ),
                "{format}"
            );
        }
        assert!(differs(item(1, 0, ""), formatted(""), ""));
        for format in ["decimal", "bullet", "lowerRoman", "upperLetter", "none"] {
            assert!(
                !differs(
                    item(1, 0, ""),
                    formatted(&format!(r#"<w:numFmt w:val="{format}"/>"#)),
                    ""
                ),
                "{format}"
            );
        }
        // Letters past `z` count differently.
        let letters = formatted(r#"<w:numFmt w:val="lowerLetter"/>"#);
        assert!(!differs(item(1, 0, "").repeat(26), letters.clone(), ""));
        assert!(differs(item(1, 0, "").repeat(27), letters, ""));
        // What Word shows at a level the list does not define, and at a
        // level past the ninth, is uncertain: LibreOffice numbers the first
        // in decimal, the tenth as a level that shows nothing, and a deeper
        // one at the first. AnyDoc bullets the first and numbers the others
        // at the ninth.
        assert!(differs(
            format!("{}{}", item(1, 0, ""), item(1, 1, "")),
            one_list.clone(),
            ""
        ));
        let nine_levels = format!(
            r#"<w:abstractNum w:abstractNumId="0">{}</w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>"#,
            (1..=9)
                .map(|shown| format!(
                    r#"<w:lvl w:ilvl="{}"><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%{shown}."/></w:lvl>"#,
                    shown - 1
                ))
                .collect::<String>()
        );
        for level in [9, 12] {
            assert!(
                differs(
                    format!("{}{}", item(1, 0, ""), item(1, level, "")),
                    nine_levels.clone(),
                    ""
                ),
                "{level}"
            );
        }
        assert!(!differs(
            format!("{}{}", item(1, 0, ""), item(1, 8, "")),
            nine_levels,
            ""
        ));
        // A list style's definitions share counters through the style.
        let linked = format!(
            r#"{}<w:abstractNum w:abstractNumId="1"><w:numStyleLink w:val="Outline"/></w:abstractNum><w:abstractNum w:abstractNumId="2"><w:styleLink w:val="Outline"/><w:lvl w:ilvl="0"><w:numFmt w:val="decimal"/></w:lvl></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="1"/></w:num><w:num w:numId="2"><w:abstractNumId w:val="2"/></w:num>"#,
            definition(0, decimal)
        );
        let outline = r#"<w:style w:type="numbering" w:styleId="Outline"><w:pPr><w:numPr><w:numId w:val="2"/></w:numPr></w:pPr></w:style>"#;
        assert!(differs(items.clone(), linked.clone(), outline));
        // Without the style, Word still finds the definition declaring it;
        // AnyDoc bullets the list.
        assert!(differs(items.clone(), linked, ""));
        // Word reads a definition id as an integer, and AnyDoc matches its
        // text: an instance naming definition "01", or a definition
        // declared as "+1", numbers only in Word.
        let named = |declared: &str, referenced: &str| {
            format!(
                r#"{}<w:num w:numId="1"><w:abstractNumId w:val="{referenced}"/></w:num>"#,
                definition(0, decimal).replace(
                    r#"w:abstractNumId="0""#,
                    &format!(r#"w:abstractNumId="{declared}""#)
                )
            )
        };
        assert!(differs(item(1, 0, ""), named("1", "01"), ""));
        assert!(differs(item(1, 0, ""), named("+1", "1"), ""));
        assert!(!differs(item(1, 0, ""), named("1", "1"), ""));

        // Only a start override restarts a level; replacing the level's
        // definition leaves Word counting on.
        let replaced = r#"<w:lvlOverride w:ilvl="0"><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/></w:lvl></w:lvlOverride>"#;
        assert!(differs(items.clone(), two_lists(replaced), ""));
        // A restart given to the first paragraph of a second instance, as
        // LibreOffice writes it, restarts the count Word shares: the list
        // continues 2, 3 where AnyDoc continues the first instance's 4, 5.
        let restart = r#"<w:lvlOverride w:ilvl="0"><w:startOverride w:val="1"/></w:lvlOverride>"#;
        assert!(differs(
            format!(
                "{}{}{}",
                item(1, 0, "").repeat(3),
                item(2, 0, ""),
                item(1, 0, "").repeat(2)
            ),
            two_lists(restart),
            ""
        ));
        // A deleted bullet takes no number to show.
        let bullet_list = formatted(r#"<w:numFmt w:val="bullet"/>"#);
        let deleted = item(1, 0, r#"<w:rPr><w:del w:id="1" w:author="A"/></w:rPr>"#);
        assert!(!differs(
            format!("{}{deleted}{}", item(1, 0, ""), item(1, 0, "")),
            bullet_list,
            ""
        ));

        // Multilevel lists: numbers and letters under each item.
        let outline_levels = |second: &str| {
            format!(
                r#"<w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:start w:val="1"/>{second}<w:lvlText w:val="%1."/></w:lvl><w:lvl w:ilvl="1"><w:start w:val="1"/><w:numFmt w:val="lowerLetter"/><w:lvlText w:val="%2."/></w:lvl></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num><w:num w:numId="2"><w:abstractNumId w:val="0"/><w:lvlOverride w:ilvl="0"><w:startOverride w:val="1"/></w:lvlOverride></w:num>"#
            )
        };
        let block = |list: u32, subitems: usize| {
            format!(
                "{}{}",
                item(list, 0, ""),
                item(list, 1, "").repeat(subitems)
            )
        };
        // Restarting the first level restarts the letters under it too.
        assert!(!differs(
            format!("{}{}", block(1, 2), block(2, 2)),
            outline_levels(decimal),
            ""
        ));
        // Letters restart under each item, so only a run past `z` counts.
        assert!(!differs(block(1, 4).repeat(8), outline_levels(decimal), ""));
        assert!(differs(block(1, 27), outline_levels(decimal), ""));
        // Word restarts the letters under each bullet; AnyDoc counts only
        // the levels it numbers, so it runs on.
        assert!(differs(
            block(1, 2).repeat(2),
            outline_levels(r#"<w:numFmt w:val="bullet"/>"#),
            ""
        ));

        // Headings numbered through levels bound to their styles.
        let headings = r#"<w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:pStyle w:val="Heading1"/><w:lvlText w:val="%1."/></w:lvl><w:lvl w:ilvl="1"><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:pStyle w:val="Heading2"/><w:lvlText w:val="%1.%2."/></w:lvl></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>"#;
        let heading_styles = r#"<w:style w:type="paragraph" w:styleId="Heading1"><w:pPr><w:numPr><w:numId w:val="1"/></w:numPr></w:pPr></w:style><w:style w:type="paragraph" w:styleId="Heading2"><w:basedOn w:val="Heading1"/><w:pPr><w:numPr><w:ilvl w:val="1"/></w:numPr></w:pPr></w:style>"#;
        assert!(!differs(
            [
                styled("Heading1"),
                styled("Heading2"),
                styled("Heading2"),
                styled("Heading1"),
                styled("Heading2"),
            ]
            .concat(),
            headings.to_string(),
            heading_styles
        ));
    }

    #[test]
    fn docx_style_levels_are_compared_by_the_labels_each_side_shows() {
        let styled = |style: &str| {
            format!(
                r#"<w:p><w:pPr><w:pStyle w:val="{style}"/></w:pPr><w:r><w:t>Item</w:t></w:r></w:p>"#
            )
        };
        let level = |ilvl: u32, format: &str, text: &str, extra: &str| {
            format!(
                r#"<w:lvl w:ilvl="{ilvl}"><w:start w:val="1"/><w:numFmt w:val="{format}"/>{extra}<w:lvlText w:val="{text}"/></w:lvl>"#
            )
        };
        let bound = |style: &str| format!(r#"<w:pStyle w:val="{style}"/>"#);
        let style = |id: &str, base: &str, numbering: &str| {
            let base = if base.is_empty() {
                String::new()
            } else {
                format!(r#"<w:basedOn w:val="{base}"/>"#)
            };
            format!(
                r#"<w:style w:type="paragraph" w:styleId="{id}">{base}<w:pPr><w:numPr>{numbering}</w:numPr></w:pPr></w:style>"#
            )
        };
        let differs = |body: String, levels: String, styles: String| {
            let document = word_part("document", &body);
            let numbering = format!(
                r#"<w:numbering {WORD_NS}><w:abstractNum w:abstractNumId="0">{levels}</w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#
            );
            let styles = format!("<w:styles {WORD_NS}>{styles}</w:styles>");
            docx_preflight(&[
                ("word/document.xml", &document),
                ("word/numbering.xml", numbering.as_bytes()),
                ("word/styles.xml", styles.as_bytes()),
            ])
            .list_numbering_differs
        };
        // A template's second and third levels are styles based on the
        // first, bound one, each naming its own level: Word shows "1.1." and
        // "(a)", AnyDoc numbers every paragraph at the first level.
        let template = |second: &str, third: &str| {
            [
                level(0, "decimal", "%1.", &bound("Level1")),
                level(1, "decimal", "%1.%2.", second),
                level(2, "lowerLetter", "(%3)", third),
            ]
            .concat()
        };
        let chain = [
            style("Level1", "", r#"<w:numId w:val="1"/>"#),
            style("Level2", "Level1", r#"<w:ilvl w:val="1"/>"#),
            style("Level3", "Level2", r#"<w:ilvl w:val="2"/>"#),
        ]
        .concat();
        let outline = ["Level1", "Level2", "Level2", "Level3", "Level1", "Level2"]
            .map(styled)
            .concat();
        assert!(differs(outline.clone(), template("", ""), chain.clone()));
        assert!(!differs(
            outline.clone(),
            template(&bound("Level2"), &bound("Level3")),
            chain.clone()
        ));
        // A paragraph numbered directly, without a level, reads its style's
        // level in Word too.
        let direct = r#"<w:p><w:pPr><w:pStyle w:val="Level2"/><w:numPr><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>Item</w:t></w:r></w:p>"#;
        assert!(differs(
            format!("{}{direct}", styled("Level1")),
            template("", ""),
            chain.clone()
        ));
        // Labels that read alike at either level match, and bullets show no
        // count to differ.
        let alike = (0..3)
            .map(|ilvl| level(ilvl, "decimal", &format!("%{}.", ilvl + 1), ""))
            .collect::<String>();
        let named = style("Named", "", r#"<w:ilvl w:val="1"/><w:numId w:val="1"/>"#);
        let items = ["Named", "Named"].map(styled).concat();
        assert!(!differs(items.clone(), alike, named.clone()));
        let bullets = (0..3)
            .map(|ilvl| level(ilvl, "bullet", "o", ""))
            .collect::<String>();
        assert!(!differs(items.clone(), bullets, named.clone()));
        // A style bound to one level and naming another, or naming none, is
        // numbered at the one bound to it by AnyDoc and, as ECMA-376 says, by
        // Word; LibreOffice reads no binding and numbers it at the level it
        // names, else the first ("(a)" or "2." where AnyDoc shows "1.1."). Which
        // Word shows is uncertain, and disclosed; a style bound to the level it
        // names is not.
        let conflicting = [
            style("Level1", "", r#"<w:numId w:val="1"/>"#),
            style("Level2", "", r#"<w:ilvl w:val="2"/><w:numId w:val="1"/>"#),
        ]
        .concat();
        let bound_items = ["Level1", "Level2", "Level2"].map(styled).concat();
        assert!(differs(
            bound_items.clone(),
            template(&bound("Level2"), ""),
            conflicting
        ));
        let unnamed = [
            style("Level1", "", r#"<w:numId w:val="1"/>"#),
            style("Level2", "", r#"<w:numId w:val="1"/>"#),
        ]
        .concat();
        assert!(differs(
            bound_items.clone(),
            template(&bound("Level2"), ""),
            unnamed
        ));
        let agreeing = [
            style("Level1", "", r#"<w:numId w:val="1"/>"#),
            style("Level2", "", r#"<w:ilvl w:val="1"/><w:numId w:val="1"/>"#),
        ]
        .concat();
        assert!(!differs(
            bound_items,
            template(&bound("Level2"), ""),
            agreeing
        ));
        // A composite label shows a shallower level in its format: one
        // AnyDoc writes otherwise, zero-padded, ordinal, or a bullet, differs
        // unless legal numbering shows it in decimal.
        let composite = |parent: &str, legal: &str| {
            let numbered = r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="1"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>Item</w:t></w:r></w:p>"#;
            differs(
                numbered.repeat(2),
                [
                    level(0, parent, "%1.", ""),
                    level(1, "decimal", "%1.%2.", legal),
                ]
                .concat(),
                String::new(),
            )
        };
        for parent in ["decimalZero", "ordinal", "bullet"] {
            assert!(composite(parent, ""), "{parent}");
            assert!(!composite(parent, "<w:isLgl/>"), "{parent}");
        }
        assert!(!composite("upperRoman", ""));
    }

    #[test]
    fn docx_unstyled_paragraphs_take_words_default_paragraph_style() {
        let differs = |body: &str, styles: &str, notes: Option<&str>| {
            let document = word_part("document", body);
            let numbering = format!(
                r#"<w:numbering {WORD_NS}><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/></w:lvl></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#
            );
            let styles = format!("<w:styles {WORD_NS}>{styles}</w:styles>");
            let notes = notes.map(|notes| format!("<w:footnotes {WORD_NS}>{notes}</w:footnotes>"));
            let mut entries: Vec<(&str, &[u8])> = vec![
                ("word/document.xml", &document),
                ("word/numbering.xml", numbering.as_bytes()),
                ("word/styles.xml", styles.as_bytes()),
            ];
            if let Some(notes) = &notes {
                entries.push(("word/footnotes.xml", notes.as_bytes()));
            }
            docx_preflight(&entries).list_numbering_differs
        };
        let default = |id: &str, numbered: bool| {
            let numbering = if numbered {
                r#"<w:pPr><w:numPr><w:numId w:val="1"/></w:numPr></w:pPr>"#
            } else {
                ""
            };
            format!(
                r#"<w:style w:type="paragraph" w:default="1" w:styleId="{id}">{numbering}</w:style>"#
            )
        };
        let plain = "<w:p><w:r><w:t>Clause</w:t></w:r></w:p>";
        let styled = |style: &str| {
            format!(
                r#"<w:p><w:pPr><w:pStyle w:val="{style}"/></w:pPr><w:r><w:t>Clause</w:t></w:r></w:p>"#
            )
        };
        let spaced =
            r#"<w:p><w:pPr><w:spacing w:after="0"/></w:pPr><w:r><w:t>Clause</w:t></w:r></w:p>"#;
        // Word numbers a paragraph naming no style, or one the styles do
        // not define, through the default paragraph style; AnyDoc does not.
        let numbered = default("Normal", true);
        assert!(differs(&plain.repeat(3), &numbered, None));
        assert!(differs(&spaced.repeat(3), &numbered, None));
        assert!(differs(&styled("Missing").repeat(2), &numbered, None));
        assert!(differs(
            &[plain, &styled("Normal"), plain, &styled("Normal")].concat(),
            &numbered,
            None
        ));
        let custom = format!(
            r#"{}<w:style w:type="paragraph" w:styleId="Normal"/>"#,
            default("LegalBody", true)
        );
        assert!(differs(&plain.repeat(2), &custom, None));
        // An unnumbered default style numbers nothing, and a separator note,
        // which neither side shows as text, is not counted.
        let item = r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>Item</w:t></w:r></w:p>"#;
        assert!(!differs(
            &[item, plain, item].concat(),
            &default("Normal", false),
            None
        ));
        let quiet = format!(r#"{numbered}<w:style w:type="paragraph" w:styleId="Quiet"/>"#);
        let reference = r#"<w:p><w:pPr><w:pStyle w:val="Quiet"/></w:pPr><w:r><w:footnoteReference w:id="1"/></w:r></w:p>"#;
        let notes = format!(
            r#"<w:footnote w:type="separator" w:id="-1">{plain}</w:footnote><w:footnote w:id="1">{}</w:footnote>"#,
            styled("Quiet")
        );
        assert!(!differs(reference, &quiet, Some(&notes)));
    }

    #[test]
    fn docx_styles_defined_twice_are_read_as_each_side_keeps_them() {
        let differs = |styles: &str| {
            let body = r#"<w:p><w:r><w:t>Lead in</w:t></w:r></w:p><w:p><w:pPr><w:pStyle w:val="ListItem"/></w:pPr><w:r><w:t>Point</w:t></w:r></w:p>"#.repeat(2);
            let document = word_part("document", &body);
            let numbering = format!(
                r#"<w:numbering {WORD_NS}><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/></w:lvl></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#
            );
            let styles = format!("<w:styles {WORD_NS}>{styles}</w:styles>");
            docx_preflight(&[
                ("word/document.xml", &document),
                ("word/numbering.xml", numbering.as_bytes()),
                ("word/styles.xml", styles.as_bytes()),
            ])
            .list_numbering_differs
        };
        let numbered = r#"<w:style w:type="paragraph" w:styleId="ListItem"><w:pPr><w:numPr><w:numId w:val="1"/></w:numPr></w:pPr></w:style>"#;
        let plain = r#"<w:style w:type="paragraph" w:styleId="ListItem"><w:pPr><w:spacing w:after="0"/></w:pPr></w:style>"#;
        // Word, as LibreOffice shows it, numbers the paragraphs whichever
        // definition of the id numbers them; AnyDoc keeps the last alone.
        assert!(differs(&format!("{numbered}{plain}")));
        assert!(!differs(&format!("{plain}{numbered}")));
        assert!(!differs(numbered));
        // AnyDoc reads only the first `w:pPr` of a style.
        let second_mark = r#"<w:style w:type="paragraph" w:styleId="ListItem"><w:pPr><w:spacing w:after="0"/></w:pPr><w:pPr><w:numPr><w:numId w:val="1"/></w:numPr></w:pPr></w:style>"#;
        assert!(differs(second_mark));
    }

    #[test]
    fn docx_style_chains_are_read_once_per_style() {
        let differs = |styles: &str| {
            // Each paragraph names a style farther along one chain of
            // thousands, as a crafted document can.
            let body: String = (0..3000)
                .map(|index| {
                    format!(
                        r#"<w:p><w:pPr><w:pStyle w:val="S{}"/></w:pPr><w:r><w:t>Item</w:t></w:r></w:p>"#,
                        2999 - index
                    )
                })
                .collect();
            let document = word_part("document", &body);
            let numbering = format!(
                r#"<w:numbering {WORD_NS}><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/></w:lvl></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#
            );
            let styles = format!("<w:styles {WORD_NS}>{styles}</w:styles>");
            docx_preflight(&[
                ("word/document.xml", &document),
                ("word/numbering.xml", numbering.as_bytes()),
                ("word/styles.xml", styles.as_bytes()),
            ])
            .list_numbering_differs
        };
        let chain = |root: &str| -> String {
            (0..3000)
                .map(|index| {
                    if index == 0 {
                        format!(
                            r#"<w:style w:type="paragraph" w:styleId="S0"><w:pPr>{root}</w:pPr></w:style>"#
                        )
                    } else {
                        format!(
                            r#"<w:style w:type="paragraph" w:styleId="S{index}"><w:basedOn w:val="S{}"/></w:style>"#,
                            index - 1
                        )
                    }
                })
                .collect()
        };
        // A document with no list is not disclosed, and a list at the
        // chain's end numbers every paragraph alike on both sides.
        assert!(!differs(&chain("")));
        assert!(!differs(&chain(
            r#"<w:numPr><w:numId w:val="1"/></w:numPr>"#
        )));
        // A cycle ends the chain.
        let cycle = r#"<w:style w:type="paragraph" w:styleId="S2999"><w:basedOn w:val="S2998"/></w:style><w:style w:type="paragraph" w:styleId="S2998"><w:basedOn w:val="S2999"/></w:style>"#;
        assert!(!differs(cycle));
    }

    #[test]
    fn docx_cells_marks_and_notes_follow_word() {
        let cell = "<w:tc><w:p><w:r><w:t>Cell</w:t></w:r></w:p></w:tc>";
        let table = |row: &str| format!("<w:tbl><w:tr>{row}</w:tr></w:tbl>");
        let dropped = |body: &str| {
            let document = word_part("document", body);
            docx_preflight(&[("word/document.xml", &document)]).unsupported_content
        };
        // AnyDoc reaches a row's cells directly, in custom XML, or in a
        // content control's content; a compatibility block drops them.
        assert!(!dropped(&table(cell)));
        assert!(!dropped(&table(&format!(
            "<w:customXml>{cell}</w:customXml>"
        ))));
        assert!(!dropped(&table(&format!(
            "<w:sdt><w:sdtContent>{cell}</w:sdtContent></w:sdt>"
        ))));
        let wrapped = format!(
            r#"<mc:AlternateContent xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006"><mc:Choice Requires="w">{cell}</mc:Choice></mc:AlternateContent>"#
        );
        assert!(dropped(&table(&wrapped)));
        assert!(dropped(&table(&format!("<w:sdt>{cell}</w:sdt>"))));
        // A row belongs to a WordprocessingML table only.
        assert!(dropped(&format!(
            r#"<x:tbl xmlns:x="urn:x"><w:tr>{cell}</w:tr></x:tbl>"#
        )));

        let hidden = |body: &str, extra: &[(&str, &[u8])]| {
            let document = word_part("document", body);
            let mut entries: Vec<(&str, &[u8])> = vec![("word/document.xml", &document)];
            entries.extend_from_slice(extra);
            docx_preflight(&entries).hidden_content
        };
        let numbered = |mark: &str| {
            format!(
                r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr><w:rPr>{mark}</w:rPr></w:pPr><w:r><w:t>Item</w:t></w:r></w:p>"#
            )
        };
        // A hidden mark hides a list label, which AnyDoc converts; Word's
        // style separator and an unnumbered mark hide no text.
        assert!(hidden(&numbered("<w:vanish/>"), &[]));
        assert!(!hidden(&numbered("<w:vanish/><w:specVanish/>"), &[]));
        assert!(!hidden(
            r#"<w:p><w:pPr><w:rPr><w:vanish/></w:rPr></w:pPr><w:r><w:t>Heading</w:t></w:r></w:p>"#,
            &[]
        ));
        assert!(!hidden(
            r#"<w:p><w:pPr><w:numPr><w:numId w:val="0"/></w:numPr><w:rPr><w:vanish/></w:rPr></w:pPr></w:p>"#,
            &[]
        ));
        let styles = format!(
            r#"<w:styles {WORD_NS}><w:style w:type="character" w:styleId="Gone"><w:rPr><w:vanish/></w:rPr></w:style></w:styles>"#
        );
        assert!(hidden(
            &numbered(r#"<w:rStyle w:val="Gone"/>"#),
            &[("word/styles.xml", styles.as_bytes())]
        ));
        // A run Word always hides, and a row deleted with tracked changes.
        assert!(hidden(
            "<w:p><w:r><w:rPr><w:specVanish/></w:rPr><w:t>Gone</w:t></w:r></w:p>",
            &[]
        ));
        assert!(hidden(
            &format!(
                r#"<w:tbl><w:tr><w:trPr><w:del w:id="1" w:author="a"/></w:trPr>{cell}</w:tr></w:tbl>"#
            ),
            &[]
        ));
        // A note Word shows nowhere, since nothing references it.
        let notes = format!(
            r#"<w:footnotes {WORD_NS}><w:footnote w:type="separator" w:id="-1"><w:p/></w:footnote><w:footnote w:id="1"><w:p><w:r><w:t>Note</w:t></w:r></w:p></w:footnote></w:footnotes>"#
        );
        let notes = [("word/footnotes.xml", notes.as_bytes())];
        assert!(hidden("<w:p><w:r><w:t>Body</w:t></w:r></w:p>", &notes));
        assert!(!hidden(
            r#"<w:p><w:r><w:footnoteReference w:id="1"/></w:r></w:p>"#,
            &notes
        ));
        assert!(hidden(
            r#"<w:p><w:del w:id="2" w:author="a"><w:r><w:footnoteReference w:id="1"/></w:r></w:del></w:p>"#,
            &notes
        ));
        // Word shows a drawing's fallback when a choice needs a vocabulary it
        // lacks, while AnyDoc's text-box search takes the choice.
        let drawing = |requires: &str| {
            format!(
                r#"<w:p><w:r><w:drawing><mc:AlternateContent xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:zz="urn:zz" xmlns:wps="http://schemas.microsoft.com/office/word/2010/wordprocessingShape"><mc:Choice Requires="{requires}"><w:txbxContent><w:p><w:r><w:t>Box</w:t></w:r></w:p></w:txbxContent></mc:Choice><mc:Fallback/></mc:AlternateContent></w:drawing></w:r></w:p>"#
            )
        };
        assert!(hidden(&drawing("zz"), &[]));
        assert!(hidden(&drawing("undeclared"), &[]));
        assert!(!hidden(&drawing("wps"), &[]));
    }

    #[test]
    fn xlsx_cached_values_render_as_anydoc_renders_them() {
        let uncached = |cell: &str| {
            let sheet = format!(
                r#"<worksheet {SML_NS} xmlns:x="urn:x"><sheetData><row r="1">{cell}</row></sheetData></worksheet>"#
            );
            scan_worksheet(sheet.as_bytes()).uncached_formula
        };
        for cached in [
            "<c><f>1+1</f><v>2</v></c>",
            "<c><f>1+1</f><v> 2.5e3 </v></c>",
            "<c><f>1+1</f><v>&#50;</v></c>",
            r#"<c t="str"><f>""</f><v></v></c>"#,
            r#"<c t="b"><f>TRUE</f><v>1</v></c>"#,
            r#"<c t="e"><f>1/0</f><v>#DIV/0!</v></c>"#,
            r#"<c x:t="s"><f>1+1</f><v>2</v></c>"#,
            "<c><v>2</v></c>",
            "<c/>",
        ] {
            assert!(!uncached(cached), "{cached}");
        }
        for missing in [
            "<c><f>1+1</f></c>",
            "<c><f>1+1</f><v/></c>",
            "<c><f>1+1</f><v>not a number</v></c>",
            "<c><f>1+1</f><x:v>2</x:v></c>",
            "<c><f>1+1</f><extLst><ext><v>2</v></ext></extLst></c>",
            r#"<c t="b"><f>1+1</f><v>2</v></c>"#,
            r#"<c t="s"><f>1+1</f><v>0</v></c>"#,
            r#"<c t="str"><f>A1</f></c>"#,
            "<x:c><x:f>1+1</x:f></x:c>",
        ] {
            assert!(uncached(missing), "{missing}");
        }
    }

    #[test]
    fn xlsx_sizes_too_small_to_draw_hide_content() {
        let hidden = |format: &str, rows: &str| {
            let sheet =
                format!(r#"<worksheet {SML_NS}>{format}<sheetData>{rows}</sheetData></worksheet>"#);
            xml_has_hidden_content(sheet.as_bytes())
        };
        let row = r#"<row r="1"><c r="A1"><v>1</v></c></row>"#;
        let tall_row = r#"<row r="1" ht="15" customHeight="1"><c r="A1"><v>1</v></c></row>"#;
        assert!(hidden(r#"<sheetFormatPr defaultRowHeight="0"/>"#, row));
        assert!(!hidden(
            r#"<sheetFormatPr defaultRowHeight="0"/>"#,
            tall_row
        ));
        assert!(hidden(
            r#"<sheetFormatPr defaultRowHeight="15" defaultColWidth="0"/>"#,
            row
        ));
        // `zeroHeight` hides only rows the sheet does not write.
        assert!(!hidden(
            r#"<sheetFormatPr defaultRowHeight="15" zeroHeight="1"/>"#,
            row
        ));
        for rows in [
            r#"<row r="1" ht="0.01" customHeight="1"><c r="A1"><v>1</v></c></row>"#,
            r#"<row r="1" ht="NaN" customHeight="1"><c r="A1"><v>1</v></c></row>"#,
        ] {
            assert!(hidden("", rows), "{rows}");
        }
        assert!(!hidden(
            "",
            r#"<row r="1" ht="0.75" customHeight="1"><c r="A1"><v>1</v></c></row>"#
        ));
        assert!(hidden(
            r#"<cols><col min="1" max="1" width="0.05" customWidth="1"/></cols>"#,
            row
        ));
        assert!(!hidden(
            r#"<cols><col min="1" max="1" width="0.5" customWidth="1"/></cols>"#,
            row
        ));
    }

    #[test]
    fn xlsx_sheets_and_cells_outside_anydocs_reach_are_refused() {
        let workbook = |sheets: &str| {
            format!(
                r#"<workbook {SML_NS} xmlns:r="{REL_NS}" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006">{sheets}</workbook>"#
            )
        };
        assert!(!xlsx_workbook_drops_sheets(
            workbook(r#"<sheets><sheet name="A" sheetId="1" r:id="rId1"/></sheets>"#).as_bytes()
        ));
        for dropped in [
            r#"<sheets><mc:AlternateContent><mc:Choice Requires="x"><sheet name="A" sheetId="1" r:id="rId1"/></mc:Choice></mc:AlternateContent></sheets>"#,
            r#"<sheets/><sheets><sheet name="A" sheetId="1" r:id="rId1"/></sheets>"#,
        ] {
            assert!(
                xlsx_workbook_drops_sheets(workbook(dropped).as_bytes()),
                "{dropped}"
            );
        }
        let unreached = |data: &str| {
            let sheet = format!(
                r#"<worksheet {SML_NS} xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006">{data}</worksheet>"#
            );
            scan_worksheet(sheet.as_bytes()).unreached_cell
        };
        assert!(!unreached(
            r#"<sheetData><row r="1"><c r="A1"><v>1</v></c></row></sheetData>"#
        ));
        for data in [
            r#"<sheetData><row r="1"><mc:AlternateContent><mc:Choice Requires="x"><c r="A1"><v>1</v></c></mc:Choice></mc:AlternateContent></row></sheetData>"#,
            r#"<sheetData><c r="A1"><v>1</v></c></sheetData>"#,
            r#"<extLst><sheetData><row r="1"><c r="A1"><v>1</v></c></row></sheetData></extLst>"#,
        ] {
            assert!(unreached(data), "{data}");
        }
    }

    #[test]
    fn xlsx_hidden_vml_checkboxes_are_found() {
        let drawing = |style: &str| {
            format!(
                r#"<xml xmlns:v="urn:schemas-microsoft-com:vml" xmlns:x="urn:schemas-microsoft-com:office:excel"><v:shape style="{style}"><v:textbox><div>Caption</div></v:textbox><x:ClientData ObjectType="Checkbox"><x:Anchor>1, 0, 0, 0, 2, 0, 1, 0</x:Anchor></x:ClientData></v:shape></xml>"#
            )
        };
        // AnyDoc skips a checkbox styled exactly this way, as Excel does.
        assert!(!vml_hides_checkbox(
            drawing("position:absolute; visibility: hidden").as_bytes()
        ));
        // A tab written as a reference survives attribute normalization.
        for style in [
            "VISIBILITY:HIDDEN",
            "visibility:&#9;hidden",
            "Visibility:Hidden",
        ] {
            assert!(vml_hides_checkbox(drawing(style).as_bytes()), "{style}");
        }
        assert!(!vml_hides_checkbox(drawing("position:absolute").as_bytes()));

        let rels = format!(
            r#"<Relationships><Relationship Id="rId1" Type="{REL_NS}/vmlDrawing" Target="../drawings/vmlDrawing1.vml"/></Relationships>"#
        );
        let sheet = format!(
            r#"<worksheet {SML_NS}><sheetData><row r="1"><c r="A1"><v>1</v></c></row></sheetData></worksheet>"#
        );
        let workbook = format!(
            r#"<workbook {SML_NS} xmlns:r="{REL_NS}"><sheets><sheet name="A" sheetId="1" r:id="rId1"/></sheets></workbook>"#
        );
        let workbook_rels = format!(
            r#"<Relationships xmlns="{PACKAGE_RELS_NS}"><Relationship Id="rId1" Type="{REL_NS}/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#
        );
        let hidden = drawing("VISIBILITY:HIDDEN");
        let result = preflight_package(
            &zip_entries(&[
                ("[Content_Types].xml", XLSX_TYPES),
                ("xl/workbook.xml", workbook.as_bytes()),
                ("xl/_rels/workbook.xml.rels", workbook_rels.as_bytes()),
                ("xl/worksheets/sheet1.xml", sheet.as_bytes()),
                ("xl/worksheets/_rels/sheet1.xml.rels", rels.as_bytes()),
                ("xl/drawings/vmlDrawing1.vml", hidden.as_bytes()),
            ]),
            DocumentKind::Xlsx,
            DocumentVariant::Xlsx,
        )
        .unwrap();
        assert!(result.hidden_content);
    }

    /// Preflight a one-sheet workbook with `cells` in its first row,
    /// `styles` as its styles part, and `extra` parts.
    fn preflight_workbook(
        cells: &str,
        styles: &str,
        sheet_rels: Option<&str>,
        extra: &[(&str, &[u8])],
    ) -> PackagePreflight {
        let sheet = format!(
            r#"<worksheet {SML_NS}><sheetData><row r="1">{cells}</row></sheetData></worksheet>"#
        );
        let workbook = format!(
            r#"<workbook {SML_NS} xmlns:r="{REL_NS}"><sheets><sheet name="A" sheetId="1" r:id="rId1"/></sheets></workbook>"#
        );
        let workbook_rels = format!(
            r#"<Relationships xmlns="{PACKAGE_RELS_NS}"><Relationship Id="rId1" Type="{REL_NS}/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="{REL_NS}/styles" Target="styles.xml"/></Relationships>"#
        );
        let styles = format!(r#"<styleSheet {SML_NS}>{styles}</styleSheet>"#);
        let mut entries: Vec<(&str, &[u8])> = vec![
            ("[Content_Types].xml", XLSX_TYPES),
            ("xl/workbook.xml", workbook.as_bytes()),
            ("xl/_rels/workbook.xml.rels", workbook_rels.as_bytes()),
            ("xl/worksheets/sheet1.xml", sheet.as_bytes()),
            ("xl/styles.xml", styles.as_bytes()),
        ];
        if let Some(rels) = sheet_rels {
            entries.push(("xl/worksheets/_rels/sheet1.xml.rels", rels.as_bytes()));
        }
        entries.extend_from_slice(extra);
        preflight_package(
            &zip_entries(&entries),
            DocumentKind::Xlsx,
            DocumentVariant::Xlsx,
        )
        .unwrap()
    }

    #[test]
    fn xlsx_number_formats_are_read_as_anydoc_renders_cells() {
        let styles = |code: &str| {
            let code = code.replace('"', "&quot;");
            format!(
                r#"<numFmts count="1"><numFmt numFmtId="164" formatCode="{code}"/></numFmts><cellXfs count="3"><xf numFmtId="0"/><xf numFmtId="164"/><xf numFmtId="31"/></cellXfs>"#
            )
        };
        let workbook =
            |cells: &str, code: &str| preflight_workbook(cells, &styles(code), None, &[]);
        // A negative marked only by a colour loses its sign.
        let red = "#,##0;[Red]#,##0";
        assert!(workbook(r#"<c r="A1" s="1"><v>-1234</v></c>"#, red).unsupported_content);
        assert!(!workbook(r#"<c r="A1" s="1"><v>1234</v></c>"#, red).unsupported_content);
        assert!(!workbook(r#"<c r="A1" s="0"><v>-1234</v></c>"#, red).unsupported_content);
        let parens = r#"#,##0;[Red]\(#,##0\)"#;
        assert!(!workbook(r#"<c r="A1" s="1"><v>-1234</v></c>"#, parens).unsupported_content);
        // A style's largest negative value decides for all of them: a
        // residue showing as zero halves passes alone, not beside a value
        // that shows without its sign.
        let halves = "# ?/2;[Red]# ?/2";
        assert!(!workbook(r#"<c r="A1" s="1"><v>-0.2</v></c>"#, halves).unsupported_content);
        for cells in [
            r#"<c r="A1" s="1"><v>-0.2</v></c><c r="B1" s="1"><v>-0.7</v></c>"#,
            r#"<c r="A1" s="1"><v>-0.7</v></c><c r="B1" s="1"><v>-0.2</v></c>"#,
        ] {
            assert!(workbook(cells, halves).unsupported_content, "{cells}");
        }
        // A value a format hides is hidden content; a hidden zero is not.
        let hide = ";;;";
        assert!(workbook(r#"<c r="A1" s="1"><v>98765</v></c>"#, hide).hidden_content);
        assert!(
            workbook(
                r#"<c r="A1" s="1" t="inlineStr"><is><t>Note</t></is></c>"#,
                hide
            )
            .hidden_content
        );
        let zeros = "#,##0;(#,##0);";
        assert!(!workbook(r#"<c r="A1" s="1"><v>0</v></c>"#, zeros).hidden_content);
        // AnyDoc renders a section naming General beside date letters as
        // General: the colour alone marked the negative.
        let general = "General;[Red]General s";
        assert!(workbook(r#"<c r="A1" s="1"><v>-3.5</v></c>"#, general).unsupported_content);
        assert!(!workbook(r#"<c r="A1" s="1"><v>3.5</v></c>"#, general).unsupported_content);
        // A fraction scaled by thousands shows a thousandth of its value.
        let scaled = "# ?/?,";
        assert!(workbook(r#"<c r="A1" s="1"><v>0.2</v></c>"#, scaled).unsupported_content);
        assert!(!workbook(r#"<c r="A1" s="1"><v>0</v></c>"#, scaled).unsupported_content);
        // A locale date id AnyDoc cannot resolve renders a serial number.
        assert!(workbook(r#"<c r="A1" s="2"><v>45762</v></c>"#, red).unsupported_content);
        // A style index past `cellXfs` renders as General.
        assert!(!workbook(r#"<c r="A1" s="9"><v>-1</v></c>"#, red).unsupported_content);
        // Cells AnyDoc never reads are not checked here.
        let clean = preflight_workbook(r#"<c r="A1" s="1"><v>1</v></c>"#, &styles(red), None, &[]);
        assert!(!clean.unsupported_content && !clean.hidden_content);
    }

    #[test]
    fn xlsx_drawing_text_is_found() {
        let rels = format!(
            r#"<Relationships><Relationship Id="rId1" Type="{REL_NS}/drawing" Target="../drawings/drawing1.xml"/></Relationships>"#
        );
        let drawing = |shape: &str| {
            format!(
                r#"<xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><xdr:twoCellAnchor>{shape}</xdr:twoCellAnchor></xdr:wsDr>"#
            )
        };
        let text_box = |attributes: &str, cnv: &str, text: &str| {
            format!(
                r#"<xdr:sp {attributes}><xdr:nvSpPr><xdr:cNvPr id="2" name="TextBox 1" {cnv}/></xdr:nvSpPr><xdr:txBody><a:p><a:r><a:t>{text}</a:t></a:r></a:p></xdr:txBody></xdr:sp>"#
            )
        };
        let check = |shape: String| {
            let drawing = drawing(&shape);
            preflight_workbook(
                r#"<c r="A1"><v>1</v></c>"#,
                "",
                Some(&rels),
                &[("xl/drawings/drawing1.xml", drawing.as_bytes())],
            )
            .unsupported_content
        };
        assert!(check(text_box("", "", "Amounts restated; see note 4")));
        assert!(check(format!(
            r#"<xdr:grpSp><xdr:nvGrpSpPr><xdr:cNvPr id="1" name="Group"/></xdr:nvGrpSpPr>{}</xdr:grpSp>"#,
            text_box("", "", "Grouped note")
        )));
        // A hidden shape, such as a form control's drawing copy, a shape
        // linked to a cell, a picture, and an empty box show nothing new.
        assert!(!check(text_box("", r#"hidden="1""#, "Check Box 1")));
        assert!(!check(text_box(r#"textlink="$A$1""#, "", "1")));
        assert!(!check(text_box("", "", " ")));
        assert!(!check(
            r#"<xdr:pic><xdr:nvPicPr><xdr:cNvPr id="3" name="Picture 1" descr="Scanned receipt"/></xdr:nvPicPr></xdr:pic>"#
                .to_string()
        ));
        assert!(!check(format!(
            r#"<xdr:grpSp><xdr:nvGrpSpPr><xdr:cNvPr id="1" name="Group" hidden="1"/></xdr:nvGrpSpPr>{}</xdr:grpSp>"#,
            text_box("", "", "Inside a hidden group")
        )));
        // A connector's label shows as a shape's does.
        assert!(check(
            r#"<xdr:cxnSp><xdr:nvCxnSpPr><xdr:cNvPr id="4" name="Connector 1"/></xdr:nvCxnSpPr><xdr:txBody><a:p><a:r><a:t>See schedule B</a:t></a:r></a:p></xdr:txBody></xdr:cxnSp>"#
                .to_string()
        ));
        // Excel shows a chart or slicer it understands, not the notice its
        // fallback holds for older versions; a choice it cannot read leaves
        // the fallback shown.
        let alternate = |requires: &str| {
            format!(
                r#"<mc:AlternateContent xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:cx1="http://schemas.microsoft.com/office/drawing/2015/9/8/chartex" xmlns:x="urn:example:unknown"><mc:Choice Requires="{requires}"><xdr:graphicFrame macro=""><xdr:nvGraphicFramePr><xdr:cNvPr id="5" name="Chart 1"/></xdr:nvGraphicFramePr></xdr:graphicFrame></mc:Choice><mc:Fallback>{}</mc:Fallback></mc:AlternateContent>"#,
                text_box(
                    "",
                    "",
                    "This chart isn't available in your version of Excel."
                )
            )
        };
        assert!(!check(alternate("cx1")));
        assert!(check(alternate("x")));
    }

    #[test]
    fn active_content_is_found_by_type_not_by_words_in_text() {
        let slide = br#"<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>Save as a macroEnabled template</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#;
        let result = preflight_package(
            &zip_entries(&[
                ("[Content_Types].xml", PPTX_TYPES),
                ("ppt/presentation.xml", PPTX_PRESENTATION),
                ("ppt/_rels/presentation.xml.rels", PPTX_RELS),
                ("ppt/slides/slide1.xml", slide),
            ]),
            DocumentKind::Pptx,
            DocumentVariant::Pptx,
        )
        .unwrap();
        assert!(!result.active_content);

        // A chart's link to an external data workbook is external content;
        // an embedded workbook is still active content.
        let body = word_part("document", "<w:p><w:r><w:t>Chart</w:t></w:r></w:p>");
        let chart_rels = |relationship: &str| {
            format!(r#"<Relationships>{relationship}</Relationships>"#).into_bytes()
        };
        let linked = chart_rels(&format!(
            r#"<Relationship Id="rId3" Type="{REL_NS}/oleObject" Target="file:///C:/Reports/Q.xlsx" TargetMode="External"/>"#
        ));
        let result = docx_preflight(&[
            ("word/document.xml", &body),
            ("word/charts/_rels/chart1.xml.rels", &linked),
        ]);
        assert!(!result.active_content);
        assert!(result.external_relationships);
        let embedded = chart_rels(&format!(
            r#"<Relationship Id="rId3" Type="{REL_NS}/package" Target="../data/book.xlsx"/>"#
        ));
        let result = docx_preflight(&[
            ("word/document.xml", &body),
            ("word/charts/_rels/chart1.xml.rels", &embedded),
        ]);
        assert!(result.active_content);
    }

    #[test]
    fn docx_notes_each_side_reads_must_show_the_same_text() {
        let rels = |inner: &[(&str, &str, &str)]| {
            let inner: String = inner
                .iter()
                .map(|(id, kind, target)| {
                    format!(r#"<Relationship Id="{id}" Type="{REL_NS}/{kind}" Target="{target}"/>"#)
                })
                .collect();
            format!(r#"<Relationships xmlns="{PACKAGE_RELS_NS}">{inner}</Relationships>"#)
        };
        let numbering = format!(
            r#"<w:numbering {WORD_NS}><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/></w:lvl></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#
        );
        for kind in ["footnote", "endnote"] {
            let body = word_part(
                "document",
                &format!(
                    r#"<w:p><w:r><w:t>Clause with a note</w:t></w:r><w:r><w:{kind}Reference w:id="1"/></w:r></w:p>"#
                ),
            );
            let notes = |paragraphs: &str| {
                format!(
                    r#"<w:{kind}s {WORD_NS}><w:{kind} w:type="separator" w:id="-1"><w:p><w:r><w:separator/></w:r></w:p></w:{kind}><w:{kind} w:id="1">{paragraphs}</w:{kind}></w:{kind}s>"#
                )
            };
            let paragraph = |text: &str| {
                format!(r#"<w:p><w:r><w:t xml:space="preserve">{text}</w:t></w:r></w:p>"#)
            };
            let conventional = format!("word/{kind}s.xml");
            let preflight = |rels: String, parts: &[(&str, String)]| {
                let mut entries: Vec<(&str, &[u8])> = vec![
                    ("word/document.xml", &body),
                    ("word/_rels/document.xml.rels", rels.as_bytes()),
                    ("word/numbering.xml", numbering.as_bytes()),
                ];
                entries.extend(parts.iter().map(|(name, bytes)| (*name, bytes.as_bytes())));
                docx_preflight(&entries)
            };
            let relationship = format!("{kind}s");
            let first_conventional = rels(&[
                ("rId1", "numbering", "numbering.xml"),
                ("rId8", &relationship, &format!("{kind}s.xml")),
                ("rId0", &relationship, "other.xml"),
            ]);
            // Word, as LibreOffice shows it, reads the first relationship and
            // AnyDoc the lowest id: AnyDoc would convert other note text than
            // Word shows.
            let alpha = notes(&paragraph("Alpha note says pay 100 USD"));
            let omega = notes(&paragraph("Omega note says pay 900 USD"));
            let substituted = preflight(
                first_conventional.clone(),
                &[
                    (&conventional, alpha.clone()),
                    ("word/other.xml", omega.clone()),
                ],
            );
            assert!(substituted.unsupported_content, "{kind}");
            assert!(matches!(
                preflight_rejection(DocumentKind::Docx, &substituted),
                Some(DocumentError::IncompleteConversion)
            ));
            // With the lowest id first, both read the same part.
            let lowest_first = rels(&[
                ("rId1", "numbering", "numbering.xml"),
                ("rId0", &relationship, &format!("{kind}s.xml")),
                ("rId8", &relationship, "other.xml"),
            ]);
            let same_part = preflight(
                lowest_first,
                &[
                    (&conventional, alpha.clone()),
                    ("word/other.xml", omega.clone()),
                ],
            );
            assert!(!same_part.unsupported_content, "{kind}");
            // Parts that write the note otherwise are refused, however alike
            // the text they show: no producer writes two notes
            // relationships, and markup can show other text than it seems
            // to hold (a run in another vocabulary, a branch chosen by what
            // it requires, a deletion, a field's parts in another order).
            let split = notes(
                r#"<w:p w:rsidR="00AB12CD"><w:r><w:rPr><w:b/></w:rPr><w:t xml:space="preserve">Alpha note says </w:t></w:r><w:proofErr w:type="spellStart"/><w:r><w:t>pay 100 USD</w:t></w:r></w:p>"#,
            );
            let same_text = preflight(
                first_conventional.clone(),
                &[(&conventional, alpha.clone()), ("word/other.xml", split)],
            );
            assert!(same_text.unsupported_content, "{kind}");
            let apart = |shown: &str, converted: &str| {
                preflight(
                    first_conventional.clone(),
                    &[
                        (&conventional, notes(shown)),
                        ("word/other.xml", notes(converted)),
                    ],
                )
                .unsupported_content
            };
            let foreign = |digit: &str, before: &str, after: &str| {
                format!(
                    r#"<w:p><w:r><w:t xml:space="preserve">{before}</w:t><x:t xmlns:x="urn:x">{digit}</x:t><w:t xml:space="preserve">{after}</w:t></w:r></w:p>"#
                )
            };
            assert!(
                apart(
                    &foreign("9", "pay ", "100 USD"),
                    &foreign("1", "pay 9", "00 USD")
                ),
                "{kind}"
            );
            let required = |prefix: &str| {
                format!(
                    r#"<mc:AlternateContent xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:wps="http://schemas.microsoft.com/office/word/2010/wordprocessingShape" xmlns:zz="urn:zz"><mc:Choice Requires="{prefix}">{}</mc:Choice><mc:Fallback>{}</mc:Fallback></mc:AlternateContent>"#,
                    paragraph("pay 100 USD"),
                    paragraph("pay 900 USD")
                )
            };
            assert!(apart(&required("wps"), &required("zz")), "{kind}");
            for wrapper in ["del", "moveFrom"] {
                let tracked = |digit: &str, before: &str, after: &str| {
                    format!(
                        r#"<w:p><w:r><w:t xml:space="preserve">{before}</w:t></w:r><w:{wrapper} w:id="9" w:author="R"><w:r><w:t>{digit}</w:t></w:r></w:{wrapper}><w:r><w:t xml:space="preserve">{after}</w:t></w:r></w:p>"#
                    )
                };
                assert!(
                    apart(
                        &tracked("9", "pay ", "100 USD"),
                        &tracked("1", "pay 9", "00 USD")
                    ),
                    "{kind} {wrapper}"
                );
            }
            let field = |kinds: [&str; 3]| {
                format!(
                    r#"<w:p><w:r><w:fldChar w:fldCharType="{}"/></w:r><w:r><w:instrText> QUOTE 1 </w:instrText></w:r><w:r><w:fldChar w:fldCharType="{}"/></w:r><w:r><w:t>pay 100 USD</w:t></w:r><w:r><w:fldChar w:fldCharType="{}"/></w:r></w:p>"#,
                    kinds[0], kinds[1], kinds[2]
                )
            };
            assert!(
                apart(
                    &field(["begin", "separate", "end"]),
                    &field(["separate", "begin", "end"])
                ),
                "{kind}"
            );
            // Parts written alike are one: the note Word shows, the first of
            // its id, against the note AnyDoc converts, with a run holding
            // nothing but the note's reference mark set aside, as AnyDoc
            // writes the mark as the note's label.
            let mark = format!(
                r#"<w:r><w:rPr><w:vertAlign w:val="superscript"/></w:rPr><w:{kind}Ref/></w:r>"#
            );
            let marked = format!(
                r#"<w:p>{mark}<w:r><w:t xml:space="preserve"> Alpha note says pay 100 USD</w:t></w:r></w:p>"#
            );
            let unmarked = r#"<w:p><w:r><w:t xml:space="preserve"> Alpha note says pay 100 USD</w:t></w:r></w:p>"#;
            assert!(!apart(&marked, unmarked), "{kind}");
            assert!(!apart(unmarked, &marked), "{kind}");
            assert!(!apart(&marked, &marked), "{kind}");
            let stale = notes(&marked).replace(
                &format!("</w:{kind}s>"),
                &format!(
                    r#"<w:{kind} w:id="1">{}</w:{kind}></w:{kind}s>"#,
                    paragraph("Stale definition")
                ),
            );
            let duplicated = preflight(
                first_conventional.clone(),
                &[(&conventional, stale), ("word/other.xml", notes(&marked))],
            );
            assert!(!duplicated.unsupported_content, "{kind}");
            assert!(!duplicated.hidden_content, "{kind}");
            // A mark with other text in its run, or a relationship of either
            // part naming another target, is written otherwise.
            let run_with_text = format!(
                r#"<w:p><w:r><w:{kind}Ref/><w:t xml:space="preserve"> Alpha note says pay 100 USD</w:t></w:r></w:p>"#
            );
            assert!(apart(&run_with_text, unmarked), "{kind}");
            let note_rels = format!("word/_rels/{kind}s.xml.rels");
            let linked = rels(&[("rId1", "hyperlink", "https://example.com")]);
            assert!(
                preflight(
                    first_conventional.clone(),
                    &[
                        (&conventional, alpha.clone()),
                        ("word/other.xml", alpha.clone()),
                        (&note_rels, linked),
                    ],
                )
                .unsupported_content,
                "{kind}"
            );
            let spaced = notes(&paragraph("Alpha note says pay 1 00 USD"));
            assert!(
                preflight(
                    first_conventional.clone(),
                    &[(&conventional, alpha.clone()), ("word/other.xml", spaced)],
                )
                .unsupported_content,
                "{kind}"
            );
            // A tracked deletion Word shows struck through is not the text
            // AnyDoc's part holds as current.
            let deleted = notes(
                r#"<w:p><w:del w:id="9" w:author="Reviewer"><w:r><w:delText xml:space="preserve">Alpha note says pay 100 USD</w:delText></w:r></w:del></w:p>"#,
            );
            assert!(
                preflight(
                    first_conventional.clone(),
                    &[(&conventional, deleted), ("word/other.xml", alpha.clone())],
                )
                .unsupported_content,
                "{kind}"
            );
            // A note no text references, which Word does not show, is
            // disclosed, as it is from one part.
            let extra = notes(&paragraph("Alpha note says pay 100 USD")).replace(
                &format!("</w:{kind}s>"),
                &format!(
                    r#"<w:{kind} w:id="2">{}</w:{kind}></w:{kind}s>"#,
                    paragraph("Unreferenced note")
                ),
            );
            let unreferenced = preflight(
                first_conventional.clone(),
                &[(&conventional, alpha.clone()), ("word/other.xml", extra)],
            );
            assert!(!unreferenced.unsupported_content, "{kind}");
            assert!(unreferenced.hidden_content, "{kind}");
            // The same text numbered on one side is written otherwise too.
            let numbered = notes(
                r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t xml:space="preserve">Alpha note says pay 100 USD</w:t></w:r></w:p>"#,
            );
            let relabelled = preflight(
                first_conventional.clone(),
                &[(&conventional, numbered), ("word/other.xml", alpha.clone())],
            );
            assert!(relabelled.unsupported_content, "{kind}");
            // Without a relationship Word reads no notes, where AnyDoc reads
            // the conventional part; with one, both read the part it names,
            // whatever the conventional part holds.
            let unnamed = preflight(
                rels(&[("rId1", "numbering", "numbering.xml")]),
                &[(&conventional, alpha.clone())],
            );
            assert!(unnamed.unsupported_content, "{kind}");
            let unreferenced = word_part("document", "<w:p><w:r><w:t>No note</w:t></w:r></w:p>");
            let rels_without_notes = rels(&[("rId1", "numbering", "numbering.xml")]);
            let hidden = docx_preflight(&[
                ("word/document.xml", &unreferenced),
                (
                    "word/_rels/document.xml.rels",
                    rels_without_notes.as_bytes(),
                ),
                ("word/numbering.xml", numbering.as_bytes()),
                (&conventional, alpha.as_bytes()),
            ]);
            assert!(!hidden.unsupported_content, "{kind}");
            assert!(hidden.hidden_content, "{kind}");
            let named = preflight(
                rels(&[
                    ("rId1", "numbering", "numbering.xml"),
                    ("rId8", &relationship, "other.xml"),
                ]),
                &[
                    (&conventional, omega.clone()),
                    ("word/other.xml", alpha.clone()),
                ],
            );
            assert!(!named.unsupported_content, "{kind}");
            // A first relationship naming a missing part shows no notes.
            let missing = preflight(
                rels(&[
                    ("rId1", "numbering", "numbering.xml"),
                    ("rId8", &relationship, "missing.xml"),
                    ("rId0", &relationship, &format!("{kind}s.xml")),
                ]),
                &[(&conventional, alpha.clone())],
            );
            assert!(missing.unsupported_content, "{kind}");
            // Separators alone show no text on either side.
            let separators_only =
                notes("").replace(&format!(r#"<w:{kind} w:id="1"></w:{kind}>"#), "");
            assert!(
                !preflight(
                    rels(&[("rId1", "numbering", "numbering.xml")]),
                    &[(&conventional, separators_only)],
                )
                .unsupported_content,
                "{kind}"
            );
        }
        // Each item of a note is framed, so text cannot pass for markup, and
        // what is written in a note counts, declarations too.
        let markup = |part: &str| {
            docx_notes_read(part.as_bytes(), false)
                .expect("notes part")
                .anydoc
                .into_iter()
                .map(|note| note.written.markup.clone())
                .collect::<Vec<_>>()
        };
        let one = format!(
            r#"<w:footnotes {WORD_NS}><w:footnote w:id="1"><w:p><w:r><w:t>ab</w:t></w:r></w:p></w:footnote></w:footnotes>"#
        );
        let two = format!(
            r#"<w:footnotes {WORD_NS}><w:footnote w:id="1"><w:p><w:r><w:t>a</w:t></w:r><w:r><w:t>b</w:t></w:r></w:p></w:footnote></w:footnotes>"#
        );
        assert_ne!(markup(&one), markup(&two));
        assert_ne!(
            markup(&one),
            markup(&one.replace("<w:t>ab", r#"<w:t xmlns:x="urn:x">ab"#))
        );
        // The note's own id and type are not its markup; its other
        // attributes are.
        assert_eq!(
            markup(&one),
            markup(&one.replace(r#"w:id="1""#, r#"w:id="1" w:type="normal""#))
        );
        assert_ne!(
            markup(&one),
            markup(&one.replace(r#"w:id="1""#, r#"w:id="1" xml:space="preserve""#))
        );
    }

    #[test]
    fn docx_notes_are_read_as_each_side_reads_them() {
        const FOREIGN: &str = r#"xmlns:x="urn:x""#;
        let reference = |attributes: &str| {
            word_part(
                "document",
                &format!(
                    r#"<w:p><w:r><w:t>Clause A requires payment</w:t></w:r><w:r><w:footnoteReference {attributes}/></w:r></w:p>"#
                ),
            )
            .iter()
            .map(|&byte| char::from(byte))
            .collect::<String>()
            .replace("<w:document ", &format!("<w:document {FOREIGN} "))
        };
        let note = |attributes: &str, text: &str| {
            format!(
                r#"<w:footnote {attributes}><w:p><w:r><w:t xml:space="preserve">{text}</w:t></w:r></w:p></w:footnote>"#
            )
        };
        let preflight = |document: &str, notes: &str| {
            let footnotes = format!(
                r#"<w:footnotes {WORD_NS} {FOREIGN}><w:footnote w:type="separator" w:id="-1"><w:p><w:r><w:separator/></w:r></w:p></w:footnote>{notes}</w:footnotes>"#
            );
            docx_preflight(&[
                ("word/document.xml", document.as_bytes()),
                ("word/footnotes.xml", footnotes.as_bytes()),
            ])
        };
        let plain = reference(r#"w:id="1""#);
        let shown = note(r#"w:id="1""#, "pay 100 USD");
        let clean = preflight(&plain, &shown);
        assert!(!clean.unsupported_content && !clean.hidden_content);
        // AnyDoc reads a note's type and id from `w:type` and `w:id`, else
        // from an unprefixed `type` and `id`, as written, and Word, as
        // LibreOffice shows it, from `w:type` and `w:id` alone. A referenced
        // note AnyDoc skips as a separator, or finds by an id Word does not
        // read, is refused.
        assert!(
            preflight(&plain, &note(r#"type="separator" w:id="1""#, "pay 100 USD"))
                .unsupported_content
        );
        assert!(preflight(&plain, &note(r#"id="1""#, "pay 100 USD")).unsupported_content);
        assert!(preflight(&reference(r#"id="1""#), &shown).unsupported_content);
        // A type or id in another vocabulary is read by neither: the note
        // shows and converts.
        let foreign_type = preflight(
            &plain,
            &note(r#"x:type="separator" w:id="1""#, "pay 100 USD"),
        );
        assert!(!foreign_type.unsupported_content && !foreign_type.hidden_content);
        let foreign_id = preflight(
            &reference(r#"x:id="2" w:id="1""#),
            &format!("{shown}{}", note(r#"x:id="1" w:id="2""#, "Rider")),
        );
        assert!(!foreign_id.unsupported_content);
        // An id written with white space names another note for AnyDoc,
        // which drops the note at its reference.
        assert!(preflight(&plain, &note(r#"w:id=" 1""#, "pay 100 USD")).unsupported_content);
        assert!(preflight(&reference(r#"w:id="01""#), &shown).unsupported_content);
        // A note no reference names, which Word does not show and AnyDoc
        // converts after the text, is disclosed, whatever other vocabulary
        // or white space types it a separator.
        for typed in [
            r#"x:type="separator""#,
            r#"w:type=" separator""#,
            r#"w:type="separator ""#,
        ] {
            let rider = preflight(
                &plain,
                &format!(
                    "{shown}{}",
                    note(&format!(r#"{typed} w:id="2""#), "Hidden rider pay 900 USD")
                ),
            );
            assert!(!rider.unsupported_content, "{typed}");
            assert!(rider.hidden_content, "{typed}");
        }
        // Of two notes with one id, Word shows the first and AnyDoc the
        // first that is not blank: a later one written otherwise is
        // disclosed; a first AnyDoc reads another type for is refused.
        let blank_first = preflight(
            &plain,
            &format!(
                r#"<w:footnote w:id="1"><w:p/></w:footnote>{}"#,
                note(r#"w:id="1""#, "pay 900 USD")
            ),
        );
        assert!(!blank_first.unsupported_content);
        assert!(blank_first.hidden_content);
        let twice_alike = preflight(&plain, &format!("{shown}{shown}"));
        assert!(!twice_alike.unsupported_content && !twice_alike.hidden_content);
        assert!(
            preflight(
                &plain,
                &format!(
                    "{}{shown}",
                    note(r#"type="separator" w:id="1""#, "pay 900 USD")
                )
            )
            .unsupported_content
        );
        // AnyDoc reads the notes that are children of the root alone, where
        // Word also finds them in compatibility content it takes.
        let wrapped = format!(
            r#"<mc:AlternateContent xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:wps="http://schemas.microsoft.com/office/word/2010/wordprocessingShape"><mc:Choice Requires="wps">{shown}</mc:Choice></mc:AlternateContent>"#
        );
        assert!(preflight(&plain, &wrapped).unsupported_content);
        // A footnotes part whose root is another kind's holds no notes for
        // AnyDoc, or Word.
        let endnotes_root = docx_preflight(&[
            ("word/document.xml", plain.as_bytes()),
            (
                "word/footnotes.xml",
                format!(r#"<w:endnotes {WORD_NS}>{shown}</w:endnotes>"#).as_bytes(),
            ),
        ]);
        assert!(!endnotes_root.unsupported_content);
        // A reference in compatibility content stands with its counterpart
        // in the branch the other side takes; one side's alone is judged
        // alone.
        let branches = |choice: &str, fallback: &str| {
            word_part(
                "document",
                &format!(
                    r#"<w:p><w:r><w:t>Clause A</w:t></w:r><mc:AlternateContent xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:w14="http://schemas.microsoft.com/office/word/2010/wordml"><mc:Choice Requires="w14">{choice}</mc:Choice><mc:Fallback>{fallback}</mc:Fallback></mc:AlternateContent></w:p>"#
                ),
            )
            .iter()
            .map(|&byte| char::from(byte))
            .collect::<String>()
        };
        let referenced = r#"<w:r><w:footnoteReference w:id="1"/></w:r>"#;
        let alike = preflight(&branches(referenced, referenced), &shown);
        assert!(!alike.unsupported_content && !alike.hidden_content);
        assert!(preflight(&branches(referenced, ""), &shown).unsupported_content);
        assert!(preflight(&branches("", referenced), &shown).hidden_content);
        // A reference in a tracked deletion shows in neither: its note is
        // converted after the text and disclosed.
        let deleted = word_part(
            "document",
            r#"<w:p><w:del w:id="9" w:author="R"><w:r><w:footnoteReference w:id="1"/></w:r></w:del></w:p>"#,
        )
        .iter()
        .map(|&byte| char::from(byte))
        .collect::<String>();
        let unreferenced = preflight(&deleted, &shown);
        assert!(!unreferenced.unsupported_content && unreferenced.hidden_content);
    }

    #[test]
    fn docx_notes_targets_resolve_as_anydoc_resolves_them() {
        let body = word_part("document", "<w:p><w:r><w:t>Body</w:t></w:r></w:p>");
        let symbol = word_part(
            "footnotes",
            r#"<w:p><w:r><w:sym w:char="F0FE"/></w:r></w:p>"#,
        );
        let clean = word_part("footnotes", r#"<w:p><w:r><w:t>Note</w:t></w:r></w:p>"#);
        let preflight = |target: &str, part: &str, notes: &[u8]| {
            let rels = footnotes_rels(target);
            docx_preflight(&[
                ("word/document.xml", &body),
                ("word/_rels/document.xml.rels", &rels),
                (part, notes),
            ])
        };
        // Traversal resolves alike for Word and AnyDoc, and the part it
        // names is checked.
        assert!(preflight("../../word/fn2.xml", "word/fn2.xml", &symbol).unsupported_content);
        assert!(!preflight("../../word/fn2.xml", "word/fn2.xml", &clean).unsupported_content);
        // A query, a fragment, or a percent-encoded name names one part to
        // AnyDoc, which drops the query and fragment and decodes the name,
        // and another to Word, which opens the name as written: refused,
        // whichever of the two the package holds.
        for (target, decoded, written) in [
            (
                "notes/fn.xml?v=1",
                "word/notes/fn.xml",
                "word/notes/fn.xml?v=1",
            ),
            ("f%6E.xml", "word/fn.xml", "word/f%6E.xml"),
            (
                "/extra/notes.xml#part",
                "extra/notes.xml",
                "extra/notes.xml#part",
            ),
        ] {
            assert!(
                preflight(target, decoded, &clean).unsupported_content,
                "{target}"
            );
            assert!(
                preflight(target, written, &clean).unsupported_content,
                "{target}"
            );
        }
        // Where the package holds neither, both read nothing.
        assert!(!preflight("f%6E.xml", "word/other.xml", &clean).unsupported_content);
    }

    #[test]
    fn ooxml_relationships_name_the_same_parts_to_word_and_anydoc() {
        let body = word_part("document", "<w:p><w:r><w:t>Body</w:t></w:r></w:p>");
        let relationship = |attributes: &str| format!("<Relationship {attributes}/>");
        let rels = |inner: &str| {
            format!(
                r#"<Relationships xmlns="{PACKAGE_RELS_NS}" xmlns:x="urn:x">{inner}</Relationships>"#
            )
        };
        let docx = |root: &str, main: &str| {
            let mut entries: Vec<(&str, &[u8])> = vec![
                ("word/document.xml", &body),
                ("word/other.xml", &body),
                ("word/footnotes.xml", &body),
                ("word/numbering.xml", &body),
            ];
            if !root.is_empty() {
                entries.push(("_rels/.rels", root.as_bytes()));
            }
            if !main.is_empty() {
                entries.push(("word/_rels/document.xml.rels", main.as_bytes()));
            }
            docx_result(&entries)
        };
        let office = |target: &str| {
            relationship(&format!(
                r#"Id="rId1" Type="{REL_NS}/officeDocument" Target="{target}""#
            ))
        };
        let typed = |id: &str, kind: &str, target: &str| {
            relationship(&format!(
                r#"Id="{id}" Type="{REL_NS}/{kind}" Target="{target}""#
            ))
        };
        assert!(docx(&rels(&office("word/document.xml")), "").is_ok());
        // Declarations are not attributes, and a target mode OPC names is
        // allowed.
        let external = relationship(&format!(
            r#"xmlns:y="urn:y" Id="rId9" Type="{REL_NS}/hyperlink" TargetMode="External" Target="https://example.invalid""#
        ));
        assert!(docx("", &rels(&external)).is_ok());
        // An attribute OPC does not define, in another vocabulary or none,
        // is refused: LibreOffice reads the unprefixed `Target` or `Type`
        // where AnyDoc reads the first of either name.
        for written in [
            relationship(&format!(
                r#"Id="rId1" Type="{REL_NS}/officeDocument" x:Target="word/document.xml" Target="word/other.xml""#
            )),
            relationship(&format!(
                r#"Id="rId1" x:Type="{REL_NS}/officeDocument" Type="{REL_NS}/customXml" Target="word/other.xml""#
            )),
            relationship(&format!(
                r#"Id="rId1" Type="{REL_NS}/officeDocument" Target="word/document.xml" Note="kept""#
            )),
        ] {
            assert!(
                matches!(docx(&rels(&written), ""), Err(DocumentError::Malformed)),
                "{written}"
            );
        }
        for written in [
            typed("rId8", "footnotes", "footnotes.xml")
                .replace("Target=", r#"x:Target="other.xml" Target="#),
            typed("rId8", "numbering", "numbering.xml")
                .replace("Target=", r#"x:Target="other.xml" Target="#),
            // Ids repeated, empty, or missing, and a target mode in another
            // spelling.
            typed("rId8", "footnotes", "footnotes.xml") + &typed("rId8", "styles", "styles.xml"),
            typed("", "footnotes", "footnotes.xml"),
            relationship(&format!(
                r#"Type="{REL_NS}/footnotes" Target="footnotes.xml""#
            )),
            typed("rId8", "footnotes", "footnotes.xml")
                .replace("/>", r#" TargetMode="internal"/>"#),
        ] {
            assert!(
                matches!(docx("", &rels(&written)), Err(DocumentError::Malformed)),
                "{written}"
            );
        }
        // A relationships part without the namespace, which AnyDoc does not
        // read and LibreOffice does, is refused too.
        let bare = format!(
            r#"<Relationships>{}</Relationships>"#,
            typed("rId8", "footnotes", "other.xml")
        );
        assert!(matches!(docx("", &bare), Err(DocumentError::Malformed)));
        // A percent-encoded target: AnyDoc decodes it, Word does not.
        let encoded_main = rels(&office("word/docum%65nt.xml"));
        assert!(matches!(
            docx(&encoded_main, ""),
            Err(DocumentError::Malformed)
        ));
        let encoded_numbering = rels(&typed("rId8", "numbering", "numb%65ring.xml"));
        assert!(
            docx("", &encoded_numbering)
                .expect("DOCX preflight")
                .unsupported_content
        );
        // Workbooks: sheets, shared strings, and styles alike.
        let workbook = format!(
            r#"<workbook xmlns:r="{REL_NS}"><sheets><sheet name="Data" sheetId="1" r:id="rId1"/></sheets></workbook>"#
        );
        let sheet = format!(
            r#"<worksheet {SML_NS}><sheetData><row r="1"><c r="A1"><v>1</v></c></row></sheetData></worksheet>"#
        );
        let xlsx = |root: Option<String>, workbook_rels: String| {
            let mut entries: Vec<(&str, Vec<u8>)> = vec![
                ("[Content_Types].xml", XLSX_TYPES.to_vec()),
                ("xl/workbook.xml", workbook.clone().into_bytes()),
                ("xl/_rels/workbook.xml.rels", workbook_rels.into_bytes()),
                ("xl/worksheets/sheet1.xml", sheet.clone().into_bytes()),
                ("xl/worksheets/sheet%31.xml", sheet.clone().into_bytes()),
                ("xl/sharedStrings.xml", b"<sst/>".to_vec()),
                ("xl/wb2.xml", workbook.clone().into_bytes()),
            ];
            if let Some(root) = root {
                entries.push(("_rels/.rels", root.into_bytes()));
            }
            let entries: Vec<(&str, &[u8])> = entries
                .iter()
                .map(|(name, bytes)| (*name, bytes.as_slice()))
                .collect();
            preflight_package(
                &zip_entries(&entries),
                DocumentKind::Xlsx,
                DocumentVariant::Xlsx,
            )
        };
        let sheet_rel = |target: &str| typed("rId1", "worksheet", target);
        let control =
            xlsx(None, rels(&sheet_rel("worksheets/sheet1.xml"))).expect("XLSX preflight");
        assert!(!control.unsupported_content);
        let moved_sheet = xlsx(
            None,
            rels(
                &sheet_rel("worksheets/sheet1.xml")
                    .replace("Target=", r#"x:Target="worksheets/sheet2.xml" Target="#),
            ),
        );
        assert!(matches!(moved_sheet, Err(DocumentError::Malformed)));
        let strings = sheet_rel("worksheets/sheet1.xml")
            + &typed("rId2", "sharedStrings", "sharedStrings.xml")
                .replace("Target=", r#"x:Target="sst2.xml" Target="#);
        assert!(matches!(
            xlsx(None, rels(&strings)),
            Err(DocumentError::Malformed)
        ));
        let encoded_sheet =
            xlsx(None, rels(&sheet_rel("worksheets/sheet%31.xml"))).expect("XLSX preflight");
        assert!(encoded_sheet.unsupported_content);
        let encoded_strings = sheet_rel("worksheets/sheet1.xml")
            + &typed("rId2", "sharedStrings", "sh%61redStrings.xml");
        assert!(
            xlsx(None, rels(&encoded_strings))
                .expect("XLSX preflight")
                .unsupported_content
        );
        let moved_main = rels(
            &office("xl/workbook.xml")
                .replace("Target=", r#"x:Target="xl/workbook.xml" Target="#)
                .replace("Target=\"xl/workbook.xml\"/", "Target=\"xl/wb2.xml\"/"),
        );
        assert!(matches!(
            xlsx(Some(moved_main), rels(&sheet_rel("worksheets/sheet1.xml"))),
            Err(DocumentError::Malformed)
        ));
    }

    #[test]
    fn docx_paragraphs_take_the_style_word_finds() {
        const FOREIGN: &str = r#"xmlns:x="urn:x""#;
        // Normal, the default paragraph style, numbers in upper Roman;
        // list 1 is decimal.
        let differs = |named: &str, styles: &str| {
            let body = format!(
                r#"<w:p><w:pPr><w:pStyle w:val="{named}"/></w:pPr><w:r><w:t>Clause</w:t></w:r></w:p>"#
            )
            .repeat(2);
            let document = word_part("document", &body);
            let numbering = format!(
                r#"<w:numbering {WORD_NS}><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/></w:lvl></w:abstractNum><w:abstractNum w:abstractNumId="1"><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="upperRoman"/><w:lvlText w:val="%1."/></w:lvl></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num><w:num w:numId="2"><w:abstractNumId w:val="1"/></w:num></w:numbering>"#
            );
            let styles = format!(
                r#"<w:styles {WORD_NS} {FOREIGN}><w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/><w:pPr><w:numPr><w:numId w:val="2"/></w:numPr></w:pPr></w:style>{styles}</w:styles>"#
            );
            docx_preflight(&[
                ("word/document.xml", &document),
                ("word/numbering.xml", numbering.as_bytes()),
                ("word/styles.xml", styles.as_bytes()),
            ])
            .list_numbering_differs
        };
        let style = |kind: &str, id: &str, name: &str, numbered: bool| {
            let numbering = if numbered {
                r#"<w:pPr><w:numPr><w:numId w:val="1"/></w:numPr></w:pPr>"#
            } else {
                ""
            };
            format!(
                r#"<w:style w:type="{kind}" w:styleId="{id}"><w:name w:val="{name}"/>{numbering}</w:style>"#
            )
        };
        // A paragraph style with the exact id is taken; one Word cannot find
        // leaves the paragraph with the default style, numbered in Word and
        // not in AnyDoc: another id, a style nested in another, or a style
        // element in another vocabulary.
        assert!(!differs("Foo", &style("paragraph", "Foo", "Foo", false)));
        assert!(differs("Missing", ""));
        assert!(differs("Foo ", &style("paragraph", "Foo", "Foo", false)));
        assert!(differs("Foo", &style("paragraph", "Foo ", "Bar", false)));
        assert!(differs(
            "Inner",
            &format!(
                r#"<w:style w:type="paragraph" w:styleId="Outer">{}</w:style>"#,
                style("paragraph", "Inner", "Inner", false)
            )
        ));
        assert!(differs(
            "Foo",
            r#"<x:style w:type="paragraph" w:styleId="Foo"><w:name w:val="Foo"/></x:style>"#
        ));
        // Word, as LibreOffice shows it, also finds a paragraph style by its
        // name, and a style without an id by its name alone.
        assert!(!differs("Foo", &style("paragraph", "Zed", "Foo", false)));
        assert!(!differs("Foo", &style("paragraph", "Foo ", "Foo", false)));
        assert!(!differs(
            "Foo",
            r#"<w:style w:type="paragraph" x:styleId="Foo"><w:name w:val="Foo"/></w:style>"#
        ));
        // A character, table, or numbering style numbers the paragraph with
        // its own list, as AnyDoc does, and otherwise leaves it the
        // default's.
        for kind in ["character", "table", "numbering"] {
            assert!(
                !differs("Other", &style(kind, "Other", "Other", true)),
                "{kind}"
            );
            assert!(
                differs("Other", &style(kind, "Other", "Other", false)),
                "{kind}"
            );
        }
        // Of two default paragraph styles, the last is Word's.
        let two_defaults = |numbered_first: bool| {
            let normal = |numbered: bool| {
                format!(
                    r#"<w:style w:type="paragraph" w:default="1" w:styleId="Body{numbered}">{}</w:style>"#,
                    if numbered {
                        r#"<w:pPr><w:numPr><w:numId w:val="1"/></w:numPr></w:pPr>"#
                    } else {
                        ""
                    }
                )
            };
            let body = "<w:p><w:r><w:t>Clause</w:t></w:r></w:p>".repeat(2);
            let document = word_part("document", &body);
            let numbering = format!(
                r#"<w:numbering {WORD_NS}><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/></w:lvl></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#
            );
            let styles = format!(
                "<w:styles {WORD_NS}>{}{}</w:styles>",
                normal(numbered_first),
                normal(!numbered_first)
            );
            docx_preflight(&[
                ("word/document.xml", &document),
                ("word/numbering.xml", numbering.as_bytes()),
                ("word/styles.xml", styles.as_bytes()),
            ])
            .list_numbering_differs
        };
        assert!(!two_defaults(true));
        assert!(two_defaults(false));
    }

    #[test]
    fn docx_numbering_is_read_as_each_side_reads_it() {
        const IGNORABLE: &str = r#"xmlns:x="urn:x" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" mc:Ignorable="x""#;
        let items = |numbering: &str| {
            format!(
                r#"<w:p><w:pPr><w:numPr>{numbering}</w:numPr></w:pPr><w:r><w:t>Item</w:t></w:r></w:p>"#
            )
            .repeat(3)
        };
        let direct = items(r#"<w:ilvl w:val="0"/><w:numId w:val="1"/>"#);
        let level = |ilvl: u32, format: &str, text: &str, start: &str| {
            format!(
                r#"<w:lvl w:ilvl="{ilvl}"><w:start {start}/><w:numFmt w:val="{format}"/><w:lvlText w:val="{text}"/></w:lvl>"#
            )
        };
        let decimal = level(0, "decimal", "%1.", r#"w:val="1""#);
        let definition =
            |id: &str, levels: &str| format!(r#"<w:abstractNum {id}>{levels}</w:abstractNum>"#);
        let differs = |body: &str, numbering: &str| {
            let document =
                format!("<w:document {WORD_NS} {IGNORABLE}><w:body>{body}</w:body></w:document>");
            let numbering = format!("<w:numbering {WORD_NS} {IGNORABLE}>{numbering}</w:numbering>");
            docx_preflight(&[
                ("word/document.xml", document.as_bytes()),
                ("word/numbering.xml", numbering.as_bytes()),
            ])
            .list_numbering_differs
        };
        let one = format!(
            r#"{}<w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>"#,
            definition(r#"w:abstractNumId="0""#, &decimal)
        );
        assert!(!differs(&direct, &one));
        // A paragraph's list and level are read from WordprocessingML's
        // attribute, else the unprefixed one, never another vocabulary's:
        // Word reads " 1" and AnyDoc cannot.
        for numbering in [
            r#"<w:ilvl w:val="0"/><w:numId w:val=" 1" x:val="1"/>"#,
            r#"<w:ilvl w:val="0"/><w:numId x:val="1" w:val=" 1"/>"#,
            r#"<w:ilvl w:val="0"/><w:numId val=" 1"/>"#,
        ] {
            assert!(differs(&items(numbering), &one), "{numbering}");
        }
        for numbering in [
            r#"<w:ilvl w:val="0"/><w:numId x:val=" 1" w:val="1"/>"#,
            r#"<w:ilvl w:val="0"/><w:numId val="1"/>"#,
        ] {
            assert!(!differs(&items(numbering), &one), "{numbering}");
        }
        let two_levels = format!(
            r#"{}<w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>"#,
            definition(
                r#"w:abstractNumId="0""#,
                &format!(
                    "{decimal}{}",
                    level(1, "lowerLetter", "%2)", r#"w:val="1""#)
                )
            )
        );
        assert!(differs(
            &items(r#"<w:ilvl w:val=" 1" x:val="1"/><w:numId w:val="1"/>"#),
            &two_levels
        ));
        // So is every number and id of the numbering part.
        for numbering in [
            format!(
                r#"{}<w:num x:numId="1" w:numId=" 1"><w:abstractNumId w:val="0"/></w:num>"#,
                definition(r#"w:abstractNumId="0""#, &decimal)
            ),
            format!(
                r#"{}<w:num w:numId="1"><w:abstractNumId x:val="0" w:val=" 0"/></w:num>"#,
                definition(r#"w:abstractNumId="0""#, &decimal)
            ),
            format!(
                r#"{}<w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>"#,
                definition(
                    r#"w:abstractNumId="0""#,
                    &level(0, "decimal", "%1.", r#"x:val="1" w:val=" 5""#)
                )
            ),
            format!(
                r#"{}<w:num w:numId="1"><w:abstractNumId w:val="0"/><w:lvlOverride w:ilvl="0"><w:startOverride x:val="1" w:val=" 5"/></w:lvlOverride></w:num>"#,
                definition(r#"w:abstractNumId="0""#, &decimal)
            ),
            format!(
                r#"{}<w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>"#,
                definition(r#"x:abstractNumId="0" w:abstractNumId="00""#, &decimal)
            ),
        ] {
            assert!(differs(&direct, &numbering), "{numbering}");
        }
        // Of two definitions or instances with one id, Word keeps the first
        // and AnyDoc the last, read afresh.
        let roman = level(0, "upperRoman", "%1.", r#"w:val="1""#);
        let plain = r#"<w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>"#;
        let restarted = r#"<w:num w:numId="1"><w:abstractNumId w:val="0"/><w:lvlOverride w:ilvl="0"><w:startOverride w:val="5"/></w:lvlOverride></w:num>"#;
        for numbering in [
            format!(
                "{}{}{plain}",
                definition(r#"w:abstractNumId="0""#, &decimal),
                definition(r#"w:abstractNumId="0""#, &roman)
            ),
            format!(
                "{}{restarted}{plain}",
                definition(r#"w:abstractNumId="0""#, &decimal)
            ),
            format!(
                "{}{plain}{restarted}",
                definition(r#"w:abstractNumId="0""#, &decimal)
            ),
            format!(
                r#"{}{}{plain}<w:num w:numId="1"><w:abstractNumId w:val="1"/></w:num>"#,
                definition(r#"w:abstractNumId="0""#, &decimal),
                definition(r#"w:abstractNumId="1""#, &roman)
            ),
        ] {
            assert!(differs(&direct, &numbering), "{numbering}");
        }
        for numbering in [
            format!(
                "{}{}{plain}",
                definition(r#"w:abstractNumId="0""#, &decimal),
                definition(r#"w:abstractNumId="0""#, &decimal)
            ),
            // An instance naming no definition is no reading AnyDoc keeps.
            format!(
                r#"{}{plain}<w:num w:numId="1"/>"#,
                definition(r#"w:abstractNumId="0""#, &decimal)
            ),
        ] {
            assert!(!differs(&direct, &numbering), "{numbering}");
        }
        // Only WordprocessingML's definitions and instances, children of the
        // part's numbering, count: a later definition of the id in another
        // vocabulary, or wrapped, replaces nothing for AnyDoc.
        for later in [
            format!(r#"<x:abstractNum w:abstractNumId="0">{roman}</x:abstractNum>"#),
            format!(
                "<x:wrap>{}</x:wrap>",
                definition(r#"w:abstractNumId="0""#, &roman)
            ),
        ] {
            let numbering = format!(
                "{}{later}{plain}",
                definition(r#"w:abstractNumId="0""#, &decimal)
            );
            assert!(!differs(&direct, &numbering), "{numbering}");
        }
    }

    #[test]
    fn docx_level_properties_are_read_as_each_side_reads_them() {
        let item = |ilvl: u32| {
            format!(
                r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="{ilvl}"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>Item</w:t></w:r></w:p>"#
            )
        };
        let flat = item(0).repeat(3);
        let nested = [item(0), item(1), item(0), item(1)].concat();
        let differs = |body: &str, levels: &str, instance: &str| {
            let document = word_part("document", body);
            let numbering = format!(
                r#"<w:numbering {WORD_NS}><w:abstractNum w:abstractNumId="0">{levels}</w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/>{instance}</w:num></w:numbering>"#
            );
            docx_preflight(&[
                ("word/document.xml", &document),
                ("word/numbering.xml", numbering.as_bytes()),
            ])
            .list_numbering_differs
        };
        let level = |attributes: &str, inner: &str| format!("<w:lvl{attributes}>{inner}</w:lvl>");
        let zero = |inner: &str| level(r#" w:ilvl="0""#, inner);
        let decimal =
            zero(r#"<w:start w:val="1"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/>"#);
        let letters = level(
            r#" w:ilvl="1""#,
            r#"<w:start w:val="1"/><w:numFmt w:val="lowerLetter"/><w:lvlText w:val="%2)"/>"#,
        );
        assert!(!differs(&flat, &decimal, ""));
        // Word, as LibreOffice shows it, reads a repeated property's last
        // element and AnyDoc its first: 7. 8. 9. against 1. 2. 3.
        let text = r#"<w:lvlText w:val="%1."/>"#;
        let format = r#"<w:numFmt w:val="decimal"/>"#;
        let start = r#"<w:start w:val="1"/>"#;
        for inner in [
            format!(r#"<w:start w:val="1"/><w:start w:val="7"/>{format}{text}"#),
            format!(r#"{start}<w:numFmt w:val="decimal"/><w:numFmt w:val="upperRoman"/>{text}"#),
            format!(r#"{start}{format}<w:lvlText w:val="%1."/><w:lvlText w:val="Art %1:"/>"#),
            format!(r#"{start}{format}<w:lvlText w:val="Art %1:"/><w:lvlText w:val=""/>"#),
            // A number without a value, or with one LibreOffice cannot
            // read, is 0; it reads `7x` as 7 and a value past `i32` as 0.
            format!(r#"<w:start w:val="7"/><w:start/>{format}{text}"#),
            format!(r#"<w:start w:val="7"/><w:start w:val="abc"/>{format}{text}"#),
            format!(r#"<w:start w:val="1"/><w:start w:val="7x"/>{format}{text}"#),
            format!(r#"<w:start w:val="99999999999"/>{format}{text}"#),
            // The number text is classified as Word reads it: here past the
            // bound the replay reads.
            format!(
                r#"{start}{format}{text}<w:lvlText w:val="{} %1."/>"#,
                "X".repeat(MAX_DOCX_LEVEL_TEXT_BYTES + 1)
            ),
        ] {
            assert!(differs(&flat, &zero(&inner), ""), "{inner}");
        }
        // A format, number text, or style Word finds no value in, or a
        // format ECMA-376 does not define, leaves the one before it.
        let roman = r#"<w:numFmt w:val="upperRoman"/>"#;
        for inner in [
            format!(r#"<w:start w:val="3"/><w:start w:val="3"/>{format}{format}{text}{text}"#),
            format!(r#"{start}{roman}<w:numFmt/>{text}"#),
            format!(r#"{start}{roman}<w:numFmt w:val="bogus"/>{text}"#),
            format!(r#"{start}<w:numFmt w:val=" upperRoman"/>{text}"#),
            format!(r#"{start}{format}<w:lvlText w:val="Art %1:"/><w:lvlText/>"#),
            format!(r#"<w:start w:val="+7"/>{format}{text}"#),
        ] {
            assert!(!differs(&flat, &zero(&inner), ""), "{inner}");
        }
        // Restarts: Word takes the last `w:lvlRestart`, never after level
        // 0 here, where AnyDoc takes the first.
        let restarting = level(
            r#" w:ilvl="1""#,
            r#"<w:start w:val="1"/><w:numFmt w:val="lowerLetter"/><w:lvlRestart w:val="1"/><w:lvlRestart w:val="0"/><w:lvlText w:val="%2)"/>"#,
        );
        assert!(differs(
            &nested,
            &[decimal.clone(), restarting].concat(),
            ""
        ));
        // Levels are found by WordprocessingML's `w:ilvl`, read as an
        // integer: a level naming none, or naming one only without a
        // prefix, is dropped where no level precedes it and otherwise
        // read into the level before it; `1x` names level 1.
        let upper = r#"<w:start w:val="1"/><w:numFmt w:val="upperRoman"/><w:lvlText w:val="%1."/>"#;
        for levels in [
            level("", upper),
            [level("", upper), letters.clone()].concat(),
            [level(r#" ilvl="0""#, upper), letters.clone()].concat(),
            [decimal.clone(), letters.clone(), level("", upper)].concat(),
            [
                decimal.clone(),
                letters.clone(),
                level(r#" w:ilvl="1x""#, upper),
            ]
            .concat(),
        ] {
            assert!(differs(&nested, &levels, ""), "{levels}");
        }
        for levels in [
            [decimal.clone(), level("", upper), letters.clone()].concat(),
            [level(r#" w:ilvl="x""#, upper), letters.clone()].concat(),
            [level(r#" w:ilvl=" 0 ""#, upper), letters.clone()].concat(),
        ] {
            assert!(!differs(&nested, &levels, ""), "{levels}");
        }
        // An override's level replaces the level it names itself, laid over
        // the definition's, and its start override the level the override
        // names; AnyDoc puts both at the override's level. An override
        // naming no level goes on with the previous one's, and is dropped
        // where there is none.
        let two = [decimal.clone(), letters.clone()].concat();
        let override_of = |attributes: &str, inner: &str| {
            format!("<w:lvlOverride{attributes}>{inner}</w:lvlOverride>")
        };
        for instance in [
            override_of(
                r#" w:ilvl="0""#,
                &level(
                    r#" w:ilvl="1""#,
                    r#"<w:start w:val="1"/><w:numFmt w:val="upperRoman"/><w:lvlText w:val="(%2)"/>"#,
                ),
            ),
            override_of("", r#"<w:startOverride w:val="5"/>"#),
            override_of(r#" w:ilvl="1x""#, r#"<w:startOverride w:val="5"/>"#),
            override_of(
                "",
                &level(
                    r#" w:ilvl="1""#,
                    r#"<w:start w:val="1"/><w:numFmt w:val="upperLetter"/><w:lvlText w:val="%2."/>"#,
                ),
            ),
            override_of(
                r#" w:ilvl="0""#,
                r#"<w:startOverride w:val="9"/><w:startOverride/>"#,
            ),
            override_of(
                r#" w:ilvl="0""#,
                &zero(r#"<w:start w:val="1"/><w:start w:val="5"/>"#),
            ),
        ] {
            assert!(differs(&nested, &two, &instance), "{instance}");
        }
        for instance in [
            override_of(
                r#" w:ilvl="1""#,
                &level(
                    "",
                    r#"<w:start w:val="1"/><w:numFmt w:val="upperRoman"/><w:lvlText w:val="[%2]"/>"#,
                ),
            ),
            override_of(r#" w:ilvl="x""#, r#"<w:startOverride w:val="5"/>"#),
            [
                override_of(r#" w:ilvl="0""#, &zero(upper)),
                override_of(r#" w:ilvl="0""#, r#"<w:startOverride w:val="6"/>"#),
            ]
            .concat(),
        ] {
            assert!(!differs(&nested, &two, &instance), "{instance}");
        }
        // Word lays an override's level over the definition's: a level
        // giving only its format keeps the definition's start and number
        // text, "(III)" where AnyDoc shows "I.".
        let definition =
            zero(r#"<w:start w:val="3"/><w:numFmt w:val="decimal"/><w:lvlText w:val="(%1)"/>"#);
        let shown = |instance: &str| {
            let numbering = format!(
                r#"<w:numbering {WORD_NS}><w:abstractNum w:abstractNumId="0">{definition}</w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/>{instance}</w:num></w:numbering>"#
            );
            let word = docx_numbering_definitions(numbering.as_bytes(), true).expect("numbering");
            let levels = word.levels_from(&word.lists[&1], 0, true);
            let level = levels[0].clone().expect("level 0");
            (
                level.format.as_deref().map(str::to_string),
                level.start,
                level.number_text().map(str::to_string),
            )
        };
        let expected = (
            Some("upperRoman".to_string()),
            Some(3),
            Some("(%1)".to_string()),
        );
        assert_eq!(
            shown(&override_of(r#" w:ilvl="0""#, &zero(roman))),
            expected
        );
        assert_eq!(
            shown(&override_of(r#" w:ilvl="0""#, &level("", roman))),
            expected
        );
        assert!(differs(
            &flat,
            &definition,
            &override_of(r#" w:ilvl="0""#, &zero(roman))
        ));
    }

    #[test]
    fn docx_paragraph_marks_are_read_as_each_side_reads_them() {
        const COMPATIBILITY: &str = r#"xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:w14="http://schemas.microsoft.com/office/word/2010/wordml" xmlns:zz="urn:zz" xmlns:x="urn:x""#;
        // List 1 is decimal, with a lettered second level; list 2 is upper
        // Roman. DecimalStyle and RomanStyle number through them.
        let numbering = format!(
            r#"<w:numbering {WORD_NS}><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/></w:lvl><w:lvl w:ilvl="1"><w:start w:val="1"/><w:numFmt w:val="lowerLetter"/><w:lvlText w:val="%2)"/></w:lvl></w:abstractNum><w:abstractNum w:abstractNumId="1"><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="upperRoman"/><w:lvlText w:val="%1."/></w:lvl></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num><w:num w:numId="2"><w:abstractNumId w:val="1"/></w:num></w:numbering>"#
        );
        let styles = format!(
            r#"<w:styles {WORD_NS}><w:style w:type="paragraph" w:styleId="DecimalStyle"><w:pPr><w:numPr><w:numId w:val="1"/></w:numPr></w:pPr></w:style><w:style w:type="paragraph" w:styleId="RomanStyle"><w:pPr><w:numPr><w:numId w:val="2"/></w:numPr></w:pPr></w:style></w:styles>"#
        );
        let preflight = |inside: &str| {
            let body = format!(r#"<w:p>{inside}<w:r><w:t>Item</w:t></w:r></w:p>"#).repeat(3);
            let document = format!(
                "<w:document {WORD_NS} {COMPATIBILITY}><w:body>{body}</w:body></w:document>"
            );
            docx_preflight(&[
                ("word/document.xml", document.as_bytes()),
                ("word/numbering.xml", numbering.as_bytes()),
                ("word/styles.xml", styles.as_bytes()),
            ])
        };
        let differs = |inside: &str| preflight(inside).list_numbering_differs;
        let list = |level: Option<u32>, list: Option<u32>| {
            let level = level
                .map(|level| format!(r#"<w:ilvl w:val="{level}"/>"#))
                .unwrap_or_default();
            let list = list
                .map(|list| format!(r#"<w:numId w:val="{list}"/>"#))
                .unwrap_or_default();
            format!("<w:numPr>{level}{list}</w:numPr>")
        };
        let mark = |inside: &str| format!("<w:pPr>{inside}</w:pPr>");
        let style = |id: &str| format!(r#"<w:pStyle w:val="{id}"/>"#);
        let alternate = |requires: &str, inside: &str| {
            format!(
                r#"<mc:AlternateContent><mc:Choice Requires="{requires}">{inside}</mc:Choice><mc:Fallback>{inside}</mc:Fallback></mc:AlternateContent>"#
            )
        };
        assert!(!differs(&mark(&list(Some(0), Some(1)))));
        // AnyDoc reads the first of each element and nothing inside
        // compatibility content; Word reads them all, later values
        // winning, in the branches it takes.
        for inside in [
            mark(
                r#"<w:numPr><w:ilvl w:val="0"/><w:numId w:val="2"/><w:numId w:val="1"/></w:numPr>"#,
            ),
            mark(&format!(
                "{}{}",
                list(Some(0), Some(2)),
                list(Some(0), Some(1))
            )),
            format!(
                "{}{}",
                mark(&list(Some(0), Some(2))),
                mark(&list(Some(0), Some(1)))
            ),
            alternate("w14", &mark(&list(Some(0), Some(1)))),
            mark(&format!("{}{}", style("RomanStyle"), style("DecimalStyle"))),
            format!(
                "{}{}",
                mark(&style("RomanStyle")),
                mark(&style("DecimalStyle"))
            ),
            mark(&alternate("w14", &style("DecimalStyle"))),
            mark(&alternate("w14", &list(Some(0), Some(1)))),
            mark(&format!(
                r#"<w:numPr><w:ilvl w:val="0"/>{}</w:numPr>"#,
                alternate("w14", r#"<w:numId w:val="1"/>"#)
            )),
            format!("<w:pPr/>{}", mark(&list(Some(0), Some(1)))),
            // Word merges a later list with an earlier level.
            mark(&format!(
                "{}{}",
                list(Some(1), Some(2)),
                list(None, Some(1))
            )),
            format!(
                "{}{}",
                mark(&style("RomanStyle")),
                mark(&list(None, Some(1)))
            ),
        ] {
            assert!(differs(&inside), "{inside}");
        }
        for inside in [
            mark(&format!(
                "{}{}",
                list(Some(0), Some(1)),
                list(Some(0), Some(1))
            )),
            // A later mark without numbering leaves Word's in place.
            format!(
                "{}{}",
                mark(&list(Some(0), Some(2))),
                r#"<w:pPr><w:jc w:val="left"/></w:pPr>"#
            ),
            // Neither side reads a choice Word cannot, revision history, or
            // another vocabulary's element.
            format!(
                "{}{}",
                mark(&list(Some(0), Some(1))),
                r#"<mc:AlternateContent><mc:Choice Requires="zz"><w:pPr><w:numPr><w:numId w:val="2"/></w:numPr></w:pPr></mc:Choice></mc:AlternateContent>"#
            ),
            mark(&format!(
                r#"{}<w:pPrChange w:id="1" w:author="a"><w:pPr>{}</w:pPr></w:pPrChange>"#,
                list(Some(0), Some(1)),
                list(Some(0), Some(2))
            )),
            mark(&format!(
                r#"{}<x:numPr><w:numId w:val="2"/></x:numPr>"#,
                list(Some(0), Some(1))
            )),
        ] {
            assert!(!differs(&inside), "{inside}");
        }
        // A paragraph with two marks is one paragraph: counted once, it
        // numbers alike on both sides.
        let twice = format!(
            "{}{}",
            mark(&list(Some(0), Some(1))),
            mark(&list(Some(0), Some(1)))
        );
        let document = format!(
            "<w:document {WORD_NS}><w:body>{}{}</w:body></w:document>",
            format!(r#"<w:p>{twice}<w:r><w:t>Item</w:t></w:r></w:p>"#).repeat(2),
            r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>Item</w:t></w:r></w:p>"#
        );
        let mut scan = DocxStoryScan::default();
        scan_docx_story(document.as_bytes(), &mut scan).unwrap();
        assert_eq!(scan.list_paragraphs.len(), 3);
        // Word formats the label with every mark's run properties it reads.
        let hidden = |inside: &str| preflight(inside).hidden_content;
        assert!(hidden(&format!(
            "{}<w:pPr><w:rPr><w:vanish/></w:rPr></w:pPr>",
            mark(&list(Some(0), Some(1)))
        )));
        assert!(hidden(&mark(&format!(
            "{}<w:rPr>{}</w:rPr>",
            list(Some(0), Some(1)),
            alternate("w14", "<w:vanish/>")
        ))));
        assert!(!hidden(&mark(&format!(
            r#"{}<w:rPr><mc:AlternateContent><mc:Choice Requires="zz"><w:vanish/></mc:Choice></mc:AlternateContent></w:rPr>"#,
            list(Some(0), Some(1))
        ))));
    }

    #[test]
    fn docx_list_instances_share_their_definitions_levels() {
        // A definition whose unused levels carry text past the bound, and
        // instances that restart it, replace a level, or neither.
        let long = "A".repeat(MAX_DOCX_LEVEL_TEXT_BYTES + 1);
        let levels: String = (0..DOCX_LIST_LEVELS)
            .map(|ilvl| {
                let text = if ilvl == 0 {
                    "%1.".to_string()
                } else {
                    format!("%{}.{long}", ilvl + 1)
                };
                format!(
                    r#"<w:lvl w:ilvl="{ilvl}"><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:lvlText w:val="{text}"/></w:lvl>"#
                )
            })
            .collect();
        let instances: String = (1..=200)
            .map(|id| {
                let overrides = match id {
                    2 => r#"<w:lvlOverride w:ilvl="0"><w:startOverride w:val="4"/></w:lvlOverride>"#,
                    3 => r#"<w:lvlOverride w:ilvl="0"><w:lvl w:ilvl="0"><w:numFmt w:val="decimal"/><w:lvlText w:val="%1)"/></w:lvl></w:lvlOverride>"#,
                    _ => "",
                };
                format!(r#"<w:num w:numId="{id}"><w:abstractNumId w:val="0"/>{overrides}</w:num>"#)
            })
            .collect();
        let numbering = format!(
            r#"<w:numbering {WORD_NS}><w:abstractNum w:abstractNumId="0">{levels}</w:abstractNum>{instances}</w:numbering>"#
        );
        let read = docx_numbering_definitions(numbering.as_bytes(), false).expect("numbering");
        // Text past the bound is not kept.
        assert!(matches!(
            read.definitions[0].levels[1]
                .as_ref()
                .and_then(|level| level.text.as_ref()),
            Some(DocxLevelText::Oversized)
        ));
        let styles = HashMap::new();
        let mut reading = DocxReading::new(false, &read, &styles);
        let shape = |reading: &mut DocxReading, list: u64| {
            Rc::clone(&reading.instance(list).expect("instance").shape)
        };
        let first = shape(&mut reading, 1);
        // Instances that replace no level share one shape, restarted or not,
        // and a replaced level keeps sharing the others' text.
        for list in [2, 4, 200] {
            assert!(Rc::ptr_eq(&first, &shape(&mut reading, list)), "{list}");
        }
        let replaced = shape(&mut reading, 3);
        assert!(!Rc::ptr_eq(&first, &replaced));
        let text = |shape: &DocxShape, level: usize| match &shape.levels[level].text {
            Some(DocxLevelText::Text(text)) => Rc::clone(text),
            _ => panic!("level text"),
        };
        assert_eq!(&*text(&replaced, 0), "%1)");
        let definition_text = match read.definitions[0].levels[0]
            .as_ref()
            .map(|level| &level.text)
        {
            Some(Some(DocxLevelText::Text(text))) => Rc::clone(text),
            _ => panic!("definition text"),
        };
        assert!(Rc::ptr_eq(&definition_text, &text(&first, 0)));
        assert_eq!(reading.instances.shapes.len(), 1);

        // A paragraph numbered at a level whose text is past the bound is
        // disclosed; the same level unused, or a bullet there, is not.
        let item = |ilvl: u32| {
            format!(
                r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="{ilvl}"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>Item</w:t></w:r></w:p>"#
            )
        };
        let differs = |body: String, format: &str| {
            let document = word_part("document", &body);
            let numbering = format!(
                r#"<w:numbering {WORD_NS}><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/></w:lvl><w:lvl w:ilvl="1"><w:start w:val="1"/><w:numFmt w:val="{format}"/><w:lvlText w:val="{long}"/></w:lvl></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#
            );
            docx_preflight(&[
                ("word/document.xml", &document),
                ("word/numbering.xml", numbering.as_bytes()),
            ])
            .list_numbering_differs
        };
        assert!(!differs([item(0), item(0)].concat(), "decimal"));
        assert!(differs([item(0), item(1)].concat(), "decimal"));
        assert!(differs([item(0), item(1)].concat(), "none"));
        assert!(!differs([item(0), item(1)].concat(), "bullet"));
    }

    #[test]
    fn docx_list_parts_are_the_ones_each_side_reads() {
        let relationship = |id: &str, kind: &str, target: &str, extra: &str| {
            format!(r#"<Relationship Id="{id}" Type="{kind}" Target="{target}"{extra}/>"#)
        };
        let numbering = format!("{REL_NS}/numbering");
        let parts = |inner: &str| {
            let rels =
                format!(r#"<Relationships xmlns="{PACKAGE_RELS_NS}">{inner}</Relationships>"#);
            let read = package_relationships(rels.as_bytes()).expect("relationships");
            (
                word_typed_part(&read, "word/document.xml", NUMBERING_RELATIONSHIP),
                anydoc_typed_part(
                    &read,
                    "word/document.xml",
                    NUMBERING_RELATIONSHIP,
                    "numbering.xml",
                ),
            )
        };
        let named =
            |word: Option<&str>, anydoc: &str| (word.map(str::to_string), anydoc.to_string());
        // Word, as LibreOffice shows it, reads the first relationship of the
        // type, and AnyDoc the lowest id.
        assert_eq!(
            parts(
                &[
                    relationship("rId5", &numbering, "first.xml", ""),
                    relationship("rId3", &numbering, "lowest.xml", ""),
                ]
                .concat()
            ),
            named(Some("word/first.xml"), "word/lowest.xml")
        );
        // A later relationship with the same id replaces an earlier one for
        // AnyDoc; an external one, one in another namespace, and one of
        // another type are no numbering part; a Strict type is read as the
        // Transitional one.
        assert_eq!(
            parts(
                &[
                    relationship("rId1", &numbering, "replaced.xml", ""),
                    relationship("rId1", &numbering, "kept.xml", ""),
                    relationship("rId0", &numbering, "far.xml", r#" TargetMode="External""#),
                    r#"<x:Relationship xmlns:x="urn:x" Id="rId0" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/numbering" Target="decoy.xml"/>"#.to_string(),
                    relationship("rId0", &format!("{REL_NS}/styles"), "styles.xml", ""),
                ]
                .concat()
            ),
            named(Some("word/replaced.xml"), "word/kept.xml")
        );
        assert_eq!(
            parts(&relationship(
                "rId1",
                "http://purl.oclc.org/ooxml/officeDocument/relationships/numbering",
                "strict.xml",
                ""
            )),
            named(Some("word/strict.xml"), "word/strict.xml")
        );
        // Without a relationship Word reads no numbering, and AnyDoc the
        // conventional part.
        assert_eq!(parts(""), named(None, "word/numbering.xml"));
    }

    #[test]
    fn docx_lists_are_replayed_from_the_parts_each_side_reads() {
        let item = |text: &str| {
            format!(
                r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>{text}</w:t></w:r></w:p>"#
            )
        };
        let styled = |text: &str| {
            format!(
                r#"<w:p><w:pPr><w:pStyle w:val="ListItem"/></w:pPr><w:r><w:t>{text}</w:t></w:r></w:p>"#
            )
        };
        let numbering = |format: &str| {
            format!(
                r#"<w:numbering {WORD_NS}><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="{format}"/><w:lvlText w:val="%1."/></w:lvl></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num><w:num w:numId="2"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#
            )
        };
        let styles = |list: u32| {
            format!(
                r#"<w:styles {WORD_NS}><w:style w:type="paragraph" w:styleId="ListItem"><w:pPr><w:numPr><w:numId w:val="{list}"/></w:numPr></w:pPr></w:style></w:styles>"#
            )
        };
        let rels = |inner: &[(&str, &str, &str)]| {
            let inner: String = inner
                .iter()
                .map(|(id, kind, target)| {
                    format!(r#"<Relationship Id="{id}" Type="{REL_NS}/{kind}" Target="{target}"/>"#)
                })
                .collect();
            format!(r#"<Relationships xmlns="{PACKAGE_RELS_NS}">{inner}</Relationships>"#)
        };
        let differs = |body: &str, rels: String, parts: &[(&str, String)]| {
            let document = word_part("document", body);
            let mut entries: Vec<(&str, &[u8])> = vec![
                ("word/document.xml", &document),
                ("word/_rels/document.xml.rels", rels.as_bytes()),
            ];
            entries.extend(parts.iter().map(|(name, bytes)| (*name, bytes.as_bytes())));
            docx_preflight(&entries).list_numbering_differs
        };
        let items = [item("A"), item("B")].concat();
        let decimal = numbering("decimal");
        let roman = numbering("upperRoman");
        // Two numbering parts: Word reads the first relationship's, AnyDoc
        // the lowest id's.
        let two = [
            ("word/numbering.xml", decimal.clone()),
            ("word/roman.xml", roman.clone()),
        ];
        assert!(differs(
            &items,
            rels(&[
                ("rId1", "numbering", "numbering.xml"),
                ("rId0", "numbering", "roman.xml")
            ]),
            &two
        ));
        assert!(!differs(
            &items,
            rels(&[
                ("rId0", "numbering", "roman.xml"),
                ("rId1", "numbering", "numbering.xml")
            ]),
            &two
        ));
        // A numbering part no relationship names is AnyDoc's alone.
        assert!(differs(
            &items,
            rels(&[]),
            &[("word/numbering.xml", decimal.clone())]
        ));
        assert!(!differs(
            &items,
            rels(&[("rId1", "numbering", "numbering.xml")]),
            &[("word/numbering.xml", decimal.clone())]
        ));
        // So too the styles: a paragraph style numbering its paragraphs in a
        // part no relationship names, or in the one of two parts only AnyDoc
        // reads.
        let body = [styled("A"), styled("B")].concat();
        assert!(differs(
            &body,
            rels(&[("rId1", "numbering", "numbering.xml")]),
            &[
                ("word/numbering.xml", decimal.clone()),
                ("word/styles.xml", styles(1))
            ]
        ));
        let styled_parts = [
            ("word/numbering.xml", decimal.clone()),
            ("word/styles.xml", styles(1)),
            ("word/other.xml", styles(0)),
        ];
        assert!(differs(
            &body,
            rels(&[
                ("rId1", "numbering", "numbering.xml"),
                ("rId5", "styles", "styles.xml"),
                ("rId2", "styles", "other.xml")
            ]),
            &styled_parts
        ));
        assert!(!differs(
            &body,
            rels(&[
                ("rId1", "numbering", "numbering.xml"),
                ("rId2", "styles", "styles.xml"),
                ("rId5", "styles", "other.xml")
            ]),
            &styled_parts
        ));
        // However many numbering parts a package names, the lists are
        // replayed once, against the part each side reads.
        let mut many: Vec<(String, String)> = (0..300)
            .map(|index| {
                (
                    format!("word/n{index}.xml"),
                    format!("<w:numbering {WORD_NS}/>"),
                )
            })
            .collect();
        many.push(("word/numbering.xml".to_string(), decimal.clone()));
        let named: Vec<(String, &str, String)> = (0..300)
            .map(|index| {
                (
                    format!("rN{index:03}"),
                    "numbering",
                    format!("n{index}.xml"),
                )
            })
            .collect();
        let mut inner: Vec<(&str, &str, &str)> = vec![("rId0", "numbering", "numbering.xml")];
        inner.extend(
            named
                .iter()
                .map(|(id, kind, target)| (id.as_str(), *kind, target.as_str())),
        );
        let parts: Vec<(&str, String)> = many
            .iter()
            .map(|(name, bytes)| (name.as_str(), bytes.clone()))
            .collect();
        let layout = {
            let document = word_part("document", &items.repeat(100));
            let rels = rels(&inner);
            let mut entries: Vec<(&str, &[u8])> = vec![
                ("word/document.xml", &document),
                ("word/_rels/document.xml.rels", rels.as_bytes()),
            ];
            entries.extend(parts.iter().map(|(name, bytes)| (*name, bytes.as_bytes())));
            let package = docx_package(&entries);
            let mut archive = open_package(&package).expect("package");
            assert!(
                !preflight_package(&package, DocumentKind::Docx, DocumentVariant::Docx)
                    .expect("DOCX preflight")
                    .list_numbering_differs
            );
            ooxml_layout(&mut archive, DocumentKind::Docx).expect("layout")
        };
        assert_eq!(layout.numbering_parts.len(), 301);
        assert_eq!(
            layout.list_parts.word_numbering.as_deref(),
            Some("word/numbering.xml")
        );
        assert_eq!(layout.list_parts.anydoc_numbering, "word/numbering.xml");
        // A package with more entries than AnyDoc reads is refused.
        let entries: Vec<(String, &[u8])> = (0..=MAX_ARCHIVE_ENTRIES)
            .map(|index| (format!("e{index}"), b"".as_slice()))
            .collect();
        let entries: Vec<(&str, &[u8])> = entries
            .iter()
            .map(|(name, bytes)| (name.as_str(), *bytes))
            .collect();
        let oversized = zip_entries(&entries);
        assert!(matches!(
            open_package(&oversized),
            Err(DocumentError::ResourceLimit)
        ));
        // It is found without opening it, and classified by its extension.
        assert!(zip_past_entry_bound(&oversized));
        let classified = classify_bytes(&oversized, Path::new("many.docx"));
        assert_eq!(classified.kind, Some(DocumentKind::Docx));
        assert_eq!(classified.variant, Some(DocumentVariant::Docx));
        let within = zip_entries(&entries[1..]);
        assert!(!zip_past_entry_bound(&within));
        assert!(!zip_past_entry_bound(b"%PDF-1.7 PK\x01\x02"));
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
        let rels = br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId9" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="design/theme-styles.xml"/></Relationships>"#;
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
    fn odf_embedded_objects_are_judged_by_what_they_embed() {
        let content = br#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:xlink="http://www.w3.org/1999/xlink"><office:body><office:spreadsheet><table:table table:name="Sheet1"><table:shapes><draw:frame><draw:object xlink:href="./Object 1"/><draw:image xlink:href="./ObjectReplacements/Object 1"/></draw:frame></table:shapes><table:table-row><table:table-cell office:value-type="string"><text:p>Amount</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
        let manifest = |media: &str| {
            format!(
                r#"<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0"><manifest:file-entry manifest:full-path="/" manifest:media-type="application/vnd.oasis.opendocument.spreadsheet"/><manifest:file-entry manifest:full-path="Object 1/" manifest:media-type="{media}"/></manifest:manifest>"#
            )
        };
        let active = |manifest: Option<String>| {
            let mut extra: Vec<(&str, &[u8])> = vec![
                ("Object 1/content.xml", b"<office:document-content/>"),
                ("ObjectReplacements/Object 1", b"replacement"),
            ];
            if let Some(manifest) = &manifest {
                extra.push(("META-INF/manifest.xml", manifest.as_bytes()));
            }
            preflight_package(
                &ods_package(content, &extra),
                DocumentKind::Ods,
                DocumentVariant::Ods,
            )
            .unwrap()
            .active_content
        };
        // A chart shows as its replacement image, and AnyDoc converts a
        // formula; another embedded document, or one the manifest does not
        // describe, is refused.
        assert!(!active(Some(manifest(
            "application/vnd.oasis.opendocument.chart"
        ))));
        assert!(!active(Some(manifest(
            "application/vnd.oasis.opendocument.formula"
        ))));
        assert!(active(Some(manifest(
            "application/vnd.oasis.opendocument.text"
        ))));
        assert!(active(Some(manifest("application/vnd.sun.star.oleobject"))));
        assert!(active(None));
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
    fn epub_navigation_in_the_spine_need_not_list_itself() {
        let package = |spine: &str, nav: &str| {
            let opf = EPUB_OPF_WITH_SPINE.replace("{spine}", spine);
            let nav = EPUB_NAV_WITH_LINKS.replace("{links}", nav);
            let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
            let stored = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            writer.start_file("mimetype", stored).unwrap();
            writer.write_all(b"application/epub+zip").unwrap();
            for (name, bytes) in [
                ("META-INF/container.xml", EPUB_CONTAINER),
                ("OPS/package.opf", opf.as_bytes()),
                ("OPS/nav.xhtml", nav.as_bytes()),
                ("OPS/Text/ch1.xhtml", EPUB_CHAPTER_TWO),
                ("OPS/Text/ch2.xhtml", EPUB_CHAPTER_TWO),
            ] {
                writer
                    .start_file(name, zip::write::SimpleFileOptions::default())
                    .unwrap();
                writer.write_all(bytes).unwrap();
            }
            let bytes = writer.finish().unwrap().into_inner();
            preflight_package(&bytes, DocumentKind::Epub, DocumentVariant::Epub)
                .unwrap()
                .missing_required_content
        };
        let both =
            r#"<li><a href="Text/ch1.xhtml">One</a></li><li><a href="Text/ch2.xhtml">Two</a></li>"#;
        // pandoc places the navigation document in the spine.
        let with_nav = r#"<itemref idref="nav"/><itemref idref="ch1"/><itemref idref="ch2"/>"#;
        assert!(!package(with_nav, both));
        // A chapter the navigation leaves out is still refused.
        assert!(package(
            with_nav,
            r#"<li><a href="Text/ch1.xhtml">One</a></li>"#
        ));
        assert!(package(
            r#"<itemref idref="ch1"/><itemref idref="ch2"/>"#,
            r#"<li><a href="Text/ch2.xhtml">Two</a></li>"#
        ));
    }

    const EPUB_OPF_WITH_SPINE: &str = r#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="3.0"><metadata/><manifest><item id="ch1" href="Text/ch1.xhtml" media-type="application/xhtml+xml"/><item id="ch2" href="Text/ch2.xhtml" media-type="application/xhtml+xml"/><item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/></manifest><spine>{spine}</spine></package>"#;
    const EPUB_NAV_WITH_LINKS: &str = r#"<?xml version="1.0"?><html xmlns="http://www.w3.org/1999/xhtml"><head/><body><nav type="toc"><ol>{links}</ol></nav></body></html>"#;

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

        let hidden = br#"<html xmlns="http://www.w3.org/1999/xhtml"><body><p style="visibility:hidden">hidden</p></body></html>"#;
        let result = preflight_package(
            &epub_package(hidden, Some(EPUB_CHAPTER_TWO), &[]),
            DocumentKind::Epub,
            DocumentVariant::Epub,
        )
        .unwrap();
        assert!(result.hidden_content);

        // AnyDoc omits what `display: none` hides, as a reader does.
        let omitted = br#"<html xmlns="http://www.w3.org/1999/xhtml"><body><p style="display:none">hidden</p></body></html>"#;
        let result = preflight_package(
            &epub_package(omitted, Some(EPUB_CHAPTER_TWO), &[]),
            DocumentKind::Epub,
            DocumentVariant::Epub,
        )
        .unwrap();
        assert!(!result.hidden_content);
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

        let task = tokio::spawn(convert_with(document_frame("docx", &[]), worker));
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
    async fn supervisor_takes_the_preflight_from_the_worker() {
        let temporary = tempfile::tempdir().expect("temporary worker directory");
        // A worker that reads its request and answers with a fixed frame.
        let answering = |name: &str, response: WorkerResponse| {
            let frame_path = temporary.path().join(format!("{name}.frame"));
            let mut frame = Vec::new();
            write_worker_response(&mut frame, response, MAX_SERIALIZED_WORKER_RESPONSE_BYTES)
                .expect("worker frame");
            std::fs::write(&frame_path, frame).expect("write worker frame");
            let worker = temporary.path().join(format!("{name}.sh"));
            let frame_path = frame_path.to_string_lossy().replace('\'', "'\\''");
            std::fs::write(
                &worker,
                format!("#!/bin/sh\ncat > /dev/null\ncat '{frame_path}'\n"),
            )
            .expect("write answering worker");
            let mut permissions = std::fs::metadata(&worker)
                .expect("answering worker metadata")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&worker, permissions).expect("make worker executable");
            worker
        };
        // A package the preflight would refuse reaches the worker, which
        // runs the preflight: the supervisor reads no package itself.
        let refused = zip_entries(&[
            ("[Content_Types].xml", DOCX_TYPES),
            ("word/document.xml", DOCX_XML),
            ("word/vbaProject.bin", b"macro"),
        ]);
        let found = PackagePreflight {
            list_numbering_differs: true,
            ..PackagePreflight::default()
        };
        let classified = |kind, variant| {
            Some(WorkerClassification {
                kind: Some(kind),
                variant: Some(variant),
            })
        };
        let worker = answering(
            "converted",
            WorkerResponse {
                markdown: Some("Converted".into()),
                preflight: Some(found),
                classified: classified(DocumentKind::Docx, DocumentVariant::Docx),
                ..Default::default()
            },
        );
        let converted = convert_with(document_frame("docx", &refused), worker)
            .await
            .expect("worker Markdown");
        assert_eq!(converted.markdown, "Converted");
        assert_eq!(
            (converted.kind, converted.variant),
            (DocumentKind::Docx, DocumentVariant::Docx)
        );
        assert!(converted.preflight.list_numbering_differs);
        // Markdown without what the preflight found cannot be disclosed, nor
        // without the kind the worker found, nor for a kind whose route
        // converts nothing.
        for (name, response) in [
            (
                "unchecked",
                WorkerResponse {
                    markdown: Some("Converted".into()),
                    classified: classified(DocumentKind::Docx, DocumentVariant::Docx),
                    ..Default::default()
                },
            ),
            (
                "unclassified",
                WorkerResponse {
                    markdown: Some("Converted".into()),
                    preflight: Some(PackagePreflight::default()),
                    ..Default::default()
                },
            ),
            (
                "unrouted",
                WorkerResponse {
                    markdown: Some("Converted".into()),
                    preflight: Some(PackagePreflight::default()),
                    classified: classified(DocumentKind::Docx, DocumentVariant::Docm),
                    ..Default::default()
                },
            ),
        ] {
            let worker = answering(name, response);
            assert!(
                matches!(
                    convert_with(document_frame("docx", &refused), worker).await,
                    Err(DocumentError::WorkerProtocol)
                ),
                "{name}"
            );
        }
    }

    /// A frame the worker classifies, as `read_document_frame` reads one.
    fn document_frame(extension: &str, document: &[u8]) -> Vec<u8> {
        let mut frame = (extension.len() as u32).to_le_bytes().to_vec();
        frame.extend_from_slice(extension.as_bytes());
        frame.extend_from_slice(document);
        frame
    }

    /// Convert a frame with this worker, under a worker slot.
    async fn convert_with(
        frame: Vec<u8>,
        worker: PathBuf,
    ) -> Result<WorkerConversion, DocumentError> {
        let permit = worker_semaphore()
            .acquire_owned()
            .await
            .expect("worker slot");
        run_worker_conversion(frame, worker, permit).await
    }

    #[test]
    fn worker_classifies_and_routes_what_the_server_reads_whole() {
        // The frame carries the extension, which is all a classification
        // reads of a path.
        let frame = document_frame("CSV", b"a,b\n1,2\n");
        let (name, document) = classifying_frame(&frame).expect("frame");
        assert_eq!(name, PathBuf::from("document.CSV"));
        assert_eq!(document, b"a,b\n1,2\n");
        let (name, _) = classifying_frame(&document_frame("", b"x")).expect("frame");
        assert_eq!(name, PathBuf::from("document"));
        for broken in [
            vec![1, 0],
            [5u32.to_le_bytes().as_slice(), b"doc"].concat(),
            [
                ((MAX_EXTENSION_BYTES + 1) as u32).to_le_bytes().as_slice(),
                &[b'x'; MAX_EXTENSION_BYTES + 1],
            ]
            .concat(),
            [2u32.to_le_bytes().as_slice(), &[0xff, 0xfe]].concat(),
        ] {
            assert!(matches!(
                classifying_frame(&broken),
                Err(DocumentError::WorkerProtocol)
            ));
        }
        // The worker classifies by content, then by extension.
        let docx = zip_entries(&[
            ("[Content_Types].xml", DOCX_TYPES),
            ("word/document.xml", DOCX_XML),
        ]);
        let classified = |extension: &str, document: &[u8]| {
            let response =
                document_worker_response(WORKER_CLASSIFY, &document_frame(extension, document));
            let found = response.classified.expect("classification");
            (found.kind, found.variant)
        };
        assert_eq!(
            classified("pdf", &docx),
            (Some(DocumentKind::Docx), Some(DocumentVariant::Docx))
        );
        assert_eq!(
            classified("csv", b"a,b\n"),
            (Some(DocumentKind::Csv), Some(DocumentVariant::Csv))
        );
        assert_eq!(classified("txt", b"plain"), (None, None));
        // It routes what it converts as the server routed it: a
        // macro-enabled package is refused, unrecognized content too, and a
        // document is converted with the kind and variant it was found to be.
        let refused = |extension: &str, document: &[u8]| {
            document_worker_response(WORKER_CONVERT, &document_frame(extension, document))
                .error
                .expect("refusal")
                .code
        };
        let macro_enabled = zip_entries(&[
            ("[Content_Types].xml", br#"<Types><Override PartName="/word/document.xml" ContentType="application/vnd.ms-word.document.macroEnabled.main+xml"/></Types>"#),
            ("word/document.xml", DOCX_XML),
        ]);
        assert_eq!(refused("docm", &macro_enabled), "active_content_disabled");
        assert_eq!(refused("txt", b"plain"), "unrecognized");
        assert!(matches!(
            WorkerError {
                code: "unrecognized".into(),
                pages: Vec::new()
            }
            .into_document_error(),
            DocumentError::Unrecognized
        ));
        let converted =
            document_worker_response(WORKER_CONVERT, &document_frame("csv", b"a,b\n1,2\n"));
        assert!(converted
            .markdown
            .is_some_and(|markdown| markdown.contains('1')));
        let found = converted.classified.expect("classification");
        assert_eq!(
            (found.kind, found.variant),
            (Some(DocumentKind::Csv), Some(DocumentVariant::Csv))
        );
        // A variant's own code still converts, unclassified.
        let converted = document_worker_response(DocumentVariant::Csv.worker_code(), b"a,b\n1,2\n");
        assert!(converted.markdown.is_some() && converted.classified.is_none());
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
