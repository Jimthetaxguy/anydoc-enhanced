# AnyDoc network-containment continuation

**Status:** complete
**Date:** 2026-08-28
**Branch:** `agent/codex-align-firecrawl-20260828`

## Scope

Implemented and verified the platform-scoped worker network boundary after the
EPUB containment-only corpus slice. The macOS worker now launches through the
built-in `sandbox-exec -n no-network` profile. Linux retains the worker
address-space ceiling and seccomp network-denial filter; unsupported targets
advertise no enabled worker lanes and return `worker_unavailable`.

## Evidence

- Darwin focused worker canary: passed.
- `cargo test --workspace --locked`: 85 passed, 0 failed.
- Release MCP document-tool tests: 17 passed, 0 failed.
- `cargo fmt --all -- --check`: passed.
- Locked metadata, workspace check, and warnings-as-errors Clippy: passed.
- Linux `x86_64-unknown-linux-gnu` check and Clippy: passed.
- Release workspace build: passed.
- Public-hygiene and `git diff --check`: passed.
- `cargo audit`: no vulnerability findings; allowed warnings remain for
  unmaintained `ttf-parser 0.25.1` and yanked `chacha20 0.10.0`.
- `cargo-deny` is not installed locally; the existing CI policy remains the
  external advisory/license/source gate.

## Remaining gates

EPUB remains disabled. Before enabling it, add hostile-resource measurements,
filesystem-isolation evidence, non-Linux memory containment, and broader
cross-host runtime evidence. Do not vendor Firecrawl source, broaden the
allowlist, or merge the dirty branch without explicit review and commit.
