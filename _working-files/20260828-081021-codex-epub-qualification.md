# AnyDoc EPUB qualification corpus

- **Status:** complete
- **Date:** 2026-08-28
- **Branch:** agent/codex-align-firecrawl-20260828
- **Base:** f5c4859cf952a8b74d1bb0b83bc97fc9986c1fae
- **Scope:** Continue the Firecrawl-aligned AnyDoc implementation with a test-only EPUB 3 qualification boundary; preserve the disabled public route.

## Work completed in this slice

- Added `scripts/build-epub-corpus.py` with fixed ZIP timestamps/permissions and ten synthetic, non-PII fixtures.
- Added `test-corpus/epub/oracle.json` and `test-corpus/epub/README.md` with expected dispositions, structural markers, provenance, and SHA-256 values.
- Added EPUB navigation-to-spine order validation and exact-once marker ordering to the skillkit tests.
- Added a production MCP test proving EPUB is recognized but remains disabled and returns the stable `unsupported` error.
- Kept EPUB out of the ADW1 worker protocol and capability enablement.

## Specialist input

- SOL selected a fixture-backed, containment-only EPUB 3 qualification slice and required spine order, nav agreement, missing/malformed, external, active, hidden, encrypted, and archive-amplification cases.
- MiniMax transport was unavailable on two fresh attempts; no new MiniMax result was fabricated. A prior MiniMax review remains recorded in the activity ledger.

## Final verification

- EPUB semantic oracle, README hash manifest, first-entry `mimetype`, and archive invariants: pass for 10 qualification fixtures; 14 EPUB packages are tracked in total.
- `cargo fmt --all -- --check`: pass.
- `cargo metadata --locked --format-version 1`: pass.
- `cargo check --workspace --locked`: pass.
- `cargo test --workspace --locked`: 81 passed, 0 failed, 0 ignored.
- `cargo clippy --workspace --all-targets --locked -- -D warnings`: pass.
- `cargo build --workspace --release --locked`: pass.
- `cargo test --release -p pdf-inspector-mcp --test document_tool --locked`: 15 passed, 0 failed.
- `cargo audit`: only the documented unmaintained `ttf-parser` and yanked `chacha20` warnings; no vulnerability failure.
- `bash scripts/check-public-hygiene.sh` and `git diff --check`: pass.

## Explicit open gates

Public EPUB enablement still requires chapter-order output from a real parser path, no-network evidence, hostile-input resource measurements, and non-Linux containment evidence. Existing enabled lanes retain their cross-host gates. No upstream dependency update or source vendoring is justified.
