---
name: anydoc-enhanced
description: >
  Convert local office documents and PDFs to Markdown, classify PDFs, and
  run the tax-form, IRC, and SEC extractors. Use when a user supplies a
  PDF, DOCX, PPTX, XLSX, ODS, ODT, ODP, CSV, or EPUB and wants offline
  text or a tax, Title 26, or 10-K/10-Q reading. PDFs stay on
  pdf-inspector, including per-page OCR needs. Other formats use the
  bounded AnyDoc worker. No network and no OCR.
license: MIT OR Apache-2.0
compatibility: >
  Requires Python 3.10+ and the bundled or installed pdf-inspector-mcp
  binary. No Rust toolchain and no network. OCR is disabled. Strict CSV,
  ODP, and EPUB conversion are Linux-only. macOS and Linux run PDF, DOCX,
  PPTX, XLSX, ODS, and ODT. Windows builds are not published.
metadata:
  author: anydoc-enhanced
  version: "0.1.0"
---

# anydoc-enhanced

Offline document skill. The scripts speak JSON-RPC to the `pdf-inspector-mcp` binary. That binary is also the bounded worker: it re-executes itself, so `ANYDOC_WORKER_BIN` is not needed. Rust parses and extracts. Python only passes a path and prints JSON.

Do not install upstream `anydoc` or `pdf-inspector` from PyPI. Do not call a hosted OCR API.

## Which command

| File | Command |
|---|---|
| PDF | `scripts/classify_pdf.py` or `scripts/pdf_to_markdown.py` |
| DOCX, PPTX, XLSX, ODS, ODT | `scripts/document_to_markdown.py` |
| CSV, ODP, EPUB | `scripts/document_to_markdown.py` on Linux only |
| Kind check | `scripts/classify_document.py` |
| Tax form | `scripts/identify_tax_form.py` |
| Title 26 | `scripts/parse_irc_sections.py` |
| 10-K / 10-Q | `scripts/split_sec_filing.py` |

A PDF passed to `document_to_markdown.py` is refused. AnyDoc would fail the whole file when any page needs OCR. pdf-inspector keeps text from pages it can read and lists the rest.

## Run

```bash
python3 scripts/classify_pdf.py --path test-corpus/source/sample-1.pdf
python3 scripts/pdf_to_markdown.py --path test-corpus/source/sample-1.pdf
python3 scripts/document_to_markdown.py --path test-corpus/docx/public-fixture.docx
python3 scripts/classify_document.py --path test-corpus/docx/public-fixture.docx
python3 scripts/identify_tax_form.py --path test-corpus/source/sample-1.pdf
python3 scripts/parse_irc_sections.py --path test-corpus/source/sample-1.pdf
python3 scripts/split_sec_filing.py --path test-corpus/source/sample-1.pdf
```

Every command takes `--path` and `--help`. There is no prompt. JSON goes to stdout. A short message goes to stderr on failure.

The binary is resolved in this order:

1. `PDF_INSPECTOR_MCP_BIN`
2. `bin/<target>/pdf-inspector-mcp` inside this skill folder
3. `pdf-inspector-mcp` next to the current Python, or on `PATH` (the wheel install)

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success. `classify_pdf.py` also exits 0 when it reports a scan. |
| 1 | The file or the server failed. |
| 2 | Usage error, such as a missing `--path`. |
| 3 | One or more pages need OCR, which is disabled. Readable pages stay in `result`. |
| 4 | `pdf-inspector-mcp` was not found. |

`pages_needing_ocr` and `ocr_reasons_by_page[].page` are already 1-based. Do not add 1. Region tools in the MCP server, which this skill does not wrap, use 0-based page indexes. Those are a different field.

A fully scanned PDF is not passed to the tax, IRC, or SEC parsers. An empty parse would look like a real answer.

## Platforms

PDF classification and Markdown, plus DOCX, PPTX, XLSX, ODS, and ODT, run on macOS and Linux.

Strict CSV, ODP, and EPUB run only on Linux. They stay disabled unless the worker can enforce an address-space ceiling, which this build does on Linux and not on macOS. `classify_document.py` reports `enabled: false` for those three on macOS.

Windows wheels and zips are not published.

## macOS downloads

A skill zip downloaded in a browser gets a quarantine flag. Gatekeeper can then refuse the bundled binary. An ad-hoc signature, which the release job applies, does not satisfy Gatekeeper for a downloaded file. Clear the flag after unzip:

```bash
xattr -dr com.apple.quarantine anydoc-enhanced
```

`pip install` of a wheel and a git checkout do not set that flag.

## Build identity

Each result includes `build` from the binary that ran: server version, git commit, pinned `pdf-inspector` and `anydoc` versions, and the target triple. See [references/diffs.md](references/diffs.md) for how this repository differs from upstream.
