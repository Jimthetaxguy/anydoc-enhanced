//! PDF tools over MCP stdio, including the bounded worker route.

use std::{
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

fn fixture(relative: &str) -> String {
    format!(
        "{}/../../test-corpus/{relative}",
        env!("CARGO_MANIFEST_DIR")
    )
}

/// Call several tools in one server session and return each parsed result.
fn call_tools(
    calls: &[(&str, serde_json::Value)],
    worker: Option<&Path>,
) -> Vec<serde_json::Value> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pdf-inspector-mcp"));
    if let Some(worker) = worker {
        command.env("ANYDOC_WORKER_BIN", worker);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("MCP binary must be available to integration tests");
    let mut stdin = child.stdin.take().expect("child stdin");
    let initialize = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "pdf-tools-test", "version": "1" }
        }
    });
    writeln!(stdin, "{initialize}").expect("write initialize");
    writeln!(
        stdin,
        "{}",
        serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })
    )
    .expect("write initialized");
    stdin.flush().expect("flush requests");

    let mut stdout = BufReader::new(child.stdout.take().expect("child stdout"));
    let mut results = Vec::new();
    // Calls go one at a time so each response can be tied to its request.
    for (index, (name, arguments)) in calls.iter().enumerate() {
        let id = index + 1;
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments }
        });
        writeln!(stdin, "{request}").expect("write tool call");
        stdin.flush().expect("flush tool call");
        let response = loop {
            let mut line = String::new();
            let bytes = stdout.read_line(&mut line).expect("read JSON-RPC response");
            assert!(bytes > 0, "server closed before answering {name}");
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) {
                if value.get("id") == Some(&serde_json::Value::from(id)) {
                    break value;
                }
            }
        };
        let text = response["result"]["content"][0]["text"]
            .as_str()
            .expect("tool text result");
        // Arguments the input schema rejects come back as an error result
        // with a plain-text reason.
        results.push(if response["result"]["isError"] == true {
            serde_json::json!({ "is_error": true, "reason": text })
        } else {
            serde_json::from_str(text).expect("tool JSON result")
        });
    }
    drop(stdin);
    assert!(child.wait().expect("wait for MCP server").success());
    results
}

#[test]
fn pdf_tools_return_public_fixture_results() {
    let pdf = fixture("source/sample-1.pdf");
    let results = call_tools(
        &[
            ("classify_pdf", serde_json::json!({ "path": pdf })),
            ("pdf_to_markdown", serde_json::json!({ "path": pdf })),
            ("analyze_layout", serde_json::json!({ "path": pdf })),
            ("parse_irc_sections", serde_json::json!({ "path": pdf })),
            (
                "extract_text_regions",
                serde_json::json!({
                    "path": pdf,
                    "regions": [{ "page": 0, "rects": [[0.0, 0.0, 612.0, 792.0]] }]
                }),
            ),
            (
                "batch_classify",
                serde_json::json!({ "paths": [pdf, fixture("scanned/sample-1.pdf"), "missing.pdf"] }),
            ),
        ],
        None,
    );
    let [classified, markdown, layout, irc, regions, batch] = results.as_slice() else {
        panic!("expected six results");
    };
    assert_eq!(classified["pdf_type"], "TextBased");
    assert_eq!(classified["page_count"], 4);
    assert!(classified.get("layout").is_none());
    assert!(markdown["markdown"]
        .as_str()
        .is_some_and(|text| text.contains("§1398")));
    assert!(layout["layout"]["pages_with_columns"].is_array());
    assert_eq!(irc["total_sections"], 2);
    assert_eq!(irc["sections"][0]["section_number"], "§1398");
    assert!(regions[0]["regions"][0]["text"]
        .as_str()
        .is_some_and(|text| !text.is_empty()));
    assert_eq!(batch[0]["classification"]["pdf_type"], "TextBased");
    assert_eq!(batch[1]["classification"]["pdf_type"], "Scanned");
    // The batch echoes each supplied path by design; errors stay path-free.
    assert_eq!(batch[2]["path"], "missing.pdf");
    assert_eq!(batch[2]["error"], "File not found or inaccessible");
}

/// A PDF file holding `objects` as objects 1, 2, …, with object 1 as the
/// catalog.
fn pdf_file(objects: &[Vec<u8>]) -> Vec<u8> {
    let mut pdf = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (index, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
        pdf.extend_from_slice(object);
        pdf.extend_from_slice(b"\nendobj\n");
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

/// A stream object with `dictionary` entries and `content`.
fn stream(dictionary: &str, content: &[u8]) -> Vec<u8> {
    let mut object = format!("<< {dictionary} /Length {} >>\nstream\n", content.len()).into_bytes();
    object.extend_from_slice(content);
    object.extend_from_slice(b"\nendstream");
    object
}

/// A one-page PDF displayed turned by `/Rotate 90`, with `ROTATED-MARKER`
/// near the top-left of the unturned page and a small ledger table below it.
fn rotated_page_pdf() -> Vec<u8> {
    let mut content = String::from("BT /F1 12 Tf 72 700 Td (ROTATED-MARKER) Tj ET");
    let rows = [
        ["Account", "Opening", "Closing"],
        ["Cash", "100", "150"],
        ["Receivables", "200", "250"],
        ["Inventory", "300", "350"],
    ];
    for (row, cells) in rows.iter().enumerate() {
        for (cell, x) in cells.iter().zip([72, 200, 300]) {
            let y = 500 - 15 * row;
            content.push_str(&format!("\nBT /F1 10 Tf {x} {y} Td ({cell}) Tj ET"));
        }
    }
    pdf_file(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Rotate 90 \
          /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>"
            .to_vec(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
            .to_vec(),
        stream("", content.as_bytes()),
    ])
}

/// A scanned page made searchable: a page-sized image, an invisible text
/// layer in a form as ocrmypdf writes it, and a visible header and Bates
/// number stamped on top. pdf-inspector 1.24.0 alone reads it as a text page
/// holding only the stamps, with no page for OCR: the header is too long
/// for its sparse-text check.
fn stamped_scan_pdf() -> Vec<u8> {
    // A page's worth of lines, so the detector reads the page as text.
    let lines = [
        "Form 1040 Individual Income Tax Return",
        "Wages, salaries, tips 85000",
        "Taxable interest 1250",
        "Total income 86250",
    ];
    let layer: String = lines
        .iter()
        .cycle()
        .take(24)
        .enumerate()
        .map(|(index, line)| format!("1 0 0 1 72 {} Tm ({line}) Tj\n", 740 - 28 * index))
        .collect();
    let page = "q 612 0 0 792 0 0 cm /Im1 Do Q q /Fm1 Do Q \
                BT /F1 9 Tf 1 0 0 1 72 770 Tm (CONFIDENTIAL - PREPARED FOR EXAMINATION - CLIENT COPY) Tj ET \
                BT /F1 9 Tf 1 0 0 1 480 20 Tm (BATES-000123) Tj ET";
    pdf_file(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
          /Resources << /Font << /F1 4 0 R >> /XObject << /Im1 6 0 R /Fm1 7 0 R >> >> \
          /Contents 5 0 R >>"
            .to_vec(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
            .to_vec(),
        stream("", page.as_bytes()),
        stream(
            "/Type /XObject /Subtype /Image /Width 64 /Height 64 \
             /ColorSpace /DeviceGray /BitsPerComponent 8",
            &[200; 64 * 64],
        ),
        stream(
            "/Type /XObject /Subtype /Form /BBox [0 0 612 792] \
             /Resources << /Font << /F1 4 0 R >> >>",
            format!("BT 3 Tr /F1 11 Tf\n{layer}ET").as_bytes(),
        ),
    ])
}

#[test]
fn scans_whose_text_layer_is_dropped_are_reported_for_ocr() {
    let temporary = tempfile::tempdir().expect("temporary PDF directory");
    let pdf = temporary.path().join("stamped-scan.pdf");
    std::fs::write(&pdf, stamped_scan_pdf()).expect("write scan");
    let pdf = pdf.to_str().expect("UTF-8 path").to_string();
    let results = call_tools(
        &[
            ("pdf_to_markdown", serde_json::json!({ "path": pdf })),
            ("classify_pdf", serde_json::json!({ "path": pdf })),
        ],
        None,
    );
    for result in &results {
        assert_eq!(
            result["pages_needing_ocr"],
            serde_json::json!([1]),
            "{result}"
        );
        assert_eq!(
            result["ocr_reasons_by_page"],
            serde_json::json!([{ "page": 1, "reasons": ["invisible_text_layer"] }]),
            "{result}"
        );
    }
    // The stamp is the only text read; the layer is never copied out.
    let markdown = results[0]["markdown"].as_str().unwrap_or_default();
    assert!(markdown.contains("BATES-000123"), "{markdown}");
    assert!(!markdown.contains("Taxable interest"), "{markdown}");
}

/// A one-page card statement whose second purchase line is painted twice,
/// as an overprint leaves it (pdf-inspector #317, #377).
fn overprinted_statement_pdf() -> Vec<u8> {
    let line = |y: u32, text: &str| format!("BT /F1 10 Tf 1 0 0 1 72 {y} Tm ({text}) Tj ET\n");
    let mut content = line(740, "Card transactions for April");
    for (index, text) in [
        "04/02 Grocery store 84.19",
        "04/05 Fuel 41.00",
        "04/09 Pharmacy 12.35",
    ]
    .iter()
    .enumerate()
    {
        content.push_str(&line(700 - 20 * index as u32, text));
    }
    content.push_str(&line(680, "04/05 Fuel 41.00"));
    pdf_file(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>".to_vec(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".to_vec(),
        stream("", content.as_bytes()),
    ])
}

#[test]
fn text_the_markdown_repeats_is_reported() {
    let temporary = tempfile::tempdir().expect("temporary PDF directory");
    let pdf = temporary.path().join("overprinted.pdf");
    std::fs::write(&pdf, overprinted_statement_pdf()).expect("write PDF");
    let pdf = pdf.to_str().expect("UTF-8 path").to_string();
    let sample = fixture("source/sample-2.pdf");
    let results = call_tools(
        &[
            ("pdf_to_markdown", serde_json::json!({ "path": pdf })),
            ("classify_pdf", serde_json::json!({ "path": pdf })),
            ("pdf_to_markdown", serde_json::json!({ "path": sample })),
        ],
        None,
    );
    // A line painted twice over itself is repeated, and reported by page.
    let warnings = results[0]["warnings"].as_array().expect("warnings");
    assert!(
        warnings
            .iter()
            .any(|warning| warning["code"] == "text_painted_twice"
                && warning["pages"] == serde_json::json!([1])),
        "{}",
        results[0]
    );
    // Classification reads no text, so it reports nothing of it.
    assert!(results[1].get("warnings").is_none(), "{}", results[1]);
    // pdf-inspector 1.24.0 leaves a rate table's first row in the paragraph
    // above it in the public Title 26 sample (#406); when a release fixes
    // that, this expectation goes.
    let warnings = results[2]["warnings"].as_array().expect("warnings");
    assert!(
        warnings
            .iter()
            .any(|warning| warning["code"] == "table_row_repeated"),
        "{}",
        results[2]
    );
    assert!(
        !warnings
            .iter()
            .any(|warning| warning["code"] == "text_painted_twice"),
        "{}",
        results[2]
    );
}

/// A one-page closing statement in a subset font whose differences name the
/// space at code 26 and leave code 32 unused, with a price kerned inside
/// its digits (pdf-inspector #532).
fn kerned_subset_statement_pdf() -> Vec<u8> {
    let widths: Vec<String> = (1..=120)
        .map(|code| match code {
            26 => "278".to_string(),
            32 => "0".to_string(),
            _ => "556".to_string(),
        })
        .collect();
    let font = format!(
        "<< /Type /Font /Subtype /TrueType /BaseFont /ABCDEF+StatementSans /FirstChar 1 \
         /LastChar 120 /Widths [{}] /Encoding << /Type /Encoding \
         /BaseEncoding /WinAnsiEncoding /Differences [26 /space] >> >>",
        widths.join(" ")
    );
    let line = |y: u32, parts: &str| format!("BT /F1 10 Tf 1 0 0 1 72 {y} Tm [{parts}] TJ ET\n");
    let content = [
        line(740, "(Closing\\032statement)"),
        line(
            700,
            "(Purchase\\032price) -4000 (8) -106 (5,000) -108 (.00)",
        ),
        line(680, "(Deposit) -4000 (1,020.00)"),
    ]
    .concat();
    pdf_file(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>".to_vec(),
        font.into_bytes(),
        stream("", content.as_bytes()),
    ])
}

#[test]
fn word_gaps_judged_against_the_wrong_space_are_reported() {
    let temporary = tempfile::tempdir().expect("temporary PDF directory");
    let pdf = temporary.path().join("statement.pdf");
    std::fs::write(&pdf, kerned_subset_statement_pdf()).expect("write PDF");
    let pdf = pdf.to_str().expect("UTF-8 path").to_string();
    let sample = fixture("source/sample-2.pdf");
    let results = call_tools(
        &[
            ("pdf_to_markdown", serde_json::json!({ "path": pdf })),
            ("pdf_to_markdown", serde_json::json!({ "path": sample })),
        ],
        None,
    );
    let warnings = results[0]["warnings"].as_array().expect("warnings");
    assert!(
        warnings
            .iter()
            .any(|warning| warning["code"] == "word_gaps_misread"
                && warning["pages"] == serde_json::json!([1])),
        "{}",
        results[0]
    );
    // pdf-inspector 1.24.0 splits the kerned price; when a release fixes
    // #532, this expectation goes.
    assert!(
        results[0]["markdown"]
            .as_str()
            .is_some_and(|markdown| markdown.contains("8 5,000 .00")),
        "{}",
        results[0]
    );
    // The public sample's fonts are read as their fix would read them.
    assert!(
        !results[1]["warnings"]
            .as_array()
            .expect("warnings")
            .iter()
            .any(|warning| warning["code"] == "word_gaps_misread"),
        "{}",
        results[1]
    );
}

/// The bold serif glyphs of `glyph_by_glyph_pdf`: each glyph's width in
/// thousandths of an em, and the whole-pixel advance a browser hints it to
/// at 8 px.
const HINTED_GLYPHS: &[(char, u32, u32)] = &[
    (' ', 250, 2),
    ('A', 722, 6),
    ('B', 667, 6),
    ('C', 722, 6),
    ('D', 722, 6),
    ('E', 667, 5),
    ('F', 611, 5),
    ('I', 389, 3),
    ('L', 667, 5),
    ('M', 944, 8),
    ('N', 722, 6),
    ('P', 611, 5),
    ('R', 722, 6),
    ('S', 556, 4),
    ('T', 667, 5),
    ('U', 722, 6),
    ('Y', 722, 6),
];

/// A page a browser printed glyph by glyph, one string per glyph and each
/// word space painted as a space glyph, at advances hinted to whole pixels,
/// or, when `hinted` is false, at the glyphs' own widths (pdf-inspector
/// #531).
fn glyph_by_glyph_pdf(hinted: bool) -> Vec<u8> {
    glyph_by_glyph_pages(
        hinted,
        &[&[
            (60, "LIABILITIES"),
            (80, "BALANCE DUE AFTER PAYMENTS AND CREDITS"),
        ]],
    )
}

/// Pages of `glyph_by_glyph_pdf`, each the lines given, by their height.
fn glyph_by_glyph_pages(hinted: bool, pages: &[&[(u32, &str)]]) -> Vec<u8> {
    let glyph = |character: char| {
        HINTED_GLYPHS
            .iter()
            .find(|(known, ..)| *known == character)
            .expect("a glyph of the font")
    };
    let widths: Vec<String> = (32..=90u8)
        .map(|code| {
            HINTED_GLYPHS
                .iter()
                .find(|(known, ..)| *known == char::from(code))
                .map_or(0, |(_, width, _)| *width)
                .to_string()
        })
        .collect();
    let font = format!(
        "<< /Type /Font /Subtype /Type1 /BaseFont /SyntheticSerif-Bold /FirstChar 32 \
         /LastChar 90 /Widths [{}] /Encoding /WinAnsiEncoding >>",
        widths.join(" ")
    );
    let kids: Vec<String> = (0..pages.len())
        .map(|index| format!("{} 0 R", 4 + 2 * index))
        .collect();
    let mut objects = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        format!(
            "<< /Type /Pages /Kids [{}] /Count {} >>",
            kids.join(" "),
            pages.len()
        )
        .into_bytes(),
        font.into_bytes(),
    ];
    for (index, lines) in pages.iter().enumerate() {
        let mut content = String::from("1 0 0 -1 0 792 cm 0.75 0 0 0.75 0 0 cm\n");
        for (y, text) in lines.iter() {
            content.push_str(&format!("BT /F1 8 Tf 1 0 0 -1 0 0 Tm 40 -{y} Td"));
            for (position, character) in text.chars().enumerate() {
                if position > 0 {
                    let (_, width, advance) = glyph(text.chars().nth(position - 1).expect("glyph"));
                    let travel = if hinted {
                        f64::from(*advance)
                    } else {
                        f64::from(*width) * 8.0 / 1000.0
                    };
                    content.push_str(&format!(" {travel} 0 Td"));
                }
                content.push_str(&format!(" <{:02x}> Tj", u32::from(character)));
            }
            content.push_str(" ET\n");
        }
        objects.push(
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
                 /Resources << /Font << /F1 3 0 R >> >> /Contents {} 0 R >>",
                5 + 2 * index
            )
            .into_bytes(),
        );
        objects.push(stream("", content.as_bytes()));
    }
    pdf_file(&objects)
}

#[test]
fn words_split_at_hinted_glyph_advances_are_reported() {
    let temporary = tempfile::tempdir().expect("temporary PDF directory");
    let hinted = temporary.path().join("hinted.pdf");
    std::fs::write(&hinted, glyph_by_glyph_pdf(true)).expect("write PDF");
    let exact = temporary.path().join("exact.pdf");
    std::fs::write(&exact, glyph_by_glyph_pdf(false)).expect("write PDF");
    let results = call_tools(
        &[
            (
                "pdf_to_markdown",
                serde_json::json!({ "path": hinted.to_str().expect("UTF-8 path") }),
            ),
            (
                "pdf_to_markdown",
                serde_json::json!({ "path": exact.to_str().expect("UTF-8 path") }),
            ),
        ],
        None,
    );
    let reported = |result: &serde_json::Value| {
        result["warnings"].as_array().is_some_and(|warnings| {
            warnings.iter().any(|warning| {
                warning["code"] == "word_gaps_misread" && warning["pages"] == serde_json::json!([1])
            })
        })
    };
    assert!(reported(&results[0]), "{}", results[0]);
    // pdf-inspector 1.24.0 splits a word at a hinted advance narrower than
    // the glyph; when a release fixes #531, this expectation goes.
    assert!(
        results[0]["markdown"]
            .as_str()
            .is_some_and(|markdown| markdown.contains("LIAB ILITIES")),
        "{}",
        results[0]
    );
    // Glyphs advanced by their own widths are read whole, and nothing is
    // reported.
    assert!(!reported(&results[1]), "{}", results[1]);
    // A page whose words stand alone on their lines is read in a font seen
    // painting its spaces on another page.
    let alone = temporary.path().join("alone.pdf");
    std::fs::write(
        &alone,
        glyph_by_glyph_pages(
            true,
            &[
                &[
                    (60, "BALANCE DUE AFTER PAYMENTS AND CREDITS"),
                    (80, "AMENDED RETURN DUE AFTER CREDITS"),
                    (100, "PAYMENTS AND CREDITS APPLIED"),
                    (120, "BALANCE DUE AFTER PAYMENTS"),
                ],
                &[
                    (60, "LIABILITIES"),
                    (80, "ASSETS"),
                    (100, "SURPLUS"),
                    (120, "PAYMENTS"),
                    (140, "CREDITS"),
                    (160, "BALANCE"),
                ],
            ],
        ),
    )
    .expect("write PDF");
    let alone_results = call_tools(
        &[(
            "pdf_to_markdown",
            serde_json::json!({ "path": alone.to_str().expect("UTF-8 path") }),
        )],
        None,
    );
    assert!(
        alone_results[0]["warnings"]
            .as_array()
            .is_some_and(|warnings| {
                warnings.iter().any(|warning| {
                    warning["code"] == "word_gaps_misread"
                        && warning["pages"]
                            .as_array()
                            .is_some_and(|pages| pages.contains(&serde_json::json!(2)))
                })
            }),
        "{}",
        alone_results[0]
    );
    assert!(
        results[1]["markdown"].as_str().is_some_and(
            |markdown| markdown.contains("LIABILITIES") && markdown.contains("BALANCE DUE")
        ),
        "{}",
        results[1]
    );
}

/// A W-2 summary page whose box lines a form draws, reached through
/// `/Outer`, a form with the given entries (pdf-inspector #312).
fn nested_form_pdf(outer_entries: &str) -> Vec<u8> {
    let form = "/Type /XObject /Subtype /Form /BBox [0 0 612 792]";
    let lines = [
        "Box 1 Wages, tips, other compensation 85,000.00",
        "Box 2 Federal income tax withheld 12,400.00",
        "Box 3 Social security wages 88,000.00",
    ];
    let inner: String = lines
        .iter()
        .enumerate()
        .map(|(index, line)| format!("BT /F1 10 Tf 72 {} Td ({line}) Tj ET\n", 700 - 18 * index))
        .collect();
    pdf_file(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 4 0 R >> /XObject << /Outer 6 0 R /Inner 7 0 R >> >> /Contents 5 0 R >>".to_vec(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".to_vec(),
        stream(
            "",
            b"BT /F1 14 Tf 72 740 Td (Employer payroll summary for 2025) Tj ET q /Outer Do Q",
        ),
        stream(&format!("{form} {outer_entries}"), b"q /Inner Do Q"),
        stream(
            &format!("{form} /Resources << /Font << /F1 4 0 R >> >>"),
            inner.as_bytes(),
        ),
    ])
}

#[test]
fn text_drawn_through_forms_pdf_inspector_misses_is_reported() {
    let temporary = tempfile::tempdir().expect("temporary PDF directory");
    let bare = temporary.path().join("bare.pdf");
    std::fs::write(&bare, nested_form_pdf("")).expect("write PDF");
    let bound = temporary.path().join("bound.pdf");
    std::fs::write(
        &bound,
        nested_form_pdf("/Resources << /XObject << /Inner 7 0 R >> >>"),
    )
    .expect("write PDF");
    let results = call_tools(
        &[
            (
                "pdf_to_markdown",
                serde_json::json!({ "path": bare.to_str().expect("UTF-8 path") }),
            ),
            (
                "pdf_to_markdown",
                serde_json::json!({ "path": bound.to_str().expect("UTF-8 path") }),
            ),
        ],
        None,
    );
    let reported = |result: &serde_json::Value| {
        result["warnings"].as_array().is_some_and(|warnings| {
            warnings.iter().any(|warning| {
                warning["code"] == "form_text_unread" && warning["pages"] == serde_json::json!([1])
            })
        })
    };
    assert!(reported(&results[0]), "{}", results[0]);
    // pdf-inspector 1.24.0 does not read the form the bare form draws; when
    // a release fixes #312, this expectation goes.
    assert!(
        results[0]["markdown"]
            .as_str()
            .is_some_and(|markdown| !markdown.contains("85,000.00")),
        "{}",
        results[0]
    );
    // Bound in the drawing form's own resources, the form is read.
    assert!(!reported(&results[1]), "{}", results[1]);
    assert!(
        results[1]["markdown"]
            .as_str()
            .is_some_and(|markdown| markdown.contains("85,000.00")),
        "{}",
        results[1]
    );
}

/// A card statement page listing `rows` purchases under Date, Description
/// and Amount headings, with the amounts right-aligned at the far edge and
/// the new balance on a line of its own below them (pdf-inspector #424).
fn card_statement_pdf(rows: usize) -> Vec<u8> {
    let text = |font: &str, x: &str, y: usize, text: &str| {
        format!("BT /{font} 9 Tf 1 0 0 1 {x} {y} Tm ({text}) Tj ET\n")
    };
    let mut content = text("F1", "72", 750, "Card statement for March");
    for (heading, x) in [("Date", "72"), ("Description", "130"), ("Amount", "509.98")] {
        content.push_str(&text("F2", x, 720, heading));
    }
    for row in 0..rows {
        let y = 704 - 14 * row;
        content.push_str(&text("F1", "72", y, &format!("03/{:02}", row + 1)));
        let purchase = format!("Purchase at merchant {}", row + 1);
        content.push_str(&text("F1", "130", y, &purchase));
        content.push_str(&text("F1", "514.98", y, &format!("{}.40", 12 + 3 * row)));
    }
    content.push_str(&text("F1", "499.97", 690 - 14 * rows, "1,106.00"));
    pdf_file(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 4 0 R /F2 5 0 R >> >> /Contents 6 0 R >>".to_vec(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".to_vec(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold /Encoding /WinAnsiEncoding >>".to_vec(),
        stream("", content.as_bytes()),
    ])
}

#[test]
fn table_amounts_pushed_out_of_their_rows_are_reported() {
    let temporary = tempfile::tempdir().expect("temporary PDF directory");
    let mut calls = Vec::new();
    for rows in [20, 12] {
        let pdf = temporary.path().join(format!("statement-{rows}.pdf"));
        std::fs::write(&pdf, card_statement_pdf(rows)).expect("write PDF");
        let path = pdf.to_str().expect("UTF-8 path").to_string();
        calls.push(("pdf_to_markdown", serde_json::json!({ "path": path })));
    }
    let results = call_tools(&calls, None);
    let reported = |result: &serde_json::Value| {
        result["warnings"].as_array().is_some_and(|warnings| {
            warnings
                .iter()
                .any(|warning| warning["code"] == "table_values_detached")
        })
    };
    // pdf-inspector 1.24.0 drops the Amount column of the long statement
    // from its table and lists the amounts after it; when a release fixes
    // #424, this expectation goes.
    let markdown = results[0]["markdown"].as_str().unwrap_or_default();
    assert!(
        markdown.contains("|03/01|Purchase at merchant 1|\n") && markdown.contains("\n12.40\n"),
        "{markdown}"
    );
    assert!(reported(&results[0]), "{}", results[0]);
    // The short statement keeps its amounts in their rows, and the balance
    // after the table stands on a line of its own, as the page sets it.
    let markdown = results[1]["markdown"].as_str().unwrap_or_default();
    assert!(
        markdown.contains("|03/01|Purchase at merchant 1|12.40|")
            && markdown.contains("\n1,106.00"),
        "{markdown}"
    );
    assert!(!reported(&results[1]), "{}", results[1]);
}

/// A consolidated statement with a page for each account, each headed by
/// its account number under the bank's name, given by `headers`, and
/// listing its deposits (pdf-inspector issue #483).
fn consolidated_statement_pdf(headers: &[String]) -> Vec<u8> {
    let mut objects = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        Vec::new(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
            .to_vec(),
    ];
    let mut kids = Vec::new();
    for (page, header) in headers.iter().enumerate() {
        let mut content = format!(
            "BT /F1 10 Tf 1 0 0 1 72 740 Tm (Example Bank consolidated statement) Tj ET\n\
             BT /F1 10 Tf 1 0 0 1 72 722 Tm ({header}) Tj ET\n"
        );
        for row in 0..12 {
            let y = 690 - 16 * row;
            content.push_str(&format!(
                "BT /F1 10 Tf 1 0 0 1 72 {y} Tm (03/{:02} Deposit {}) Tj ET\n\
                 BT /F1 10 Tf 1 0 0 1 450 {y} Tm ({}.00) Tj ET\n",
                row + 1,
                row + 1,
                100 + 7 * row + 13 * page
            ));
        }
        objects.push(stream("", content.as_bytes()));
        objects.push(
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 3 0 R >> >> /Contents {} 0 R >>",
                objects.len()
            )
            .into_bytes(),
        );
        kids.push(format!("{} 0 R", objects.len()));
    }
    objects[1] = format!(
        "<< /Type /Pages /Kids [{}] /Count {} >>",
        kids.join(" "),
        kids.len()
    )
    .into_bytes();
    pdf_file(&objects)
}

#[test]
fn lines_dropped_as_running_headers_that_differ_are_reported() {
    let temporary = tempfile::tempdir().expect("temporary PDF directory");
    let accounts = ["12345678", "87654321", "55501234"];
    let documents = [
        // A page for each of three accounts.
        accounts
            .map(|number| format!("Account number {number}"))
            .to_vec(),
        // One account on every page.
        vec!["Account number 12345678".to_string(); 3],
        // A header numbering its pages.
        (1..=4)
            .map(|page| format!("Account number 12345678, page {page}"))
            .collect(),
    ];
    let mut calls = Vec::new();
    for (index, headers) in documents.iter().enumerate() {
        let pdf = temporary.path().join(format!("consolidated-{index}.pdf"));
        std::fs::write(&pdf, consolidated_statement_pdf(headers)).expect("write PDF");
        let path = pdf.to_str().expect("UTF-8 path").to_string();
        calls.push(("pdf_to_markdown", serde_json::json!({ "path": path })));
    }
    let results = call_tools(&calls, None);
    let reported = |result: &serde_json::Value| -> Option<serde_json::Value> {
        result["warnings"].as_array().and_then(|warnings| {
            warnings
                .iter()
                .find(|warning| warning["code"] == "header_footer_dropped")
                .map(|warning| warning["pages"].clone())
        })
    };
    // pdf-inspector 1.24.0 keeps the first account's number and drops the
    // others' as the same running header; when a release fixes #483, this
    // expectation goes.
    let markdown = results[0]["markdown"].as_str().unwrap_or_default();
    assert!(
        markdown.contains("Account number 12345678") && !markdown.contains("87654321"),
        "{markdown}"
    );
    assert_eq!(
        reported(&results[0]),
        Some(serde_json::json!([2, 3])),
        "{}",
        results[0]
    );
    // A header repeated as it is, or numbering its pages, drops nothing
    // the Markdown lacks.
    assert_eq!(reported(&results[1]), None, "{}", results[1]);
    assert_eq!(reported(&results[2]), None, "{}", results[2]);
}

/// A filled one-page form whose fields are `fields`, objects 6 on, each
/// given its number; `annotations` lists the page's widgets (pdf-inspector
/// issue #504).
fn filled_form_pdf(fields: &[(u32, &str)], annotations: &[u32]) -> Vec<u8> {
    let references = |ids: &mut dyn Iterator<Item = u32>| {
        ids.map(|id| format!("{id} 0 R"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let top: Vec<u32> = fields
        .iter()
        .filter(|(_, field)| !field.contains("/Parent"))
        .map(|(id, _)| *id)
        .collect();
    let mut objects = vec![
        format!(
            "<< /Type /Catalog /Pages 2 0 R /AcroForm << /Fields [{}] >> >>",
            references(&mut top.into_iter())
        )
        .into_bytes(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R /Annots [{}] >>",
            references(&mut annotations.iter().copied())
        )
        .into_bytes(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".to_vec(),
        stream(
            "",
            b"BT /F1 12 Tf 72 740 Td (Request for Taxpayer Identification Number) Tj ET \
              BT /F1 10 Tf 72 700 Td (Filing status: Single or Married filing jointly) Tj ET",
        ),
    ];
    for (id, field) in fields {
        assert_eq!(
            *id as usize,
            objects.len() + 1,
            "fields are numbered from 6"
        );
        objects.push(field.as_bytes().to_vec());
    }
    pdf_file(&objects)
}

#[test]
fn form_values_pdf_inspector_garbles_or_leaves_out_are_reported() {
    let temporary = tempfile::tempdir().expect("temporary PDF directory");
    let widget = |rect: &str| format!("/Type /Annot /Subtype /Widget /P 3 0 R /Rect [{rect}]");
    // "José García" in UTF-16, "São Paulo" in PDFDocEncoding.
    let name = format!(
        "<< /FT /Tx /T (payee_name) /V <FEFF004A006F007300E90020004700610072006300ED0061> {} >>",
        widget("150 696 400 712")
    );
    let city = format!(
        "<< /FT /Tx /T (city) /V <53E36F205061756C6F> {} >>",
        widget("150 666 400 682")
    );
    // A group of radio buttons keeps its choice on itself, not its widgets.
    let status =
        "<< /FT /Btn /Ff 49152 /T (filing_status) /V /MFJ /Kids [7 0 R 8 0 R] >>".to_string();
    let single = format!("<< /Parent 6 0 R /AS /Off {} >>", widget("72 680 84 692"));
    let joint = format!("<< /Parent 6 0 R /AS /MFJ {} >>", widget("172 680 184 692"));
    let amount = format!(
        "<< /FT /Tx /T (amount) /V (1,250.00) {} >>",
        widget("150 636 400 652")
    );
    let documents = [
        filled_form_pdf(&[(6, &name), (7, &city)], &[6, 7]),
        filled_form_pdf(&[(6, &status), (7, &single), (8, &joint)], &[7, 8]),
        filled_form_pdf(&[(6, &amount)], &[6]),
    ];
    let mut calls = Vec::new();
    for (index, pdf) in documents.iter().enumerate() {
        let path = temporary.path().join(format!("form-{index}.pdf"));
        std::fs::write(&path, pdf).expect("write PDF");
        let path = path.to_str().expect("UTF-8 path").to_string();
        calls.push(("pdf_to_markdown", serde_json::json!({ "path": path })));
    }
    let results = call_tools(&calls, None);
    let reported = |result: &serde_json::Value| -> Option<serde_json::Value> {
        result["warnings"].as_array().and_then(|warnings| {
            warnings
                .iter()
                .find(|warning| warning["code"] == "form_values_misread")
                .map(|warning| warning["pages"].clone())
        })
    };
    // pdf-inspector 1.24.0 reads the values as UTF-8, and never reads the
    // group's choice; when a release fixes #504, these expectations go.
    let markdown = results[0]["markdown"].as_str().unwrap_or_default();
    assert!(
        markdown.contains("S\u{FFFD}o Paulo") && !markdown.contains("José"),
        "{markdown:?}"
    );
    let markdown = results[1]["markdown"].as_str().unwrap_or_default();
    assert!(!markdown.contains("MFJ"), "{markdown:?}");
    for result in &results[..2] {
        assert_eq!(reported(result), Some(serde_json::json!([1])), "{result}");
    }
    // A plain value on its own widget reads as filled.
    let markdown = results[2]["markdown"].as_str().unwrap_or_default();
    assert!(markdown.contains("amount: 1,250.00"), "{markdown}");
    assert_eq!(reported(&results[2]), None, "{}", results[2]);
}

/// A statement page a reviewer marked up: a text box typed onto it and a
/// stamp drawn in text, both annotations; `flattened` also sets their text
/// in the page's own content.
fn annotated_statement_pdf(flattened: bool) -> Vec<u8> {
    let mut content = String::from(
        "BT /F1 12 Tf 72 740 Td (Brokerage statement realized gains) Tj ET \
         BT /F1 10 Tf 72 700 Td (100 sh XYZ CORP sold 03/02/25 proceeds 5,210.00) Tj ET",
    );
    if flattened {
        content.push_str(
            " BT /F1 10 Tf 302 666 Td (Adjusted basis 12,500.00 per preparer) Tj ET \
              BT /F1 12 Tf 404 736 Td (RECEIVED APR 15 2025) Tj ET",
        );
    }
    pdf_file(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R /Annots [6 0 R 7 0 R] >>".to_vec(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".to_vec(),
        stream("", content.as_bytes()),
        b"<< /Type /Annot /Subtype /FreeText /Rect [300 660 560 680] /F 4 /DA (/Helv 10 Tf 0 g) /Contents (Adjusted basis 12,500.00 per preparer) >>".to_vec(),
        b"<< /Type /Annot /Subtype /Stamp /Rect [400 720 560 760] /F 4 /Contents (RECEIVED APR 15 2025) /AP << /N 8 0 R >> >>".to_vec(),
        stream(
            "/Type /XObject /Subtype /Form /BBox [0 0 160 40] /Resources << /Font << /F1 4 0 R >> >>",
            b"BT /F1 12 Tf 4 16 Td (RECEIVED APR 15 2025) Tj ET",
        ),
    ])
}

#[test]
fn annotation_text_pdf_inspector_never_reads_is_reported() {
    let temporary = tempfile::tempdir().expect("temporary PDF directory");
    let mut calls = Vec::new();
    for flattened in [false, true] {
        let path = temporary.path().join(format!("annotated-{flattened}.pdf"));
        std::fs::write(&path, annotated_statement_pdf(flattened)).expect("write PDF");
        let path = path.to_str().expect("UTF-8 path").to_string();
        calls.push(("pdf_to_markdown", serde_json::json!({ "path": path })));
    }
    let results = call_tools(&calls, None);
    let reported = |result: &serde_json::Value| -> Option<serde_json::Value> {
        result["warnings"].as_array().and_then(|warnings| {
            warnings
                .iter()
                .find(|warning| warning["code"] == "annotation_text_unread")
                .map(|warning| warning["pages"].clone())
        })
    };
    // pdf-inspector 1.24.0 reads no annotation but links and form fields;
    // when a release reads them, this expectation goes.
    let markdown = results[0]["markdown"].as_str().unwrap_or_default();
    assert!(
        !markdown.contains("12,500.00") && !markdown.contains("RECEIVED"),
        "{markdown}"
    );
    assert_eq!(
        reported(&results[0]),
        Some(serde_json::json!([1])),
        "{}",
        results[0]
    );
    // Text the page's own content also sets is in the Markdown.
    assert_eq!(reported(&results[1]), None, "{}", results[1]);
}

/// A filled tax form made in XFA, whose page holds only the notice a
/// viewer without XFA shows; `dynamic` marks it as needing rendering.
fn xfa_form_pdf(dynamic: bool) -> Vec<u8> {
    let needs = if dynamic { " /NeedsRendering true" } else { "" };
    pdf_file(&[
        format!(
            "<< /Type /Catalog /Pages 2 0 R{needs} /AcroForm << /Fields [] /XFA [(template) 6 0 R (datasets) 7 0 R] >> >>"
        )
        .into_bytes(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>".to_vec(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".to_vec(),
        stream(
            "",
            b"BT /F1 10 Tf 36 740 Td (Please wait... If this message is not eventually replaced by the proper contents of the document, your PDF viewer may not be able to display this type of document.) Tj ET",
        ),
        stream(
            "",
            b"<template xmlns=\"http://www.xfa.org/schema/xfa-template/3.3/\"><subform name=\"form1\"><field name=\"Wages\"/></subform></template>",
        ),
        stream(
            "",
            b"<xfa:datasets xmlns:xfa=\"http://www.xfa.org/schema/xfa-data/1.0/\"><xfa:data><form1><Wages>85000.00</Wages></form1></xfa:data></xfa:datasets>",
        ),
    ])
}

#[test]
fn dynamic_xfa_forms_pdf_inspector_cannot_read_are_reported() {
    let temporary = tempfile::tempdir().expect("temporary PDF directory");
    let mut calls = Vec::new();
    for dynamic in [true, false] {
        let path = temporary.path().join(format!("xfa-{dynamic}.pdf"));
        std::fs::write(&path, xfa_form_pdf(dynamic)).expect("write PDF");
        let path = path.to_str().expect("UTF-8 path").to_string();
        calls.push(("pdf_to_markdown", serde_json::json!({ "path": path })));
    }
    let results = call_tools(&calls, None);
    let reported = |result: &serde_json::Value| {
        result["warnings"].as_array().is_some_and(|warnings| {
            warnings
                .iter()
                .any(|warning| warning["code"] == "xfa_form_unread")
        })
    };
    // pdf-inspector 1.24.0 reads no XFA: the Markdown is the notice alone.
    let markdown = results[0]["markdown"].as_str().unwrap_or_default();
    assert!(
        markdown.contains("Please wait") && !markdown.contains("85000.00"),
        "{markdown}"
    );
    assert!(reported(&results[0]), "{}", results[0]);
    // A form that does not need rendering draws its own pages.
    assert!(!reported(&results[1]), "{}", results[1]);
}

/// A cover page bundling two embedded tax forms, as a portfolio when
/// `portfolio`, else as attachments.
fn bundled_forms_pdf(portfolio: bool) -> Vec<u8> {
    let collection = if portfolio {
        " /Collection << /Type /Collection >>"
    } else {
        ""
    };
    pdf_file(&[
        format!(
            "<< /Type /Catalog /Pages 2 0 R{collection} /Names << /EmbeddedFiles << /Names [(1099-DIV.pdf) 6 0 R (1099-INT.pdf) 8 0 R] >> >> >>"
        )
        .into_bytes(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>".to_vec(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".to_vec(),
        stream(
            "",
            b"BT /F1 12 Tf 72 740 Td (Year-end tax package: open this portfolio to see its documents.) Tj ET",
        ),
        b"<< /Type /Filespec /F (1099-DIV.pdf) /EF << /F 7 0 R >> >>".to_vec(),
        stream("/Type /EmbeddedFile", b"%PDF-1.4 dividends 1,250.00"),
        b"<< /Type /Filespec /F (1099-INT.pdf) /EF << /F 9 0 R >> >>".to_vec(),
        stream("/Type /EmbeddedFile", b"%PDF-1.4 interest 310.00"),
    ])
}

#[test]
fn embedded_files_pdf_inspector_never_reads_are_reported() {
    let temporary = tempfile::tempdir().expect("temporary PDF directory");
    let mut calls = Vec::new();
    for portfolio in [true, false] {
        let path = temporary.path().join(format!("bundle-{portfolio}.pdf"));
        std::fs::write(&path, bundled_forms_pdf(portfolio)).expect("write PDF");
        let path = path.to_str().expect("UTF-8 path").to_string();
        calls.push(("pdf_to_markdown", serde_json::json!({ "path": path })));
    }
    calls.push((
        "pdf_to_markdown",
        serde_json::json!({ "path": fixture("source/sample-1.pdf") }),
    ));
    let results = call_tools(&calls, None);
    let message = |result: &serde_json::Value| -> Option<String> {
        result["warnings"].as_array().and_then(|warnings| {
            warnings
                .iter()
                .find(|warning| warning["code"] == "embedded_files_unread")
                .and_then(|warning| warning["message"].as_str())
                .map(str::to_string)
        })
    };
    // pdf-inspector 1.24.0 reads the cover's page alone.
    let markdown = results[0]["markdown"].as_str().unwrap_or_default();
    assert!(
        markdown.contains("Year-end tax package") && !markdown.contains("1,250.00"),
        "{markdown}"
    );
    let portfolio = message(&results[0]).unwrap_or_default();
    assert!(
        portfolio.contains("portfolio of 2 embedded files"),
        "{portfolio}"
    );
    let attachments = message(&results[1]).unwrap_or_default();
    assert!(
        attachments.contains("carries 2 embedded files"),
        "{attachments}"
    );
    assert_eq!(message(&results[2]), None, "{}", results[2]);
}

#[test]
fn region_tools_read_rectangles_in_the_requested_frame() {
    let temporary = tempfile::tempdir().expect("temporary PDF directory");
    let pdf = temporary.path().join("rotated.pdf");
    std::fs::write(&pdf, rotated_page_pdf()).expect("write rotated PDF");
    let pdf = pdf.to_str().expect("UTF-8 path").to_string();
    // The text runs down the right margin of the rendered page.
    let display = serde_json::json!([{ "page": 0, "rects": [[690.0, 60.0, 720.0, 260.0]] }]);
    // The table sits left of it once the page is turned.
    let table = serde_json::json!([{ "page": 0, "rects": [[445.0, 60.0, 520.0, 360.0]] }]);
    let results = call_tools(
        &[
            (
                "extract_text_regions",
                serde_json::json!({ "path": pdf, "regions": display, "frame": "display" }),
            ),
            (
                "extract_text_regions",
                serde_json::json!({ "path": pdf, "regions": display }),
            ),
            (
                "extract_table_regions",
                serde_json::json!({ "path": pdf, "regions": table, "frame": "display" }),
            ),
            (
                "extract_text_regions",
                serde_json::json!({ "path": pdf, "regions": display, "frame": "rotated" }),
            ),
            (
                "extract_table_regions",
                serde_json::json!({ "path": pdf, "regions": table }),
            ),
        ],
        None,
    );
    let text = |result: &serde_json::Value| {
        result[0]["regions"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    };
    assert!(
        text(&results[0]).contains("ROTATED-MARKER"),
        "{}",
        results[0]
    );
    // Without a frame the rectangles are read on the unturned page.
    assert!(
        !text(&results[1]).contains("ROTATED-MARKER"),
        "{}",
        results[1]
    );
    // Table regions take the frame too.
    let ledger = text(&results[2]);
    assert!(
        ledger.contains("Receivables") && ledger.contains("350"),
        "{}",
        results[2]
    );
    assert!(!text(&results[4]).contains("Receivables"), "{}", results[4]);
    // An unknown frame is rejected, not ignored.
    assert_eq!(results[3]["is_error"], true, "{}", results[3]);
    assert!(results[3]["reason"]
        .as_str()
        .is_some_and(|reason| reason.contains("`sheet` or `display`")));
}

#[cfg(unix)]
fn fake_worker(directory: &Path, name: &str, script: &str) -> PathBuf {
    let worker = directory.join(name);
    std::fs::write(&worker, script).expect("write fake worker");
    let mut permissions = std::fs::metadata(&worker)
        .expect("fake worker metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&worker, permissions).expect("make fake worker executable");
    worker
}

/// A worker the kernel stops for a resource reason (Rust aborts when an
/// allocation fails under the address-space ceiling) must surface as a
/// resource limit, not as a protocol error, and must not leak the path.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn pdf_tools_run_in_the_worker_and_map_aborts_to_resource_limits() {
    let temporary = tempfile::tempdir().expect("temporary worker directory");
    let aborting = fake_worker(
        temporary.path(),
        "aborting-worker.sh",
        "#!/bin/sh\ncat >/dev/null\nkill -ABRT $$\n",
    );
    let silent = fake_worker(
        temporary.path(),
        "silent-worker.sh",
        "#!/bin/sh\ncat >/dev/null\nexit 0\n",
    );
    let pdf = fixture("source/sample-1.pdf");

    let aborted = call_tools(
        &[("classify_pdf", serde_json::json!({ "path": pdf }))],
        Some(&aborting),
    );
    assert_eq!(
        aborted[0]["error"],
        "PDF processing exceeded a resource limit"
    );
    let unanswered = call_tools(
        &[("parse_irc_sections", serde_json::json!({ "path": pdf }))],
        Some(&silent),
    );
    assert_eq!(
        unanswered[0]["error"],
        "PDF worker returned an invalid response"
    );
    for result in aborted.iter().chain(&unanswered) {
        assert!(!result.to_string().contains("worker.sh"));
    }
}

/// A page whose content stream inflates to 1 GiB from about 1 MB drove the
/// in-process server past 2 GiB on pdf-inspector 1.24.0. Behind the worker's
/// address-space ceiling it fails alone and the server keeps serving.
#[cfg(target_os = "linux")]
#[test]
fn pdf_page_content_bomb_is_contained_by_the_worker() {
    let temporary = tempfile::tempdir().expect("temporary bomb directory");
    let bomb = temporary.path().join("page-content-bomb.pdf");
    std::fs::write(&bomb, page_content_bomb(1024)).expect("write bomb");

    let started = Instant::now();
    let results = call_tools(
        &[
            ("pdf_to_markdown", serde_json::json!({ "path": bomb })),
            ("classify_pdf", serde_json::json!({ "path": bomb })),
            (
                "classify_pdf",
                serde_json::json!({ "path": fixture("source/sample-1.pdf") }),
            ),
        ],
        None,
    );
    assert_eq!(
        results[0]["error"],
        "PDF processing exceeded a resource limit"
    );
    assert_eq!(
        results[1]["error"],
        "PDF processing exceeded a resource limit"
    );
    assert_eq!(results[2]["pdf_type"], "TextBased");
    assert!(started.elapsed() < Duration::from_secs(60));
}

/// A zlib stream that inflates to `mib` MiB of spaces. One MiB is compressed
/// once with a full flush, which ends its blocks on a byte boundary and resets
/// the dictionary, so copies of it concatenate into a valid stream without
/// compressing a gibibyte in a debug build.
#[cfg(target_os = "linux")]
fn zlib_spaces(mib: usize) -> Vec<u8> {
    use flate2::{Compress, Compression, FlushCompress};

    let block = vec![b' '; 1024 * 1024];
    let mut compressor = Compress::new(Compression::best(), false);
    let mut chunk = Vec::with_capacity(64 * 1024);
    compressor
        .compress_vec(&block, &mut chunk, FlushCompress::Full)
        .expect("compress block");
    assert_eq!(compressor.total_in(), block.len() as u64);
    let mut last = Vec::with_capacity(64);
    Compress::new(Compression::best(), false)
        .compress_vec(&[], &mut last, FlushCompress::Finish)
        .expect("final block");

    // Adler-32 of `n` bytes of value 32: A = 1 + 32n, B = n + 32 n(n+1)/2.
    let n = (mib as u128) * 1024 * 1024;
    let a = (1 + 32 * n) % 65521;
    let b = (n + 32 * n * (n + 1) / 2) % 65521;
    let adler = ((b << 16) | a) as u32;

    let mut stream = vec![0x78, 0xDA];
    for _ in 0..mib {
        stream.extend_from_slice(&chunk);
    }
    stream.extend_from_slice(&last);
    stream.extend_from_slice(&adler.to_be_bytes());
    stream
}

#[cfg(target_os = "linux")]
#[test]
fn repeated_full_flush_blocks_form_a_valid_zlib_stream() {
    use std::io::Read;

    let mut inflated = Vec::new();
    flate2::read::ZlibDecoder::new(zlib_spaces(3).as_slice())
        .read_to_end(&mut inflated)
        .expect("stream and checksum must verify");
    assert_eq!(inflated.len(), 3 * 1024 * 1024);
    assert!(inflated.iter().all(|byte| *byte == b' '));
}

/// A one-page PDF whose Flate page content inflates to `mib` MiB of spaces.
#[cfg(target_os = "linux")]
fn page_content_bomb(mib: usize) -> Vec<u8> {
    let content = zlib_spaces(mib);

    let mut pdf = b"%PDF-1.5\n".to_vec();
    let mut offsets = Vec::new();
    let mut object = |pdf: &mut Vec<u8>, body: &[u8]| {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n", offsets.len()).as_bytes());
        pdf.extend_from_slice(body);
        pdf.extend_from_slice(b"\nendobj\n");
    };
    object(&mut pdf, b"<< /Type /Catalog /Pages 2 0 R >>");
    object(&mut pdf, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>");
    object(
        &mut pdf,
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
          /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>",
    );
    let mut stream = format!(
        "<< /Filter /FlateDecode /Length {} >>\nstream\n",
        content.len()
    )
    .into_bytes();
    stream.extend_from_slice(&content);
    stream.extend_from_slice(b"\nendstream");
    object(&mut pdf, &stream);
    object(
        &mut pdf,
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
    );
    let xref = pdf.len();
    pdf.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len() + 1).as_bytes(),
    );
    for offset in &offsets {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            offsets.len() + 1
        )
        .as_bytes(),
    );
    pdf
}

/// Tool names are a compatibility contract, and every tool only reads local
/// files: clients may rely on the annotations to allow calls without asking.
#[test]
fn tools_list_keeps_names_and_declares_read_only_annotations() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_pdf-inspector-mcp"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("MCP binary must be available to integration tests");
    let mut stdin = child.stdin.take().expect("child stdin");
    for request in [
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "tools-list-test", "version": "1" }
            }
        }),
        serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
    ] {
        writeln!(stdin, "{request}").expect("write request");
    }
    stdin.flush().expect("flush requests");
    let mut stdout = BufReader::new(child.stdout.take().expect("child stdout"));
    let listing = loop {
        let mut line = String::new();
        assert!(stdout.read_line(&mut line).expect("read response") > 0);
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) {
            if value.get("id") == Some(&serde_json::Value::from(1)) {
                break value;
            }
        }
    };
    drop(stdin);
    assert!(child.wait().expect("wait for MCP server").success());

    let tools = listing["result"]["tools"].as_array().expect("tool array");
    let mut names: Vec<_> = tools
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name"))
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "analyze_layout",
            "batch_classify",
            "classify_document",
            "classify_pdf",
            "compare_line_items",
            "document_capabilities",
            "document_to_markdown",
            "extract_table_regions",
            "extract_text_regions",
            "identify_tax_form",
            "list_tax_packages",
            "parse_irc_sections",
            "pdf_to_markdown",
            "render_review_memo",
            "review_tax_package",
            "split_sec_filing",
        ]
    );
    for name in ["extract_text_regions", "extract_table_regions"] {
        let schema = &tools
            .iter()
            .find(|tool| tool["name"] == name)
            .expect("region tool")["inputSchema"];
        assert!(
            schema["properties"]["frame"].is_object(),
            "{name} declares its optional frame"
        );
        let required = schema["required"].as_array().expect("required list");
        assert!(!required.iter().any(|field| field == "frame"), "{name}");
        assert!(required.iter().any(|field| field == "regions"), "{name}");
    }
    for tool in tools {
        let annotations = &tool["annotations"];
        assert_eq!(annotations["readOnlyHint"], true, "{tool}");
        assert_eq!(annotations["destructiveHint"], false, "{tool}");
        assert_eq!(annotations["idempotentHint"], true, "{tool}");
        assert_eq!(annotations["openWorldHint"], false, "{tool}");
        assert!(annotations["title"]
            .as_str()
            .is_some_and(|title| !title.is_empty()));
    }
}
