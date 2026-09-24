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
/// Wraps `PdfProcessResult` with serialization support for MCP tools.
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
}

impl From<PdfProcessResult> for PdfInfo {
    fn from(r: PdfProcessResult) -> Self {
        Self {
            pdf_type: format!("{:?}", r.pdf_type),
            confidence: r.confidence,
            page_count: r.page_count,
            pages_needing_ocr: r.pages_needing_ocr,
            has_encoding_issues: r.has_encoding_issues,
            title: r.title,
            markdown: r.markdown,
            processing_time_ms: r.processing_time_ms,
        }
    }
}

/// Wrapper for `pdf_inspector::RegionText` with Serialize support.
#[derive(Debug, Serialize)]
pub struct RegionTextOutput {
    pub text: String,
    pub needs_ocr: bool,
}

impl From<pdf_inspector::RegionText> for RegionTextOutput {
    fn from(r: pdf_inspector::RegionText) -> Self {
        Self {
            text: r.text,
            needs_ocr: r.needs_ocr,
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

/// Classify a PDF without extracting text.
pub fn classify(path: impl AsRef<Path>) -> Result<PdfInfo, SkillkitError> {
    let canonical = validate_path(&path)?;
    let result = pdf_inspector::detect_pdf(&canonical)?;
    Ok(PdfInfo::from(result))
}

/// Full pipeline: classify + extract + markdown.
pub fn process(path: impl AsRef<Path>) -> Result<PdfInfo, SkillkitError> {
    let canonical = validate_path(&path)?;
    let result = pdf_inspector::process_pdf(&canonical)?;
    Ok(PdfInfo::from(result))
}

/// Process with custom options (page filter, process mode, etc.).
pub fn process_with_options(
    path: impl AsRef<Path>,
    options: PdfOptions,
) -> Result<PdfInfo, SkillkitError> {
    let canonical = validate_path(&path)?;
    let result = pdf_inspector::process_pdf_with_options(&canonical, options)?;
    Ok(PdfInfo::from(result))
}

/// Analyze layout complexity of a PDF without full text extraction.
pub fn analyze(path: impl AsRef<Path>) -> Result<PdfInfo, SkillkitError> {
    let canonical = validate_path(&path)?;
    let options = PdfOptions::new().mode(ProcessMode::Analyze);
    let result = pdf_inspector::process_pdf_with_options(&canonical, options)?;
    Ok(PdfInfo::from(result))
}

/// Extract text within bounding-box regions from a PDF.
///
/// `regions` is `&[(page_0indexed, Vec<[x1, y1, x2, y2]>) ]` in PDF points
/// with top-left origin.
pub fn extract_text_regions(
    path: impl AsRef<Path>,
    regions: &[(u32, Vec<[f32; 4]>)],
) -> Result<Vec<PageRegionResultOutput>, SkillkitError> {
    let canonical = validate_path(&path)?;
    let buffer = std::fs::read(&canonical)
        .map_err(|_| SkillkitError::FileNotFound(canonical.display().to_string()))?;
    let results = pdf_inspector::extract_text_in_regions_mem(&buffer, regions)?;
    Ok(results
        .into_iter()
        .map(PageRegionResultOutput::from)
        .collect())
}

/// Extract tables within bounding-box regions from a PDF as markdown pipe-tables.
///
/// Similar to `extract_text_regions` but runs table detection and returns
/// markdown pipe-tables instead of flat text.
pub fn extract_table_regions(
    path: impl AsRef<Path>,
    regions: &[(u32, Vec<[f32; 4]>)],
) -> Result<Vec<PageRegionResultOutput>, SkillkitError> {
    let canonical = validate_path(&path)?;
    let buffer = std::fs::read(&canonical)
        .map_err(|_| SkillkitError::FileNotFound(canonical.display().to_string()))?;
    let results = pdf_inspector::extract_tables_in_regions_mem(&buffer, regions)?;
    Ok(results
        .into_iter()
        .map(PageRegionResultOutput::from)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

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
