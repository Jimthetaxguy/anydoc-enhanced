//! Facade crate wrapping firecrawl/pdf-inspector for the agent stack.
//!
//! All MCP tools and domain post-processors depend on this crate — never
//! on pdf-inspector directly. This gives us a single file to update when
//! the upstream API surface changes.

use serde::Serialize;
use std::path::Path;

// Re-export upstream types that callers need
pub use pdf_inspector::{
    DetectionConfig, LayoutComplexity, MarkdownOptions, PdfOptions, PdfProcessResult, PdfType,
    ProcessMode, ScanStrategy, TextItem,
};

/// Exact Firecrawl `pdf-inspector` release resolved in `Cargo.lock`.
///
/// Provider records on the wire read this constant; the
/// `provider_versions_match_lockfile` test fails if a dependency bump leaves
/// it behind.
pub const PDF_INSPECTOR_VERSION: &str = "1.24.0";

/// Exact Firecrawl AnyDoc release resolved in `Cargo.lock`.
pub const ANYDOC_VERSION: &str = "0.2.4";

/// Unified result for classification + optional extraction.
///
/// Wraps `PdfProcessResult` with serialization support for MCP tools. Fields
/// after `processing_time_ms` are additive: older callers can ignore them.
#[derive(Debug, Serialize)]
pub struct PdfInfo {
    pub pdf_type: String,
    pub confidence: f32,
    pub page_count: u32,
    pub pages_needing_ocr: Vec<u32>,
    pub has_encoding_issues: bool,
    pub title: Option<String>,
    pub markdown: Option<String>,
    pub processing_time_ms: u64,
    /// Why each page in `pages_needing_ocr` needs OCR, as upstream's fixed
    /// reason identifiers (`scanned`, `no_text`, `vector_text`,
    /// `suspected_garbled_text`, `invisible_text_layer`).
    pub ocr_reasons_by_page: Vec<PageOcrReasonsOutput>,
    /// Tables and columns found on each page. Absent for classification,
    /// which runs detection only and never analyzes layout.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layout: Option<LayoutOutput>,
    /// Fonts whose Unicode mapping lacked entries for codes the document
    /// shows, and how many were guessed or lost. Absent for
    /// classification, which decodes no text; empty when every code mapped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cmap_gaps: Option<Vec<FontCMapGapsOutput>>,
    /// Provenance fields from the document information dictionary.
    #[serde(skip_serializing_if = "PdfProvenance::is_empty")]
    pub provenance: PdfProvenance,
}

/// OCR reasons for one 1-indexed page.
#[derive(Debug, Serialize)]
pub struct PageOcrReasonsOutput {
    pub page: u32,
    pub reasons: Vec<String>,
}

/// Layout complexity with 1-indexed page numbers.
#[derive(Debug, Serialize)]
pub struct LayoutOutput {
    /// True when any page has tables or multiple text columns.
    pub is_complex: bool,
    pub pages_with_tables: Vec<u32>,
    pub pages_with_columns: Vec<u32>,
}

/// Unicode-mapping gaps for one font; `codes - interpolated - unmapped`
/// codes had an entry, and each `unmapped` code is a U+FFFD in the text.
#[derive(Debug, Serialize)]
pub struct FontCMapGapsOutput {
    pub font: String,
    pub codes: u32,
    pub interpolated: u32,
    pub unmapped: u32,
}

/// Which software wrote the PDF and when, as recorded by the document.
///
/// Free-text `/Author`, `/Subject`, and `/Keywords` entries are withheld:
/// they are invisible on the rendered page, commonly carry personal names,
/// and give a document a channel to address the agent that no reader of the
/// page would see.
#[derive(Debug, Default, Serialize)]
pub struct PdfProvenance {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub creator: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub producer: Option<String>,
    /// PDF date string as written, such as `D:20240115103000+01'00'`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub creation_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mod_date: Option<String>,
}

impl PdfProvenance {
    pub fn is_empty(&self) -> bool {
        self.creator.is_none()
            && self.producer.is_none()
            && self.creation_date.is_none()
            && self.mod_date.is_none()
    }
}

/// Upper bounds on document-controlled strings copied out of the PDF
/// structure, so a crafted document cannot inflate responses through them.
const MAX_TITLE_CHARS: usize = 1024;
const MAX_PROVENANCE_CHARS: usize = 256;
const MAX_FONT_NAME_CHARS: usize = 128;
const MAX_CMAP_GAP_FONTS: usize = 64;

fn truncate_chars(value: String, max_chars: usize) -> String {
    match value.char_indices().nth(max_chars) {
        Some((end, _)) => value[..end].to_string(),
        None => value,
    }
}

fn bounded(value: Option<String>, max_chars: usize) -> Option<String> {
    value.map(|value| truncate_chars(value, max_chars))
}

impl PdfInfo {
    /// Convert an upstream result, keeping only the signals `mode` computed:
    /// detection alone leaves layout and Unicode-mapping gaps at defaults
    /// that would otherwise read as "no tables" and "no gaps".
    pub fn from_result(r: PdfProcessResult, mode: &ProcessMode) -> Self {
        let analyzed = !matches!(mode, ProcessMode::DetectOnly);
        let layout = analyzed.then_some(LayoutOutput {
            is_complex: r.layout.is_complex,
            pages_with_tables: r.layout.pages_with_tables,
            pages_with_columns: r.layout.pages_with_columns,
        });
        let cmap_gaps = analyzed.then(|| {
            // Keep the fonts that lost the most text when the list is capped.
            let mut gaps = r.cmap_gaps;
            gaps.sort_by_key(|gap| std::cmp::Reverse((gap.unmapped, gap.interpolated)));
            gaps.into_iter()
                .take(MAX_CMAP_GAP_FONTS)
                .map(|gap| FontCMapGapsOutput {
                    font: truncate_chars(gap.font, MAX_FONT_NAME_CHARS),
                    codes: gap.codes,
                    interpolated: gap.interpolated,
                    unmapped: gap.unmapped,
                })
                .collect()
        });
        Self {
            pdf_type: format!("{:?}", r.pdf_type),
            confidence: r.confidence,
            page_count: r.page_count,
            pages_needing_ocr: r.pages_needing_ocr,
            has_encoding_issues: r.has_encoding_issues,
            title: bounded(r.title, MAX_TITLE_CHARS),
            markdown: r.markdown,
            processing_time_ms: r.processing_time_ms,
            ocr_reasons_by_page: r
                .ocr_reasons_by_page
                .into_iter()
                .map(|page| PageOcrReasonsOutput {
                    page: page.page,
                    reasons: page.reasons,
                })
                .collect(),
            layout,
            cmap_gaps,
            provenance: PdfProvenance {
                creator: bounded(r.creator, MAX_PROVENANCE_CHARS),
                producer: bounded(r.producer, MAX_PROVENANCE_CHARS),
                creation_date: bounded(r.creation_date, MAX_PROVENANCE_CHARS),
                mod_date: bounded(r.mod_date, MAX_PROVENANCE_CHARS),
            },
        }
    }
}

impl From<PdfProcessResult> for PdfInfo {
    /// Treats the result as a full-pipeline run; prefer
    /// [`PdfInfo::from_result`] when the mode is known.
    fn from(r: PdfProcessResult) -> Self {
        Self::from_result(r, &ProcessMode::Full)
    }
}

/// Wrapper for `pdf_inspector::RegionText` with Serialize support.
#[derive(Debug, Serialize)]
pub struct RegionTextOutput {
    pub text: String,
    pub needs_ocr: bool,
    /// Upstream OCR reason identifier when `needs_ocr` is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ocr_reason: Option<String>,
}

impl From<pdf_inspector::RegionText> for RegionTextOutput {
    fn from(r: pdf_inspector::RegionText) -> Self {
        Self {
            text: r.text,
            needs_ocr: r.needs_ocr,
            ocr_reason: r.ocr_reason,
        }
    }
}

/// Wrapper for `pdf_inspector::PageRegionResult` with Serialize support.
#[derive(Debug, Serialize)]
pub struct PageRegionResultOutput {
    pub page: u32,
    pub regions: Vec<RegionTextOutput>,
}

impl From<pdf_inspector::PageRegionResult> for PageRegionResultOutput {
    fn from(r: pdf_inspector::PageRegionResult) -> Self {
        Self {
            page: r.page,
            regions: r.regions.into_iter().map(RegionTextOutput::from).collect(),
        }
    }
}

pub mod document;
pub mod domain;
pub mod pdf_worker;

/// Errors from the facade layer.
#[derive(Debug, thiserror::Error)]
pub enum SkillkitError {
    #[error("PDF processing failed")]
    PdfError(#[from] pdf_inspector::PdfError),

    /// Original path retained for programmatic diagnostics. Its `Display`
    /// representation is deliberately path-free for MCP and log safety.
    #[error("File not found or inaccessible")]
    FileNotFound(String),

    #[error("File exceeds size limit ({size_bytes} > {limit_bytes})")]
    FileTooLarge { size_bytes: u64, limit_bytes: u64 },
}

/// Maximum input file size (50 MB).
const MAX_FILE_SIZE: u64 = 50 * 1024 * 1024;

/// Validate a path: canonicalize, check existence, check size cap.
pub fn validate_path(path: impl AsRef<Path>) -> Result<std::path::PathBuf, SkillkitError> {
    let canonical = std::fs::canonicalize(path.as_ref())
        .map_err(|_| SkillkitError::FileNotFound(path.as_ref().display().to_string()))?;

    let meta = std::fs::metadata(&canonical)
        .map_err(|_| SkillkitError::FileNotFound(canonical.display().to_string()))?;

    if meta.len() > MAX_FILE_SIZE {
        return Err(SkillkitError::FileTooLarge {
            size_bytes: meta.len(),
            limit_bytes: MAX_FILE_SIZE,
        });
    }

    Ok(canonical)
}

/// Read a validated PDF into memory, re-checking the size cap against the
/// bytes actually read in case the file grew after validation.
pub fn read_validated(path: impl AsRef<Path>) -> Result<Vec<u8>, SkillkitError> {
    let canonical = validate_path(&path)?;
    let buffer = std::fs::read(&canonical)
        .map_err(|_| SkillkitError::FileNotFound(canonical.display().to_string()))?;
    check_size(&buffer)?;
    Ok(buffer)
}

fn check_size(buffer: &[u8]) -> Result<(), SkillkitError> {
    if buffer.len() as u64 > MAX_FILE_SIZE {
        return Err(SkillkitError::FileTooLarge {
            size_bytes: buffer.len() as u64,
            limit_bytes: MAX_FILE_SIZE,
        });
    }
    Ok(())
}

/// Classify a PDF without extracting text.
pub fn classify(path: impl AsRef<Path>) -> Result<PdfInfo, SkillkitError> {
    classify_bytes(&read_validated(path)?)
}

/// Full pipeline: classify + extract + markdown.
pub fn process(path: impl AsRef<Path>) -> Result<PdfInfo, SkillkitError> {
    process_bytes(&read_validated(path)?)
}

/// Process with custom options (page filter, process mode, etc.).
pub fn process_with_options(
    path: impl AsRef<Path>,
    options: PdfOptions,
) -> Result<PdfInfo, SkillkitError> {
    process_bytes_with_options(&read_validated(path)?, options)
}

/// Analyze layout complexity of a PDF without full text extraction.
pub fn analyze(path: impl AsRef<Path>) -> Result<PdfInfo, SkillkitError> {
    analyze_bytes(&read_validated(path)?)
}

/// Extract text within bounding-box regions from a PDF.
///
/// `regions` is `&[(page_0indexed, Vec<[x1, y1, x2, y2]>) ]` in PDF points
/// with top-left origin.
pub fn extract_text_regions(
    path: impl AsRef<Path>,
    regions: &[(u32, Vec<[f32; 4]>)],
) -> Result<Vec<PageRegionResultOutput>, SkillkitError> {
    extract_text_regions_bytes(&read_validated(path)?, regions)
}

/// Extract tables within bounding-box regions from a PDF as markdown pipe-tables.
///
/// Similar to `extract_text_regions` but runs table detection and returns
/// markdown pipe-tables instead of flat text.
pub fn extract_table_regions(
    path: impl AsRef<Path>,
    regions: &[(u32, Vec<[f32; 4]>)],
) -> Result<Vec<PageRegionResultOutput>, SkillkitError> {
    extract_table_regions_bytes(&read_validated(path)?, regions)
}

// Byte-level entry points shared by the path functions above and the PDF
// worker, so both routes run one implementation.

/// Classify PDF bytes without extracting text.
pub fn classify_bytes(buffer: &[u8]) -> Result<PdfInfo, SkillkitError> {
    process_bytes_with_options(buffer, PdfOptions::detect_only())
}

/// Classify, extract, and render Markdown from PDF bytes.
pub fn process_bytes(buffer: &[u8]) -> Result<PdfInfo, SkillkitError> {
    process_bytes_with_options(buffer, PdfOptions::new())
}

/// Analyze layout complexity of PDF bytes without rendering Markdown.
pub fn analyze_bytes(buffer: &[u8]) -> Result<PdfInfo, SkillkitError> {
    process_bytes_with_options(buffer, PdfOptions::new().mode(ProcessMode::Analyze))
}

/// Process PDF bytes with custom options.
pub fn process_bytes_with_options(
    buffer: &[u8],
    options: PdfOptions,
) -> Result<PdfInfo, SkillkitError> {
    check_size(buffer)?;
    let mode = options.mode.clone();
    let result = pdf_inspector::process_pdf_mem_with_options(buffer, options)?;
    Ok(PdfInfo::from_result(result, &mode))
}

/// Extract text within bounding-box regions from PDF bytes.
pub fn extract_text_regions_bytes(
    buffer: &[u8],
    regions: &[(u32, Vec<[f32; 4]>)],
) -> Result<Vec<PageRegionResultOutput>, SkillkitError> {
    check_size(buffer)?;
    let results = pdf_inspector::extract_text_in_regions_mem(buffer, regions)?;
    Ok(results
        .into_iter()
        .map(PageRegionResultOutput::from)
        .collect())
}

/// Extract tables within bounding-box regions from PDF bytes.
pub fn extract_table_regions_bytes(
    buffer: &[u8],
    regions: &[(u32, Vec<[f32; 4]>)],
) -> Result<Vec<PageRegionResultOutput>, SkillkitError> {
    check_size(buffer)?;
    let results = pdf_inspector::extract_tables_in_regions_mem(buffer, regions)?;
    Ok(results
        .into_iter()
        .map(PageRegionResultOutput::from)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_respects_character_boundaries() {
        assert_eq!(truncate_chars("§§§§".to_string(), 2), "§§");
        assert_eq!(truncate_chars("short".to_string(), 16), "short");
        assert_eq!(bounded(None, 4), None);
    }

    #[test]
    fn provider_versions_match_lockfile() {
        let lock = include_str!("../../../Cargo.lock");
        for (name, expected) in [
            ("pdf-inspector", PDF_INSPECTOR_VERSION),
            ("anydoc", ANYDOC_VERSION),
        ] {
            let marker = format!("name = \"{name}\"\nversion = \"");
            let start = lock
                .find(&marker)
                .unwrap_or_else(|| panic!("{name} is missing from Cargo.lock"))
                + marker.len();
            let end = start + lock[start..].find('"').expect("closing quote");
            assert_eq!(
                &lock[start..end],
                expected,
                "{name} provider version drifted from Cargo.lock"
            );
        }
    }

    #[test]
    fn validate_path_rejects_missing_file() {
        let supplied = "sensitive-document-name.pdf";
        let error = validate_path(supplied).unwrap_err();
        let message = error.to_string();
        assert!(matches!(&error, SkillkitError::FileNotFound(_)));
        assert_eq!(message, "File not found or inaccessible");
        assert!(!message.contains(supplied));
    }
}
