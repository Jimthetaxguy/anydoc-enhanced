//! Record the build that produced this binary.
//!
//! The skill scripts print this identity with every result. The values are
//! fixed at compile time so a copied binary still reports the crate version,
//! the pinned parser versions, the target triple, and the git commit.

use std::path::Path;
use std::process::Command;

fn main() {
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=PDF_INSPECTOR_MCP_TARGET={target}");

    let commit = git_commit();
    println!("cargo:rustc-env=PDF_INSPECTOR_MCP_GIT_COMMIT={commit}");
    println!("cargo:rerun-if-env-changed=ANYDOC_ENHANCED_GIT_COMMIT");

    if let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") {
        let head = Path::new(&manifest).join("../../.git/HEAD");
        if head.is_file() {
            println!("cargo:rerun-if-changed={}", head.display());
        }
    }
}

fn git_commit() -> String {
    if let Ok(value) = std::env::var("ANYDOC_ENHANCED_GIT_COMMIT") {
        let value = value.trim();
        if !value.is_empty() && !value.contains(['\n', '\r']) {
            return value.to_string();
        }
    }

    let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") else {
        return "unknown".to_string();
    };
    let Ok(output) = Command::new("git")
        .args(["-C", &manifest, "rev-parse", "HEAD"])
        .output()
    else {
        return "unknown".to_string();
    };
    if !output.status.success() {
        return "unknown".to_string();
    }
    let Ok(text) = String::from_utf8(output.stdout) else {
        return "unknown".to_string();
    };
    let text = text.trim();
    if text.is_empty() || text.contains(['\n', '\r']) {
        return "unknown".to_string();
    }
    text.to_string()
}
