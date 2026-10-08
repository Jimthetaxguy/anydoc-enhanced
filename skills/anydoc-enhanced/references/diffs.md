# How this repository differs from upstream

This repository does not vendor or fork Firecrawl parser source. `CONTEXT.md` requires exact released dependencies. The workspace pins:

- `pdf-inspector = "=1.25.0"` (crates.io, MIT)
- `anydoc = "=0.2.4"` (crates.io, MIT)

The local change is the `pdf-inspector-skillkit` layer and the `pdf-inspector-mcp` server, not a patched parser.

## What the skillkit adds

- Package preflight for the enabled office lanes. Content the pinned AnyDoc build would drop or misread is refused or disclosed. The parsers themselves are the released crates.
- PDF warnings for text pdf-inspector 1.25.0 repeats, merges, drops, or reads from a hidden layer. The Markdown is the upstream extraction.
- Domain extractors on top of that Markdown: tax-form identification, IRC section parsing, and SEC item splitting.
- A supervised worker. PDF and office conversion run in a child process with a deadline, a slot limit, a Linux address-space ceiling, and a network filter. The child is this same `pdf-inspector-mcp` binary, started with `--anydoc-worker`.

## Routing

| Input | Path |
|---|---|
| PDF | pdf-inspector, page by page. `pages_needing_ocr` is 1-based, copied from `PdfResult`, and is not incremented again. |
| DOCX, PPTX, XLSX, ODS, ODT | AnyDoc 0.2.4 behind skillkit preflight, through `document_to_markdown`. |
| CSV, ODP, EPUB | Same AnyDoc worker, enabled only where the Linux address-space ceiling exists. |
| Hosted Firecrawl OCR | Not wired. OCR stays off. |

Upstream AnyDoc's `ConvertError::NeedsOcr` fails the whole PDF and returns no text. This skill does not send PDFs down that path. A mixed PDF keeps the pages pdf-inspector could read and exits 3 so the unread pages are visible.

## Packaging

The wheel and the skill zip both ship the already compiled `pdf-inspector-mcp` binary. Installing them does not compile Rust. `maturin` with `bindings = "bin"` puts that binary on `PATH`. The skill zip places the same binary at `bin/<target>/pdf-inspector-mcp`. Linux release binaries are statically linked musl builds, and their wheels are tagged manylinux2014 so pip on glibc can install them. The zip directory is the musl triple.

`--provenance` prints the crate version, git commit, pinned pdf-inspector and anydoc versions, and the target triple. Script output copies that object into `build`.
