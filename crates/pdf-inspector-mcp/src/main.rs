//! MCP server for pdf-inspector — exposes classify, extract, and layout
//! tools to coding agents over stdio transport.
//!
//! Tools: classify_pdf, pdf_to_markdown, analyze_layout, batch_classify,
//! extract_text_regions, extract_table_regions, identify_tax_form,
//! split_sec_filing, parse_irc_sections, list_tax_packages,
//! review_tax_package, compare_line_items, render_review_memo, document_capabilities, classify_document, document_to_markdown.
//!
//! All tool handlers are wrapped in a 30-second timeout to bound worst-case
//! latency on pathological PDFs. PDF and document parsing runs in a private
//! worker process that the server kills on timeout and, on Linux, confines to
//! an address-space ceiling; hosts without the worker sandbox parse PDFs
//! in-process. Logs go to stderr — stdout is reserved for the JSON-RPC
//! channel and contaminating it would break the MCP protocol.

use pdf_inspector_skillkit::pdf_worker::{self, PdfOperation, PdfToolError};
use pdf_inspector_skillkit::RegionFrame;
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router, ServerHandler, ServiceExt,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Per-tool wall-clock cap. Pathological PDFs can spin pdf-inspector for
/// minutes; bound it so the agent caller can recover.
const TOOL_TIMEOUT: Duration = Duration::from_secs(30);

/// Accept only a level for this crate. Raw tracing directives are rejected so
/// callers cannot re-enable dependency logs that may contain MCP arguments.
fn local_log_level(requested: Option<&str>) -> &'static str {
    match requested.map(str::trim) {
        Some(value) if value.eq_ignore_ascii_case("off") => "off",
        Some(value) if value.eq_ignore_ascii_case("error") => "error",
        Some(value) if value.eq_ignore_ascii_case("warn") => "warn",
        Some(value) if value.eq_ignore_ascii_case("info") => "info",
        Some(value) if value.eq_ignore_ascii_case("debug") => "debug",
        Some(value) if value.eq_ignore_ascii_case("trace") => "trace",
        _ => "info",
    }
}

fn server_log_filter(requested: Option<&str>) -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::new(format!(
        "off,{}={}",
        env!("CARGO_CRATE_NAME"),
        local_log_level(requested)
    ))
}

/// Render a uniform JSON error envelope. Matches the shape used by the
/// per-tool error branches so callers see one schema regardless of source.
fn json_error(msg: impl std::fmt::Display) -> String {
    serde_json::json!({ "error": msg.to_string() }).to_string()
}

/// Run a future with a wall-clock timeout. On timeout, return a structured
/// JSON error string (not a panic) so the rmcp tool schema — which expects
/// `String` — stays intact.
///
/// `timeout` is a parameter (rather than always reading the `TOOL_TIMEOUT`
/// constant) so this is unit-testable with a short duration instead of
/// waiting out the real 30s production timeout.
///
/// For this to actually preempt a caller, `fut` must contain a genuine
/// await point. `tokio::time::timeout` races `fut` against a timer inside
/// a single task's poll loop — if `fut` never yields (e.g. it runs a
/// synchronous, CPU-bound closure inline with no `.await`), the executor
/// can't interleave the timer, so the timeout can never fire until the
/// work finishes on its own. `dispatch` below avoids this by running
/// blocking work via `tokio::task::spawn_blocking`, whose `JoinHandle`
/// await is a real yield point.
async fn with_timeout<F>(tool: &'static str, timeout: Duration, fut: F) -> String
where
    F: std::future::Future<Output = String>,
{
    match tokio::time::timeout(timeout, fut).await {
        Ok(s) => s,
        Err(_) => {
            tracing::warn!(tool, ?timeout, "tool timed out");
            json_error(format!("tool '{tool}' timed out after {timeout:?}"))
        }
    }
}

/// Map a `JoinError` from a `spawn_blocking` task (panic or cancellation)
/// into the same structured JSON error envelope used elsewhere, so callers
/// always see `{"error": "..."}` regardless of failure mode.
fn join_error_to_json(tool: &'static str, err: tokio::task::JoinError) -> String {
    let reason = if err.is_panic() {
        "panicked".to_string()
    } else if err.is_cancelled() {
        "was cancelled".to_string()
    } else {
        err.to_string()
    };
    tracing::warn!(tool, error = %reason, "blocking task failed");
    json_error(format!("tool '{tool}' failed: blocking task {reason}"))
}

/// Common dispatch envelope: log invocation, run the (CPU-bound,
/// synchronous) work on tokio's blocking thread pool under the timeout,
/// serialize success or render error. Used by the in-process Sweet review
/// tools; PDF tools go through `dispatch_pdf` and the worker.
///
/// `work` runs inside `tokio::task::spawn_blocking` rather than inline: it
/// is CPU-bound and can run for a while on pathological PDFs, and running
/// it directly on the async executor thread both blocks that worker for
/// other tasks and — since it has no `.await` inside — starves the
/// `with_timeout` timer of any point at which it could fire.
async fn dispatch<T, E, F>(tool: &'static str, work: F) -> String
where
    T: Serialize + Send + 'static,
    E: std::fmt::Display + Send + 'static,
    F: FnOnce() -> Result<T, E> + Send + 'static,
{
    tracing::debug!(tool, "tool invoked");
    with_timeout(tool, TOOL_TIMEOUT, async move {
        match tokio::task::spawn_blocking(work).await {
            Ok(Ok(v)) => serde_json::to_string_pretty(&v).unwrap_or_else(json_error),
            Ok(Err(e)) => {
                tracing::warn!(tool, error = %e, "tool failed");
                json_error(e)
            }
            Err(join_err) => join_error_to_json(tool, join_err),
        }
    })
    .await
}

/// Dispatch envelope for the PDF tools: the operation runs in the bounded
/// worker (or in-process where the worker sandbox is unavailable) and returns
/// its serialized result, or the uniform `{"error": ...}` envelope.
async fn dispatch_pdf<F>(tool: &'static str, work: F) -> String
where
    F: std::future::Future<Output = Result<String, PdfToolError>>,
{
    tracing::debug!(tool, "tool invoked");
    with_timeout(tool, TOOL_TIMEOUT, async move {
        match work.await {
            Ok(json) => json,
            Err(error) => {
                tracing::warn!(tool, error = %error, "tool failed");
                json_error(error)
            }
        }
    })
    .await
}

async fn run_pdf(
    operation: PdfOperation,
    path: String,
    regions: Vec<(u32, Vec<[f32; 4]>)>,
) -> Result<String, PdfToolError> {
    run_pdf_regions(operation, path, regions, RegionFrame::Sheet).await
}

async fn run_pdf_regions(
    operation: PdfOperation,
    path: String,
    regions: Vec<(u32, Vec<[f32; 4]>)>,
    frame: RegionFrame,
) -> Result<String, PdfToolError> {
    pdf_worker::run_in_frame(operation, &path, &regions, frame)
        .await
        .map(|json| json.get().to_string())
}

fn to_pretty_json(value: &impl Serialize) -> Result<String, PdfToolError> {
    serde_json::to_string_pretty(value).map_err(|_| PdfToolError::Protocol)
}

/// Parse and serialize worker Markdown on the blocking pool: it can run to
/// tens of megabytes, and work on an executor thread is work the tool
/// timeout cannot interrupt.
async fn on_blocking_pool<F>(work: F) -> Result<String, PdfToolError>
where
    F: FnOnce() -> Result<String, PdfToolError> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| PdfToolError::Processing)?
}

fn document_json_error(error: &pdf_inspector_skillkit::document::DocumentError) -> String {
    let mut value = serde_json::json!({
        "error": error.to_string(),
        "code": error.code(),
    });
    if let pdf_inspector_skillkit::document::DocumentError::OcrRequired { pages } = error {
        value["pages"] = serde_json::json!(pages);
    }
    value.to_string()
}

async fn dispatch_document<T, F, Fut>(tool: &'static str, work: F) -> String
where
    T: Serialize,
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T, pdf_inspector_skillkit::document::DocumentError>>,
{
    tracing::debug!(tool, "tool invoked");
    match tokio::time::timeout(TOOL_TIMEOUT, work()).await {
        Ok(Ok(value)) => serde_json::to_string_pretty(&value).unwrap_or_else(json_error),
        Ok(Err(error)) => {
            tracing::warn!(tool, error = %error, "tool failed");
            document_json_error(&error)
        }
        Err(_) => {
            tracing::warn!(tool, "document tool timed out");
            serde_json::json!({
                "error": format!("tool {tool} timed out after {TOOL_TIMEOUT:?}"),
                "code": "worker_timeout",
            })
            .to_string()
        }
    }
}

async fn dispatch_document_sync<T, F>(tool: &'static str, work: F) -> String
where
    T: Serialize + Send + 'static,
    F: FnOnce() -> Result<T, pdf_inspector_skillkit::document::DocumentError> + Send + 'static,
{
    dispatch_document(tool, move || async move {
        tokio::task::spawn_blocking(work)
            .await
            .map_err(|_| pdf_inspector_skillkit::document::DocumentError::ConversionFailed)?
    })
    .await
}

/// Input for single-path tools (classify, markdown, analyze).
#[derive(Deserialize, JsonSchema)]
struct PathInput {
    /// Absolute or relative path to the PDF file.
    path: String,
}

/// Input for generic document classification and conversion.
#[derive(Deserialize, JsonSchema)]
struct DocumentPathInput {
    /// Absolute or relative path to a local document.
    path: String,
}

/// Input for the generic document capability declaration.
#[derive(Deserialize, JsonSchema)]
struct DocumentCapabilitiesInput {
    /// Stable format name, such as `docx`, `pdf`, `pptx`, `xlsx`, `ods`, `odt`, or `epub`.
    kind: String,
}

/// Input for batch_classify tool.
#[derive(Deserialize, JsonSchema)]
struct BatchClassifyInput {
    /// List of absolute or relative paths to PDF files.
    paths: Vec<String>,
}

/// A single region on a page specified in PDF points with top-left origin.
#[derive(Deserialize, JsonSchema)]
struct RegionSpec {
    /// 0-indexed page number.
    page: u32,
    /// List of rectangles `[x1, y1, x2, y2]` in PDF points (top-left origin).
    rects: Vec<[f32; 4]>,
}

/// The coordinate frame region rectangles are given in.
#[derive(Clone, Copy, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum RegionFrameInput {
    /// The page as laid out in its content stream, `/Rotate` not applied: PDF
    /// points, top-left origin of the visible page box, `y` down. The default,
    /// and the frame these tools have always used.
    #[default]
    Sheet,
    /// The page as rendered, turned clockwise by its `/Rotate`, with the same
    /// origin conventions: use it for boxes taken from a page image, such as a
    /// layout model's detections on a rotated scan.
    Display,
}

impl From<RegionFrameInput> for RegionFrame {
    fn from(frame: RegionFrameInput) -> Self {
        match frame {
            RegionFrameInput::Sheet => Self::Sheet,
            RegionFrameInput::Display => Self::Display,
        }
    }
}

/// Input for extract_text_regions and extract_table_regions tools.
#[derive(Deserialize, JsonSchema)]
struct RegionInput {
    /// Absolute or relative path to the PDF file.
    path: String,
    /// Regions to extract from, specified as (page, rects) pairs.
    regions: Vec<RegionSpec>,
    /// Frame the rectangles are given in: `sheet` (default) or `display`.
    #[serde(default)]
    frame: RegionFrameInput,
}

/// Input for Sweet package review and memo tools.
#[derive(Deserialize, JsonSchema)]
struct SweetPackageInput {
    /// Demo package id, such as `demo_1040_w2_schedule_c`.
    package_id: String,
}

/// Input for comparing one return line against one source document line.
#[derive(Deserialize, JsonSchema)]
struct SweetCompareLineItemsInput {
    /// Human-readable label for the comparison.
    label: String,
    /// Return form and line reference, such as `Form 1040 line 1a`.
    return_reference: String,
    /// Source document reference, such as `W-2 Box 1`.
    source_reference: String,
    /// Amount shown on the return. Demo values are whole dollars.
    return_amount: i64,
    /// Amount shown on the source document. Demo values are whole dollars.
    source_amount: i64,
    /// Allowed absolute difference before the comparison is flagged.
    #[serde(default)]
    tolerance: Option<i64>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct PdfInspectorServer {
    tool_router: ToolRouter<Self>,
}

impl PdfInspectorServer {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl PdfInspectorServer {
    /// Return the generic document contract for one known format.
    #[tool(
        description = "Return capabilities and safety boundaries for a generic document format",
        annotations(
            title = "Document format capabilities",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn document_capabilities(&self, params: Parameters<DocumentCapabilitiesInput>) -> String {
        let kind = params.0.kind;
        dispatch_document_sync("document_capabilities", move || {
            let kind = pdf_inspector_skillkit::document::DocumentKind::from_name(&kind)
                .ok_or(pdf_inspector_skillkit::document::DocumentError::Unrecognized)?;
            Ok::<_, pdf_inspector_skillkit::document::DocumentError>(
                pdf_inspector_skillkit::document::capabilities(kind),
            )
        })
        .await
    }

    /// Classify a local document without converting it.
    #[tool(
        description = "Classify a local document by content signature and report whether its generic route is enabled",
        annotations(
            title = "Classify document",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn classify_document(&self, params: Parameters<DocumentPathInput>) -> String {
        let path = params.0.path;
        dispatch_document("classify_document", move || async move {
            pdf_inspector_skillkit::document::classify(path).await
        })
        .await
    }

    /// Convert an enabled DOCX, exact `.pptx`, exact `.xlsx`, exact `.ods`, exact `.odt`, strict EPUB, or bounded CSV package through the supervised document worker.
    #[tool(
        description = "Convert an enabled local DOCX, exact `.pptx`, exact `.xlsx`, exact `.ods`, exact `.odt`, exact `.odp`, strict EPUB, or bounded CSV input to sanitized Markdown through a bounded worker. A document whose conversion would lose or misrender what its application shows is refused with a stable code; `completeness: partial` and the warnings name anything the Markdown shows differently",
        annotations(
            title = "Document to Markdown",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn document_to_markdown(&self, params: Parameters<DocumentPathInput>) -> String {
        let path = params.0.path;
        dispatch_document("document_to_markdown", move || async move {
            pdf_inspector_skillkit::document::to_markdown(path).await
        })
        .await
    }

    /// Classify a PDF as TextBased, Scanned, ImageBased, or Mixed.
    #[tool(
        description = "Classify a PDF as TextBased/Scanned/ImageBased/Mixed with confidence score, the pages that need OCR and why, and its recorded creation and modification dates",
        annotations(
            title = "Classify PDF",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn classify_pdf(&self, params: Parameters<PathInput>) -> String {
        let path = params.0.path;
        dispatch_pdf(
            "classify_pdf",
            run_pdf(PdfOperation::Classify, path, Vec::new()),
        )
        .await
    }

    /// Convert a PDF to clean Markdown.
    #[tool(
        description = "Convert a PDF to clean Markdown with headings, tables, lists, and code blocks; also reports per-page OCR reasons, layout, and fonts whose text may be garbled. Text on pages listed as needing OCR is missing or unreliable; `warnings` name text the Markdown repeats, pages whose word spacing may be misread, whose text drawn through forms is missed, or that lose a line taken for a running header, form values that are garbled or missing, annotation text, dynamic XFA forms, and embedded files that are never read, text read from layers a reader hides, painted invisibly, or set off the page, visible text left out as invisible or in content too dense to read, numbers a span's replacement text gives otherwise than its glyphs, Japanese, Chinese, or Korean text read without its font's map or set in vertical columns, table amounts that may sit in the wrong row or column or after their table, and pages these checks could not reach",
        annotations(
            title = "PDF to Markdown",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn pdf_to_markdown(&self, params: Parameters<PathInput>) -> String {
        let path = params.0.path;
        dispatch_pdf(
            "pdf_to_markdown",
            run_pdf(PdfOperation::Markdown, path, Vec::new()),
        )
        .await
    }

    /// Analyze layout complexity of a PDF (tables, multi-column, etc.).
    #[tool(
        description = "Analyze layout complexity of a PDF — returns pages with tables, pages with multiple columns, per-page OCR reasons, and fonts whose text may be garbled, without Markdown",
        annotations(
            title = "Analyze PDF layout",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn analyze_layout(&self, params: Parameters<PathInput>) -> String {
        let path = params.0.path;
        dispatch_pdf(
            "analyze_layout",
            run_pdf(PdfOperation::Analyze, path, Vec::new()),
        )
        .await
    }

    /// Batch classify multiple PDFs.
    ///
    /// Bespoke because per-item errors are folded into the response array
    /// rather than failing the whole call — so the dispatch helper doesn't fit.
    /// Items run concurrently within the PDF worker bound and are reported in
    /// input order.
    #[tool(
        description = "Classify multiple PDFs — returns array of {path, classification} objects",
        annotations(
            title = "Batch classify PDFs",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn batch_classify(&self, params: Parameters<BatchClassifyInput>) -> String {
        let paths = params.0.paths;
        tracing::debug!(tool = "batch_classify", count = paths.len(), "tool invoked");
        with_timeout("batch_classify", TOOL_TIMEOUT, async move {
            let mut paths = paths.into_iter().enumerate();
            let mut tasks = tokio::task::JoinSet::new();
            let mut results = Vec::new();
            loop {
                // Start no more classifications than the worker runs at
                // once, so a long list neither takes a task per path up
                // front nor holds the runtime before the timeout can fire.
                while tasks.len() < pdf_worker::MAX_IN_FLIGHT {
                    let Some((index, path)) = paths.next() else {
                        break;
                    };
                    tasks.spawn(async move {
                        let result = pdf_worker::run(PdfOperation::Classify, &path, &[]).await;
                        (index, path, result)
                    });
                }
                let Some(joined) = tasks.join_next().await else {
                    break;
                };
                let Ok((index, path, result)) = joined else {
                    return json_error("tool 'batch_classify' failed: classification task failed");
                };
                let entry = match result.and_then(|json| {
                    serde_json::from_str::<serde_json::Value>(json.get())
                        .map_err(|_| PdfToolError::Protocol)
                }) {
                    Ok(info) => serde_json::json!({
                        "path": path,
                        "classification": info
                    }),
                    Err(e) => {
                        tracing::warn!(error = %e, "tool failed");
                        serde_json::json!({
                            "path": path,
                            "error": e.to_string()
                        })
                    }
                };
                results.push((index, entry));
            }
            results.sort_by_key(|(index, _)| *index);
            let results: Vec<_> = results.into_iter().map(|(_, entry)| entry).collect();
            serde_json::to_string_pretty(&results).unwrap_or_else(json_error)
        })
        .await
    }

    /// Extract text from specified regions of a PDF.
    ///
    /// Each region is defined by a page number (0-indexed) and a list of
    /// bounding rectangles `[x1, y1, x2, y2]` in PDF points with top-left origin.
    #[tool(
        description = "Extract text from specified rectangular regions of a PDF — returns text per region with OCR hints. Rects are PDF points, top-left origin; set frame to display for boxes taken from a rendered page image (rotated pages)",
        annotations(
            title = "Extract PDF text regions",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn extract_text_regions(&self, params: Parameters<RegionInput>) -> String {
        let RegionInput {
            path,
            regions,
            frame,
        } = params.0;
        let regions = regions.into_iter().map(|r| (r.page, r.rects)).collect();
        dispatch_pdf(
            "extract_text_regions",
            run_pdf_regions(PdfOperation::TextRegions, path, regions, frame.into()),
        )
        .await
    }

    /// Extract tables from specified regions of a PDF as markdown pipe-tables.
    ///
    /// Similar to extract_text_regions but runs table detection and returns
    /// markdown pipe-tables instead of flat text.
    #[tool(
        description = "Extract tables from specified rectangular regions of a PDF as markdown pipe-tables. Rects are PDF points, top-left origin; set frame to display for boxes taken from a rendered page image (rotated pages)",
        annotations(
            title = "Extract PDF table regions",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn extract_table_regions(&self, params: Parameters<RegionInput>) -> String {
        let RegionInput {
            path,
            regions,
            frame,
        } = params.0;
        let regions = regions.into_iter().map(|r| (r.page, r.rects)).collect();
        dispatch_pdf(
            "extract_table_regions",
            run_pdf_regions(PdfOperation::TableRegions, path, regions, frame.into()),
        )
        .await
    }

    /// Identify the type of tax form in a PDF (W-2, 1099, K-1, 1040, schedules).
    #[tool(
        description = "Identify the type of tax form in a PDF (W-2, 1099, K-1, 1040, schedules)",
        annotations(
            title = "Identify tax form",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn identify_tax_form(&self, params: Parameters<PathInput>) -> String {
        let path = params.0.path;
        dispatch_pdf("identify_tax_form", async move {
            let markdown = pdf_worker::markdown(&path).await?;
            on_blocking_pool(move || {
                to_pretty_json(
                    &pdf_inspector_skillkit::domain::tax::identify_tax_form_markdown(&markdown),
                )
            })
            .await
        })
        .await
    }

    /// Split a SEC 10-K/10-Q filing into sections by Item number.
    #[tool(
        description = "Split a SEC 10-K/10-Q filing into sections by Item number — returns array of {name, item_number, content, char_offset}",
        annotations(
            title = "Split SEC filing",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn split_sec_filing(&self, params: Parameters<PathInput>) -> String {
        let path = params.0.path;
        dispatch_pdf("split_sec_filing", async move {
            let markdown = pdf_worker::markdown(&path).await?;
            on_blocking_pool(move || {
                to_pretty_json(&pdf_inspector_skillkit::domain::sec::split_sec_markdown(
                    &markdown,
                ))
            })
            .await
        })
        .await
    }

    /// Parse IRC (Internal Revenue Code) sections from a Title 26 PDF.
    #[tool(
        description = "Parse IRC (Internal Revenue Code) sections from a Title 26 PDF — returns sections with §numbers, titles, provisions labeled by full citation such as (d)(2)(A)(i), repeal flags, and editorial notes kept apart from statutory text",
        annotations(
            title = "Parse IRC sections",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn parse_irc_sections(&self, params: Parameters<PathInput>) -> String {
        let path = params.0.path;
        dispatch_pdf("parse_irc_sections", async move {
            let markdown = pdf_worker::markdown(&path).await?;
            on_blocking_pool(move || {
                to_pretty_json(
                    &pdf_inspector_skillkit::domain::irc::parse_irc_markdown_with_source(
                        &markdown,
                        std::path::Path::new(&path),
                    ),
                )
            })
            .await
        })
        .await
    }

    /// List built-in Sweet tax review demo packages.
    #[tool(
        description = "List built-in Sweet tax review demo packages across 1040, 1120, 1065, 1120-S, K-1, and 1099 workflows",
        annotations(
            title = "List Sweet demo packages",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn list_tax_packages(&self) -> String {
        dispatch("list_tax_packages", || {
            Ok::<_, std::convert::Infallible>(
                pdf_inspector_skillkit::domain::sweet::list_tax_packages(),
            )
        })
        .await
    }

    /// Review a built-in Sweet tax package and return structured findings.
    #[tool(
        description = "Run deterministic Sweet tax review checks for a built-in demo package and return structured findings",
        annotations(
            title = "Review Sweet demo package",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn review_tax_package(&self, params: Parameters<SweetPackageInput>) -> String {
        let package_id = params.0.package_id;
        dispatch("review_tax_package", move || {
            pdf_inspector_skillkit::domain::sweet::review_tax_package(&package_id)
        })
        .await
    }

    /// Compare one return line item against one source document value.
    #[tool(
        description = "Compare one tax return line against one source document line with an optional tolerance",
        annotations(
            title = "Compare line items",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn compare_line_items(&self, params: Parameters<SweetCompareLineItemsInput>) -> String {
        let input = params.0;
        dispatch("compare_line_items", move || {
            Ok::<_, std::convert::Infallible>(
                pdf_inspector_skillkit::domain::sweet::compare_line_items(
                    pdf_inspector_skillkit::domain::sweet::LineComparisonInput {
                        label: input.label,
                        return_reference: input.return_reference,
                        source_reference: input.source_reference,
                        return_amount: input.return_amount,
                        source_amount: input.source_amount,
                        tolerance: input.tolerance,
                    },
                ),
            )
        })
        .await
    }

    /// Render a Markdown review memo for a built-in Sweet demo package.
    #[tool(
        description = "Render a Markdown tax review memo for a built-in Sweet demo package",
        annotations(
            title = "Render review memo",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn render_review_memo(&self, params: Parameters<SweetPackageInput>) -> String {
        let package_id = params.0.package_id;
        dispatch("render_review_memo", move || {
            pdf_inspector_skillkit::domain::sweet::render_review_memo(&package_id)
        })
        .await
    }
}

#[tool_handler]
impl ServerHandler for PdfInspectorServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(
                "PDF classification, text extraction, and layout analysis. \
             Local and offline, with no bundled OCR engine; classification \
             reports which pages need OCR and why. PDF and document parsing \
             runs in a bounded worker process. \
             Also exposes bounded DOCX, strict PPTX, strict XLSX, strict ODS, strict ODT, Linux-memory-gated strict ODP, Linux-memory-gated strict EPUB, and Linux-memory-gated strict CSV conversion paths. \
             Document results state their completeness (complete or partial) \
             and carry fixed warnings; check both before relying on the \
             Markdown. Content the converter would drop silently fails closed. \
             Includes Sweet tax-review demo tools for deterministic package \
             review, line-item comparison, and Markdown memo rendering.",
            )
            .with_server_info(rmcp::model::Implementation::new(
                "pdf-inspector-mcp",
                env!("CARGO_PKG_VERSION"),
            ))
    }
}

fn main() -> anyhow::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("--anydoc-worker") {
        // The worker is synchronous. Avoid constructing Tokio before the
        // Linux supervisor applies the worker address-space ceiling.
        return pdf_inspector_skillkit::document::run_worker().map_err(anyhow::Error::from);
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async_main())
}

async fn async_main() -> anyhow::Result<()> {
    // CRITICAL: stdout is the MCP JSON-RPC channel. All logs MUST go to stderr
    // or the protocol breaks. Dependency logs stay disabled because protocol
    // libraries can include full request arguments in debug events. The
    // validated local level defaults to `info`.
    let requested_log_level = std::env::var("PDF_INSPECTOR_MCP_LOG").ok();
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(server_log_filter(requested_log_level.as_deref()))
        .init();

    // The default hook prints panic payloads before `JoinError` mapping can
    // redact them. Keep the server-side event constant and path-free.
    std::panic::set_hook(Box::new(|_| {
        eprintln!("pdf-inspector-mcp: internal panic");
    }));

    tracing::info!("pdf-inspector-mcp starting");

    let transport = rmcp::transport::io::stdio();
    let server = PdfInspectorServer::new();
    let service = server.serve(transport).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn log_level_rejects_dependency_filter_directives() {
        assert_eq!(local_log_level(None), "info");
        assert_eq!(local_log_level(Some("DEBUG")), "debug");
        assert_eq!(local_log_level(Some("trace,rmcp=trace")), "info");
    }

    /// A blocking closure that sleeps far longer than its timeout must
    /// still return the `{"error": "... timed out ..."}` envelope promptly,
    /// rather than hanging until the closure finishes. This is the
    /// regression test for the fix: `with_timeout` races its future
    /// against a timer, and that future must contain a real `.await`
    /// point (here, `spawn_blocking`'s `JoinHandle`) for the timer to
    /// ever get a chance to win.
    #[tokio::test]
    async fn with_timeout_preempts_a_slow_blocking_closure() {
        let start = Instant::now();

        let result = with_timeout("slow_tool", Duration::from_millis(50), async {
            match tokio::task::spawn_blocking(|| {
                std::thread::sleep(Duration::from_millis(200));
                "should not be observed before the timeout fires".to_string()
            })
            .await
            {
                Ok(s) => s,
                Err(e) => join_error_to_json("slow_tool", e),
            }
        })
        .await;

        let elapsed = start.elapsed();

        // The 50ms timeout must win the race against the 200ms blocking
        // closure — proving the timeout actually preempted rather than
        // blocking the executor until `work` finished on its own.
        assert!(
            elapsed < Duration::from_millis(200),
            "with_timeout did not preempt the slow closure: took {elapsed:?}"
        );

        let parsed: serde_json::Value =
            serde_json::from_str(&result).expect("timeout must return a JSON error envelope");
        let msg = parsed["error"]
            .as_str()
            .expect("envelope must have a string `error` key");
        assert!(
            msg.contains("timed out"),
            "expected a timeout message, got: {msg}"
        );
    }

    /// A closure that completes well within the timeout must return its
    /// own result unaffected — the timeout should not be a false trigger.
    #[tokio::test]
    async fn with_timeout_passes_through_fast_work() {
        let result = with_timeout("fast_tool", Duration::from_millis(200), async {
            match tokio::task::spawn_blocking(|| "ok".to_string()).await {
                Ok(s) => s,
                Err(e) => join_error_to_json("fast_tool", e),
            }
        })
        .await;

        assert_eq!(result, "ok");
    }
}
