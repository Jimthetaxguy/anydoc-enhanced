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
- CI checks the declared minimum Rust (1.88) in a new job. The lockfile pins
  `aes` 0.9.2: 0.9.3 raised its own minimum to 1.89 and fixed nothing.
- EPUB text a reader hides is refused only when AnyDoc would convert it. Text
  that AnyDoc also omits (a `display: none` from a bare tag or class rule, or
  an inline style) converts, so the Markdown matches what a reader shows. The
  public `hidden-content.epub` fixture now hides its text with
  `visibility: hidden`, which AnyDoc ignores.
- The DOCX `hidden_content_preserved` message now names every case it
  discloses: hidden text, tracked deletions, and unreferenced notes.
- XLSX refuses cells, rows, and sheets outside the positions AnyDoc reads.
  ODS and ODP refuse text in positions AnyDoc's walkers skip, as ODT already
  refused dropped content. EPUB refuses text a reader shows and AnyDoc drops.
- DOCX conversion reports `partial` with a new `list_numbering_differs`
  warning when AnyDoc's list numbers differ from Word's (see Fixed).
- PDF results list a scanned page for OCR, with upstream's
  `invisible_text_layer` reason, when its text is mostly an invisible layer
  that pdf-inspector 1.24.0 does not read (see Fixed).

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
- Review round four: main parts are found by exact name, as AnyDoc finds
  them. A case-variant `XL/workbook.xml` decoy no longer passes while AnyDoc
  falls back to the binary `xl/workbook.bin`. An ODF package's lane follows the
  first `office:body` in the office namespace; a same-named element elsewhere
  no longer decides it.
- EPUB stylesheets are evaluated by a new module that models a reading system
  and AnyDoc side by side. The reader model uses a CSS Syntax 3 tokenizer, media
  queries, the full selector grammar, the cascade, and the user-agent rules
  that hide content. The AnyDoc model is a port of AnyDoc's own subset, applied
  only to the elements its walker styles. Hiding the old checks missed is now
  found: comments and escapes in declarations and selectors, namespaced and
  quoted-attribute selectors, `@import` in any spelling, a second prefixed
  `rel` or `href`, SVG `display` and `visibility` attributes,
  `<?xml-stylesheet?>`, `content-visibility`, `hidden`, and closed dialogs.
- PPTX speaker notes are followed through each slide's notesSlide
  relationship, wherever they are stored. A slide list inside
  `mc:AlternateContent` is refused.
- DOCX refuses a table cell AnyDoc's row walker does not reach (such as one
  wrapped in `mc:AlternateContent`) and a row outside a WordprocessingML table.
  It discloses as hidden: a hidden mark on a directly numbered paragraph, which
  hides its list label; `w:specVanish` runs; tracked-deleted rows; unreferenced
  footnotes and endnotes; and a drawing's `mc:Choice` that needs a vocabulary
  outside Office's, which Word replaces with its fallback.
- XLSX formula caches must render as AnyDoc's `cell_text` renders them. Sheet
  default row heights and column widths too small to draw, and rows or columns
  under one pixel, count as hidden. A VML checkbox that Excel hides but
  AnyDoc's case-sensitive test converts counts as hidden.
- ODS formula caches must render as AnyDoc's `value_text` renders them. A
  streaming model of AnyDoc's ODF walkers refuses text in positions they skip:
  page-anchored frames and `text:numbered-paragraph` in ODT, and frames in
  `draw:a`, shapes AnyDoc does not walk, and notes stored in shapes in ODP.
  ODP shapes with `draw:display` set to `none` or `printer`, shapes on hidden
  layers, and slides hidden through an inherited or default drawing-page style
  count as hidden. So do ODF rows and columns under half a pixel.
- EPUB text a reader shows and AnyDoc drops is refused, found from AnyDoc's
  open pull requests on its HTML walker (#147, #149). AnyDoc reads only `li`
  children of a list and only row groups, rows, cells, and the first caption
  of a table, so text or a paragraph placed directly in either was lost, as
  was `noscript` content. Its stylesheet split applies every rule after the
  first inside an at-rule block everywhere. So the common Kindle pair,
  `@media amzn-mobi { .mobi-only {display: block} .kf8-only {display: none} }`,
  dropped the `.kf8-only` text EPUB readers show, and the book converted as
  complete. Script, style, and head text inside `pre`, which readers hide and
  AnyDoc converts, now counts as hidden.
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
- `scripts/build-anydoc-hardening-corpus.py` and thirty-eight synthetic
  fixtures that reproduce pinned-AnyDoc behaviors the local contract does not
  inherit, or that must convert.
- `docs/upstream-drift-audit-2026-09-24.md`: the upstream refresh audit, the
  disposition of all 58 open AnyDoc pull requests, and the pdf-inspector pull
  requests that reach the PDF tools.
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
- EPUB chapters whose blocks AnyDoc runs together are refused, found from its
  older EPUB pull request (#4). AnyDoc walks a `div`, `section`, `figure`,
  `figcaption`, `dd`, or other container without block children inline, so
  minified markup such as `<div>Balance due</div><div>1,250.00</div>`, which a
  reader shows on two lines, converted as "Balance due1,250.00", and an
  image's alt text ran into its caption. Markup with white space between the
  blocks converts as before. Of 303 public documents, only the three built to
  reproduce it changed.
- Converted Markdown keeps line breaks inside table cells. The sanitizer
  removed AnyDoc's `<br>`, so a cell reading "52,000" over "1,250" came out as
  "52,0001,250", one wrong number, in DOCX, PPTX, ODT, ODS, ODP, and EPUB
  tables. It also no longer deletes angle-bracket text AnyDoc escaped
  (`\<Client name>`) or text inside code spans and code blocks.
- A scanned PDF made searchable, with its words in an invisible OCR layer and a
  visible header or Bates number on top, was read by pdf-inspector 1.24.0 as a
  text page holding only the stamps, at confidence 1.0, with no page listed
  for OCR (open upstream #479, #501). A bounded local scan of each page's
  content now lists such pages for OCR. The layer's own text is never copied
  out, since it need not match the page. On the public corpus it adds 1–5 ms
  per PDF call and changes no output.
- XLSX and ODS values whose format AnyDoc renders differently are refused,
  from its open spreadsheet pull requests (#72, #151). A negative marked only
  by a colour, as in `#,##0;[Red]#,##0`, rendered as 25,000 for -25,000. A
  value its format hides (`;;;`) is still held by the workbook, and ODS
  converted it in place of the empty display; a hidden zero is not counted. A
  date whose format AnyDoc cannot resolve rendered as its serial number. Text
  boxes over a sheet, or anchored to an ODS cell, were never read.
- DOCX list numbers that differ from Word's are disclosed (#129). AnyDoc counts
  per list instance where Word counts per definition, so a second instance
  restarted at 1 where Word continues, including headings numbered through a
  style. AnyDoc's own upstream fixtures show "1." and "I." where Word shows
  5 and V. It also numbered paragraphs deleted with tracked changes, and
  rendered ordinals and spelled-out numbers as plain decimals.
- Password-protected DOCX, XLSX, and PPTX files are reported as `encrypted`
  rather than `malformed`. The check read their stream names in ASCII, but a
  compound file stores them in UTF-16LE.
- Decks using PowerPoint Sections convert again. Section entries
  (`p14:sldId`) were read as slides without relationships, which refused the
  deck as incomplete.
- Matching slides to relationships is a single pass; a crafted deck had made
  it quadratic. So is a deck that repeats one relationship id for every slide.
- EPUB books with common publisher CSS convert again. A `[hidden]` reset,
  print and Kindle media blocks, and user-agent rules for `head` had refused
  the whole book although nothing they hide would convert.
- A web address written in an EPUB chapter no longer refuses the book. The
  external check had matched `http:` anywhere in chapter text, in stylesheet
  comments, and in `@namespace` identifiers. Stylesheet references are now read
  from `url()`, `image-set()`, and `@import`, and the Markdown sanitizer still
  removes addresses from the output.
- EPUB stylesheet parsing is linear. An `@import` without a closing semicolon
  had made it quadratic: 16,000 imports took 9.3 s and 2.2 GiB, and 32,000
  exhausted the server. They now take 0.01 s and 13 MiB, under caps on tokens,
  imports, import depth, sheet applications, rules, and matching work.
- The word `macroEnabled` in slide, note, or cell text no longer marks a
  package as active content; only the content-types part can.
- A chart's link to an external data workbook is reported as an external
  relationship rather than refused as active content.

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
- Visual concealment (text color, size, opacity, clipping, or off-screen
  positioning) is not detected in any lane. EPUB text in a closed `<details>`
  and SVG `<title>` and `<desc>`, which read like image alt text, are not
  flagged.
- Spreadsheet pictures and charts are not converted, and a picture's alt text
  is not flagged. Text boxes and shapes with text are refused.
- A spreadsheet format AnyDoc cannot parse, such as `£#,##0.00`, renders as
  General: the value and its sign are kept, the currency symbol and rounding
  are not. A value in a cell a merge covers is omitted, as Excel and
  LibreOffice hide it. Neither is flagged.
- Comments and notes (DOCX comments, XLSX notes, ODS annotations) are not
  converted and are not flagged, nor are DOCX headers and footers, so a
  "DRAFT" marking or client name placed only in a header is missing.
- EPUB block containers without block children, such as indented `div`s or a
  `figure` and its caption, are walked inline by AnyDoc and convert as one
  paragraph, their text joined with spaces. This is not flagged; text that
  would run together is refused.
- ODP decks whose speaker notes sit in shapes, as LibreOffice writes them when
  converting from PowerPoint, are refused: AnyDoc reads notes only from frames.
- EPUB `noscript` content is treated as shown, as readers without scripting
  show it, and MathML as converted whole. Books using the Kindle stylesheet
  pair are refused rather than converted without their `.kf8-only` text.
- EPUB page numbers hidden through attribute or descendant selectors, such as
  Project Gutenberg's `.x-ebookmaker .pagenum`, refuse the book, because
  AnyDoc converts them.
- DOCX: a hidden paragraph mark is disclosed for direct numbering (`w:numPr`)
  only, not numbering applied through a paragraph style. List numbering
  follows Word's counters by definition and list style; `w:lvlRestart` and
  legal numbering (`w:isLgl`) are not compared. Where Word supports
  a block-level `mc:Choice` that AnyDoc does not, AnyDoc converts the
  `mc:Fallback`; the check assumes the two branches hold the same text.
