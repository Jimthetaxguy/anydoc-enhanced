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
