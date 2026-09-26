//! PDF tools executed in the private worker process.
//!
//! pdf-inspector 1.25.0 bounds object streams at load and page content during
//! extraction, but other streams (the detector's page scan, fonts, CMaps,
//! form XObjects) still inflate without a limit: a 1 MB page-content Flate
//! bomb peaks near 2.1 GiB when parsed inside the MCP server. Running the PDF
//! facade in the supervised worker that already contains the document lanes
//! gives every PDF call the address-space ceiling, network denial, and a
//! timeout that kills the parse instead of abandoning a blocking thread.
//! Hosts without the worker sandbox keep the in-process path.

use std::path::Path;
use std::time::Duration;

use serde_json::value::RawValue;
use tokio::io::AsyncReadExt;

use crate::document::{self, DocumentError, WorkerJob, WorkerResponse};
use crate::{RegionFrame, SkillkitError};

/// Kill a PDF worker before the MCP tool's 30-second budget expires, so the
/// caller receives a specific timeout rather than the generic tool timeout.
const PDF_WORKER_TIMEOUT: Duration = Duration::from_secs(25);

/// Largest serialized PDF result a worker may return. PDF Markdown has no
/// separate cap, so this bounds only pathological outputs.
pub(crate) const MAX_PDF_RESPONSE_BYTES: usize = 128 * 1024 * 1024;

/// One PDF tool operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PdfOperation {
    Classify,
    Markdown,
    Analyze,
    TextRegions,
    TableRegions,
}

impl PdfOperation {
    /// Worker frame codes; document lanes use 1 through 8.
    fn code(self) -> u8 {
        match self {
            Self::Classify => 16,
            Self::Markdown => 17,
            Self::Analyze => 18,
            Self::TextRegions => 19,
            Self::TableRegions => 20,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            16 => Self::Classify,
            17 => Self::Markdown,
            18 => Self::Analyze,
            19 => Self::TextRegions,
            20 => Self::TableRegions,
            _ => return None,
        })
    }

    fn takes_regions(self) -> bool {
        matches!(self, Self::TextRegions | Self::TableRegions)
    }
}

/// Page regions as `(page_0indexed, [[x1, y1, x2, y2], …])` in PDF points.
pub type PageRegions = [(u32, Vec<[f32; 4]>)];
type OwnedRegions = Vec<(u32, Vec<[f32; 4]>)>;

/// Stable, path-free failures of a PDF tool call.
#[derive(Debug, thiserror::Error)]
pub enum PdfToolError {
    /// Path, size, or parser errors, with the facade's existing messages.
    #[error(transparent)]
    Input(#[from] SkillkitError),
    #[error("PDF processing failed")]
    Processing,
    #[error("PDF processing exceeded a resource limit")]
    ResourceLimit,
    #[error("PDF processing timed out")]
    Timeout,
    #[error("PDF output exceeded the response limit")]
    OutputTooLarge,
    #[error("PDF region request exceeds the parameter limit")]
    RequestTooLarge,
    #[error("PDF worker is unavailable")]
    WorkerUnavailable,
    #[error("PDF worker returned an invalid response")]
    Protocol,
}

impl From<DocumentError> for PdfToolError {
    fn from(error: DocumentError) -> Self {
        match error {
            DocumentError::WorkerTimeout => Self::Timeout,
            DocumentError::ResourceLimit => Self::ResourceLimit,
            DocumentError::OutputTooLarge => Self::OutputTooLarge,
            DocumentError::WorkerUnavailable | DocumentError::WorkerBusy => Self::WorkerUnavailable,
            _ => Self::Protocol,
        }
    }
}

/// Run one PDF operation on a local file and return its serialized result:
/// the same JSON the in-process facade produces for that operation. Region
/// rectangles are read in the [`RegionFrame::Sheet`] frame.
pub async fn run(
    operation: PdfOperation,
    path: impl AsRef<Path>,
    regions: &PageRegions,
) -> Result<Box<RawValue>, PdfToolError> {
    run_in_frame(operation, path, regions, RegionFrame::Sheet).await
}

/// [`run`], with region rectangles read in `frame`.
pub async fn run_in_frame(
    operation: PdfOperation,
    path: impl AsRef<Path>,
    regions: &PageRegions,
    frame: RegionFrame,
) -> Result<Box<RawValue>, PdfToolError> {
    let canonical = crate::validate_path(&path)?;
    let regions: OwnedRegions = if operation.takes_regions() {
        regions.to_vec()
    } else {
        Vec::new()
    };
    // Take a slot before reading, so a batch of large files waits holding
    // paths rather than their bytes.
    let permit = document::pdf_worker_permit().await?;
    if !document::worker_available() {
        return run_in_process(operation, canonical, (regions, frame), permit).await;
    }

    let params = if operation.takes_regions() {
        serde_json::to_vec(&RegionParams {
            regions: encode_regions(&regions),
            frame,
        })
        .map_err(|_| PdfToolError::Protocol)?
    } else {
        Vec::new()
    };
    if params.len() > document::MAX_WORKER_PARAMS_BYTES {
        return Err(PdfToolError::RequestTooLarge);
    }
    let unavailable = || SkillkitError::FileNotFound(canonical.display().to_string());
    let mut file = tokio::fs::File::open(&canonical)
        .await
        .map_err(|_| unavailable())?;
    let size_hint = file.metadata().await.map_or(0, |meta| meta.len());
    // Read the document straight into the frame after the parameter block,
    // so a large PDF is not held twice by the server.
    let mut payload =
        Vec::with_capacity(4 + params.len() + size_hint.min(document::MAX_DOCUMENT_SIZE) as usize);
    payload.extend_from_slice(&(params.len() as u32).to_le_bytes());
    payload.extend_from_slice(&params);
    let prefix = payload.len();
    (&mut file)
        .take(document::MAX_DOCUMENT_SIZE + 1)
        .read_to_end(&mut payload)
        .await
        .map_err(|_| unavailable())?;
    let size = (payload.len() - prefix) as u64;
    if size > document::MAX_DOCUMENT_SIZE {
        return Err(SkillkitError::FileTooLarge {
            size_bytes: size,
            limit_bytes: document::MAX_DOCUMENT_SIZE,
        }
        .into());
    }

    let response = document::run_pdf_worker_job(
        WorkerJob {
            code: operation.code(),
            payload,
            timeout: PDF_WORKER_TIMEOUT,
            max_response_bytes: MAX_PDF_RESPONSE_BYTES,
        },
        permit,
    )
    .await?;
    match (response.json, response.error) {
        (Some(json), None) => Ok(json),
        (None, Some(error)) => Err(match error.code.as_str() {
            "pdf_error" => PdfToolError::Processing,
            "resource_limit" => PdfToolError::ResourceLimit,
            "output_too_large" => PdfToolError::OutputTooLarge,
            // A worker binary without PDF support answers `unsupported`.
            "unsupported" => PdfToolError::WorkerUnavailable,
            _ => PdfToolError::Protocol,
        }),
        _ => Err(PdfToolError::Protocol),
    }
}

/// The route for hosts without the worker sandbox. `slot` bounds the
/// requests being served. An in-process parse cannot be cancelled: when its
/// caller times out, the slot is released with the caller and the parse
/// finishes unattended, as every in-process parse did before the worker
/// route existed, so a few slow files cannot hold every PDF tool.
async fn run_in_process<Slot: Send>(
    operation: PdfOperation,
    canonical: std::path::PathBuf,
    (regions, frame): (OwnedRegions, RegionFrame),
    slot: Slot,
) -> Result<Box<RawValue>, PdfToolError> {
    let result = tokio::task::spawn_blocking(move || {
        let buffer = crate::read_validated(&canonical)?;
        execute_operation(operation, &buffer, &regions, frame)
    })
    .await
    .map_err(|_| PdfToolError::Processing)?;
    drop(slot);
    result
}

/// Markdown for a local PDF through the bounded route, for the domain tools.
pub async fn markdown(path: impl AsRef<Path>) -> Result<String, PdfToolError> {
    #[derive(serde::Deserialize)]
    struct MarkdownOnly {
        markdown: Option<String>,
    }
    let json = run(PdfOperation::Markdown, path, &[]).await?;
    // A response can reach MAX_PDF_RESPONSE_BYTES; decode it off the executor.
    tokio::task::spawn_blocking(move || {
        serde_json::from_str::<MarkdownOnly>(json.get())
            .map(|parsed| parsed.markdown.unwrap_or_default())
            .map_err(|_| PdfToolError::Protocol)
    })
    .await
    .map_err(|_| PdfToolError::Processing)?
}

/// Worker side: run a PDF operation frame. Returns `None` for codes that
/// belong to the document lanes.
pub(crate) fn execute(code: u8, frame: &[u8]) -> Option<WorkerResponse> {
    let operation = PdfOperation::from_code(code)?;
    let result = decode_frame(frame).and_then(|(regions, region_frame, buffer)| {
        if buffer.len() as u64 > crate::document::MAX_DOCUMENT_SIZE {
            return Err(DocumentError::ResourceLimit);
        }
        execute_operation(operation, buffer, &regions, region_frame).map_err(|error| match error {
            PdfToolError::OutputTooLarge => DocumentError::OutputTooLarge,
            _ => DocumentError::ConversionFailed,
        })
    });
    Some(match result {
        Ok(json) => WorkerResponse {
            json: Some(json),
            ..Default::default()
        },
        Err(DocumentError::ConversionFailed) => WorkerResponse {
            error: Some(document::WorkerError {
                code: "pdf_error".into(),
                pages: Vec::new(),
            }),
            ..Default::default()
        },
        Err(error) => document::worker_response_for_error(&error),
    })
}

/// Region parameters as they travel to the worker.
#[derive(serde::Serialize, serde::Deserialize)]
struct RegionParams {
    regions: EncodedRegions,
    #[serde(default)]
    frame: RegionFrame,
}

/// Split a PDF frame into its region parameters and document bytes.
fn decode_frame(frame: &[u8]) -> Result<(OwnedRegions, RegionFrame, &[u8]), DocumentError> {
    let (length, rest) = frame
        .split_first_chunk::<4>()
        .ok_or(DocumentError::WorkerProtocol)?;
    let length = u32::from_le_bytes(*length) as usize;
    if length > document::MAX_WORKER_PARAMS_BYTES || length > rest.len() {
        return Err(DocumentError::ResourceLimit);
    }
    let (params, buffer) = rest.split_at(length);
    if params.is_empty() {
        return Ok((Vec::new(), RegionFrame::Sheet, buffer));
    }
    let params: RegionParams =
        serde_json::from_slice(params).map_err(|_| DocumentError::WorkerProtocol)?;
    Ok((decode_regions(params.regions), params.frame, buffer))
}

/// Regions with coordinates as their exact `f32` bit patterns: JSON has no
/// infinity or NaN, and the worker must receive exactly what the in-process
/// route would pass to pdf-inspector.
type EncodedRegions = Vec<(u32, Vec<[u32; 4]>)>;

fn encode_regions(regions: &PageRegions) -> EncodedRegions {
    regions
        .iter()
        .map(|(page, rects)| {
            (
                *page,
                rects.iter().map(|rect| rect.map(f32::to_bits)).collect(),
            )
        })
        .collect()
}

fn decode_regions(encoded: EncodedRegions) -> OwnedRegions {
    encoded
        .into_iter()
        .map(|(page, rects)| {
            (
                page,
                rects
                    .into_iter()
                    .map(|rect| rect.map(f32::from_bits))
                    .collect(),
            )
        })
        .collect()
}

/// Run one operation in this process and serialize its result exactly as the
/// MCP tools render it.
fn execute_operation(
    operation: PdfOperation,
    buffer: &[u8],
    regions: &PageRegions,
    frame: RegionFrame,
) -> Result<Box<RawValue>, PdfToolError> {
    let json = match operation {
        PdfOperation::Classify => to_pretty(&crate::classify_bytes(buffer)?),
        PdfOperation::Markdown => to_pretty(&crate::process_bytes(buffer)?),
        PdfOperation::Analyze => to_pretty(&crate::analyze_bytes(buffer)?),
        PdfOperation::TextRegions => to_pretty(&crate::extract_text_regions_bytes_in_frame(
            buffer, regions, frame,
        )?),
        PdfOperation::TableRegions => to_pretty(&crate::extract_table_regions_bytes_in_frame(
            buffer, regions, frame,
        )?),
    }?;
    if json.len() > MAX_PDF_RESPONSE_BYTES {
        return Err(PdfToolError::OutputTooLarge);
    }
    RawValue::from_string(json).map_err(|_| PdfToolError::Protocol)
}

fn to_pretty(value: &impl serde::Serialize) -> Result<String, PdfToolError> {
    serde_json::to_string_pretty(value).map_err(|_| PdfToolError::Protocol)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn public_fixture() -> Vec<u8> {
        std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../test-corpus/source/sample-1.pdf"
        ))
        .expect("public fixture")
    }

    fn frame(params: &[u8], buffer: &[u8]) -> Vec<u8> {
        let mut frame = (params.len() as u32).to_le_bytes().to_vec();
        frame.extend_from_slice(params);
        frame.extend_from_slice(buffer);
        frame
    }

    #[tokio::test]
    async fn in_process_parse_frees_its_slot_when_the_caller_stops_waiting() {
        let slots = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let slot = slots.clone().acquire_owned().await.expect("slot");
        let path = crate::validate_path(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../test-corpus/source/sample-2.pdf"
        ))
        .expect("public fixture");
        let call = run_in_process(
            PdfOperation::Markdown,
            path,
            (Vec::new(), RegionFrame::Sheet),
            slot,
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(1), call)
                .await
                .is_err(),
            "converting the fixture takes longer than the caller waits"
        );
        assert_eq!(
            slots.available_permits(),
            1,
            "a caller that stopped waiting holds no slot"
        );
    }

    #[tokio::test]
    async fn in_process_route_returns_the_facade_result() {
        let path = crate::validate_path(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../test-corpus/source/sample-1.pdf"
        ))
        .expect("public fixture");
        let json = run_in_process(
            PdfOperation::Classify,
            path,
            (Vec::new(), RegionFrame::Sheet),
            (),
        )
        .await
        .expect("classify");
        let value: serde_json::Value = serde_json::from_str(json.get()).expect("JSON");
        assert_eq!(value["pdf_type"], "TextBased");
    }

    #[test]
    fn operation_codes_round_trip_and_stay_clear_of_document_lanes() {
        for operation in [
            PdfOperation::Classify,
            PdfOperation::Markdown,
            PdfOperation::Analyze,
            PdfOperation::TextRegions,
            PdfOperation::TableRegions,
        ] {
            assert!(operation.code() > 8);
            assert_eq!(PdfOperation::from_code(operation.code()), Some(operation));
        }
        for document_code in 0..=8 {
            assert!(execute(document_code, &[]).is_none());
        }
    }

    #[test]
    fn worker_execution_matches_the_in_process_facade() {
        let buffer = public_fixture();
        let response =
            execute(PdfOperation::Markdown.code(), &frame(&[], &buffer)).expect("PDF operation");
        let json = response.json.expect("PDF result");
        let mut from_worker: serde_json::Value =
            serde_json::from_str(json.get()).expect("worker JSON");
        let mut in_process =
            serde_json::to_value(crate::process_bytes(&buffer).expect("process")).unwrap();
        for value in [&mut from_worker, &mut in_process] {
            value
                .as_object_mut()
                .expect("PdfInfo object")
                .remove("processing_time_ms");
        }
        assert_eq!(from_worker, in_process);
        // The operation serializes the struct itself, keeping its field order.
        assert!(json.get().starts_with("{\n  \"pdf_type\""));
    }

    #[test]
    fn region_parameters_travel_ahead_of_the_document() {
        let buffer = public_fixture();
        let regions: Vec<(u32, Vec<[f32; 4]>)> = vec![(0, vec![[0.0, 0.0, 612.0, 200.0]])];
        for region_frame in [RegionFrame::Sheet, RegionFrame::Display] {
            let params = serde_json::to_vec(&RegionParams {
                regions: encode_regions(&regions),
                frame: region_frame,
            })
            .unwrap();
            let response = execute(PdfOperation::TextRegions.code(), &frame(&params, &buffer))
                .expect("PDF operation");
            let json = response.json.expect("region result");
            let parsed: serde_json::Value = serde_json::from_str(json.get()).unwrap();
            assert_eq!(parsed[0]["page"], 0);
            assert!(parsed[0]["regions"][0]["text"].is_string());
            // The public fixture is unrotated, so both frames agree.
            let in_process =
                crate::extract_text_regions_bytes_in_frame(&buffer, &regions, region_frame)
                    .unwrap();
            assert_eq!(parsed, serde_json::to_value(in_process).unwrap());
        }
    }

    #[test]
    fn region_encoding_round_trips_every_coordinate() {
        let regions: Vec<(u32, Vec<[f32; 4]>)> =
            vec![(3, vec![[f32::INFINITY, -0.0, f32::NAN, f32::MIN_POSITIVE]])];
        let wire = serde_json::to_vec(&encode_regions(&regions)).unwrap();
        let decoded = decode_regions(serde_json::from_slice(&wire).unwrap());
        assert_eq!(decoded[0].0, 3);
        let bits = |rect: [f32; 4]| rect.map(f32::to_bits);
        assert_eq!(bits(decoded[0].1[0]), bits(regions[0].1[0]));
    }

    #[test]
    fn malformed_frames_fail_with_stable_codes() {
        let short = execute(PdfOperation::Classify.code(), &[1, 0]).expect("PDF operation");
        assert_eq!(short.error.expect("error").code, "worker_protocol");

        let oversized_params = ((document::MAX_WORKER_PARAMS_BYTES + 1) as u32).to_le_bytes();
        let response =
            execute(PdfOperation::TextRegions.code(), &oversized_params).expect("PDF operation");
        assert_eq!(response.error.expect("error").code, "resource_limit");

        let not_pdf = execute(PdfOperation::Classify.code(), &frame(&[], b"plain text"))
            .expect("PDF operation");
        assert_eq!(not_pdf.error.expect("error").code, "pdf_error");
    }
}
