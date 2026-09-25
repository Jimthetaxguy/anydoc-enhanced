# Upstream drift audit — 2026-09-24

## Result

This refresh adopts the upstream parser releases published since the
2026-08-28 audit and converts what was learned from AnyDoc's open pull requests
into local fail-closed checks:

- `pdf-inspector` moves from 1.17.0 to 1.24.0 (`lopdf` 0.42.0 to 0.45.0). PDF
  parsing now runs inside the bounded worker, and the new upstream signals are
  reported additively.
- AnyDoc stays at 0.2.4, which is still the latest release; `main` is
  README-only after it. Seventeen open pull requests were reviewed against the
  pinned parser, and each is dispositioned below.
- `rmcp` moves from 3.1.4 to 3.4.1, and every tool declares read-only MCP
  annotations. Tool names and input schemas are unchanged.
- The document lanes gain checks for AnyDoc 0.2.4 behaviors that the local
  contract must not inherit. These are content the parser drops silently, and
  places where the parser and the local checks could read different documents.

No upstream source was copied into this repository. Every dependency resolves
from crates.io, and cargo-deny now rejects Git sources (`allow-git = []`).

## Observed upstream state

Observed 2026-09-24 from anonymous clones and the crates.io API; re-verified
2026-09-25 with no change.

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

Disposition: **adopted.** The new fields are additive. `layout` and `cmap_gaps`
are omitted when a mode did not compute them, so absence is never reported as
"no tables" or "no gaps". Only creation and modification dates matching the PDF
date grammar are reported. The pre-existing `title` stays bounded to 1,024
characters. Author, subject, keywords, creator, and producer are
document-controlled free text and are not copied into responses.

## AnyDoc open pull requests

AnyDoc has no release after 0.2.4. Each open pull request was fetched from its
`refs/pull/*/head` ref and compared with the pinned parser.

| PR | Subject | Local disposition |
|---|---|---|
| #177 | fix(docx): preserve symbol checkbox states; reject malformed symbol codes | The pinned parser drops every `w:sym`. The local DOCX preflight refuses symbols, legacy form checkboxes, and drop-downs outside tracked deletions (`incomplete_conversion`). The PR renders four Wingdings / Wingdings 2 checkbox codes; after adoption the refusal can relax for exactly those. |
| #176 | fix(doc): preserve symbol checkbox states | Legacy `.doc` only; that lane is disabled. |
| #148 | fix: bound parser paths reachable from a crafted document | The number-format part is reproduced locally: an 8 MiB `formatCode` in a 10.6 KB workbook peaked at 855 MiB. The XLSX preflight refuses codes over 4,096 bytes with `resource_limit`. The legacy DOC, PPT, and RTF parts do not apply because those lanes are disabled. |
| #171 | fix(markdown): preserve link and image destinations | Upstream output will carry more destinations, so the local sanitizer now decodes each destination before classifying it. It removes every external or local-path destination, including `mailto:`, `file:`, `data:`, `tel:`, UNC, and `www.` forms, and survives stray brackets and joined tags. |
| #169 | fix(package): classify recoverable allocation failures as resource limits | Already local: a worker killed by SIGABRT, SIGKILL, SIGSEGV, or SIGBUS maps to `resource_limit`. |
| #158 | fix(xml): keep a part whose text carries a bare ampersand | Already fail-closed: a bare `&` in the DOCX body is `malformed`, and in a footnote the recovery diagnostic yields `incomplete_conversion`. |
| #154 | docs: clarify the `#[non_exhaustive]` migration requirement | The next AnyDoc bump needs wildcard arms on `Format`; unknown formats must map to unrecognized or disabled, never to an enabled lane. |
| #175 | fix(pdf): bump pdf-inspector to 1.20.0 for right-to-left text | Superseded: this repository calls pdf-inspector 1.24.0 directly for PDFs. |
| #174 | fix(doc): omit deleted revision text | Legacy `.doc` only; disabled lane. |
| #153 | feat(pdf): convert the readable pages when others need OCR | Not applicable: PDFs never reach AnyDoc here. |
| #161 | feat: OCR scanned PDFs with a vision model via LiteLLM | Out of scope: the MCP boundary stays offline. |
| #147, #149 | HTML tree-builder end-tag and scope fixes | Not reachable from the enabled lanes; EPUB chapters are parsed as XML. Revisit with the next release. |
| #160, #164 | `.msg` code page and `.eml` edge cases | Email lanes are not enabled. |
| #163, #166 | Documentation and review follow-ups | No production change. |

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
| One long `formatCode` amplifies into hundreds of MiB | `resource_limit` before conversion | `xlsx/oversized-number-format.xlsx` |
| A UTF-16 part is transcoded before parsing, so a byte-level check misses it | checks read the transcoded text: `incomplete_conversion` | `docx/utf16-footnote-symbol.docx` |
| Hidden text through a style in a UTF-16 styles part | disclosed with `hidden_content_preserved` | `docx/utf16-hidden-style.docx` |
| `path::resolve` drops a fragment before applying `..`, so a slide target can leave `ppt/slides/` | `incomplete_conversion` | `pptx/fragment-slide-target.pptx` |
| Parts are read by exact name, so a case-variant decoy could stand in for them | `incomplete_conversion` | `pptx/case-variant-presentation-rels.pptx` |
| Spine hrefs are percent-decoded, so a decoy can sit under the encoded name | `incomplete_conversion` | `epub/encoded-chapter-href.epub` |

Before this refresh, the last five packages each converted as `complete`, with
no warning, while carrying content the policy refuses or discloses.

The package checks now mirror AnyDoc's own reading:

- Every XML part is decoded as AnyDoc's `to_utf8` decodes it (the same
  `encoding_rs` crate).
- Every reference resolves through a port of `package::path::resolve`.
- DOCX notes, styles, and XLSX sheets follow AnyDoc's relationship lookup;
  sheets use any relationship type.
- The officeDocument relationship must name the part the checks read.

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
- `cargo test --workspace --locked`: 169 tests pass (125 skillkit unit, 12
  skillkit integration, 24 document-tool and 5 PDF-tool MCP integration, 3
  MCP unit)
- `bash scripts/check-public-hygiene.sh`
- Golden comparison of every tool over the public corpus against the previous
  review build: 113 calls, no changed output, identical tool list
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

## Sources

- <https://github.com/firecrawl/pdf-inspector>
- <https://github.com/firecrawl/anydoc>
- Previous audit: [`upstream-drift-audit-2026-08-28.md`](upstream-drift-audit-2026-08-28.md)
- Provenance ledger: [`upstream-provenance.md`](upstream-provenance.md)
