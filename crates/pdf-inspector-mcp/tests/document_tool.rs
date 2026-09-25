use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

fn run_classify_document(path: String, client_name: &str) -> serde_json::Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_pdf-inspector-mcp"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("MCP binary must be available to integration tests");
    let requests = [
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": client_name, "version": "1" }
            }
        }),
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }),
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "classify_document",
                "arguments": { "path": path }
            }
        }),
    ];
    let mut stdin = child.stdin.take().expect("child stdin");
    for request in requests {
        writeln!(stdin, "{request}").expect("write JSON-RPC request");
    }
    stdin.flush().expect("flush JSON-RPC request");

    let mut stdout = BufReader::new(child.stdout.take().expect("child stdout"));
    let response = loop {
        let mut line = String::new();
        let bytes = stdout.read_line(&mut line).expect("read JSON-RPC response");
        assert!(bytes > 0, "classification response");
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) {
            if value.get("id") == Some(&serde_json::Value::from(2)) {
                break value;
            }
        }
    };
    drop(stdin);
    assert!(child.wait().expect("wait for MCP server").success());
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .expect("classification text result");
    serde_json::from_str(text).expect("classification JSON result")
}

fn run_document_tool(path: String, client_name: &str) -> serde_json::Value {
    run_document_tool_with_worker(path, client_name, None)
}

fn run_document_tool_with_worker(
    path: String,
    client_name: &str,
    worker_bin: Option<&Path>,
) -> serde_json::Value {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pdf-inspector-mcp"));
    if let Some(worker_bin) = worker_bin {
        command.env("ANYDOC_WORKER_BIN", worker_bin);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("MCP binary must be available to integration tests");
    let requests = [
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": client_name, "version": "1" }
            }
        }),
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }),
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "document_to_markdown",
                "arguments": { "path": path }
            }
        }),
    ];
    let mut stdin = child.stdin.take().expect("child stdin");
    for request in requests {
        writeln!(stdin, "{request}").expect("write JSON-RPC request");
    }
    stdin.flush().expect("flush JSON-RPC request");

    let mut stdout = BufReader::new(child.stdout.take().expect("child stdout"));
    let response = loop {
        let mut line = String::new();
        let bytes = stdout.read_line(&mut line).expect("read JSON-RPC response");
        assert!(bytes > 0, "document tool response");
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) {
            if value.get("id") == Some(&serde_json::Value::from(2)) {
                break value;
            }
        }
    };
    drop(stdin);
    assert!(child.wait().expect("wait for MCP server").success());
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .expect("tool text result");
    serde_json::from_str(text).expect("document JSON result")
}

#[test]
fn epub_external_reference_is_rejected_before_any_network_access() {
    let temporary = tempfile::tempdir().expect("temporary EPUB directory");
    let package = temporary.path().join("network-canary.epub");
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("loopback canary listener");
    let port = listener
        .local_addr()
        .expect("loopback listener address")
        .port();
    let chapter = format!(
        "<?xml version=\"1.0\"?><html xmlns=\"http://www.w3.org/1999/xhtml\"><body><p><img src=\"http://127.0.0.1:{port}/image.png\"/></p></body></html>"
    );
    let container = "<?xml version=\"1.0\"?><container xmlns=\"urn:oasis:names:tc:opendocument:xmlns:container\"><rootfiles><rootfile full-path=\"OPS/package.opf\" media-type=\"application/oebps-package+xml\"/></rootfiles></container>";
    let opf = "<?xml version=\"1.0\"?><package xmlns=\"http://www.idpf.org/2007/opf\" version=\"3.0\"><metadata/><manifest><item id=\"chapter\" href=\"Text/chapter.xhtml\" media-type=\"application/xhtml+xml\"/></manifest><spine><itemref idref=\"chapter\"/></spine></package>";
    let file = std::fs::File::create(&package).expect("create EPUB canary");
    let mut archive = zip::ZipWriter::new(file);
    let stored =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    archive
        .start_file("mimetype", stored)
        .expect("EPUB mimetype");
    archive
        .write_all(b"application/epub+zip")
        .expect("EPUB mimetype bytes");
    for (name, contents) in [
        ("META-INF/container.xml", container.as_bytes()),
        ("OPS/package.opf", opf.as_bytes()),
        ("OPS/Text/chapter.xhtml", chapter.as_bytes()),
    ] {
        archive
            .start_file(name, zip::write::SimpleFileOptions::default())
            .expect("EPUB package part");
        archive.write_all(contents).expect("EPUB package bytes");
    }
    archive.finish().expect("finish EPUB canary");

    let document = run_document_tool(
        package.to_string_lossy().into_owned(),
        "epub-network-canary-test",
    );
    if cfg!(target_os = "linux") {
        assert_eq!(document["code"], "incomplete_conversion");
        assert_eq!(
            document["error"],
            "document conversion omitted required content"
        );
    } else {
        assert_eq!(document["code"], "unsupported");
        assert_eq!(document["error"], "document format is not enabled");
    }
    assert!(!document.to_string().contains("127.0.0.1"));

    listener
        .set_nonblocking(true)
        .expect("configure loopback canary listener");
    let deadline = Instant::now() + Duration::from_millis(250);
    while Instant::now() < deadline {
        match listener.accept() {
            Ok((_stream, address)) => panic!("external EPUB fetch reached canary at {address}"),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("loopback canary failed: {error}"),
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn worker_sandbox_denies_network_access() {
    let temporary = tempfile::tempdir().expect("temporary worker directory");
    let worker = temporary.path().join("network-worker.sh");
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("loopback canary listener");
    let port = listener
        .local_addr()
        .expect("loopback listener address")
        .port();
    let script = format!(
        "#!/bin/sh\n/usr/bin/python3 -c \"import socket; s=socket.socket(); s.settimeout(1); s.connect((127.0.0.1,{port}))\" >/dev/null 2>&1\nexit 0\n"
    );
    std::fs::write(&worker, script).expect("write network worker");
    let mut permissions = std::fs::metadata(&worker)
        .expect("network worker metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&worker, permissions).expect("make network worker executable");

    let fixture = format!(
        "{}/../../test-corpus/docx/public-fixture.docx",
        env!("CARGO_MANIFEST_DIR")
    );
    let document =
        run_document_tool_with_worker(fixture, "macos-worker-sandbox-test", Some(&worker));
    assert_eq!(document["code"], "worker_protocol");
    assert_eq!(
        document["error"],
        "AnyDoc worker returned an invalid response"
    );

    listener
        .set_nonblocking(true)
        .expect("configure loopback canary listener");
    let deadline = Instant::now() + Duration::from_millis(250);
    while Instant::now() < deadline {
        match listener.accept() {
            Ok((_stream, address)) => panic!("sandboxed worker reached canary at {address}"),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("loopback canary failed: {error}"),
        }
    }
}

#[cfg(unix)]
#[test]
fn mcp_worker_timeout_kills_and_reaps_descendant_process_group() {
    let temporary = tempfile::tempdir().expect("temporary worker directory");
    let worker = temporary.path().join("hang-worker.sh");
    let pid_file = temporary.path().join("hang-worker.sh.pids");
    std::fs::write(
        &worker,
        r#"#!/bin/sh
/bin/sleep 60 &
child=$!
printf '%s:%s' "$$" "$child" > "$0.pids"
wait
"#,
    )
    .expect("write hanging worker");
    let mut permissions = std::fs::metadata(&worker)
        .expect("hanging worker metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&worker, permissions).expect("make hanging worker executable");

    let fixture = format!(
        "{}/../../test-corpus/docx/public-fixture.docx",
        env!("CARGO_MANIFEST_DIR")
    );
    let started = Instant::now();
    let error =
        run_document_tool_with_worker(fixture, "timeout-reap-integration-test", Some(&worker));

    assert_eq!(error["code"], "worker_timeout");
    assert_eq!(error["error"], "AnyDoc worker timed out");
    assert!(started.elapsed() < Duration::from_secs(25));
    assert!(!error
        .to_string()
        .contains(&worker.to_string_lossy().to_string()));

    let pids = std::fs::read_to_string(&pid_file).expect("hanging worker must record its pids");
    for pid in pids.split(':') {
        let pid = pid.parse::<libc::pid_t>().expect("recorded pid");
        let mut gone = false;
        for _ in 0..200 {
            let result = unsafe { libc::kill(pid, 0) };
            if result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                gone = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(gone, "worker process {pid} remained after timeout reap");
    }
}

#[cfg(unix)]
#[derive(Debug)]
struct ResourceSample {
    lane: &'static str,
    fixture: &'static str,
    peak_rss_bytes: u64,
    response_bytes: u64,
    wall_clock_ms: u64,
}

#[cfg(unix)]
fn run_worker_with_resource_evidence(
    lane: &'static str,
    fixture: &'static str,
    worker_code: u8,
) -> ResourceSample {
    const PROTOCOL_MAGIC: &[u8; 4] = b"ADW1";
    const PROTOCOL_VERSION: u8 = 2;
    const FRAME_HEADER_BYTES: usize = 16;

    let fixture_path = format!("{}/../../test-corpus/{fixture}", env!("CARGO_MANIFEST_DIR"));
    let input = std::fs::read(&fixture_path).expect("resource fixture");
    let mut child = Command::new(env!("CARGO_BIN_EXE_pdf-inspector-mcp"))
        .arg("--anydoc-worker")
        .env("ANYDOC_RESOURCE_EVIDENCE", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("worker binary must be available to integration tests");

    let started = Instant::now();
    let stdin = child.stdin.as_mut().expect("worker stdin");
    stdin.write_all(PROTOCOL_MAGIC).expect("worker magic");
    stdin
        .write_all(&[PROTOCOL_VERSION, 0, 0, 0])
        .expect("worker protocol version");
    stdin
        .write_all(&((input.len() as u64) + 1).to_le_bytes())
        .expect("worker input length");
    stdin.write_all(&[worker_code]).expect("worker variant");
    stdin.write_all(&input).expect("worker input bytes");
    drop(child.stdin.take());

    let mut output = Vec::new();
    child
        .stdout
        .take()
        .expect("worker stdout")
        .read_to_end(&mut output)
        .expect("worker response");
    assert!(child.wait().expect("worker wait").success());
    let wall_clock_ms = started.elapsed().as_millis() as u64;

    assert!(output.len() >= FRAME_HEADER_BYTES, "worker frame header");
    assert_eq!(&output[..4], PROTOCOL_MAGIC);
    assert_eq!(output[4], PROTOCOL_VERSION);
    let payload_len = u64::from_le_bytes(
        output[8..FRAME_HEADER_BYTES]
            .try_into()
            .expect("worker payload length"),
    ) as usize;
    assert_eq!(payload_len, output.len() - FRAME_HEADER_BYTES);
    let response: serde_json::Value =
        serde_json::from_slice(&output[FRAME_HEADER_BYTES..]).expect("worker JSON");
    let peak_rss_bytes = response["resource"]["peak_rss_bytes"]
        .as_u64()
        .expect("opt-in resource evidence");
    assert!(response["markdown"].is_string(), "complete worker response");

    ResourceSample {
        lane,
        fixture,
        peak_rss_bytes,
        response_bytes: output.len() as u64,
        wall_clock_ms,
    }
}

#[cfg(unix)]
#[test]
fn worker_resource_evidence_covers_enabled_lanes() {
    let cases = [
        ("docx", "docx/public-fixture.docx", 1),
        ("pptx", "pptx/public-walkthrough.pptx", 3),
        ("xlsx", "xlsx/public-workpaper.xlsx", 2),
        ("ods", "ods/public-workpaper.ods", 4),
        ("odt", "odt/minimal.odt", 5),
        ("odp", "odp/public-presentation.odp", 7),
        ("epub", "epub/public-spine-order.epub", 8),
    ];

    for (lane, fixture, worker_code) in cases {
        let sample = run_worker_with_resource_evidence(lane, fixture, worker_code);
        assert!(sample.peak_rss_bytes > 0, "{sample:?}");
        assert!(sample.response_bytes > 16, "{sample:?}");
        assert!(
            sample.response_bytes <= 8 * 1024 * 1024,
            "worker response exceeded the public output envelope: {sample:?}"
        );
        assert!(
            sample.wall_clock_ms < 15_000,
            "worker exceeded its deadline without timing out: {sample:?}"
        );
        println!(
            "resource_evidence lane={} fixture={} peak_rss_bytes={} response_bytes={} wall_clock_ms={}",
            sample.lane,
            sample.fixture,
            sample.peak_rss_bytes,
            sample.response_bytes,
            sample.wall_clock_ms
        );
    }
}

fn assert_complete_fixture(relative_path: &str, client_name: &str, markers: &[&str]) {
    let fixture = format!(
        "{}/../../test-corpus/{relative_path}",
        env!("CARGO_MANIFEST_DIR")
    );
    let document = run_document_tool(fixture, client_name);
    assert_eq!(
        document["completeness"], "complete",
        "fixture {relative_path}"
    );
    assert!(
        document["warnings"].is_array(),
        "warning schema {relative_path}"
    );
    let markdown = document["markdown"]
        .as_str()
        .expect("complete fixture Markdown output");
    for marker in markers {
        assert!(
            markdown.contains(marker),
            "structural marker {marker:?} missing from {relative_path}"
        );
    }
    let serialized = document.to_string();
    assert!(!serialized.contains("anydoc::"));
    assert!(!serialized.contains("warning:"));
    assert!(!serialized.contains(&["/", "Users", "/"].concat()));
}

#[test]
fn enabled_complete_fixtures_preserve_structural_markers() {
    let cases: &[(&str, &str, &[&str])] = &[
        (
            "docx/public-fixture.docx",
            "structural-oracle-docx",
            &[
                "Public Fixture Heading",
                "Tax-neutral public fixture text.",
                "Second ordered paragraph.",
            ],
        ),
        (
            "pptx/public-walkthrough.pptx",
            "structural-oracle-pptx",
            &[
                "Inherited Title Slide",
                "Roman three bold via master",
                "Plain closing line",
            ],
        ),
        (
            "xlsx/public-workpaper.xlsx",
            "structural-oracle-xlsx",
            &[
                "Inputs",
                "Summary",
                "Gross receipts",
                "Other receipts",
                "Total receipts",
                "150000",
            ],
        ),
        (
            "ods/public-workpaper.ods",
            "structural-oracle-ods",
            &["Values", "Merged Grid", "15.5%", "Span two"],
        ),
        (
            "odt/minimal.odt",
            "structural-oracle-odt",
            &["Minimal public ODT."],
        ),
    ];
    for (relative_path, client_name, markers) in cases {
        assert_complete_fixture(relative_path, client_name, markers);
    }
}

#[test]
fn epub_is_recognized_and_platform_memory_gated() {
    let fixture = format!(
        "{}/../../test-corpus/epub/public-spine-order.epub",
        env!("CARGO_MANIFEST_DIR")
    );
    let classification = run_classify_document(fixture.clone(), "epub-classification-test");
    assert_eq!(classification["kind"], "epub");
    assert_eq!(classification["variant"], "epub");
    assert_eq!(
        classification["enabled"],
        serde_json::Value::from(cfg!(target_os = "linux"))
    );
    assert_eq!(
        classification["capabilities"]["enabled"],
        serde_json::Value::from(cfg!(target_os = "linux"))
    );
    assert_eq!(
        classification["capabilities"]["supported_variants"][0],
        "epub"
    );

    if cfg!(target_os = "linux") {
        let document = run_document_tool(fixture, "epub-integration-test");
        assert_eq!(document["kind"], "epub");
        assert_eq!(document["variant"], "epub");
        assert_eq!(document["schema_version"], 2);
        assert_eq!(document["provider"]["name"], "anydoc");
        assert_eq!(document["provider"]["version"], "0.2.4");
        assert_eq!(document["provider"]["source"], "firecrawl/anydoc");
        assert_eq!(document["completeness"], "complete");
        let markdown = document["markdown"].as_str().expect("EPUB Markdown output");
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
                .unwrap_or_else(|| panic!("missing EPUB marker {marker:?}"));
            assert!(
                offset >= previous,
                "EPUB marker order drifted at {marker:?}"
            );
            previous = offset;
        }
        assert!(!document.to_string().contains("https://"));
        // AnyDoc's anchors for link targets are the only markup kept.
        assert!(!without_anchors(markdown).contains("</"), "{markdown}");
        assert!(!document.to_string().contains(&["/", "Users", "/"].concat()));
    } else {
        let error = run_document_tool(fixture, "epub-disabled-route-test");
        assert_eq!(error["code"], "unsupported");
        assert_eq!(error["error"], "document format is not enabled");
    }
}

/// Markdown without the anchors AnyDoc writes for link targets,
/// `<a id="…"></a>` with an id of `[a-z0-9_-]`.
fn without_anchors(markdown: &str) -> String {
    let mut output = String::with_capacity(markdown.len());
    let mut rest = markdown;
    while let Some(start) = rest.find("<a id=\"") {
        let after = &rest[start + "<a id=\"".len()..];
        let id_length = after
            .bytes()
            .take_while(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
            .count();
        let tail = &after[id_length..];
        output.push_str(&rest[..start]);
        if id_length > 0 && tail.starts_with("\"></a>") {
            rest = &tail["\"></a>".len()..];
        } else {
            output.push_str("<a id=\"");
            rest = after;
        }
    }
    output.push_str(rest);
    output
}

#[cfg(target_os = "linux")]
#[test]
fn strict_epub_negative_fixtures_fail_closed() {
    for (name, expected_code) in [
        ("missing-spine-chapter.epub", "incomplete_conversion"),
        ("malformed-spine-chapter.epub", "incomplete_conversion"),
        // From scripts/build-anydoc-hardening-corpus.py: content only
        // AnyDoc's own reading finds.
        ("encoded-chapter-href.epub", "incomplete_conversion"),
        ("linked-css-hidden.epub", "incomplete_conversion"),
        ("escaped-selector.epub", "incomplete_conversion"),
        ("list-text-outside-items.epub", "incomplete_conversion"),
        ("kindle-media-pair.epub", "incomplete_conversion"),
        ("minified-blocks.epub", "incomplete_conversion"),
        ("nav-spine-mismatch.epub", "incomplete_conversion"),
        ("missing-local-resource.epub", "incomplete_conversion"),
        ("external-reference.epub", "incomplete_conversion"),
        ("active-content.epub", "active_content_disabled"),
        ("hidden-content.epub", "incomplete_conversion"),
        ("encrypted.epub", "encrypted"),
        ("archive-amplification.epub", "resource_limit"),
    ] {
        let fixture = format!(
            "{}/../../test-corpus/epub/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        let error = run_document_tool(fixture, "epub-negative-integration-test");
        assert_eq!(error["code"], expected_code, "{name}");
        assert!(
            !error.to_string().contains(&["/", "Users", "/"].concat()),
            "{name} leaked a local path"
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn epub_text_readers_hide_and_anydoc_omits_converts() {
    let fixture = |name: &str| {
        format!(
            "{}/../../test-corpus/epub/{name}",
            env!("CARGO_MANIFEST_DIR")
        )
    };
    // A `display: none` rule AnyDoc applies too: the text is omitted, as a
    // reader omits it.
    let document = run_document_tool(fixture("display-none-omitted.epub"), "epub-omitted-test");
    assert_eq!(document["completeness"], "complete", "{document}");
    let markdown = document["markdown"].as_str().expect("markdown");
    assert!(markdown.contains("VISIBLE-CHAPTER"));
    assert!(!markdown.contains("OMITTED-LIKE-A-READER"));
    // Indented blocks AnyDoc walks inline are joined with a space, so no
    // words run together.
    let document = run_document_tool(fixture("indented-blocks.epub"), "epub-indented-test");
    assert_eq!(document["completeness"], "complete", "{document}");
    let markdown = document["markdown"].as_str().expect("markdown");
    assert!(markdown.contains("Balance due 1,250.00"), "{markdown}");
    // A web address written in the text loads nothing; it is sanitized.
    let document = run_document_tool(fixture("web-address-in-text.epub"), "epub-address-test");
    assert_eq!(document["completeness"], "complete", "{document}");
    assert!(!document.to_string().contains("example.com"));
    assert!(document["warnings"]
        .as_array()
        .expect("warning array")
        .iter()
        .any(|warning| warning["code"] == "sanitized_output"));
}

#[test]
fn csv_is_recognized_and_platform_memory_gated() {
    let fixture = format!(
        "{}/../../test-corpus/csv/public-bank-export.csv",
        env!("CARGO_MANIFEST_DIR")
    );
    let classification = run_classify_document(fixture, "csv-classification-test");
    assert_eq!(classification["kind"], "csv");
    assert_eq!(classification["variant"], "csv");
    assert_eq!(
        classification["enabled"],
        serde_json::Value::from(cfg!(target_os = "linux"))
    );
    assert_eq!(
        classification["capabilities"]["enabled"],
        serde_json::Value::from(cfg!(target_os = "linux"))
    );
    assert_eq!(
        classification["capabilities"]["provider"]["name"],
        "local-csv"
    );
    assert_eq!(
        classification["capabilities"]["provider"]["source"],
        "firecrawl/anydoc"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn document_to_markdown_converts_bounded_csv_fixture() {
    let fixture = format!(
        "{}/../../test-corpus/csv/public-bank-export.csv",
        env!("CARGO_MANIFEST_DIR")
    );
    let document = run_document_tool(fixture, "csv-integration-test");
    assert_eq!(document["kind"], "csv");
    assert_eq!(document["variant"], "csv");
    assert_eq!(document["schema_version"], 2);
    assert_eq!(document["provider"]["name"], "local-csv");
    assert_eq!(document["provider"]["version"], "0.1.0");
    assert_eq!(document["provider"]["source"], "firecrawl/anydoc");
    assert_eq!(document["completeness"], "complete");
    let markdown = document["markdown"].as_str().expect("Markdown output");
    assert!(markdown.contains("Customer receipt \\| January"));
    assert!(markdown.contains("1250.00"));
    assert!(markdown.contains("-275.50"));
    assert!(!markdown.contains("https://"));
    assert!(!markdown.contains("</"));
}

#[test]
fn document_to_markdown_converts_public_docx_fixture() {
    let fixture = format!(
        "{}/../../test-corpus/docx/public-fixture.docx",
        env!("CARGO_MANIFEST_DIR")
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_pdf-inspector-mcp"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("MCP binary must be available to integration tests");

    let initialize = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "integration-test", "version": "1" }
        }
    });
    let initialized = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    let call = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "document_to_markdown",
            "arguments": { "path": fixture }
        }
    });

    let stdin = child.stdin.as_mut().expect("child stdin");
    for request in [initialize, initialized, call] {
        writeln!(stdin, "{request}").expect("write JSON-RPC request");
    }
    drop(child.stdin.take());

    let mut output = String::new();
    child
        .stdout
        .take()
        .expect("child stdout")
        .read_to_string(&mut output)
        .expect("read JSON-RPC responses");
    let status = child.wait().expect("wait for MCP server");
    assert!(status.success());

    let response = output
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|value| value.get("id") == Some(&serde_json::Value::from(2)))
        .expect("document tool response");
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .expect("tool text result");
    let document: serde_json::Value = serde_json::from_str(text).expect("document JSON");
    assert_eq!(document["kind"], "docx");
    assert_eq!(document["schema_version"], 2);
    assert_eq!(document["provider"]["name"], "anydoc");
    assert_eq!(document["provider"]["version"], "0.2.4");
    assert_eq!(document["provider"]["source"], "firecrawl/anydoc");
    assert_eq!(document["completeness"], "complete");
    assert!(document["markdown"]
        .as_str()
        .expect("Markdown output")
        .contains("Public Fixture Heading"));
}

#[test]
fn document_to_markdown_converts_strict_public_xlsx_fixture() {
    let fixture = format!(
        "{}/../../test-corpus/xlsx/public-workpaper.xlsx",
        env!("CARGO_MANIFEST_DIR")
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_pdf-inspector-mcp"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("MCP binary must be available to integration tests");

    let initialize = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "xlsx-integration-test", "version": "1" }
        }
    });
    let initialized = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    let call = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "document_to_markdown",
            "arguments": { "path": fixture }
        }
    });

    let stdin = child.stdin.as_mut().expect("child stdin");
    for request in [initialize, initialized, call] {
        writeln!(stdin, "{request}").expect("write JSON-RPC request");
    }
    drop(child.stdin.take());

    let mut output = String::new();
    child
        .stdout
        .take()
        .expect("child stdout")
        .read_to_string(&mut output)
        .expect("read JSON-RPC responses");
    assert!(child.wait().expect("wait for MCP server").success());

    let response = output
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|value| value.get("id") == Some(&serde_json::Value::from(2)))
        .expect("XLSX document tool response");
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .expect("tool text result");
    let document: serde_json::Value = serde_json::from_str(text).expect("document JSON");
    assert_eq!(document["kind"], "xlsx");
    assert_eq!(document["variant"], "xlsx");
    assert_eq!(document["schema_version"], 2);
    assert_eq!(document["provider"]["name"], "anydoc");
    assert_eq!(document["provider"]["version"], "0.2.4");
    assert_eq!(document["completeness"], "complete");
    let markdown = document["markdown"].as_str().expect("Markdown output");
    assert!(markdown.contains("Inputs"));
    assert!(markdown.contains("Summary"));
    assert!(markdown.contains("150000"));
}

#[test]
fn malformed_public_xlsx_returns_stable_redacted_error() {
    let fixture = format!(
        "{}/../../test-corpus/xlsx/malformed.xlsx",
        env!("CARGO_MANIFEST_DIR")
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_pdf-inspector-mcp"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("MCP binary must be available to integration tests");
    let requests = [
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "negative-integration-test", "version": "1" }
            }
        }),
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }),
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "document_to_markdown",
                "arguments": { "path": fixture }
            }
        }),
    ];
    let stdin = child.stdin.as_mut().expect("child stdin");
    for request in requests {
        writeln!(stdin, "{request}").expect("write JSON-RPC request");
    }
    drop(child.stdin.take());
    let mut output = String::new();
    child
        .stdout
        .take()
        .expect("child stdout")
        .read_to_string(&mut output)
        .expect("read JSON-RPC responses");
    assert!(child.wait().expect("wait for MCP server").success());
    let response = output
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|value| value.get("id") == Some(&serde_json::Value::from(2)))
        .expect("negative document tool response");
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .expect("tool text result");
    let error: serde_json::Value = serde_json::from_str(text).expect("error JSON");
    assert_eq!(error["code"], "malformed");
    assert_eq!(
        error["error"],
        "document is malformed or missing required content"
    );
    assert!(!text.contains(&["/", "Users", "/"].concat()));
}

#[test]
fn strict_xlsx_negative_fixtures_fail_closed() {
    let run = |fixture: &str| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_pdf-inspector-mcp"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("MCP binary must be available to integration tests");
        let initialize = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "xlsx-negative-integration-test", "version": "1" }
            }
        });
        let initialized = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        });
        let call = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "document_to_markdown",
                "arguments": { "path": fixture }
            }
        });
        let stdin = child.stdin.as_mut().expect("child stdin");
        for request in [initialize, initialized, call] {
            writeln!(stdin, "{request}").expect("write JSON-RPC request");
        }
        drop(child.stdin.take());
        let mut output = String::new();
        child
            .stdout
            .take()
            .expect("child stdout")
            .read_to_string(&mut output)
            .expect("read JSON-RPC responses");
        assert!(child.wait().expect("wait for MCP server").success());
        let response = output
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find(|value| value.get("id") == Some(&serde_json::Value::from(2)))
            .expect("negative document tool response");
        let text = response["result"]["content"][0]["text"]
            .as_str()
            .expect("tool text result");
        serde_json::from_str::<serde_json::Value>(text).expect("error JSON")
    };

    for (name, expected_code) in [
        ("hidden-content.xlsx", "incomplete_conversion"),
        ("uncached-formula.xlsx", "incomplete_conversion"),
    ] {
        let fixture = format!(
            "{}/../../test-corpus/xlsx/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        let error = run(&fixture);
        assert_eq!(error["code"], expected_code);
        assert_eq!(
            error["error"],
            "document conversion omitted required content"
        );
        assert!(!error.to_string().contains(&["/", "Users", "/"].concat()));
    }
}

#[test]
fn document_to_markdown_converts_public_pptx_fixture() {
    let fixture = format!(
        "{}/../../test-corpus/pptx/public-walkthrough.pptx",
        env!("CARGO_MANIFEST_DIR")
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_pdf-inspector-mcp"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("MCP binary must be available to integration tests");

    let requests = [
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "pptx-integration-test", "version": "1" }
            }
        }),
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }),
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "document_to_markdown",
                "arguments": { "path": fixture }
            }
        }),
    ];
    let stdin = child.stdin.as_mut().expect("child stdin");
    for request in requests {
        writeln!(stdin, "{request}").expect("write JSON-RPC request");
    }
    drop(child.stdin.take());

    let mut output = String::new();
    child
        .stdout
        .take()
        .expect("child stdout")
        .read_to_string(&mut output)
        .expect("read JSON-RPC responses");
    assert!(child.wait().expect("wait for MCP server").success());

    let response = output
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|value| value.get("id") == Some(&serde_json::Value::from(2)))
        .expect("PPTX document tool response");
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .expect("tool text result");
    let document: serde_json::Value = serde_json::from_str(text).expect("document JSON");
    assert_eq!(document["kind"], "pptx");
    assert_eq!(document["variant"], "pptx");
    assert_eq!(document["schema_version"], 2);
    assert_eq!(document["provider"]["name"], "anydoc");
    assert_eq!(document["provider"]["version"], "0.2.4");
    assert_eq!(document["completeness"], "complete");
    let markdown = document["markdown"].as_str().expect("Markdown output");
    assert!(markdown.contains("Inherited Title Slide"));
    assert!(markdown.contains("Roman three bold via master"));
}

#[test]
fn document_to_markdown_converts_strict_public_ods_fixture() {
    let fixture = format!(
        "{}/../../test-corpus/ods/public-workpaper.ods",
        env!("CARGO_MANIFEST_DIR")
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_pdf-inspector-mcp"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("MCP binary must be available to integration tests");

    let requests = [
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "ods-integration-test", "version": "1" }
            }
        }),
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }),
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "document_to_markdown",
                "arguments": { "path": fixture }
            }
        }),
    ];
    let stdin = child.stdin.as_mut().expect("child stdin");
    for request in requests {
        writeln!(stdin, "{request}").expect("write JSON-RPC request");
    }
    drop(child.stdin.take());

    let mut output = String::new();
    child
        .stdout
        .take()
        .expect("child stdout")
        .read_to_string(&mut output)
        .expect("read JSON-RPC responses");
    assert!(child.wait().expect("wait for MCP server").success());

    let response = output
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|value| value.get("id") == Some(&serde_json::Value::from(2)))
        .expect("ODS document tool response");
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .expect("tool text result");
    let document: serde_json::Value = serde_json::from_str(text).expect("document JSON");
    assert_eq!(document["kind"], "ods");
    assert_eq!(document["variant"], "ods");
    assert_eq!(document["schema_version"], 2);
    assert_eq!(document["provider"]["name"], "anydoc");
    assert_eq!(document["provider"]["version"], "0.2.4");
    assert_eq!(document["completeness"], "complete");
    let markdown = document["markdown"].as_str().expect("Markdown output");
    assert!(markdown.contains("Values"));
    assert!(markdown.contains("Merged Grid"));
    assert!(markdown.contains("15.5%"));
    assert!(markdown.contains("Span two"));
}

#[test]
fn strict_ods_resource_limit_fixtures_fail_closed() {
    for name in [
        "repeat-amplification.ods",
        "huge-repeat.ods",
        "huge-span.ods",
    ] {
        let fixture = format!(
            "{}/../../test-corpus/ods/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        let mut child = Command::new(env!("CARGO_BIN_EXE_pdf-inspector-mcp"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("MCP binary must be available to integration tests");
        let requests = [
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": { "name": "ods-resource-test", "version": "1" }
                }
            }),
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            }),
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "document_to_markdown",
                    "arguments": { "path": fixture }
                }
            }),
        ];
        let stdin = child.stdin.as_mut().expect("child stdin");
        for request in requests {
            writeln!(stdin, "{request}").expect("write JSON-RPC request");
        }
        drop(child.stdin.take());
        let mut output = String::new();
        child
            .stdout
            .take()
            .expect("child stdout")
            .read_to_string(&mut output)
            .expect("read JSON-RPC responses");
        assert!(child.wait().expect("wait for MCP server").success());
        let response = output
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find(|value| value.get("id") == Some(&serde_json::Value::from(2)))
            .expect("ODS resource-limit response");
        let text = response["result"]["content"][0]["text"]
            .as_str()
            .expect("tool text result");
        let error: serde_json::Value = serde_json::from_str(text).expect("error JSON");
        assert_eq!(error["code"], "resource_limit", "{name}");
        assert_eq!(
            error["error"], "document exceeded a parser resource limit",
            "{name}"
        );
        assert!(!text.contains(&["/", "Users", "/"].concat()));
    }
}

#[cfg(target_os = "linux")]
#[test]
fn document_to_markdown_converts_strict_public_odp_fixture() {
    let fixture = format!(
        "{}/../../test-corpus/odp/public-presentation.odp",
        env!("CARGO_MANIFEST_DIR")
    );
    let document = run_document_tool(fixture, "odp-integration-test");
    assert_eq!(document["kind"], "odp");
    assert_eq!(document["variant"], "odp");
    assert_eq!(document["schema_version"], 2);
    assert_eq!(document["provider"]["name"], "anydoc");
    assert_eq!(document["provider"]["version"], "0.2.4");
    assert_eq!(document["provider"]["source"], "firecrawl/anydoc");
    assert_eq!(document["completeness"], "complete");
    let markdown = document["markdown"].as_str().expect("Markdown output");
    for marker in [
        "Deck Title Slide",
        "Top level point",
        "Nested detail",
        "Speaker note for the intro slide.",
        "Numbers Slide",
        "North",
        "42",
        "Inside a group shape.",
        "Second slide notes mention the table.",
    ] {
        assert!(markdown.contains(marker), "missing ODP marker {marker:?}");
    }
    assert!(!markdown.contains("https://"));
    assert!(!markdown.contains("</"));
    assert!(!markdown.contains(&["/", "Users", "/"].concat()));
}

#[cfg(target_os = "linux")]
#[test]
fn strict_odp_negative_fixtures_fail_closed() {
    for (name, expected_code) in [
        ("active-content.odp", "active_content_disabled"),
        ("archive-amplification.odp", "resource_limit"),
        ("encrypted.odp", "encrypted"),
        ("external-reference.odp", "incomplete_conversion"),
        ("hidden-content.odp", "incomplete_conversion"),
        ("malformed-content.odp", "malformed"),
        ("missing-asset.odp", "incomplete_conversion"),
        ("wrong-mimetype.odp", "malformed"),
        ("linked-frame.odp", "incomplete_conversion"),
    ] {
        let fixture = format!(
            "{}/../../test-corpus/odp/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        let error = run_document_tool(fixture, "odp-negative-integration-test");
        assert_eq!(error["code"], expected_code, "{name}");
        assert!(
            !error.to_string().contains(&["/", "Users", "/"].concat()),
            "{name} leaked a local path"
        );
    }
}

#[test]
fn document_to_markdown_converts_strict_public_odt_fixture() {
    let fixture = format!(
        "{}/../../test-corpus/odt/minimal.odt",
        env!("CARGO_MANIFEST_DIR")
    );
    let document = run_document_tool(fixture, "odt-integration-test");
    assert_eq!(document["kind"], "odt");
    assert_eq!(document["variant"], "odt");
    assert_eq!(document["schema_version"], 2);
    assert_eq!(document["provider"]["name"], "anydoc");
    assert_eq!(document["provider"]["version"], "0.2.4");
    assert_eq!(document["provider"]["source"], "firecrawl/anydoc");
    assert_eq!(document["completeness"], "complete");
    let markdown = document["markdown"].as_str().expect("Markdown output");
    assert!(markdown.contains("Minimal public ODT."));
    assert!(!markdown.contains("https://"));
    assert!(!markdown.contains("</"));
    assert!(!markdown.contains(&["/", "Users", "/"].concat()));
}

#[test]
fn strict_odt_negative_fixtures_fail_closed() {
    for (name, expected_code) in [
        ("public-research-memo.odt", "incomplete_conversion"),
        ("hidden-or-tracked.odt", "incomplete_conversion"),
        ("external-reference.odt", "incomplete_conversion"),
        ("active-content.odt", "active_content_disabled"),
        ("missing-image.odt", "incomplete_conversion"),
        ("malformed-content.odt", "malformed"),
        ("encrypted.odt", "encrypted"),
        ("wrong-mimetype.odt", "malformed"),
        ("missing-content.odt", "malformed"),
    ] {
        let fixture = format!(
            "{}/../../test-corpus/odt/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        let error = run_document_tool(fixture, "odt-negative-integration-test");
        assert_eq!(error["code"], expected_code, "fixture {name}");
        assert!(!error.to_string().contains(&["/", "Users", "/"].concat()));
    }
}

#[test]
fn enabled_lanes_reject_adversarial_public_fixtures() {
    for (relative_path, expected_code) in [
        ("docx/active-content.docx", "active_content_disabled"),
        ("docx/malformed-document.docx", "malformed"),
        ("pptx/missing-slide.pptx", "incomplete_conversion"),
        ("pptx/active-content.pptx", "active_content_disabled"),
        ("xlsx/external-link.xlsx", "incomplete_conversion"),
        ("xlsx/malformed-sheet.xlsx", "incomplete_conversion"),
        ("ods/external-reference.ods", "incomplete_conversion"),
        ("ods/active-content.ods", "active_content_disabled"),
        ("ods/missing-table.ods", "incomplete_conversion"),
        // Pinned AnyDoc drops these silently or amplifies them (see
        // scripts/build-anydoc-hardening-corpus.py).
        ("docx/symbol-checkbox.docx", "incomplete_conversion"),
        ("docx/legacy-form-checkbox.docx", "incomplete_conversion"),
        ("docx/utf16-footnote-symbol.docx", "incomplete_conversion"),
        ("docx/ruby-text.docx", "incomplete_conversion"),
        ("docx/alt-chunk.docx", "incomplete_conversion"),
        ("pptx/fragment-slide-target.pptx", "incomplete_conversion"),
        (
            "pptx/case-variant-presentation-rels.pptx",
            "incomplete_conversion",
        ),
        ("docx/namespace-shadowed-main.docx", "malformed"),
        ("xlsx/binary-workbook.xlsx", "unsupported"),
        ("xlsx/oversized-number-format.xlsx", "resource_limit"),
        // Review round four: parts, cells, and values found where AnyDoc
        // finds them.
        (
            "docx/cell-in-compatibility-block.docx",
            "incomplete_conversion",
        ),
        ("pptx/relocated-notes.pptx", "incomplete_conversion"),
        ("xlsx/xlsb-fallback-decoy.xlsx", "malformed"),
        (
            "xlsx/unrendered-formula-cache.xlsx",
            "incomplete_conversion",
        ),
        ("odt/page-anchored-frame.odt", "incomplete_conversion"),
        ("ods/untyped-formula-value.ods", "incomplete_conversion"),
        // Number formats AnyDoc renders differently, and drawing text it
        // never reads.
        ("xlsx/negative-sign-by-colour.xlsx", "incomplete_conversion"),
        ("xlsx/format-hidden-value.xlsx", "incomplete_conversion"),
        ("xlsx/locale-date-format.xlsx", "incomplete_conversion"),
        ("xlsx/drawing-text-box.xlsx", "incomplete_conversion"),
        ("ods/negative-sign-by-colour.ods", "incomplete_conversion"),
        ("ods/format-hidden-value.ods", "incomplete_conversion"),
        ("ods/cell-anchored-text-box.ods", "incomplete_conversion"),
    ] {
        let fixture = format!(
            "{}/../../test-corpus/{relative_path}",
            env!("CARGO_MANIFEST_DIR")
        );
        let error = run_document_tool(fixture, "adversarial-integration-test");
        assert_eq!(error["code"], expected_code, "fixture {relative_path}");
        assert!(!error.to_string().contains(&["/", "Users", "/"].concat()));
    }
}

#[test]
fn docx_external_relationship_is_contained_and_reported() {
    let fixture = format!(
        "{}/../../test-corpus/docx/external-relationship.docx",
        env!("CARGO_MANIFEST_DIR")
    );
    let document = run_document_tool(fixture, "docx-external-integration-test");
    assert_eq!(document["kind"], "docx");
    assert_eq!(document["completeness"], "complete");
    assert!(document["warnings"]
        .as_array()
        .expect("warning array")
        .iter()
        .any(|warning| warning["code"] == "external_relationships_blocked"));
    let text = document.to_string();
    assert!(!text.contains("https://example.invalid"));
    assert!(!text.contains(&["/", "Users", "/"].concat()));
}

#[test]
fn docx_hidden_text_is_converted_and_disclosed() {
    // Hidden directly on a run, and through a style in a UTF-16 styles part.
    for (fixture, marker) in [
        ("hidden-text.docx", "HIDDEN-RUN"),
        ("utf16-hidden-style.docx", "STYLED-HIDDEN"),
    ] {
        let path = format!(
            "{}/../../test-corpus/docx/{fixture}",
            env!("CARGO_MANIFEST_DIR")
        );
        let document = run_document_tool(path, "docx-hidden-integration-test");
        assert_eq!(document["completeness"], "complete", "{fixture}");
        let markdown = document["markdown"].as_str().expect("markdown");
        assert!(
            markdown.contains(marker) && markdown.contains("HARDENING-END"),
            "{fixture}"
        );
        assert!(
            document["warnings"]
                .as_array()
                .expect("warning array")
                .iter()
                .any(|warning| warning["code"] == "hidden_content_preserved"),
            "{fixture}"
        );
    }
}

#[test]
fn pptx_section_lists_are_not_slides() {
    let fixture = format!(
        "{}/../../test-corpus/pptx/section-list.pptx",
        env!("CARGO_MANIFEST_DIR")
    );
    let document = run_document_tool(fixture, "pptx-sections-integration-test");
    assert_eq!(document["completeness"], "complete", "{document}");
    assert!(document["markdown"]
        .as_str()
        .is_some_and(|markdown| markdown.contains("SECTION-SLIDE")));
}

#[test]
fn xlsx_parenthesized_negatives_keep_their_sign() {
    let fixture = format!(
        "{}/../../test-corpus/xlsx/red-parenthesized-negative.xlsx",
        env!("CARGO_MANIFEST_DIR")
    );
    let document = run_document_tool(fixture, "xlsx-negative-integration-test");
    assert_eq!(document["completeness"], "complete", "{document}");
    assert!(document["markdown"]
        .as_str()
        .is_some_and(|markdown| markdown.contains("(25,000)")));
}

#[test]
fn docx_list_numbers_word_continues_are_reported_as_partial() {
    let fixture = format!(
        "{}/../../test-corpus/docx/shared-list-definition.docx",
        env!("CARGO_MANIFEST_DIR")
    );
    let document = run_document_tool(fixture, "docx-numbering-integration-test");
    assert_eq!(document["completeness"], "partial", "{document}");
    assert!(document["markdown"]
        .as_str()
        .is_some_and(|markdown| markdown.contains("LIST-CONTINUES")));
    assert!(document["warnings"]
        .as_array()
        .expect("warning array")
        .iter()
        .any(|warning| warning["code"] == "list_numbering_differs"));
}

#[test]
fn docx_dropped_hyphens_are_reported_as_partial() {
    let fixture = format!(
        "{}/../../test-corpus/docx/non-breaking-hyphen.docx",
        env!("CARGO_MANIFEST_DIR")
    );
    let document = run_document_tool(fixture, "docx-hyphen-integration-test");
    assert_eq!(document["completeness"], "partial");
    let markdown = document["markdown"].as_str().expect("markdown");
    assert!(
        markdown.contains("HYPHEN-MARKER") && markdown.contains("HARDENING-END"),
        "{markdown}"
    );
    assert!(document["warnings"]
        .as_array()
        .expect("warning array")
        .iter()
        .any(|warning| warning["code"] == "characters_omitted"));
}

#[test]
fn docx_external_link_destinations_are_removed() {
    let fixture = format!(
        "{}/../../test-corpus/docx/external-link-schemes.docx",
        env!("CARGO_MANIFEST_DIR")
    );
    let document = run_document_tool(fixture, "docx-link-schemes-integration-test");
    assert_eq!(document["completeness"], "complete");
    let markdown = document["markdown"].as_str().expect("markdown");
    for index in 0..5 {
        assert!(markdown.contains(&format!("LINK-{index}")), "{markdown}");
    }
    for leaked in [
        "mailto:",
        "file:",
        "data:",
        "tel:",
        "fileserver",
        "etc/passwd",
    ] {
        assert!(!markdown.contains(leaked), "{leaked} survived: {markdown}");
    }
    let codes: Vec<_> = document["warnings"]
        .as_array()
        .expect("warning array")
        .iter()
        .map(|warning| warning["code"].as_str().unwrap_or_default())
        .collect();
    assert!(codes.contains(&"external_relationships_blocked"));
    assert!(codes.contains(&"sanitized_output"));
}

/// A worker that reads its request and answers with a fixed response frame.
#[cfg(unix)]
fn answering_worker(directory: &Path, response: &serde_json::Value) -> std::path::PathBuf {
    let payload = response.to_string().into_bytes();
    let mut frame = b"ADW1".to_vec();
    frame.extend_from_slice(&[2, 0, 0, 0]);
    frame.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    frame.extend_from_slice(&payload);
    let frame_path = directory.join("response.frame");
    std::fs::write(&frame_path, frame).expect("write worker frame");
    let worker = directory.join("answering-worker.sh");
    std::fs::write(
        &worker,
        format!(
            "#!/bin/sh\ncat > /dev/null\ncat '{}'\n",
            frame_path.to_string_lossy().replace('\'', "'\\''")
        ),
    )
    .expect("write answering worker");
    let mut permissions = std::fs::metadata(&worker)
        .expect("answering worker metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&worker, permissions).expect("make answering worker executable");
    worker
}

#[cfg(unix)]
#[test]
fn document_warnings_come_from_the_workers_preflight() {
    let fixture = || {
        format!(
            "{}/../../test-corpus/docx/public-fixture.docx",
            env!("CARGO_MANIFEST_DIR")
        )
    };
    let answered = |response: serde_json::Value| {
        let temporary = tempfile::tempdir().expect("temporary worker directory");
        let worker = answering_worker(temporary.path(), &response);
        run_document_tool_with_worker(fixture(), "worker-preflight-test", Some(&worker))
    };
    // The package preflight runs in the worker, under its deadline and
    // memory ceiling; the server discloses what it found.
    let document = answered(serde_json::json!({
        "markdown": "PREFLIGHT-MARKER",
        "preflight": { "list_numbering_differs": true, "hidden_content": true }
    }));
    assert_eq!(document["completeness"], "partial", "{document}");
    assert_eq!(document["markdown"], "PREFLIGHT-MARKER");
    let codes: Vec<_> = document["warnings"]
        .as_array()
        .expect("warning array")
        .iter()
        .map(|warning| warning["code"].as_str().unwrap_or_default())
        .collect();
    assert!(codes.contains(&"list_numbering_differs"), "{document}");
    assert!(codes.contains(&"hidden_content_preserved"), "{document}");
    // What the worker's preflight rejects is refused, and Markdown without
    // the preflight's findings is not returned.
    let document = answered(serde_json::json!({
        "markdown": "PREFLIGHT-MARKER",
        "preflight": { "unsupported_content": true }
    }));
    assert_eq!(document["code"], "incomplete_conversion", "{document}");
    let document = answered(serde_json::json!({ "markdown": "PREFLIGHT-MARKER" }));
    assert_eq!(document["code"], "worker_protocol", "{document}");
}
