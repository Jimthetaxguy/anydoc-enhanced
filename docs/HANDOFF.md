# anydoc-enhanced — public handoff

**Last reconciled:** 2026-08-28
**Repository:** <https://github.com/Jimthetaxguy/anydoc-enhanced>
**Status:** PDF MCP baseline is aligned to Firecrawl pdf-inspector 1.24.0; generic document tools are live for bounded DOCX, strict PPTX, strict XLSX, strict ODS, strict ODT, strict ODP, Linux-memory-gated strict EPUB, and Linux-memory-gated strict CSV conversion.

This is the public, repository-relative entry point for future work. Do not add
home-directory paths, private corpus locations, credentials, internal agent
configuration, or identifying source-document details.

## Current system

The workspace exposes 16 MCP tools over stdio:

- Six generic PDF tools: classify, Markdown, layout, batch, and two region
  extractors.
- Three generic document tools: capability discovery, classification, and bounded DOCX/PPTX/XLSX/ODS/ODT/ODP/EPUB/CSV-to-Markdown conversion (ODP, EPUB, and CSV are enabled only on Linux hosts with the address-space ceiling).
- Three domain parsers: tax-form identification, IRC section parsing, and SEC
  filing splitting.
- Four deterministic synthetic tax-review demo tools.

The dependency direction is:

```text
pdf-inspector-mcp
        |
pdf-inspector-skillkit
        |
firecrawl/pdf-inspector 1.24.0 (released, exact Cargo lock resolution)
        |
        +-- anydoc 0.2.4 via bounded DOCX/PPTX/XLSX/ODS/ODT/ODP/EPUB worker; local strict CSV adapter shares the worker boundary
```

`pdf-inspector-skillkit` is the only crate that may call parser libraries.
MCP handlers and domain modules must depend on the skillkit boundary.

## Current source map

| Path | Responsibility |
|---|---|
| `crates/pdf-inspector-skillkit/src/lib.rs` | PDF facade, document contract, validation, and serialized result types |
| `crates/pdf-inspector-skillkit/src/domain/` | Tax, IRC, SEC, and synthetic review logic |
| `crates/pdf-inspector-mcp/src/main.rs` | MCP schemas, worker mode, tool registration, dispatch, and timeout response handling |
| `scripts/check-public-hygiene.sh` | Candidate-text obvious-identifier heuristic used locally and in CI |
| test-corpus/README.md | Public PDF/PPTX/DOCX/XLSX/ODS/ODT/ODP/CSV/EPUB fixture provenance and contributor gate |
| `docs/dependency-pr-review-2026-08-22.md` | Live review of dependency PRs #14–#18 |
| `docs/anydoc-integration-plan.md` | Authoritative dependency-ordered AnyDoc plan |
| `CONTEXT.md` | Stable project vocabulary and boundaries |

## Verified constraints

- Existing text-PDF calls are local and do not require a hosted API.
  Image-only PDFs report OCR requirements, while mixed PDFs can produce
  partial extraction with page-level OCR diagnostics; no OCR engine is
  currently shipped.
- Input paths are canonicalized and capped at 50 MiB.
- PDF MCP handlers return after a 30-second Tokio timeout; the generic DOCX/PPTX/XLSX/ODS/ODT/ODP/EPUB path
  additionally uses a 15-second killable child worker with input/output caps and a
  two-worker in-flight semaphore; Unix process-group cleanup on timeout, protocol
  error, output overflow, and caller cancellation, Linux address-space
  plus seccomp network denial, and Darwin named `no-network` profile are active.
  Filesystem isolation and non-Linux memory containment remain follow-up gates
  before broader hostile-format enablement.
- AnyDoc `v0.2.4` is a native Rust library, MIT licensed, and is resolved
  alongside the workspace `pdf-inspector 1.24.0` release. Its typed `NeedsOcr`
  result remains available for future PDF-specific evaluation and is not used to bypass the
  dedicated PDF facade.
- This workspace now uses released `pdf-inspector 1.24.0` with `lopdf 0.45.0`.
  The existing 13-tool PDF surface compiles and passes its regression suite.
  The AnyDoc dependency, provider contract, worker, DOCX happy path, strict PPTX path, strict XLSX path, strict ODS path, strict ODT path, strict ODP path, and strict EPUB path are implemented; the local strict CSV adapter is implemented through worker code 6 on Linux.
- AnyDoc CSV and RTF parsing remain unexposed because upstream issue #104 documents materialization and memory-exhaustion risk; the local strict CSV adapter is separate and Linux-memory-gated.

## Plan invariant

Existing PDF tool names and schemas must remain backward compatible throughout
the AnyDoc work.

## Public-data boundary

Only synthetic or demonstrably redistributable fixtures may enter Git. The
tracked source PDFs contain public U.S. Code text. Samples 4 and 5 are empty
slots and have no benchmark rows.

Before adding a fixture, follow the manifest and metadata checks in
[`test-corpus/README.md`](../test-corpus/README.md). Never discover arbitrary
files from a contributor's home directories during tests.

## Verification

Run from the repository root:

```bash
cargo fmt --all -- --check
cargo metadata --locked --format-version 1
cargo check --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build --workspace --release --locked
cargo audit
bash scripts/check-public-hygiene.sh
```

The candidate branch configures CI to run:

```bash
cargo deny --all-features --locked check advisories licenses bans sources
```

Treat that policy as active on the public default branch only after this exact
head passes its pull-request checks and is merged.

When MCP schemas change, initialize the built binary over stdio and verify the
exact tool-name set. When parser dependencies change, also assert that
`cargo tree -d` does not show two `pdf-inspector` versions or sources.

## Next dependency chain

1. Keep the dependency and security baseline tracked in the dated PR review.
2. Maintain the provider-neutral contract and dedicated PDF adapter as stable boundaries.
3. Expand the structural completeness oracle beyond the known public markers, then
   complete additional hostile-resource, filesystem-isolation, and cross-host memory evidence
   before broader format promotion. The worker-level Darwin network canary, Linux
   seccomp implementation/target checks, initial Darwin release-mode resource
   observations, current public adversarial fixture slice, structural marker oracle,
   reviewed-warning classifier, Unix end-to-end process-group reap proof, and tracked
   EPUB qualification corpus is green for its scoped lane.
4. Keep ODP and EPUB under their Linux-memory-gated strict contracts; add RTF
   only after its own real fixture, threat model, completeness, resource, and
   rollback evidence pass. EPUB has a tracked qualification corpus, navigation
   oracle, real-parser chapter-order/omission evidence, and Linux worker route;
   hostile-resource, filesystem, and cross-platform gates remain.

The detailed ordering remains in [`docs/anydoc-integration-plan.md`](anydoc-integration-plan.md). Do not skip parser convergence or expose upstream AnyDoc model types directly through MCP.
