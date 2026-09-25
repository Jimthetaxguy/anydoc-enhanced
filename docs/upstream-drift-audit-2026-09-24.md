# Upstream drift audit — 2026-09-24

## Result

This refresh adopts the upstream parser releases published since the
2026-08-28 audit and converts what was learned from AnyDoc's open pull requests
into local fail-closed checks:

- `pdf-inspector` moves from 1.17.0 to 1.24.0 (`lopdf` 0.42.0 to 0.45.0). PDF
  parsing now runs inside the bounded worker, and the new upstream signals are
  reported additively.
- AnyDoc stays at 0.2.4, which is still the latest release; `main` is
  README-only after it. All 58 open pull requests were reviewed against the
  pinned parser: the 20 opened since the previous audit, and the 38 older ones,
  of which those touching an enabled lane were reproduced. Each is
  dispositioned below.
- pdf-inspector's open pull requests (111) were scanned for defects in
  1.24.0 that reach this repository's PDF tools. Two confirmed one: a scanned
  page whose OCR text layer 1.24.0 drops without a signal.
- `rmcp` moves from 3.1.4 to 3.4.1, and every tool declares read-only MCP
  annotations. Tool names and input schemas are unchanged.
- The document lanes gain checks for AnyDoc 0.2.4 behaviors that the local
  contract must not inherit. These are content the parser drops silently, and
  places where the parser and the local checks could read different documents.

No upstream source was copied into this repository. Every dependency resolves
from crates.io, and cargo-deny now rejects Git sources (`allow-git = []`).

## Observed upstream state

Observed 2026-09-24 from anonymous clones and the crates.io API; re-verified
2026-09-25, twice, with no change: crates.io still lists AnyDoc 0.2.4 and
pdf-inspector 1.24.0, and neither `main` has moved.

| Upstream | Local before | Local after | Latest release | `main` |
|---|---|---|---|---|
| `firecrawl/pdf-inspector` | crates.io 1.17.0 | crates.io 1.24.0, checksum `e22dc125a533d212c847c8c85e4fcb7358f4384869ef76b2b8721f039b1b633a` | `v1.24.0` at `876fe9ac65c1b05512b9a1a182b5c56bcfdd6c39` | `f856d3481d41d564c64d20baa2a4796d98aed03c` (release CI only after the tag) |
| `firecrawl/anydoc` | crates.io 0.2.4 | crates.io 0.2.4 | `v0.2.4` at `42bf1c5ecdde9eb0d96d6bd75a9e6698cf93b14c` | `261fc257d17c3eab0f673be31c408fd9fdc2171a` (README only after the tag) |

Transitive changes: `lopdf` 0.45.0 (checksum
`bfffda0fe1ab0157e1a13c14bebd3f28671f2fccb7922f0722ec53926e6922d3`); `rmcp`
and `rmcp-macros` 3.4.1; `chacha20` 0.10.2 replaces the yanked 0.10.0.

## `pdf-inspector` 1.17.0 to 1.24.0

Tags in the range: `v1.18.0` `0d92c4c9`, `v1.19.0` `f731bea5`, `v1.20.0`
`f5682303`, `v1.21.0` `dc9bdc16`, `v1.22.0` `ba21a43c`, `v1.22.1` `43a1e95b`,
`v1.23.0` `7a340112`, `v1.24.0` `876fe9ac`.

Changes that reach this repository's surface:

- Load-time object streams are bounded (8 MiB per stream) and page content read
  during extraction is bounded (64 MiB). Some content reads remain unbounded:
  `decompressed_content()` and `get_page_content()` in the detector, content
  scan, font, ToUnicode, and XObject paths.
- Invisible text (render mode 3) is skipped by default. A Mixed PDF with no
  visible text retries with invisible text included.
- Right-to-left text extracts in logical order (1.20.0).
- Region extraction reads rectangles in an explicit frame. The default
  (`PositionFrame::Sheet`) ignores `/Rotate`, so it matches a rendered page
  image only for unrotated pages. `PositionFrame::Display` reads them on the
  rendered page. The region tools expose this as an optional `frame`, with
  `sheet` as the default.
- New result fields: per-page OCR reasons, CMap gaps, and document information
  (title, author, subject, keywords, creator, producer, creation and
  modification dates). Layout and CMap gaps are empty when a mode did not
  compute them (detect-only, scanned, or image-based PDFs).

Evidence, recorded with the release build on Linux:

| Check | 1.17.0 | 1.24.0 |
|---|---|---|
| Golden output of all 16 tools over the public corpus | baseline | one line differs (`<sup>PM</sup>` in sample 2) |
| 1 MB object-stream bomb inflating to 1 GiB, peak RSS | 1,098–1,102 MiB | 18.5–18.7 MiB |
| Page-content bomb, in-process peak RSS (classify / Markdown / analyze) | 1,098 / 2,122 / — MiB | 2,122 MiB in every mode |
| Same bomb through the PDF worker | — | `resource_limit`, worst process about 519 MiB, 0.6–1.7 s |

The page-content bomb shows that the upstream bounds do not cover every read.
PDF tools therefore now run in the same supervised worker as the document
lanes: separate process, 1 GiB address-space ceiling on Linux, seccomp network
denial, process-group kill on timeout or cancel, four in-flight PDF slots, and a
25-second PDF deadline. The worker route costs little on the public corpus.
Median `classify_pdf` time rises from 4.3 to 7.7 ms on sample 1 and from 55.8 to
65.6 ms on sample 2. `pdf_to_markdown` on sample 2 goes from 408 to 417 ms. A
`batch_classify` over 24 files of 40 MiB drops the server's high-water mark
from 1,015 to 192 MiB, because callers take a slot before reading bytes.

### Open pull requests after 1.24.0

pdf-inspector had 113 open pull requests on 2026-09-25. The recent ones that
reach this repository:

| PR | Subject | Local disposition |
|---|---|---|
| #479, #501 | Recover an invisible OCR layer on text-based documents | Reproduced on 1.24.0: a scan whose OCR text sits in an invisible layer, with a visible header and Bates number on top, classifies as text at confidence 1.0 and returns only the stamps, with no page listed for OCR. A local scan now reports such pages for OCR (`invisible_text_layer`); see the verification below. |
| #506 | Bound a cubic cost on dense rectangle clusters | Bounded here by the PDF worker's 25-second deadline, which returns `resource_limit`. |
| #583 | Explicit invisible-text inclusion in positioned extraction | Not exposed; the region tools keep upstream's default. |
| #584 | A fork's fixes: CIDs a ToUnicode CMap never names, filled per code from the embedded font's cmap; Hebrew right-to-left order and number separators | From the 1.24.0 source: a code without an entry is read from the codes around it where they spell it out and is otherwise U+FFFD, both counted per font in `cmap_gaps`, which the PDF tools return, and U+FFFD also sets `has_encoding_issues`. The fork's recovery from the embedded font is not adopted, so such a letter stays visible as lost. The right-to-left fixes concern Hebrew text order, outside this repository's corpus. |
| #586 | Release build of the Node binding with a lower glibc floor, merged after 1.24.0 | Packaging only: the Rust crate the PDF tools use is unchanged, so nothing is adopted. Checked on 2026-09-25 with the round-eight review; no other pull request was opened on either repository since this audit. |
| #578 | Render link annotations as Markdown links | On adoption, PDF Markdown gains destinations and must pass through the sanitizer. |
| #589 | Escape literal HTML in rendered text; so far a regression test only | 1.24.0 writes literal text such as `<a test>` or `<u>underline</u>` as is, so it reads as the engine's own markup and an HTML-aware renderer hides it. Not detected; the text stays in the Markdown an agent reads. |
| #590 | Keep a monospace code listing's indentation; so far a regression test only | 1.24.0 fences the listing but drops each line's leading spaces. Not detected; no characters are lost. |
| #531, #532 | Statement-style layouts; space width from `/Differences` | Reproduced: browser-printed text splits words ("LIAB ILITIES"), and a space width read from code 32 alone splits or fuses amounts ("8 5,000 .00") at confidence 1.0. #532 is reported as `word_gaps_misread` on each page with a gap 1.24.0 judges otherwise than the fix would; checked against pdf-inspector patched with the fix, it names every changed page it checks and no other. #531's split words are reported the same way where a font paints its word spaces as glyphs: each page whose own text splits a word it shows glyph by glyph (203 of 270 randomized browser-printed pages whose Markdown splits one, none in error; the 67 missed mostly leave spaces as gaps). Its #406 half is reported as `table_row_repeated`. Small capitals set as one string after a first letter are split too ("(A) L imitations" in the public Title 26 chapter 6 sample) and are not reported. |

Thirteen open pull requests that reach the PDF tools were checked against
1.24.0 with generated fixtures run through the server. Twelve defects are
still present; #299 is mostly fixed by 1.24.0's superscript handling.

| PR | Defect in 1.24.0 | Local disposition |
|---|---|---|
| #377, #317 | Text painted twice over itself is kept twice: "TToottaall aammoouunntt", "84.19 84.19", a line three times where the page shows two | Reported: the page scan notes where each placed visible run starts and adds a `text_painted_twice` warning for a page where the same run starts again within a tenth of its size (overprints, glyph-by-glyph replays, fake bold 0.3 pt off). |
| #406 | A compact table's first row is also left in the paragraph above it, so amounts appear twice (a 14 pt row gap duplicates, 16 pt does not) | Reported from the Markdown as `table_row_repeated` when the paragraph before a table ends with its first or second row and the row holds a digit, where the detector leaves it: on a line of its own, after a label or a line of form fields, or as an emphasized span. The public Title 26 sample (`sample-2.pdf`) has it in a rate table, and the upstream fixtures after an account line. |
| #424 | Adjacent numeric columns merge: a 1099-B's wash-sale column 28 pt from the basis reads "2,610.25 205.25" in one cell; a ruled table puts both years in one cell | Reported as `table_values_merged` when a body cell's amounts are separate runs on one baseline, one item a second run starts inside, or two lines whose label cell joins both; amounts stacked by design or written as one string are not. A column the grid drops, such as the sparse wash-sale column at a 30 pt pitch or a long statement's Amount column, follows the table an amount a line; that is reported as `table_values_detached` (Loop 19) where the page sets most of those amounts on the lines of the table's rows. |
| #443 | A text PDF whose Markdown is dropped as garbage keeps the detector's confidence 1.0 | Confidence 0 for a full run of a text PDF with no Markdown; `has_encoding_issues` set when a page carries `suspected_garbled_text`. |
| #407, #312 | A form XObject with indirect `/Resources`, or none, loses its fonts' Unicode maps or the text itself | #312 is reported as `form_text_unread` (Loop 18): a form drawn by a form without resources, which 1.24.0 never reads, and text a form shows in a font it does not set itself, which 1.24.0 reads byte by byte, where the font reads it otherwise. #407's high-code case stays covered by the garbled-text reason, now also an encoding issue. |
| #445 | A page with a small image and fewer than 10 text operators is read as a scan with no text, though region extraction reads it | Not detected; the page is listed for OCR, so nothing reads as complete. |
| #339 | Blank pages feed the sparse-extraction rule, which lists every page, text pages included, for OCR without a reason | Not detected; a misleading signal, not lost text. |
| #298 | Rotated column headers are scattered as single letters across cells | Not detected; numbers stay intact. |
| #299 | Raised note markers after a table | Mostly fixed by 1.24.0 (#488); the markers print after the table. No action. |

Fixtures and generators for all thirteen are kept with the review notes, not in
the corpus: they reproduce a dependency's defects, and the repository's own
tests generate the ones they need.

Disposition: **adopted.** The new fields are additive. `layout` and `cmap_gaps`
are omitted when a mode did not compute them, so absence is never reported as
"no tables" or "no gaps". Only creation and modification dates matching the PDF
date grammar are reported. The pre-existing `title` stays bounded to 1,024
characters. Author, subject, keywords, creator, and producer are
document-controlled free text and are not copied into responses.

## AnyDoc open pull requests

AnyDoc has no release after 0.2.4. It had 58 open pull requests on 2026-09-25.
Each was fetched from its `refs/pull/*/head` ref and compared with the pinned
parser. The table below lists them by their titles; an earlier revision of
this audit named some by their latest commit, and listed #160, closed on
2026-09-05, as open.

| PR | Title | Local disposition |
|---|---|---|
| #177 | fix(docx): preserve symbol checkbox states | The pinned parser drops every `w:sym`. The local DOCX preflight refuses symbols, legacy form checkboxes, and drop-downs outside tracked deletions (`incomplete_conversion`). The PR renders four Wingdings / Wingdings 2 checkbox codes; after adoption the refusal can relax for exactly those. |
| #176, #174 | fix(doc): preserve symbol checkbox states; omit deleted revision text | Legacy `.doc` only; that lane is disabled. |
| #175 | fix(pdf): bump pdf-inspector to 1.20.0 so RTL text extracts in logical order | Superseded: this repository calls pdf-inspector 1.24.0 directly for PDFs. |
| #171 | fix(markdown): preserve link and image destinations | Upstream output will carry more destinations, so the local sanitizer now decodes each destination before classifying it. It removes every external or local-path destination, including `mailto:`, `file:`, `data:`, `tel:`, UNC, and `www.` forms, and survives stray brackets and joined tags. |
| #169 | fix(package): classify recoverable allocation failures as resource limits | Already local: a worker killed by SIGABRT, SIGKILL, SIGSEGV, or SIGBUS maps to `resource_limit`. |
| #168 | feat(python): ship the anydoc CLI as a console script | Packaging only. |
| #166, #153 | fix(pdf): do not discard a text-based document over one confirmed OCR page; convert the readable pages when others need OCR | PDFs never reach AnyDoc here; the PDF tools already return the readable text and list the pages that need OCR, with reasons. |
| #164 | feat(eml): read RFC 5322 email messages | Email lanes are not enabled. |
| #163 | feat(sheet): preserve spreadsheet provenance | Sheet names and cell origins only; no cell text is lost. |
| #161 | feat: OCR scanned PDFs with a vision model via LiteLLM | Out of scope: the MCP boundary stays offline. |
| #158 | fix(xml): keep a part whose text carries a bare ampersand | Already fail-closed: a bare `&` in the DOCX body is `malformed`, and in a footnote the recovery diagnostic yields `incomplete_conversion`. |
| #154 | api: mark Format as `#[non_exhaustive]` | The next AnyDoc bump needs wildcard arms on `Format`; unknown formats must map to unrecognized or disabled, never to an enabled lane. |
| #151 | fix(xlsx): retain embedded worksheet images | 0.2.4 never follows a worksheet's drawings, so text boxes over a sheet were lost too. XLSX text boxes and shapes with text, and ODS drawings over the grid, are now refused; pictures and charts are a documented limitation. |
| #147, #149 | feat: add standalone HTML support; feat: add MHTML support | New formats, not enabled. Both change the HTML walker EPUB uses: 0.2.4 reads only `li` children of a list, so text or a paragraph placed directly in a list was lost from EPUB chapters. EPUB now refuses text a reader shows and AnyDoc drops. |
| #148 | fix: bound parser paths reachable from a crafted document | The number-format part is reproduced locally: an 8 MiB `formatCode` in a 10.6 KB workbook peaked at 855 MiB. The XLSX preflight refuses codes over 4,096 bytes with `resource_limit`. The legacy DOC, PPT, and RTF parts do not apply because those lanes are disabled. |
| #150, #152 | Documentation | No production change. |

### Older open pull requests

| PR | Title | Local disposition |
|---|---|---|
| #129 | fix(docx): continue counters across numIds that share an abstract | Reproduced. 0.2.4 keeps one counter per list instance, where Word keeps one per definition; its own upstream fixture shows "1. Two-one independent counter" where LibreOffice shows 5. It also numbers deleted paragraphs and renders ordinals and words as decimals. Such documents now convert as `partial` with `list_numbering_differs`. |
| #72 | fix(sheet): render xlsx number formats | Superseded by 0.2.4's own format engine. Three cases it still renders differently are refused: a negative marked only by a colour (−25,000 read as 25,000), a value a format hides, and a date format it cannot resolve (a serial number). |
| #39, #90 | Skip hidden worksheets; omit hidden rows and columns | Superseded: 0.2.4 omits both. The local preflight refuses them. |
| #16 | Preserve merged-cell spans past the used range | Superseded: 0.2.4 widens the grid. A merge over a whole row pads to 16,384 columns and returns `resource_limit`. |
| #17 | Unwrap single-cell tables that wrap a nested table | 0.2.4 flattens the nested table into `<br>`-joined lines. The local sanitizer removed those `<br>` tags, fusing "52,000" and "1,250" into "52,0001,250" in six lanes; it now keeps them. |
| #4 | EPUB metadata and figure captions | A container without block children is walked inline, so minified markup fused words ("Balance due1,250.00") and an image's alt text ran into its caption. Chapters where text a reader shows in separate blocks runs together are now refused; indented markup, joined with a space, converts. |
| #46 | Keep delimiters from pairing across runs | Superseded: 0.2.4 looks ahead across the paragraph. |
| #54 | Fix invalid EPUB spine references | 0.2.4 drops such entries; the local preflight refuses them. |
| #44 | Parse each package part once, and bound the total | Parts re-parsed per reference: 20,000 chart references are refused by the part cap, and 9,000 return `resource_limit` from the worker in 3.5 s. |
| #103, #130 | Accept passwords for encrypted OOXML files | Feature only. Their encrypted fixtures were refused as `malformed`, because the check read the stream names in ASCII; they are now reported as `encrypted`. |
| #19, #32, #69, #95, #126 | Heading fidelity, slide separators and anchors, sheet origins | Structure and metadata; no text is lost. |
| #7, #29, #30, #40, #42, #47, #48, #53, #55, #56, #61, #66, #70, #75, #83, #88, #89, #91, #98, #107, #145 | Bindings, CLI, packaging, documentation, PDF, OCR, legacy and new formats, CSV and RTF | Outside the enabled lanes. CSV and RTF stay unexposed per issue #104. |

## AnyDoc 0.2.4 behaviors the local contract does not inherit

Each behavior below was reproduced with synthetic packages through the real
worker. Each now has a hardening fixture from
`scripts/build-anydoc-hardening-corpus.py`, and the MCP integration tests
assert the local result.

| Behavior of the pinned parser | Local result | Fixture |
|---|---|---|
| `w:sym` characters (Wingdings checkboxes, Symbol-font letters) are dropped | `incomplete_conversion` | `docx/symbol-checkbox.docx` |
| A checked legacy FORMCHECKBOX loses its state | `incomplete_conversion` | `docx/legacy-form-checkbox.docx` |
| Hidden runs (`w:vanish`) convert as ordinary text | converted with the `hidden_content_preserved` warning | `docx/hidden-text.docx` |
| `mailto:`, `file:`, `data:`, `tel:`, and UNC link targets are written into Markdown | complete, every destination removed | `docx/external-link-schemes.docx` |
| Ruby text (`w:ruby`) loses its base text with the annotation | `incomplete_conversion` | `docx/ruby-text.docx` |
| Imported chunks (`w:altChunk`) that Word merges on opening are dropped | `incomplete_conversion` | `docx/alt-chunk.docx` |
| A non-breaking hyphen (`w:noBreakHyphen`) is dropped, joining "1040‑SR" into "1040SR" | `completeness: partial` with the `characters_omitted` warning | `docx/non-breaking-hyphen.docx` |
| One long `formatCode` amplifies into hundreds of MiB | `resource_limit` before conversion | `xlsx/oversized-number-format.xlsx` |
| Binary workbook records in `xl/workbook.xml` go to the XLSB reader | `unsupported` | `xlsx/binary-workbook.xlsx` |
| Namespace declarations are bindings, not attributes: `xmlns:Type` shadowed a relationship type for a name-only reader | `malformed` | `docx/namespace-shadowed-main.docx` |
| `visibility: hidden` from a linked stylesheet is ignored and the text converted | `incomplete_conversion` | `epub/linked-css-hidden.epub` |
| A UTF-16 part is transcoded before parsing, so a byte-level check misses it | checks read the transcoded text: `incomplete_conversion` | `docx/utf16-footnote-symbol.docx` |
| Hidden text through a style in a UTF-16 styles part | disclosed with `hidden_content_preserved` | `docx/utf16-hidden-style.docx` |
| `path::resolve` drops a fragment before applying `..`, so a slide target can leave `ppt/slides/` | `incomplete_conversion` | `pptx/fragment-slide-target.pptx` |
| Parts are read by exact name, so a case-variant decoy could stand in for them | `incomplete_conversion` | `pptx/case-variant-presentation-rels.pptx` |
| Spine hrefs are percent-decoded, so a decoy can sit under the encoded name | `incomplete_conversion` | `epub/encoded-chapter-href.epub` |
| Main parts are found by exact name, so a case-variant `XL/workbook.xml` sends AnyDoc to its binary reader | `malformed` | `xlsx/xlsb-fallback-decoy.xlsx` |
| A formula's cached value renders only when it parses for its cell type | `incomplete_conversion` | `xlsx/unrendered-formula-cache.xlsx`, `ods/untyped-formula-value.ods` |
| Speaker notes are reached through the slide's relationship, wherever stored | `incomplete_conversion` for hidden notes | `pptx/relocated-notes.pptx` |
| A table cell inside `mc:AlternateContent` is dropped by the row walker | `incomplete_conversion` | `docx/cell-in-compatibility-block.docx` |
| ODF walkers skip page-anchored frames and frames inside `draw:a` | `incomplete_conversion` | `odt/page-anchored-frame.odt`, `odp/linked-frame.odp` |
| An escaped CSS selector hides text a reader hides and AnyDoc converts | `incomplete_conversion` | `epub/escaped-selector.epub` |
| The HTML walker keeps only `li` children of a list, and its stylesheet split applies a media block's second rule everywhere | `incomplete_conversion` | `epub/list-text-outside-items.epub`, `epub/kindle-media-pair.epub` |
| A negative marked only by a colour renders unsigned | `incomplete_conversion` | `xlsx/negative-sign-by-colour.xlsx`, `ods/negative-sign-by-colour.ods` |
| A value its format hides is still held; ODS converts it in place of the empty display | `incomplete_conversion` | `xlsx/format-hidden-value.xlsx`, `ods/format-hidden-value.ods` |
| A locale date format outside its table renders as a serial number | `incomplete_conversion` | `xlsx/locale-date-format.xlsx` |
| Drawings over a sheet are never read | `incomplete_conversion` | `xlsx/drawing-text-box.xlsx`, `ods/cell-anchored-text-box.ods` |
| List counters are kept per instance, where Word keeps them per definition | `partial` with `list_numbering_differs` | `docx/shared-list-definition.docx` |
| A container without block children is walked inline, so blocks a reader shows apart run together | `incomplete_conversion` | `epub/minified-blocks.epub` |

Each of these converted as `complete`, with no warning, before the check that
now refuses or discloses it. Five fixtures must convert: `pptx/section-list.pptx`,
`epub/display-none-omitted.epub`, `epub/web-address-in-text.epub`,
`epub/indented-blocks.epub`, and `xlsx/red-parenthesized-negative.xlsx`.

The package checks now mirror AnyDoc's own reading:

- Every XML part is decoded as AnyDoc's `to_utf8` decodes it (the same
  `encoding_rs` crate).
- Every reference resolves through a port of `package::path::resolve`.
- DOCX notes, styles, and XLSX sheets follow AnyDoc's relationship lookup;
  sheets use any relationship type.
- The officeDocument relationship must name the part the checks read.
- Main parts are found by exact name, and an ODF package's lane by the first
  `office:body` in the office namespace.
- EPUB chapters are walked twice over: by a model of a reading system (CSS
  tokenizer, media queries, full selectors, cascade, user-agent rules) and by
  a port of AnyDoc's walker and stylesheet subset. Text one shows and the
  other hides or drops is refused (`epub_css`).
- ODF content is walked as AnyDoc's walkers walk it, frame by frame and cell
  by cell (`odf_walk`).
- XLSX number formats are read with AnyDoc's format grammar, section by
  section, against each cell's value (`xlsx_numfmt`).
- DOCX list numbers are replayed paragraph by paragraph, per instance as
  AnyDoc counts and per definition as Word counts, with each side's level
  formats, restarts, and style bindings.
- A negative's sign is judged against the number style's own marks: a
  colour alone is lost, while a minus, parentheses, `CR` or `DR` as words,
  or text of the negative section's own is kept.

Where AnyDoc picks one of several candidates (lowest id, a namespace-qualified
attribute), the checks cover every candidate. That can only make them stricter.
Streamed checks stop at AnyDoc's own bounds: XML depth 256 and 2,000,000 nodes
per part. Style bookkeeping is capped at 16,384 styles and 1,024-byte ids.

AnyDoc 0.2.4 also renders the DOCX body, footnotes, and endnotes, but not
headers, footers, or comments. The checks cover exactly the rendered parts.

## Domain parsers

pdf-inspector renders Title 26 section headings as Markdown headings
(`# §1398. …`). The IRC parser matched none of them, so it returned zero
sections on the public fixtures. It now reads the rendered Markdown:

- 14 sections and 284 provisions across the corpus, with no duplicate labels
- full labels such as `(d)(2)(A)(i)`
- repealed placeholders flagged (§1551, §1562, §1564)
- editorial and statutory notes kept separate from the operative text

## Dependency and security gates

- `cargo deny check`: advisories, bans, licenses, and sources pass; one
  duplicate-version warning (`syn`) remains.
- `cargo audit`: one allowed warning, `ttf-parser` unmaintained
  (`RUSTSEC-2026-0192`), reached through the PDF font stack.
- `chacha20` 0.10.0 was yanked; the lockfile resolves 0.10.2.
- `allow-git = []`: no Git source is permitted.

## Verification

Run on Linux x86-64 with Rust 1.94.1:

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets --locked -- -D warnings`
- `cargo test --workspace --locked`: 262 tests pass (207 skillkit unit, 13
  skillkit integration, 29 document-tool and 10 PDF-tool MCP integration, 3
  MCP unit)
- `cargo +1.88.0 check --workspace --all-targets --locked`, the declared
  minimum, also run in CI
- A third review round reproduced 43 packages that passed the checks while
  AnyDoc converted refused or undisclosed content, and a regression that
  refused decks using PowerPoint Sections. All 43 now fail closed or are
  disclosed, and 34 producer-shaped packages (Word, Excel, PowerPoint,
  LibreOffice, Sigil, and Calibre layouts) convert as before
- A fourth round reproduced 95 more bypasses. All fail closed or are
  disclosed, except two documented decisions: a closed `<details>` and SVG
  descriptions, which a reader shows on request, are treated like alt text
- A fifth round, checked against LibreOffice and AnyDoc's raw output, found
  4 missed losses and 12 false refusals in the checks added by loops 8-11
  (list numbers, colour-only negatives, drawings, empty-string formulas,
  clip-only PDF text), and 2 older false refusals (ODS charts refused as
  active content, empty-string formulas as uncached). All are fixed; none of
  the 303 documents below changed outcome.
- A sixth round, against the same references, found 4 missed losses (EPUB
  blocks inside links, a PDF run repainted through a second font object,
  DOCX lists continued into footnotes, a negative fraction's sign) and 7
  false positives (DOCX text boxes counted twice, table and repeat warnings
  on text pdf-inspector reads right or strips, a floated drop cap), and a
  quadratic ODF reference scan. All are fixed. The DOCX list replay now
  agrees with LibreOffice on all 320 randomized list documents from the
  fifth-round review, 38 of which it had missed. Among 206 review EPUBs,
  exactly the 20 reproductions changed to refused and the 2 false refusals
  to complete; among 71 review PDFs, each of the 21 changed warnings is a
  reviewer finding; none of the 303 documents changed outcome.
- A seventh round, against the same references and Chromium's layout,
  found EPUB boxes a reader sets apart that AnyDoc runs together (flex and
  grid items, table cells, SVG text, digits in floats, lone line breaks and
  display formulas in links, block images), signs and text generated by
  `::before` and `::after`, PDF merged cells the table warning missed and
  stacked amounts it flagged, DOCX lists through notes stored out of order
  or numbered by a style's own level, and a near-zero negative fraction
  refused. All are fixed, as are false refusals on drop caps set by rules
  the walk cannot settle and a large book that ran out of match budget.
  Among 116 review EPUBs, 24 reproductions changed to refused and 22 false
  refusals to complete, and no EPUB outside the review sets changed; the
  cost was 13 of 800 randomized chapters running words together under a
  sibling rule, converted. Loop 16 matches sibling selectors exactly: an
  oracle over Chromium's layout of the same 800 chapters finds every one
  of the 417 in which AnyDoc runs words together refused and none of the
  383 others, and among the 3,053 review and public EPUBs only one more
  review false refusal changed, to complete. Loop 17 reports words
  pdf-inspector splits in browser-printed text (#531): against the
  pdf-inspector commit that fixes it, over 400 randomized pages, it names
  203 of the 270 whose Markdown splits a word shown glyph by glyph and none
  of the 130 others; among the 256 PDFs, only the three #531 replicas
  changed.
- Across 303 documents (the public corpus, 39 LibreOffice conversions, the
  round-four regression corpus, AnyDoc's 34 upstream fixtures, and the
  pull-request reproductions), every outcome change was traced to a check
  and confirmed against LibreOffice or a reader. Among them, the round-four
  Kindle-pair sample and three upstream DOCX fixtures had converted with
  missing text or wrong list numbers.
- An oracle over the 30 EPUBs that convert as complete finds every text node
  a reader shows in the Markdown, apart from web addresses the sanitizer
  removes by design
- `bash scripts/check-public-hygiene.sh` and `cargo deny check`
- Golden comparison of every tool over the public corpus against the
  round-five build: 143 calls, identical tool list; the one changed output
  is a `sanitized_output` warning that the anchor-keeping sanitizer no
  longer raises on the public spine-order EPUB. Against the build before
  loop 13, the Title 26 sample gained `table_row_repeated`, whose rate table
  pdf-inspector repeats
- The page scan, which in a full run also reads every text page for repeated
  runs, adds 12-130 ms (20-30%) to `pdf_to_markdown` on the public text PDFs,
  nothing to classification, and nothing measurable for a 42 MB file, with
  the same peak memory
- `scripts/evaluate-upstream-abuse.py` against the AnyDoc mirror: 7/7
  `resource_limit`, recorded in `docs/resource-evidence.md`

## Next adoption checklist

When AnyDoc publishes a release after 0.2.4:

1. Add wildcard arms for the non-exhaustive `Format` (#154) that map to
   unrecognized or disabled.
2. If #177 is included, allow the four rendered Wingdings checkbox codes. Keep
   refusing every other `w:sym`, `w:checkBox`, and `w:ddList`.
3. Keep the number-format refusal even if #148 lands. Upstream renders long
   codes as General, which is lossy.
4. Re-run the hardening corpus and the golden comparison, and re-check the
   ported `to_utf8` and `path::resolve` against the new source. Any change
   there must be mirrored in the local checks before adoption.
5. Re-check the walker and format models against the new source: the HTML
   walker (#147, #149 keep a list's other children), the numbering counters
   (#129), the number-format grammar, and worksheet drawings (#151). Each
   model mirrors 0.2.4 and would refuse or disclose what a fixed release
   converts correctly.

When pdf-inspector publishes a release after 1.24.0:

1. If #479 or #501 is included, run the scanned-page fixtures through it and
   keep the local invisible-layer scan until they agree.
2. If #578 is included, route PDF Markdown through the sanitizer before
   adoption, since link destinations will appear in it.
3. Re-run the thirteen pull-request fixtures. Retire the
   `text_painted_twice`, `table_row_repeated`, `table_values_merged`, or
   `table_values_detached` warning only for a defect the release reads
   right, and update the `sample-2.pdf` and card-statement expectations in
   the PDF integration tests.
4. Re-run the sixth-round furniture, white-copy, and clip fixtures: the
   repeat warning is confirmed against the Markdown, so a release that
   strips or hides differently changes which pages it names.

## Sources

- <https://github.com/firecrawl/pdf-inspector>
- <https://github.com/firecrawl/anydoc>
- Previous audit: [`upstream-drift-audit-2026-08-28.md`](upstream-drift-audit-2026-08-28.md)
- Provenance ledger: [`upstream-provenance.md`](upstream-provenance.md)
