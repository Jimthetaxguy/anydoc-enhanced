//! `--provenance` identifies the binary that serves MCP and the worker.

use std::process::Command;

#[test]
fn provenance_flag_reports_the_pinned_build() {
    let output = Command::new(env!("CARGO_BIN_EXE_pdf-inspector-mcp"))
        .arg("--provenance")
        .output()
        .expect("run --provenance");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON");
    assert_eq!(value["server"], "pdf-inspector-mcp");
    assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(value["pdf_inspector"], "1.25.0");
    assert_eq!(value["anydoc"], "0.2.4");
    let target = value["target"].as_str().expect("target");
    assert!(!target.is_empty() && target != "unknown");
    let commit = value["git_commit"].as_str().expect("git_commit");
    assert!(
        commit == "unknown" || commit.len() >= 7,
        "unexpected git commit"
    );
    assert!(
        !output.stdout.windows(6).any(|bytes| bytes == b"/home/"),
        "provenance must not embed a home path"
    );
}
