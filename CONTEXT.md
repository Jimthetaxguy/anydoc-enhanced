# anydoc-enhanced — project context glossary

**Role:** public Rust workspace + MCP server for offline document intelligence.
**Path:** repository root (`<repo-root>`)
**Remote:** `https://github.com/Jimthetaxguy/anydoc-enhanced.git`
**Branch:** `agent/codex-align-firecrawl-20260828` (active implementation branch)

## Purpose

Expose [firecrawl/pdf-inspector](https://github.com/firecrawl/pdf-inspector) over **MCP** so coding agents classify/extract PDFs in milliseconds **without OCR-first**, then layer tax / IRC / SEC / Sweet tax-review helpers as separate tools.

## Domain vocabulary

| Term | Meaning |
|------|---------|
| **classify_pdf** | TextBased / Scanned / Mixed + confidence (~1–10 ms) |
| **pdf_to_markdown** | Born-digital PDF → clean Markdown (headings/tables/lists) |
| **analyze_layout** | Tables, columns, complexity metrics |
| **extract_text_regions / extract_table_regions** | Geometry-bounded extraction (`[x1,y1,x2,y2]`) |
| **batch_classify** | Multi-PDF classify loop |
| **identify_tax_form** | W-2 / 1099 / K-1 / 1040 / 1065 / 1120 / schedules detector |
| **parse_irc_sections** | Title 26 IRC section parser (experimental capture format) |
| **split_sec_filing** | 10-K / 10-Q Item-number splitter |
| **Sweet demo package** | Bundled structured tax-review package (list / review / compare / memo tools) |
| **skillkit** | Library crate with domain modules (`tax`, `irc`, `sec`, `sweet`) |
| **mcp crate** | `pdf-inspector-mcp` binary exposing tools via rmcp |
| **AnyDoc worker** | Firecrawl native Rust converter locked at `v0.2.4`; DOCX, exact `.pptx`, exact `.xlsx`, exact `.ods`, exact `.odt`, exact `.odp`, and strict EPUB use the bounded AnyDoc path; strict CSV uses a separate local adapter with the same worker boundary on Linux; EPUB and CSV are enabled only on Linux; other variants remain disabled |
| **parser convergence** | One resolved `pdf-inspector` version shared by the skillkit and AnyDoc; required before integration |
| **document service** | Provider-neutral contract above the PDF facade and bounded AnyDoc worker |
| **public fixture** | Redistributable, provenance-recorded test input containing no PII or private source material |

## Module map

| Path | Role |
|------|------|
| `crates/pdf-inspector-skillkit/` | Domain + extraction helpers (library) |
| `crates/pdf-inspector-skillkit/src/domain/` | `tax.rs`, `irc`, `sec`, `sweet` |
| `crates/pdf-inspector-mcp/` | MCP server binary, worker mode, and tool registration |
| `docs/` | Handoff, Sweet demo notes |
| `docs/anydoc-integration-plan.md` | Dependency-ordered AnyDoc architecture and acceptance gates |
| `test-corpus/` | Public PDF plus public PPTX, DOCX, XLSX, ODS, ODT, ODP, CSV, and EPUB qualification corpus and adversarial inputs for validation |
| `scripts/` | Local helpers |

## Real systems

- PDF parsing via released `pdf-inspector 1.24.0` (Firecrawl) + `lopdf 0.45.0` — **offline**, no cloud OCR default
- MCP over stdio for Claude/Codex/Cursor/etc.
- Demo Sweet packages are **synthetic structured examples**, not live client filings
- AnyDoc `0.2.4` is resolved and used by the bounded DOCX/PPTX/XLSX/ODS/ODT/ODP/EPUB worker; strict CSV is a local bounded adapter selected after reviewing AnyDoc `0.2.4` behavior; PDF remains on the dedicated PDF facade

## Verify

```bash
cargo check --workspace --locked
cargo test --workspace --locked
# install binary: cargo install --path crates/pdf-inspector-mcp
```

CI: GitHub Actions badge on README.

## Known limitations (from README)

- Alpha; not all 16 tools live-smoked against real PDFs
- Strict PPTX currently accepts only visible, non-macro `.pptx` packages; missing/corrupt declared slides, hidden slides, external relationships, active content, and incomplete conversion fail closed
- Strict XLSX currently accepts only visible, cached-value `.xlsx` packages; hidden content, external links, macros, binary/legacy Excel, and incomplete formulas fail closed
- Strict ODS currently accepts only visible, cached/displayed-value `.ods` packages; hidden sheets/rows/columns, external references, encrypted packages, active objects/scripts, and uncached formulas fail closed
- Strict ODT currently accepts only visible, well-formed, exact-mimetype `.odt` packages; hidden/tracked content, external references, encrypted packages, active objects/forms, malformed XML, missing internal assets, and unsupported `text:note` content fail closed
- Strict CSV is recognized everywhere but the generic route is enabled only when the worker address-space ceiling is enforceable (currently Linux); it requires valid UTF-8, equal-width RFC-4180-style rows, bounded fields/output, and escapes Markdown structure. Strict ODP and strict EPUB follow the same Linux memory gate: ODP requires exact presentation identity, visible complete slides, and local assets; EPUB requires exact EPUB 3 identity, all-spine completeness, navigation agreement, and local resources. Both reject active/external/hidden content.
- Worker process-group cleanup is implemented on Unix; Linux enforces the address-space ceiling and seccomp network denial, while Darwin uses the named `no-network` profile. Filesystem isolation, non-Linux memory ceilings, and hostile-input promotion remain gated
- The worker classifies reviewed AnyDoc omission and malformed-recovery warnings into stable incomplete results without exposing raw log text; structural marker oracles cover known public fixtures, while unobserved silent omissions still require additional cases. Strict ODP now has exact identity, visible presentation, local-asset, hidden/external/active, malformed, encryption, archive-limit, and real-worker evidence; strict EPUB now has exact OCF/OPF/spine identity, navigation, local-resource, hostile-content, archive-limit, and real-worker evidence. Initial Darwin/arm64 release-mode resource observations are recorded in `docs/resource-evidence.md`; hostile-resource, filesystem, and cross-host memory gates remain open
- Bank-direct 1099-INTs often `Unknown` for form id
- IRC section-number capture format incomplete
- Sweet tools are demo/synthetic until real packages wired

## Non-goals

- Not a general OCR pipeline (use OCR only when classify says Scanned/Mixed and user opts in)
- Do not commit real client tax PDFs or PII filings into the repo
- Do not vendor Firecrawl parser source; use exact released dependencies and record upstream lineage
- Do not add PII, private corpus paths, user-specific home paths, credentials, or internal planning links to this public repository

## Cleanup note (2026-07-10)

CONTEXT added. Sweet tax-review domain + MCP tools may land in the same hygiene checkpoint as this glossary.
