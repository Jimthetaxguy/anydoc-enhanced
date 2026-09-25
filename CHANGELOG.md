# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed
- Adopted `pdf-inspector` 1.24.0 and `lopdf` 0.45.0 (from 1.17.0 and 0.42.0).
  Every PDF tool now runs in the bounded worker used by the document lanes:
  separate process, 1 GiB address-space ceiling and seccomp network denial on
  Linux, a 25-second deadline, and four in-flight slots taken before a file is
  read. Tool names and input schemas are unchanged.
- Adopted `rmcp` 3.4.1. Every tool declares read-only, non-destructive,
  idempotent, closed-world annotations.
- Every dependency now resolves from crates.io, and cargo-deny rejects Git
  sources.
- Consolidated the `anyhow`, `serde_json`, `thiserror`, `regex`, and `tokio`
  lockfile updates after a live review of PRs #14-#18; `thiserror` advances to
  2.0.20 because the proposed 2.0.19 update is already superseded.
- Replaced home-directory PDF discovery in integration tests with a tracked,
  redistributable U.S. Code fixture.
- Updated repository identity and documentation for `anydoc-enhanced`, and
  added a dependency-ordered Firecrawl AnyDoc integration plan. AnyDoc 0.2.4
  now runs behind the bounded worker for the enabled document lanes.
- Limited feature-branch CI to the pull-request event so the same jobs are not
  duplicated by both `push` and `pull_request`.

### Security
- DOCX conversion refuses content the pinned AnyDoc parser drops silently:
  symbol-font checkboxes and letters (`w:sym`), legacy form checkbox and
  drop-down state. Hidden text is converted and disclosed with a
  `hidden_content_preserved` warning.
- DOCX conversion also refuses ruby text and imported chunks (`w:altChunk`),
  whose content the pinned parser drops. A dropped non-breaking hyphen
  ("Form 1040‑SR" converts as "Form 1040SR") is reported with
  `completeness: partial` and a `characters_omitted` warning; this is the first
  lane to emit `partial`.
- XLSX conversion refuses number-format codes over 4,096 bytes; one 8 MiB code
  had amplified to 855 MiB in the worker.
- Markdown sanitization decodes each link destination before classifying it.
  It removes external and local-path destinations in every spelling found in
  review, including `mailto:`, `file:`, `data:`, `tel:`, UNC, and `www.` forms.
- Package checks now read what AnyDoc reads. XML parts are transcoded as AnyDoc
  transcodes them (UTF-16 and declared encodings). Package references resolve
  as AnyDoc resolves them, including fragments, queries, percent-encoding, and
  `..` clamping. The PPTX presentation part is matched by exact name, and the
  officeDocument relationship must name the checked part. Each hidden-content
  or dropped-content decoy found in review converted as complete before this
  change and now fails closed or is disclosed.
- Package checks read markup as AnyDoc does. Namespace declarations are not
  attributes: `xmlns:Target` can no longer shadow `Target`. Every attribute is
  decoded before comparison.
- XLSX hidden-sheet, hidden-row, and cached-formula checks parse the XML and
  also treat zero-size rows and columns as hidden. Binary workbook records in
  `xl/workbook.xml` are refused as `unsupported`. Hidden defined names, which
  Excel adds for filters, no longer refuse a workbook.
- PPTX hidden slides and hidden shapes (`cNvPr hidden`) are parsed rather than
  matched as text, and speaker notes are checked. ODP slides hidden through a
  drawing-page style are refused. An ODF package whose body belongs to another
  lane than its mimetype reports missing content.
- DOCX: only WordprocessingML deletions exempt content, and only where
  AnyDoc's walker skips them; a deletion inside a drawing does not hide a text
  box. Table rows wrapped in content controls or custom XML are refused.
  Hidden list labels in the numbering part are disclosed. Run properties inside
  `mc:AlternateContent` are read. Embedded objects are found by relationship
  type, whatever their part name.
- EPUB hiding is evaluated in inline styles, `<style>` elements, linked
  stylesheets, and their local imports. Both `display: none` and
  `visibility: hidden`/`collapse` count, as AnyDoc's declaration parser reads
  them.
- A PDF page-content bomb that still peaks at 2.1 GiB in-process under
  `pdf-inspector` 1.24.0 now returns `resource_limit` from the worker.
- Replaced the yanked `chacha20` 0.10.0 with 0.10.2.
- Updated transitive `crossbeam-epoch` to 0.9.20 to resolve
  `RUSTSEC-2026-0204`.
- Expanded CI policy enforcement to run cargo-deny advisory, license, ban, and
  source checks; added a candidate-text obvious-identifier guard and Gitleaks;
  and added ignore rules for credentials and local/private test corpora.
- Removed identifying pilot references, machine-specific paths, and
  non-reproducible private-source benchmark rows from the tracked public tree.
- Redacted caller-supplied paths and labels from MCP errors and stderr logs,
  disabled dependency protocol-payload logging, and documented the
  backward-compatible `batch_classify` path echo explicitly.

### Added
- PDF results report per-page OCR reasons. Analysis reports layout (pages with
  tables or columns) and fonts whose text may be garbled (CMap gaps), and
  omits both when a mode did not compute them. Creation and modification dates
  are reported when they match the PDF date grammar. Region extraction reports
  why a region needs OCR.
- `extract_text_regions` and `extract_table_regions` accept an optional
  `frame`. `sheet` is the default and the previous behavior: the page as laid
  out in its content stream, `/Rotate` not applied. `display` reads the
  rectangles on the rendered page, so boxes from a page image select the right
  text on rotated pages. This uses pdf-inspector 1.24's region frames.
- `parse_irc_sections` reads the Markdown pdf-inspector renders. It returns
  full provision labels such as `(d)(2)(A)(i)`, flags repealed sections, and
  keeps editorial and statutory notes apart from the operative text.
- `scripts/build-anydoc-hardening-corpus.py` and seventeen synthetic fixtures that
  reproduce pinned-AnyDoc behaviors the local contract does not inherit.
- `docs/upstream-drift-audit-2026-09-24.md`: the upstream refresh audit and the
  disposition of each open AnyDoc pull request.
- Initial Rust workspace with `pdf-inspector-skillkit` library and `pdf-inspector-mcp` server binary
- 9 MCP tools: `classify_pdf`, `pdf_to_markdown`, `analyze_layout`, `extract_text_regions`, `extract_table_regions`, `batch_classify`, `identify_tax_form`, `parse_irc_sections`, `split_sec_filing`
- 4 Sweet tax-review demo tools: `list_tax_packages`, `review_tax_package`, `compare_line_items`, `render_review_memo` — deterministic package review, line-item comparison, and Markdown memo rendering over built-in demo packages (1040, 1120, 1065, 1120-S, K-1, 1099 workflows). Bringing the total to 13 MCP tools.
- `CONTEXT.md` project glossary documenting domain vocabulary and module map
- Validation runner example (`cargo run --example validate_domain`)
- Domain modules for tax form identification, IRC section parsing, SEC filing splitting
- `OnceLock<Regex>` cache for all 32 regexes (compile once, reuse)
- Path-free server tracing to stderr (stdout reserved for JSON-RPC)
- 30s `tokio::time::timeout` per tool handler
- Crates.io metadata (`description`, `license`, `repository`, `keywords`, `categories`)
- Dual MIT / Apache-2.0 licensing
- GitHub Actions CI: fmt + clippy `-D warnings` + test + release build
- Dependabot weekly cargo + actions updates
- README, CHANGELOG, CONTRIBUTING, THIRD_PARTY license audit

### Fixed
- Decks using PowerPoint Sections convert again. Section entries
  (`p14:sldId`) were read as slides without relationships, which refused the
  deck as incomplete.
- Matching slides to relationships is a single pass; a crafted deck had made
  it quadratic.

### Known limitations
- The DOCX checks keep at most 16,384 styles and 1,024-byte style ids per
  document and return `resource_limit` beyond them; a styles part that must be
  transcoded is checked in memory up to 4 MiB.
- PPTX, XLSX, and EPUB refuse external hyperlinks as incomplete, where DOCX
  converts them with a warning and removes the destination.
- `parse_irc_sections` reads U.S. Code Title 26 structure; Treasury Regulation
  numbering (`§ 1.401(k)-1`) is not parsed.
- `identify_tax_form`: bank-direct 1099-INTs that render as numeric tables only return `Unknown` (no header text in markdown)
- No OCR engine ships. Scanned pages report that they need OCR and why, and
  return no text for those pages.
- DOCX conversion reports a dropped non-breaking hyphen as partial but cannot
  restore it; the Markdown shows the joined words.
