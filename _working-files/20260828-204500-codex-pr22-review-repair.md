# AnyDoc PR #22 review repair

- **Date:** 2026-08-28
- **Status:** Complete; focused changes are committed and pushed on `agent/codex-align-firecrawl-20260828`.
- **PR:** https://github.com/Jimthetaxguy/anydoc-enhanced/pull/22

## Changes

- Reject EPUB 2 packages and EPUB 3 packages without exactly one navigation document before AnyDoc conversion.
- Parse OOXML relationship XML attributes instead of relying on substring matches.
- Bound serialized worker frames for worst-case JSON escaping and preserve a typed `output_too_large` response.
- Avoid constructing Tokio in synchronous worker mode before Linux `RLIMIT_AS` is applied.
- Fix the Rust 1.98 Clippy byte-string lint and make the EPUB network canary expectation platform-aware.

## Verification

- `cargo fmt --all -- --check` passed.
- `cargo check --workspace --locked` passed.
- `cargo test --workspace --locked` passed: 3 MCP unit, 18 MCP containment, 74 skillkit unit, 6 skillkit integration.
- `cargo clippy --workspace --all-targets --locked -- -D warnings` passed.
- `cargo build --workspace --release --locked` passed.
- Linux-target locked check and strict Clippy passed.
- Hosted exact-head verification passed on run `33225018140`: Ubuntu stable tests, Ubuntu worker containment, macOS worker containment, cargo-deny, and hygiene/gitleaks all passed.

## Scope

No upstream source was vendored, no dependency revision was changed, and no merge was performed.
