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
- Dependencies build optimized in the dev profile. The integration tests
  drive the debug server under its 30-second tool timeout, and unoptimized
  pdf-inspector took 11.6 s of it on the public Title 26 sample, enough to
  time out on a loaded runner; it now takes 2 s.
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
- PDF results carry a `warnings` list, absent when empty, naming text the
  Markdown repeats, pages whose word gaps pdf-inspector misjudges or whose
  form text it does not read, pages that lose a line it takes for a running
  header, form values it garbles or leaves out, annotation text, dynamic
  XFA forms and embedded files it never reads, and tables whose amounts may
  sit in the wrong row or column or after the table (see Fixed). A full run that yields no Markdown for a text PDF
  reports confidence 0, and a page whose text looks garbled sets
  `has_encoding_issues`.

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
- `scripts/build-anydoc-hardening-corpus.py` and forty synthetic
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
- Files a PDF embeds, which pdf-inspector 1.24.0 never reads, are reported.
  A portfolio bundles documents, such as a year's tax forms, as embedded
  files behind a cover page, and converted as that cover alone; attachments
  and file attachment annotations were passed over the same way, with no
  sign. A PDF embedding files now carries the new `embedded_files_unread`
  warning, which counts them and says whether the PDF is a portfolio, so
  each file can be converted on its own; the Markdown is not changed.
- A dynamic XFA form, whose content pdf-inspector 1.24.0 never reads, is
  reported. Such a form, marked as needing rendering, keeps its fields and
  filled values in XFA, which a viewer lays out; its pages hold only the
  notice a viewer without XFA shows. A filled return made so converted as
  "Please wait... If this message is not eventually replaced…" alone, at
  confidence 1.0, with the taxpayer's wages nowhere in it. It now carries
  the new `xfa_form_unread` warning; the Markdown is not changed. A form
  with XFA that draws its own pages, as the IRS's static forms do, is not
  reported: its values are read from its fields (see `form_values_misread`).
- Text shown in annotations, which pdf-inspector 1.24.0 never reads, is
  reported. A PDF shows text in annotations besides its page content: a
  text box a reviewer types onto the page (FreeText), such as "Adjusted
  basis 12,500.00 per preparer", or a stamp or watermark drawn in text,
  such as "RECEIVED APR 15 2025". pdf-inspector reads a page's content,
  links, and form values only, so such text was missing from the Markdown
  with no sign. Visible text boxes, and stamps and watermarks whose
  appearance draws text, are now read for their text (`/Contents`, or the
  plain text of their rich text), and a page whose annotation text the
  Markdown does not show carries the new `annotation_text_unread` warning;
  the Markdown is not changed. Notes shown only in a popup, and markup
  commenting on the page's own text, are not what the page shows and are
  not read. No PDF among 4,412 corpus, fixture, review, and fuzz files is
  named (only ten of them hold annotations).
- Form field values pdf-inspector 1.24.0 garbles or leaves out are reported
  (upstream issue #504). pdf-inspector writes each filled form field into
  the Markdown as its name and value, but reads the value as UTF-8, while a
  PDF writes it in PDFDocEncoding or UTF-16. A payee filled in as UTF-16,
  "José García", came out as "��\0J\0o\0s…" with control characters in the
  Markdown, and "São Paulo" as "S�o Paulo". It also reads a value only from
  a field that is its own widget, so it left out the choice of a group of
  radio buttons, such as a return's filing status, and the value of any
  field shown in more than one place. The fields are walked as
  pdf-inspector walks them, and a page whose values it garbles or leaves
  out, where the Markdown does not show them, carries the new
  `form_values_misread` warning; the Markdown is not changed. The walk
  reuses the document the page scan loads. Values in XFA forms, which
  pdf-inspector does not read, are not checked. No PDF among 4,412 corpus,
  fixture, review, and fuzz files is named (only one of them holds a form).
- Lines pdf-inspector 1.24.0 drops as running headers or footers, though
  they say what the line it keeps does not, are reported (upstream issue
  #483). In a document of three pages or more, pdf-inspector drops from
  every page but the first a line it finds near the top or bottom of three
  pages, and three in ten, at about the same height. It compares lines with
  the digits at either end left out, and drops the lines beside such a line
  with it. So a consolidated statement's second and third accounts lost the
  "Account number" line heading their pages, and every page read as the
  first account's; a payroll register's later employees lost their IDs and
  hour totals, all at confidence 1.0. The pages converted are read again as
  pdf-inspector groups their lines, and its rule is applied to them. A page
  where a dropped line says what no kept line says, other than by a page
  number, and where the Markdown shows the line kept in its place but not
  the dropped one, carries the new `header_footer_dropped` warning; the
  Markdown is not changed. Headers repeated as they are, such as a bank's
  name, and those numbering their pages are dropped by design and not
  reported. No PDF among 551 corpus, fixture, replica, and review files and
  3,970 fuzz and review files is named. The reading costs about a third
  more CPU on documents of three pages or more; it covers up to 2,000 pages
  and stops, reporting nothing, after 4 seconds.
- Amounts pdf-inspector 1.24.0 pushes out of a table's rows are reported
  (open upstream #424). The table grid can drop a column: a 1099-B's sparse
  wash-sale adjustments, or the Amount column of a long card statement,
  follow the table instead, an amount a line, so which lot or purchase each
  belongs to is lost. When the Markdown shows amounts on lines of their own
  right after a table, amounts its cells do not hold, the page's text is
  read as pdf-inspector places it, and where the page sets most of them on
  the lines of the table's rows, beside a whole date or description cell,
  the result carries the `table_values_detached` warning; the Markdown is
  not changed. A total set on a line of its own is not reported. Among 551
  corpus, upstream-fixture, replica, and review PDFs it names ten, each such
  a dropout: the three #424 replicas, the upstream `tnagriculture_06_12`
  fixture, and six generated card statements.
- Text pdf-inspector 1.24.0 misses or garbles when a form XObject draws it
  is reported (open upstream #312). A form without `/Resources` of its own
  draws with its invoker's, as renderers read the specification, but
  pdf-inspector gives it none, so a form such a form draws is never read: a
  W-2 whose box lines sit in a form drawn through a bare form converted
  with only its heading, at confidence 1.0. pdf-inspector also starts every
  form with no font, so text a form shows in the font it was drawn with is
  read byte by byte, and a subset font's space at code 3 is lost ("Total
  deposits85,000.00"). A page showing such text now carries the
  `form_text_unread` warning; the Markdown is not changed. Among 551
  corpus, upstream-fixture, replica, and review PDFs, only the five
  reproductions changed.
- Words pdf-inspector 1.24.0 splits in text a browser printed glyph by glyph
  (open upstream #531) are reported. Chromium's print to PDF shows each
  glyph as a string of its own, placed at the whole-pixel advance hinting
  gave it, while the font's widths keep the unhinted advance; a glyph set
  wider than its width crosses pdf-inspector's word-gap threshold, so the
  upstream fixture read "LIAB ILITIES" and a statement "B ALANCE DUE". Where
  a font paints its word spaces as glyphs anywhere in the document, which
  say where its words end, the page scan collects the words it shows glyph
  by glyph. When the
  Markdown shows one split by a space or a cell edge, the pages showing it
  are read again as pdf-inspector places their text, and each page whose
  own text splits it carries the `word_gaps_misread` warning; the Markdown
  is not changed. On 400 randomized browser-printed pages, it names 203 of
  the 270 whose Markdown splits such a word and none of the 130 others; 61
  of the 67 it misses leave their word spaces as gaps. Among 256 corpus,
  upstream-fixture, and replica PDFs, only the three #531 replicas are
  reported, and the public samples, which the check reads again, convert
  about 50-60 ms slower.
- EPUB selectors that rely on siblings are matched as a reader matches
  them. Round seven counted a rule with `h2 + p`, `h1 ~ p`,
  `:first-of-type`, or `:last-child` only where digits met, because one
  pass through a chapter could not settle it; words it ran together
  converted. A pass before the walk now counts each element's siblings, and
  the walk keeps the earlier siblings rules test (the first 32 and the
  latest 96, with the names, ids, and classes of any let go between).
  Against Chromium's layout of 800 randomized chapters, the check now
  refuses all 417 in which AnyDoc runs words together and none of the 383
  others; round seven missed 13 and refused 1 in error. Selector matching
  backtracks where a nearer ancestor or sibling fails, within the match
  budget, and a `~` step for a sibling a chapter lacks costs one lookup.
  Text that opens with closing punctuation, such as a period a clearfix
  box sets on a line of its own, no longer counts as run together, except
  digits meeting across a decimal point ("12." and "5"). A link holding an
  inline box with a heading inside no longer counts as broken where the
  reader keeps one line.
- Words and amounts that pdf-inspector 1.24.0 runs together or splits because
  it measures word gaps against the wrong space width (open upstream #532)
  are reported. For a subset font whose differences name the space at a code
  other than 32, it reads the space width at code 32, or 250 units when code
  32 has none, so the upstream real-estate fixture read "CBDOffice" and
  "pricingisliketheweather", and a kerned price "8 5,000 .00". A page whose
  text has a gap that pdf-inspector judges otherwise than its open fix would
  now carries a `word_gaps_misread` warning; the Markdown is not changed. The
  check follows pdf-inspector's rules for the threshold, its fallbacks, its
  tracked runs of single glyphs, and character spacing that the next run
  takes back. It reads glyphs with pdf-inspector's own ToUnicode and
  glyph-name tables. Against pdf-inspector patched with the fix, it names
  every changed page it checks and no other: 8 pages in 5 of 256 corpus,
  upstream-fixture, and replica PDFs (the ninth changed page is listed as
  needing OCR, and not checked), and 212 pages in 900 randomized fonts and
  layouts.
- DOCX list numbers are compared as Word writes its labels, found by
  checking the review fixtures against LibreOffice. A level without number
  text (`w:lvlText`) shows no number in Word, while AnyDoc numbers it; a
  composite label (`%1.%2.`) shows the shallower number as each side counts
  it, so a second list instance's "2.1." converted as "1.1."; a number the
  label does not show is no longer compared. Paragraph styles are followed
  along `w:basedOn` to the end of the chain, as AnyDoc follows them, rather
  than 32 styles; a numbering number written with white space around it,
  which Word reads and AnyDoc cannot, is disclosed. Fifteen review fixtures
  now report `list_numbering_differs`; none of the 320 randomized list
  documents or the 303 public documents changed.
- An EPUB whose navigation document sits in the spine, as pandoc places it,
  is no longer refused because the navigation does not list itself. Books
  whose navigation leaves out a chapter are still refused.
- A DOCX page number or date that Word fills in where a run shows it
  (`w:pgNum` and the legacy date blocks), which AnyDoc drops, is disclosed:
  the document converts as `partial` with the `characters_omitted` warning.
  LibreOffice does not show them either; in headers and footers, which
  AnyDoc does not convert, nothing is reported.
- The Markdown sanitizer keeps the anchors AnyDoc writes for link targets
  (`<a id="…"></a>`, with ids of its own characters), so a document's own
  links still land and a bookmark no longer raises `sanitized_output`. A web
  address or path redacted inside a code span keeps the span's closing
  backtick; the redaction had swallowed it, turning the text after it into
  code.
- Text pdf-inspector 1.24.0 repeats or merges is reported, from its open pull
  requests (#317, #377, #406, #424, #443, #531). A run painted twice over
  itself, for emphasis, as an overprint, or as a replayed row, came out twice
  ("TToottaall", "84.19 84.19") at confidence 1.0. A compact table's first row
  also ended the paragraph above it, so a statement's opening balance or first
  deposit appeared twice; a rate table in the public Title 26 sample shows it.
  Adjacent columns of amounts, such as a 1099-B's wash-sale adjustment beside
  the basis, merged into one cell. Each is now a warning,
  `text_painted_twice` by page, `table_row_repeated`, and
  `table_values_merged`; the Markdown is not changed. A text PDF whose full
  run yields no Markdown reported confidence 1.0 and now reports 0.
- Review round five found missed losses and false refusals in the checks
  added by loops 8-11; all are fixed, and no outcome among 303 public
  documents changed:
  - DOCX list numbers are replayed paragraph by paragraph as Word and AnyDoc
    count them. Lists that match (a deleted bullet, a "Restart at 1" with
    letters under it, letters counted across restarts) are no longer
    reported. A level replaced without a start override is, and so is a
    restart LibreOffice writes on a list's first paragraph only, which
    LibreOffice numbers 1, 2, 3 and AnyDoc 1, 4, 5.
  - Spreadsheets: a currency code holding "CR" or "DR" (IDR, SCR, CRC) no
    longer passes for an accounting sign, so "IDR 25,000" shown in red for
    -25,000 is refused. A negative with text of its own ("Refund $830",
    "▼3.1%"), or too small to show a digit, is not. A colour name no longer
    reads as a date. Built-in percentages 67 and 68, 15% shown as 0.15, are
    refused. Connector labels are found, and Excel's compatibility fallbacks
    for charts and slicers are no longer read as text boxes.
  - ODS: a formula returning a space or an empty string
    (`=IF(A2="";"";A2*B2)`) is cached, not missing. Chart and formula objects,
    which AnyDoc shows as images or converts, are no longer refused as active
    content; OLE objects, by element or manifest entry, are.
  - PDF: clip-only text an image or a shading is painted through, as in a
    heading filled with a picture or a gradient, is visible, so such flyers
    are no longer listed for OCR.
- Review round eight checked loops 14 to 17 and the round-seven fixes:
  - PDF: a statement whose rows repeat a cell of two amounts, such as
    "0.00 0.00" quarter- and year-to-date, made the merged-cell check time
    out with no Markdown returned. The page's positioned text is now
    indexed once and each distinct cell placed once, so the reproducers
    convert in about a second. Two runs side by side count as merged
    columns only under a heading over the second, as a 1099-B's "Wash sale"
    heads its column: an amount set beside its percentage, "1,234.56
    (9.02%)", is one cell, and a fee of "0.00" on every row no longer joins
    two rows. On a page whose text reads rotated, the scan's runs are
    turned as pdf-inspector turns its text, so a rotated 1099-B's merged
    cells are reported. A first row of three amounts or more repeated at
    the end of the text before its table is reported whatever precedes it,
    as upstream #531's equity statement shows. Doubled text holding a lone
    "-", ":", or "+" ("09/01/2025 - 09/30/2025") is confirmed; a year
    "2020" or a box "11" confirms only a repeat whose text is that number
    alone, or one paint of it. The repeat scan also finds a second paint in
    another subset font, a shadow a fifth of the size off, a second paint
    split in two strings, a `TJ` array stepping back to show a string
    again, and text doubled with no space between its copies; past the 64
    pages read again, a page is named for its own repeated text rather than
    in order. The word-gap check reads a page as pdf-inspector does: comments
    between operands stripped, each text object in render mode 0, forms
    starting with no font or spacing, glyphs in an ActualText span left to
    its text, and the pen followed over runs whose widths it knows. A
    sub-run split between digits, or on a vertical baseline, sets two items
    apart, which a space inside one item does not, as a 1099-B row read
    under a wide code 32 shows. Against pdf-inspector patched with its fix,
    over 2,900 randomized files, it names 685 of the 718 changed pages (654
    before) and 13 unchanged ones (42 before).
  - DOCX: list numbers are compared by the label each side shows, at the
    level each reads, as Word and LibreOffice show them. A level bound to a
    style, a composite label over a shallower level in a format it cannot
    render (unless the level is legal-style, `isLgl`), words in a level
    without a number, and paragraphs Word numbers through its default
    paragraph style, which AnyDoc does not read, are disclosed as
    `list_numbering_differs`. List ids and numbers are read as each side
    reads them (Word trims them as schema integers, AnyDoc parses them as
    written), which replaces loop 14's document-wide padding rule; a style
    defined twice is read as each side keeps it; and AnyDoc's branch of
    alternate content stands for Word's only when both number alike. Text
    Word shows in alternate content where AnyDoc takes no branch is refused.
    Style chains are read once per style, so a document of many styles that
    took 12.8 s converts in 0.7 s; the chain budget and its disclosure are
    gone. None of 303 public, 119 review, and 239 other regression documents
    changed; among 750 randomized ones, only a bypass the review found did.
  - XLSX: a slash is a fraction bar only right after an integer
    placeholder, as AnyDoc parses it; a fixed denominator counts by its
    value, a fraction's percent signs scale it, and a fraction AnyDoc
    rejects renders as General. Eight fraction formats that show an unsigned
    non-zero value for a negative are now refused, and 26 that show zero now
    convert; all 204 format and value pairs checked agree with LibreOffice.
  - EPUB: generated content and SVG are read as a reader shows them. A "."
    or "," a pseudo box sets between digits counts where digits follow it,
    and more signs count beside digits (the cent to yen signs, the currency
    block such as the rupee sign, and the dashes), while a sign on a list
    item AnyDoc numbers itself does not; an `::after` sign meets the text
    after it, and a pseudo box a reader hides (`opacity: 0`, `visibility:
    hidden`) or floats off the line no longer counts. SVG text in resources
    nothing references, in unknown elements, or outside a `text` element is
    refused as hidden; `switch` renders one child, `foreignObject` holds
    HTML, and label spans follow the pen through x, y, dx, and dy. A `~`
    step is matched exactly at any distance. Flex items in a row may touch,
    as a reader sets them; in a column, reversed, gapped, spread along the
    line, or set apart by a margin or padding they stand apart, and grid
    items always do; a line-clamped `-webkit-box` is a block. Fixed-layout
    runs that continue a line may touch, and a single-figure drop cap is
    read with its paragraph. Against Chromium's layout, 29 bypasses are now
    refused and 20 chapters refused in error convert; over 760 randomized
    chapters, errors of refusal fall from 48 to 10 with no join newly
    missed, and real books check as fast as before.
- Review round seven checked the round-six fixes again:
  - EPUB: a reader sets more boxes apart than the chapter walk knew, and
    AnyDoc ran their text together with no warning. Each flex or grid
    item, table cell set by style, and SVG `text` now stands apart, as
    does a floated or positioned box holding digits:
    `<p><span style="float:left">10</span>250 units</p>` had converted as
    "10250 units". So do a block image, a line feed a `::before` box keeps
    (`content: "\A"; white-space: pre`), a line break alone in a link,
    which AnyDoc drops, and a display formula in a link, which AnyDoc
    flattens into the text around it. Text a `::before` or `::after` box
    shows, from letters, digits, a counter, or `attr()`, is refused as
    text AnyDoc drops, and so is a sign beside the digits of an amount
    ("−1,250.00" converted as "1,250.00"); a hyphen bullet is not. The
    other way, rules that may not apply, such as sibling selectors, now
    count only where digits meet, which the Markdown reads as one number.
    Drop caps set by such rules, a drop cap after an opening quote, an
    InDesign drop cap of three letters going on in lower case, and floated
    images with alt text no longer refuse the chapter, and neither does a
    `::before` or `::after` box without `content`. A large book whose
    stylesheet carries rules for many other sections had run out of match
    budget since round six read `float` rules; rules whose ancestor
    classes, ids, and element names a chapter lacks are now set aside by
    lookup, as browsers filter them.
  - PDF: the evidence round six required for the table and repeat warnings
    missed real cases and still passed some by-design layouts. A table cell
    holding two amounts is now judged by where the page sets them. Separate
    runs on one baseline, like a 1099-B's wash-sale column in the basis
    cell, were merged and are reported; this now covers a one-row table and
    a column merged in every row. Rows the detector merged also count: two
    lines of amounts where the label cell joins both lines ("Capital gain
    distributions Total income"). So does one item that a second run starts
    inside. Amounts stacked by design (federal over state withholding, a
    discount under its price) and one string the producer wrote ("10.000
    25.50") are no longer reported. Short values doubled glyph by glyph
    ("22", "77", "$$55") are confirmed. An unrelated doubled amount elsewhere
    on the page no longer confirms a repeat: the text at the repeat must
    show it twice. Past the 64 pages read again, a page is named only while
    doubled text is left in the Markdown; an 80-page document with a
    doubled header had named 16 pages its Markdown does not repeat. A
    first row repeated after a line of form fields ("Acct: 5678 Period:
    April 2025"), which the detector keeps out of the table, is reported;
    the upstream #406 fixtures now show it.
  - DOCX: where Word and AnyDoc take different branches of
    `mc:AlternateContent` (a drawing canvas or a Word 2010 block Word reads,
    and the fallback AnyDoc reads), the branches hold the same list, which
    had been flagged; the first branch AnyDoc takes now stands for Word's.
    Word's branch still counts where AnyDoc takes none. A paragraph style
    naming its own list level (`w:numPr/w:ilvl`), with no level bound to
    the style, is numbered at that level by Word and at the first by
    AnyDoc ("a)" against "1."); it is disclosed. So is a list running
    through footnotes or endnotes stored in another order than their ids
    or their references: AnyDoc numbers them as stored, LibreOffice by id,
    and Word in the order the text references them.
  - XLSX and ODS: a negative residue such as -5.55e-17 in a colour-only
    fraction format shows as 0 in Excel, and is no longer refused; a
    fraction shows zero below half its smallest step.
- Review round six found four missed losses and seven false positives in
  loops 12 and 13 and the round-five fixes; all are fixed:
  - EPUB: AnyDoc flattens a link's content into the text around it, so
    `<div>Note:<a id="c1"><h2>Total 1,250.00</h2></a></div>` converted as
    "Note:Total 1,250.00" with no warning. The chapter walk now splices a
    link's blocks as AnyDoc does, with the white space and line breaks it
    drops. The reader model now reads `display`, `float`, and block
    `::before`/`::after` boxes: a floated drop cap ("O" beside "nce") and an
    inline `div` no longer refuse a book, while a `span` styled as a block,
    an `address`, and a line break, rule, or empty paragraph that only
    AnyDoc's selector quirks hide now count as the reader's line breaks. An
    image whose alt text its caption repeats, as pandoc 2 writes figures, no
    longer refuses the book.
  - DOCX: Word numbers the body, the text boxes, the footnotes, and the
    endnotes as separate stories, which AnyDoc counts through as one. A
    footnote list continuing the body's numbers converted as 4, 5 where Word
    shows 1, 2, without a warning, and a text box written both as a shape
    and as its VML fallback was counted twice, flagging lists that match.
    The replay also counts a level a deeper paragraph skips as used ("1.1.1."
    then "2."), restarts an instance once, at its first overridden level,
    and starts a level without `w:start` at 0, as Word does. On 320
    randomized list documents it now agrees with LibreOffice on every one;
    38 differences had been missed.
  - PDF: a run painted twice through two font objects was missed; runs are
    now matched by their bytes alone. `text_painted_twice` no longer names
    pages whose repeat pdf-inspector drops before its Markdown (a doubled
    header or footer stripped as furniture, a white copy in a form, a copy
    its clip hides): the text at each repeat must appear doubled in the
    Markdown, and a header kept on the first page only names that page.
    `table_values_merged` needs an empty neighbouring cell or a single
    amount elsewhere in the column, and `table_row_repeated` no longer
    fires on a sentence that restates the first row.
  - ODT and ODP: the embedded-object reference check compared every
    reference with every archive entry. A 28 MB document held a worker
    thread for 225 s after its 30 s timeout; the check is now linear.
  - XLSX and ODS: a negative fraction in a colour-only format (`# ?/?;[Red]#
    ?/?`) shows as "1/4" however small, and its lost sign is refused.
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
- Other pdf-inspector 1.24.0 defects from its open pull requests are not
  detected: amounts pushed out of their rows after a table (#424); a receipt
  with few text operators and a logo read as a scan (#445); forms with
  indirect resources (#407), reported only through the garbled-text reason
  where it applies; rotated column headers scattered
  into cells (#298); and blank pages that turn the sparse-extraction rule
  on for every page (#339). The table checks read the Markdown and name no
  page; the repeat check reads runs whose position is set, and stops after
  4 million operations per document. A repeat is confirmed in the Markdown
  on up to 64 pages; a later page is named for its own repeated text when
  its fonts read it, and otherwise while doubled text is left in the
  Markdown, so an amount a paragraph legitimately repeats ("0.00 0.00") can
  name one. A shadow offset along the baseline by less than a third of the
  size is a repeat only for runs of two glyphs or more. A table cell's
  amounts are placed from the first 64 pages converted; past them, where
  the amounts are not read as runs of their own, and where no line above
  heads a column over the second amount, a cell counts beside an empty cell
  or in a column whose other rows hold one amount. The word-gap check runs
  on the same pages; a page listed as needing OCR is not checked, and a
  dependent sign's placement and return count as two gaps, where
  pdf-inspector nets them into one. A split between digits on a line
  outside a table reads the same as a space, but is reported. Tracked runs
  of single glyphs, such as a letter-spaced heading, are judged with some
  error either way: over 2,900 randomized files, 13 unchanged pages were
  named and 33 changed ones missed. Composite (Type0) fonts are not
  checked: pdf-inspector reads their space width at code 32 or 3, which
  may be another glyph, so words shown with offsets can run together
  ("Thebalanceoftheaccountwas") with no warning. Words printed glyph by
  glyph are read only in a font that paints a word space as a glyph after a
  word somewhere in the document, so a document whose words all stand
  alone on their lines, or whose spaces are gaps, is not checked. A split word's pages are read again up to 64
  pages; past them, a page is named only when the Markdown never shows the
  word whole. A word set in small capitals, its first letter a glyph of
  its own and the rest one string, is not read, so the public Title 26
  chapter 6 sample keeps "(A) L imitations" with no warning.
- DOCX conversion reports a dropped non-breaking hyphen as partial but cannot
  restore it; the Markdown shows the joined words.
- Visual concealment (text color, size, opacity, clipping, or off-screen
  positioning) is not detected in any lane. EPUB text in a closed `<details>`
  and SVG `<title>` and `<desc>`, which read like image alt text, are not
  flagged.
- Spreadsheet pictures and charts are not converted, and a picture's alt text
  is not flagged; an ODF chart object converts as its replacement image. Text
  boxes and shapes with text are refused.
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
  would run together is refused. The reader model reads `display`, `float`,
  `position`, and flex and grid layout from style rules, and the `display`,
  `content`, `position`, and `white-space` of `::before` and `::after`
  boxes. A floated or positioned box keeps the line before it and ends the
  line after it, unless it holds a drop cap: one or two characters, or
  three going on in lower case, and no digits. A rule the walk cannot
  settle, with `:has()` or `:lang()`, or reaching a sibling let go past
  the first 32 and the latest 96, counts only where digits meet: "Balance
  due" and "1,250.00" run together under such a rule are not refused. Alt
  text meeting other text counts the same way. Generated counters refuse a
  book even where they match AnyDoc's own list numbers.
- ODP decks whose speaker notes sit in shapes, as LibreOffice writes them when
  converting from PowerPoint, are refused: AnyDoc reads notes only from frames.
- EPUB `noscript` content is treated as shown, as readers without scripting
  show it, and MathML as converted whole. Books using the Kindle stylesheet
  pair are refused rather than converted without their `.kf8-only` text.
- EPUB page numbers hidden through attribute or descendant selectors, such as
  Project Gutenberg's `.x-ebookmaker .pagenum`, refuse the book, because
  AnyDoc converts them.
- DOCX: a hidden paragraph mark is disclosed for direct numbering (`w:numPr`)
  only, not numbering applied through a paragraph style. List numbers are
  compared as the level's number text shows them, in each level's format
  or, for legal numbering (`w:isLgl`), in decimal; a format other than
  decimal, roman, or letters counts as differing. A paragraph style's own
  `w:ilvl`, which AnyDoc and ECMA-376 ignore and Word and LibreOffice read,
  is disclosed where no level is bound to the style. Where Word and AnyDoc
  take different branches of `mc:AlternateContent`, the check assumes the
  branches hold the same text. A list through notes stored out of id or
  reference order is disclosed without a verdict; no application at hand
  settles which order Word numbers them in. A level with
  `w:lvlRestart="0"` is taken never to restart, as Word does, although
  LibreOffice restarts it.
