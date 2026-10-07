# AnyDoc strict EPUB qualification handoff

- **Date:** 2026-08-28
- **Correlation:** `anydoc-iteration-20260828-next`
- **Status:** Complete for this implementation slice; changes remain uncommitted and unpushed on `agent/codex-align-firecrawl-20260828`.
- **Upstream authorities:** Firecrawl AnyDoc `v0.2.4` (`42bf1c5e`) and refreshed `main` (`261fc257`); Firecrawl pdf-inspector released `1.17.0` with refreshed `main` (`23cf1ad7`). AnyDoc `main` is README-only after the release in the reviewed range, so no dependency update or source import was needed.

## Implemented

- Added strict EPUB 3 classification and conversion through the existing provider-neutral worker protocol as format code 8.
- Added exact OCF identity, one-rootfile OPF resolution, all-spine and navigation agreement, XHTML/body validation, local-resource checks, active/external/hidden/encrypted rejection, and archive/resource limits.
- Kept EPUB and ODP/CSV public conversion Linux-memory-gated; platforms without an enforceable worker address-space ceiling classify the format but report it disabled.
- Added ten deterministic, synthetic, non-PII EPUB qualification fixtures and generator-backed hashes; preserved the earlier legacy corpus for regression coverage.
- Added real-parser chapter-order and omission oracles, MCP route coverage, and Linux-gated hostile-fixture expectations. PDFs remain on the dedicated pdf-inspector path.

## Verification

- `cargo fmt --all -- --check`, `git diff --check`, locked metadata, workspace check, full workspace tests, release build, and strict Clippy passed.
- Host tests: 97 passed (70 skillkit unit, 6 skillkit integration, 3 MCP unit, 18 MCP integration).
- Linux target check and strict Clippy passed; Linux route tests are compiled but not executable from this Darwin host.
- Release resource evidence passed for DOCX/PPTX/XLSX/ODS/ODT/ODP/EPUB; EPUB observed at 8,749,056 peak RSS bytes, 565 response bytes, and 2 ms wall time on the recorded Darwin/arm64 host.
- Mirrored AnyDoc abuse evaluator passed 7/7 expected `resource_limit` cases.
- Public hygiene passed; `cargo audit` found no vulnerabilities and retained the known unmaintained `ttf-parser` and yanked `chacha20` notices. `cargo-deny` is not installed locally.
- Two consecutive EPUB corpus generations produced identical SHA-256 outputs.

## Specialist and remaining gates

Fresh SOL and MiniMax sidecars returned no usable memo (SOL timed out; MiniMax transport closed); no specialist approval is inferred. Filesystem isolation, non-Linux memory containment, broader hostile-input coverage, and full Linux runtime execution remain promotion/CI evidence dependencies. EPUB 2, layout/source-coordinate fidelity, embedded-object execution, and external fetching are out of contract.

## Rollback boundary

Review or revert the EPUB-specific worker code 8, routing/preflight changes, fixture corpus/generator/tests, and synchronized documentation as one bounded change. Do not remove the sibling upstream mirrors or unrelated dirty work.
