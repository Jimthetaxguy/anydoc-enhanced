# anydoc-enhanced

[![CI](https://github.com/Jimthetaxguy/anydoc-enhanced/actions/workflows/ci.yml/badge.svg)](https://github.com/Jimthetaxguy/anydoc-enhanced/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

Rust workspace plus MCP server wrapping the [firecrawl/pdf-inspector](https://github.com/firecrawl/pdf-inspector)
library for offline, fast PDF classification and extraction, with tax-form,
IRC, SEC-filing, and Sweet tax-review demo helpers exposed as separate tools.

## Status

**Alpha.** The 16 MCP tools compile and load; validation against real PDFs is
partial. Use ground-truth fixtures of your own to confirm fitness for any
specific workflow.

The Firecrawl AnyDoc integration now ships bounded DOCX, strict PPTX, strict XLSX, strict ODS, strict ODT, and strict ODP lanes, plus Linux-memory-gated strict CSV and strict EPUB lanes. ODP and EPUB are enabled only where the worker address-space ceiling is enforceable. DOCX preserves the existing contract. PPTX accepts exact .pptx containers only: every declared slide must resolve to a well-formed shape tree, and hidden, externally linked, active, or incomplete presentations fail closed. XLSX accepts exact .xlsx containers only: cached values are read without formula evaluation, and hidden, externally linked, macro-enabled, binary, legacy, malformed, or incomplete workbooks fail closed. ODS accepts exact spreadsheet packages with visible, non-active content and cached/displayed values only; hidden, externally linked, encrypted, active, malformed, or formula-incomplete packages fail closed. ODT accepts exact text packages with visible content only; hidden/tracked, externally linked, encrypted, active, malformed, or missing-asset packages fail closed. EPUB accepts strict EPUB 3 packages only: every declared spine chapter must resolve in navigation order, local resources must exist, and hidden, external, active, encrypted, malformed, or archive-amplified packages fail closed. Broader formats remain disabled pending their own fixture, completeness, and resource gates.
Checks read each package the way AnyDoc reads it (its transcoding, reference resolution, and exact part names), so content the checks never saw cannot reach the output. DOCX content the pinned parser drops silently, such as symbol-font checkboxes, fails closed, and hidden DOCX text is converted with a disclosure warning. The checks also compare AnyDoc's reading with what Word, Excel, LibreOffice, or an EPUB reader shows: text one shows and the other drops, EPUB blocks a reader shows apart that AnyDoc runs together, negatives rendered without their sign, values a format hides, dates rendered as serial numbers, and drawings never read are refused, while DOCX list numbers that differ from Word's convert as `partial` with a `list_numbering_differs` warning. Scanned PDF pages whose invisible OCR layer pdf-inspector does not read are listed for OCR with the `invisible_text_layer` reason. PDF results carry `warnings` for text pdf-inspector 1.24.0 repeats (a line painted twice, a table's first row left in the paragraph above it), merges (two amounts in one cell), splits or runs together (word gaps judged against the wrong space, words a browser printed glyph by glyph), or loses: text drawn through forms it does not read, table amounts pushed out of their rows, lines it drops as running headers though they differ from the one it keeps, form values it garbles or leaves out, text boxes or stamps annotated onto a page, dynamic XFA forms, which it reads as their "Please wait..." notice, and embedded files, such as a portfolio's documents; and text it reads from layers a reader hides by default, such as a superseded figure. Each names its pages where the check can tell; the Markdown is never changed.
See the [dependency-ordered integration plan](docs/anydoc-integration-plan.md), the [2026-09-24 upstream drift audit](docs/upstream-drift-audit-2026-09-24.md) (previous: [2026-08-28](docs/upstream-drift-audit-2026-08-28.md)), [iterative improvement roadmap](docs/iterative-improvement-roadmap.md), and [worker resource evidence](docs/resource-evidence.md)
for the verified upstream constraints, architecture, measured baseline, and acceptance gates.

## Real systems

- Released Firecrawl `pdf-inspector 1.24.0` plus `lopdf 0.45.0` for local PDF parsing, run in the same bounded worker as the document lanes (25-second deadline, four slots, Linux address-space ceiling and network filter).
- Released Firecrawl `anydoc 0.2.4` for local DOCX, strict PPTX, strict XLSX, strict ODS, strict ODT, strict ODP, and strict EPUB conversion through a supervised child process; the CSV route is a separate local strict adapter.
- Strict CSV accepts bounded UTF-8 delimiter-separated text with deterministic delimiter sniffing, equal-width rows, RFC-4180-style quoting, and Markdown escaping; malformed, ragged, oversized, or unsupported inputs fail closed. The route is enabled only where the worker address-space ceiling is enforceable (currently Linux).
- MCP uses local stdio JSON-RPC; no hosted API, OCR service, telemetry, or network fetch is enabled by default.
- Optional local controls: `PDF_INSPECTOR_MCP_LOG` selects a safe log level and `ANYDOC_WORKER_BIN` selects an explicit worker-compatible executable.

Validation snapshot (see [`docs/HANDOFF.md`](docs/HANDOFF.md) for the current
public handoff):

| Tool | Real-PDF validated? | Notes |
|---|---|---|
| `classify_pdf` | Public-fixture test | Tracked 4-page U.S. Code PDF |
| `pdf_to_markdown` | Public-fixture test | Asserts expected Title 26 content |
| `analyze_layout` | Public-fixture MCP test | Layout signals through the worker on the tracked Title 26 PDF |
| `extract_text_regions` | Public-fixture MCP test | Region text through the worker on the tracked Title 26 PDF |
| `extract_table_regions` | MCP test | Display-frame rectangles on a generated `/Rotate 90` page |
| `batch_classify` | Public-fixture MCP test | Text, scanned, and missing inputs in one call |
| `identify_tax_form` | Unit-tested | No redistributable positive tax-form fixture yet |
| `parse_irc_sections` | Public-fixture test | Sections, full provision labels, and repealed placeholders on the tracked Title 26 PDFs |
| `split_sec_filing` | Unit-tested | No redistributable live-filing fixture yet |
| `list_tax_packages` | Synthetic demo | Lists bundled Sweet demo packages across six tax workflows |
| `review_tax_package` | Synthetic demo | Runs deterministic checks against bundled structured examples |
| `compare_line_items` | Synthetic demo | Compares one return value to one source value with tolerance |
| `render_review_memo` | Synthetic demo | Renders a Markdown memo from structured review findings |

## Why

Coding agents that touch PDFs almost always reach for OCR first, even when
the source is born-digital. That round-trip costs seconds-to-minutes per
document and discards the structural information already present in the
PDF (text positions, headings, table boundaries).

`pdf-inspector` reads the PDF directly: it classifies (TextBased / Scanned /
Mixed) in single-digit milliseconds and extracts to clean Markdown without
calling out to any OCR engine. This project exposes that capability over the
Model Context Protocol so any MCP-aware agent (Claude Code, Codex, Cursor,
Gemini, OpenCode) can call it identically.

On top of the generic primitives, three domain-specific tools encode patterns
we use ourselves: tax-form identification (W-2, 1099, K-1, 1040), IRC section
parsing for Title 26 PDFs, and SEC 10-K / 10-Q section splitting. These are
layered as separate MCP tools rather than baked into the core extractor so
the upstream surface stays clean.

## The 16 tools (13 PDF/domain tools plus 3 generic document tools)

| Tool | What it does | Status |
|---|---|---|
| `document_capabilities` | Report the stable generic-format capability contract | beta |
| `classify_document` | Detect a local document kind and whether its generic route is enabled | beta |
| `document_to_markdown` | Convert an enabled DOCX, exact `.pptx`, exact `.xlsx`, exact `.ods`, exact `.odt`, exact `.odp`, strict EPUB, or bounded CSV input to sanitized Markdown through a bounded worker | beta |
| `classify_pdf` | TextBased / Scanned / ImageBased / Mixed classification with confidence, the pages that need OCR and why, and validated creation/modification dates | stable |
| `pdf_to_markdown` | Full PDF to clean Markdown with headings, tables, lists | stable |
| `analyze_layout` | Pages with tables or columns, and fonts whose text may be garbled | beta |
| `extract_text_regions` | Text from `[x1,y1,x2,y2]` rectangles; `frame: display` reads boxes taken from a rendered page image | beta |
| `extract_table_regions` | Tables from rectangles as Markdown pipe-tables, with the same optional `frame` | beta |
| `batch_classify` | Classify many PDFs in one call | beta |
| `identify_tax_form` | Detect W-2 / 1099 / K-1 / 1040 / 1065 / 1120 / 1120-S / schedules | beta |
| `parse_irc_sections` | Title 26 IRC sections with full provision labels, repealed flags, and separated notes | beta |
| `split_sec_filing` | 10-K / 10-Q section splitter by Item number | beta |
| `list_tax_packages` | List bundled Sweet demo packages | demo |
| `review_tax_package` | Run deterministic review checks for a bundled package | demo |
| `compare_line_items` | Compare one return line against one source value | demo |
| `render_review_memo` | Render a Markdown review memo for a bundled package | demo |

## Install

### From source

```bash
git clone https://github.com/Jimthetaxguy/anydoc-enhanced
cd anydoc-enhanced
cargo install --locked --path crates/pdf-inspector-mcp
```

This places `pdf-inspector-mcp` in your Cargo bin directory (typically
`~/.cargo/bin/`). To pin a known location, use:

```bash
cargo install --locked --root /usr/local --path crates/pdf-inspector-mcp
```

### Wire into an MCP client

For Claude Code, use its CLI so machine-specific paths remain in user-scoped
configuration rather than this repository:

```bash
claude mcp add pdf-inspector --scope user -- "$(command -v pdf-inspector-mcp)"
claude mcp get pdf-inspector
```

See Anthropic's current [MCP setup documentation](https://docs.anthropic.com/en/docs/claude-code/mcp).
Other MCP clients can register the absolute path returned by
`command -v pdf-inspector-mcp` as a local stdio server.

## Quick start

The server speaks JSON-RPC over stdio. A minimal `tools/call` for
`classify_pdf`:

```bash
printf '%s\n%s\n%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"classify_pdf","arguments":{"path":"/path/to/file.pdf"}}}' \
  | pdf-inspector-mcp
```

The same pattern works for any of the 16 tools. Two more one-liners (drop in
the `printf` block above, replacing the third line):

```text
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"identify_tax_form","arguments":{"path":"/path/to/W2.pdf"}}}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"split_sec_filing","arguments":{"path":"/path/to/10-K.pdf"}}}
```

Sweet demo package tools do not require real PDFs; they use bundled synthetic
structured data to demonstrate a provider-neutral review layer:

```text
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_tax_packages","arguments":{}}}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"review_tax_package","arguments":{"package_id":"demo_1040_w2_schedule_c"}}}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"compare_line_items","arguments":{"label":"W-2 wages","return_reference":"Form 1040 line 1a","source_reference":"W-2 Box 1","return_amount":84732,"source_amount":87432,"tolerance":0}}}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"render_review_memo","arguments":{"package_id":"demo_1099_bundle"}}}
```

The actual response payload is JSON-encoded inside the standard MCP
`{"content":[{"type":"text","text":...}]}` envelope.

## Architecture

A caller (any MCP-aware agent) speaks JSON-RPC over stdio to the
`pdf-inspector-mcp` binary. The binary dispatches to a thin facade crate
(`pdf-inspector-skillkit`) which wraps the upstream `pdf-inspector` library.
Domain modules (tax / sec / irc) are siblings of the core extractor — each
composes the primitives with format-specific knowledge but never modifies
the upstream surface.

```
  caller (Claude Code, Codex, ...)
        |
        | JSON-RPC over stdio (MCP)
        v
  pdf-inspector-mcp        (rmcp 3.4 server; read-only tool annotations)
        |
        v
  pdf-inspector-skillkit   (facade lib)
   |       |
   |       +-- domain::tax      (identify_tax_form)
   |       +-- domain::sec      (split_sec_filing)
   |       +-- domain::irc      (parse_irc_sections)
   |       +-- domain::sweet    (demo review packages + comparisons)
   v
  bounded worker process  (PDF codes 16-20, document codes 1-8)
   |
   +-- pdf-inspector 1.24.0 (released, exact lock resolution)
   +-- anydoc 0.2.4 behind the package preflight
```

## Development

| Task | Command |
|---|---|
| Build | `cargo build --workspace --locked` |
| Test | `cargo test --workspace --locked` |
| Lint | `cargo clippy --workspace --all-targets --locked -- -D warnings` |
| Check candidate text for obvious identifiers | `bash scripts/check-public-hygiene.sh` |
| Validate domain tool against a PDF | `cargo run --example validate_domain -- <tax\|irc\|sec> <pdf-path>` |
| Run Sweet review demo | `cargo run -p pdf-inspector-skillkit --example sweet_review_demo -- demo_1040_w2_schedule_c` |

Server logs and handled errors do not include caller-supplied paths or labels.
Set `PDF_INSPECTOR_MCP_LOG` to one of `off`, `error`, `warn`, `info`, `debug`,
or `trace` to change this crate's verbosity. Raw `RUST_LOG` directives are
ignored so dependencies cannot log protocol payloads.
`batch_classify` retains each supplied `path` in its response to preserve the
existing correlation schema; use opaque staging filenames when response paths
are sensitive.

## License

Dual-licensed under either of:

- MIT License — see [LICENSE-MIT](LICENSE-MIT)
- Apache License 2.0 — see [LICENSE-APACHE](LICENSE-APACHE)

at your option.

The resolved dependency graph passes the repository's license policy. See
[THIRD_PARTY.md](THIRD_PARTY.md) for the dated inventory and selected license
terms.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for setup, code standards, and the PR
checklist (including a hard rule against committing real personal or
financial PDFs as test fixtures).
