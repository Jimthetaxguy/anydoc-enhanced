use pdf_inspector_skillkit::{
    analyze, classify, extract_text_regions_bytes, extract_text_regions_bytes_in_frame, process,
    validate_path, PdfInfo, PdfProvenance, RegionFrame, SkillkitError,
};
use std::path::PathBuf;

/// A redistributable U.S. Code fixture tracked in this repository.
///
/// Tests must never discover arbitrary documents from a contributor's home
/// directory: doing so is nondeterministic and can process private files.
fn public_test_pdf() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test-corpus/source/sample-1.pdf")
}

#[test]
fn test_classify_text_pdf() {
    let pdf_path = public_test_pdf();
    assert!(pdf_path.is_file(), "public test fixture is missing");
    let info = classify(&pdf_path).expect("classify failed");
    assert_eq!(info.pdf_type, "TextBased");
    assert_eq!(info.page_count, 4);
}

#[test]
fn test_classify_nonexistent() {
    let result = classify("/nonexistent.pdf");
    assert!(matches!(result, Err(SkillkitError::FileNotFound(_))));
}

#[test]
fn test_process_produces_markdown() {
    let pdf_path = public_test_pdf();
    let info = process(&pdf_path).expect("process failed");
    assert!(info.markdown.is_some(), "markdown should be Some");
    let markdown = info.markdown.as_deref().unwrap();
    assert!(
        markdown.contains("§1398"),
        "expected public fixture content"
    );
}

#[test]
fn test_validate_path_accepts_public_fixture() {
    let pdf_path = public_test_pdf();
    let result = validate_path(&pdf_path);
    assert!(result.is_ok(), "valid PDF path should pass validation");
}

#[test]
fn test_validate_path_canonicalizes() {
    let result = validate_path(public_test_pdf()).expect("validate_path failed");
    assert!(result.is_absolute(), "should return absolute path");
}

#[test]
fn test_pdf_info_serialization() {
    let info = PdfInfo {
        pdf_type: "TextBased".to_string(),
        confidence: 0.95,
        page_count: 10,
        pages_needing_ocr: vec![],
        has_encoding_issues: false,
        title: Some("Test Document".to_string()),
        markdown: Some("# Test\n\nHello world".to_string()),
        processing_time_ms: 123,
        ocr_reasons_by_page: vec![],
        layout: None,
        cmap_gaps: None,
        provenance: PdfProvenance::default(),
        warnings: vec![],
    };
    let json = serde_json::to_string(&info).expect("serialize failed");
    assert!(json.contains("\"pdf_type\""));
    assert!(json.contains("\"confidence\""));
    assert!(json.contains("\"page_count\""));
    assert!(json.contains("\"pages_needing_ocr\""));
    assert!(json.contains("\"has_encoding_issues\""));
    assert!(json.contains("\"title\""));
    assert!(json.contains("\"markdown\""));
    assert!(json.contains("\"processing_time_ms\""));
    assert!(json.contains("\"ocr_reasons_by_page\""));
    // Signals a mode did not compute, and empty provenance, stay off the wire.
    assert!(!json.contains("\"layout\""));
    assert!(!json.contains("\"cmap_gaps\""));
    assert!(!json.contains("\"provenance\""));
    assert!(!json.contains("\"warnings\""));
}

#[test]
fn classification_omits_signals_detection_does_not_compute() {
    let info = classify(public_test_pdf()).expect("classify failed");
    assert!(info.layout.is_none(), "detect-only mode analyzes no layout");
    assert!(info.cmap_gaps.is_none(), "detect-only mode decodes no text");
}

#[test]
fn analysis_reports_layout_for_the_public_fixture() {
    let info = analyze(public_test_pdf()).expect("analyze failed");
    let layout = info.layout.expect("analyze must report layout");
    let json = serde_json::to_value(&layout).expect("serialize layout");
    assert!(json["pages_with_tables"].is_array());
    assert!(json["pages_with_columns"].is_array());
    assert!(info.cmap_gaps.is_some(), "analysis decodes text");
    assert!(info.markdown.is_none(), "analysis skips Markdown");
}

#[test]
fn scanned_fixture_reports_no_analysis_it_did_not_run() {
    // Upstream skips extraction for scans, so "no tables" would be a guess.
    let scanned =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test-corpus/scanned/sample-1.pdf");
    for info in [analyze(&scanned), process(&scanned)] {
        let info = info.expect("scanned fixture");
        assert_eq!(info.pdf_type, "Scanned");
        assert!(info.layout.is_none());
        assert!(info.cmap_gaps.is_none());
    }
}

#[test]
fn scanned_fixture_reports_why_it_needs_ocr() {
    let scanned =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test-corpus/scanned/sample-1.pdf");
    let info = classify(&scanned).expect("classify failed");
    assert_eq!(info.pages_needing_ocr, vec![1]);
    let page = info
        .ocr_reasons_by_page
        .iter()
        .find(|page| page.page == 1)
        .expect("page 1 must carry OCR reasons");
    assert!(
        !page.reasons.is_empty(),
        "an OCR-flagged page must say why it needs OCR"
    );
}

#[test]
fn irc_parser_reads_sections_from_the_public_title_26_fixture() {
    // pdf-inspector renders sections as Markdown headings (`# §1398. …`);
    // the parser previously matched none of them on this fixture.
    let result = pdf_inspector_skillkit::domain::irc::parse_irc_sections(public_test_pdf())
        .expect("parse failed");
    let numbers: Vec<_> = result
        .sections
        .iter()
        .map(|section| section.section_number.as_str())
        .collect();
    assert_eq!(numbers, ["§1398", "§1399"]);
    assert_eq!(result.chapter.as_deref(), Some("Chapter 1"));

    let section = &result.sections[0];
    let labels: Vec<_> = section
        .subsections
        .iter()
        .map(|provision| provision.label.as_str())
        .collect();
    for expected in [
        "(a)",
        "(b)(2)",
        "(d)(2)(A)(i)",
        "(h)(2)(D)",
        "(i)",
        "(j)(2)(C)(ii)",
    ] {
        assert!(labels.contains(&expected), "missing provision {expected}");
    }
    let mut unique = labels.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(
        unique.len(),
        labels.len(),
        "provision labels must be unique"
    );
    assert!(!section.content.contains("Editorial Notes"));
    assert!(section
        .notes
        .as_deref()
        .is_some_and(|notes| notes.contains("Amendments")));
}

#[test]
fn irc_parser_flags_repealed_placeholders() {
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test-corpus/source/sample-3.pdf");
    let result =
        pdf_inspector_skillkit::domain::irc::parse_irc_sections(&fixture).expect("parse failed");
    let repealed: Vec<_> = result
        .sections
        .iter()
        .filter(|section| section.repealed)
        .map(|section| section.section_number.as_str())
        .collect();
    assert_eq!(repealed, ["§1551", "§1562", "§1564"]);
}

/// A one-page PDF whose page is displayed turned by `/Rotate 90`, with
/// `ROTATED-MARKER` set near the top-left of the unturned page.
fn rotated_page_pdf() -> Vec<u8> {
    let content = "BT /F1 12 Tf 72 700 Td (ROTATED-MARKER) Tj ET";
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Rotate 90 \
         /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>"
            .to_string(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
            .to_string(),
        format!(
            "<< /Length {} >>\nstream\n{content}\nendstream",
            content.len()
        ),
    ];
    let mut pdf = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (index, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", index + 1).as_bytes());
    }
    let xref = pdf.len();
    pdf.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for offset in offsets {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    pdf
}

#[test]
fn region_rectangles_can_be_read_on_the_rendered_page() {
    let pdf = rotated_page_pdf();
    let found = |rect: [f32; 4], frame: RegionFrame| {
        let results =
            extract_text_regions_bytes_in_frame(&pdf, &[(0, vec![rect])], frame).expect("regions");
        results[0].regions[0].text.contains("ROTATED-MARKER")
    };
    // Near the top-left of the page as laid out in the content stream.
    let sheet_rect = [60.0, 75.0, 260.0, 100.0];
    // The same text on the page as rendered: `/Rotate 90` turns the top edge
    // to the right side, so the line runs down the right margin.
    let display_rect = [690.0, 60.0, 720.0, 260.0];
    assert!(found(sheet_rect, RegionFrame::Sheet));
    assert!(!found(sheet_rect, RegionFrame::Display));
    assert!(found(display_rect, RegionFrame::Display));
    assert!(!found(display_rect, RegionFrame::Sheet));
    // The frame-less entry point keeps reading the sheet frame.
    let default = extract_text_regions_bytes(&pdf, &[(0, vec![sheet_rect])]).expect("regions");
    assert!(default[0].regions[0].text.contains("ROTATED-MARKER"));
}
