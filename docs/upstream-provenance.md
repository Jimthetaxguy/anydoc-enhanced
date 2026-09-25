# Upstream provenance

This repository is a public MCP wrapper and domain layer. It does not vendor
Firecrawl source. Upstream source is maintained in sibling local mirrors and
selected production revisions are recorded here.

## Tracked upstreams

| Logical mirror | Repository | Local production reference | Latest release observed | Upstream `main` observed | License | Audit date |
|---|---|---|---|---|---|---|
| `firecrawl-pdf-inspector` | <https://github.com/firecrawl/pdf-inspector> | crates.io `1.24.0`, checksum `e22dc125a533d212c847c8c85e4fcb7358f4384869ef76b2b8721f039b1b633a`; PDF parsing runs in the bounded worker | Git tag `v1.24.0` at `876fe9ac65c1b05512b9a1a182b5c56bcfdd6c39` | `f856d3481d41d564c64d20baa2a4796d98aed03c` (release CI only after the tag) | MIT | 2026-09-24 |
| firecrawl-anydoc | https://github.com/firecrawl/anydoc | crates.io anydoc 0.2.4, checksum recorded in Cargo.lock; local worker enables DOCX, exact PPTX, XLSX, ODS, ODT, Linux-memory-gated ODP, and Linux-memory-gated strict EPUB | v0.2.4 at 42bf1c5ecdde9eb0d96d6bd75a9e6698cf93b14c (still the latest release) | 261fc257d17c3eab0f673be31c408fd9fdc2171a (README only after the tag); 17 open pull requests reviewed | MIT | 2026-09-24 |

Local sibling mirror conventions:

- `_upstream-mirrors/firecrawl-pdf-inspector`
- `_upstream-mirrors/firecrawl-anydoc`

Absolute machine paths are intentionally excluded from this public record.

## Disposition ledger

| Upstream change | Disposition | Rationale |
|---|---|---|
| `pdf-inspector` `1.17.0` registry package | Superseded by `1.24.0` | Adopted in the 2026-08-28 alignment. |
| `pdf-inspector` `1.24.0` registry package (`lopdf` 0.45.0) | Adopted | Bounded object streams cut an object-stream bomb from about 1.1 GiB to 19 MiB peak RSS; golden output differs by one line. Per-page OCR reasons, layout, CMap gaps, and validated creation and modification dates are reported additively. See [`upstream-drift-audit-2026-09-24.md`](upstream-drift-audit-2026-09-24.md). |
| Unbounded page-content reads remaining in `pdf-inspector` 1.24.0 | Contained locally | A page-content bomb still peaks at 2.1 GiB in-process, so every PDF tool now runs in the bounded worker (1 GiB address-space ceiling on Linux, 25-second deadline, four slots) and returns `resource_limit`. |
| `pdf-inspector` current-main changes beyond the published package | Not adopted | Release CI only after `v1.24.0`. |
| AnyDoc v0.2.4 typed NeedsOcr behavior and scanned-PDF fixtures | Resolved for parser convergence; bounded DOCX, strict PPTX, strict XLSX, strict ODS, strict ODT, strict ODP and strict EPUB worker paths adopted; CSV is handled by a separate local adapter | The local contract, versioned worker, offline default, sanitizer, public fixtures, OOXML/ODF/EPUB preflight, exact-variant gates, and declared-part completeness checks are implemented; RTF and broader format completeness remain deferred. |
| AnyDoc `main` documentation-only commits after `v0.2.4` | Not adopted | No production code or dependency change identified. |
| AnyDoc open pull requests (#147–#177), reviewed 2026-09-24 | Mirrored locally where they expose a pinned-parser defect; not merged | Symbol and form checkboxes (#177) and number-format amplification (#148) fail closed; destination handling (#171) is covered by the sanitizer; allocation-failure signals (#169) and bare ampersands (#158) were already fail-closed; legacy, email, PDF, and OCR changes do not reach the enabled lanes. Dispositions per pull request are in the 2026-09-24 audit. |
| AnyDoc 0.2.4 `to_utf8` transcoding and `package::path::resolve` | Mirrored locally | The preflight decodes every XML part and resolves every package reference exactly as the pinned parser does, so a UTF-16 part or a decoy under a differently resolved name cannot hide content from the checks. Five hardening fixtures reproduce the divergences. |
| Local strict PPTX policy | Intentionally local | AnyDoc skips unreadable slides with log-only diagnostics, so this repository enables only exact visible, non-macro `.pptx` packages after declared-slide, shape-tree, external-relationship, and active-content preflight. |
| Local strict XLSX policy | Intentionally local | AnyDoc exposes a broad Excel parser, but this repository enables only exact `.xlsx` with visible content, cached formula values, no external links, and no active/binary/legacy content. |
| Local strict ODT policy | Intentionally local | AnyDoc ODF text conversion can omit unsupported or unavailable parts without a typed completeness result; this repository enables only exact visible text packages after XML, manifest, hidden/tracked, external, active, encryption, internal-asset, and known unsupported-note preflight. |
| Local DOCX main-part XML gate | Intentionally local | AnyDoc’s shared XML recovery can turn an unclosed required document part into a success-shaped result; the local boundary now requires balanced `word/document.xml` before conversion while retaining DOCX external relationships as contained warnings. |
| Local AnyDoc omission classifier | Intentionally local | AnyDoc emits recoverable omissions through `log`; the private worker logger retains only a fixed omission/recovery signal and maps it to stable `incomplete_conversion`, never exposing raw messages or paths. |
| Upstream AnyDoc abuse corpus | Evaluated, not vendored | `scripts/evaluate-upstream-abuse.py` runs the sibling mirror fixtures through the real bounded worker; the reviewed Darwin run passed 7/7 `resource_limit` cases with no source or fixture copy added to the public repository. |

## ODS audit decision

The strict ODS route was selected after the source-backed SOL use-case review and
the independent MiniMax adversarial review. AnyDoc v0.2.4 exposes ODS through
its public Format, to_markdown_bytes, and table parser; the relevant source
modules are src/formats/odf/mod.rs and src/formats/odf/table.rs at the reviewed
release commit 42bf1c5ecdde9eb0d96d6bd75a9e6698cf93b14c. The refreshed upstream
main commit 261fc257d17c3eab0f673be31c408fd9fdc2171a has no production-code
delta from that release in the reviewed range.

Adopted local delta: exact ODS mimetype/content identity, well-formed content,
visible spreadsheet/table requirement, cached/displayed formula policy,
external-reference rejection, encrypted-manifest rejection, active
object/script rejection, and the existing worker bounds. The public fixture is
test-corpus/ods/public-workpaper.ods, copied from the MIT-licensed upstream
sheet.ods fixture and hashed in the corpus manifest.

Deferred candidates: RTF and legacy/macro Office formats remain gated behind
independent parser and active-content reviews. ODP and EPUB are separately
qualified under the implementation decisions below. Initial hostile-resource evidence for the
reviewed upstream abuse corpus is recorded in [`docs/resource-evidence.md`](resource-evidence.md), but broader platform gates remain open.

## ODT audit decision

The fresh source-backed SOL review selected strict ODT as an additive format for
research memoranda, engagement letters, procedures, evidence narratives, and
exported workpapers. The independent MiniMax review concurred that ODF has a
bounded ZIP/XML foundation but identified recovery and optional asset behavior
that cannot be represented as complete success by AnyDoc alone.

Source evidence: AnyDoc v0.2.4 release commit
42bf1c5ecdde9eb0d96d6bd75a9e6698cf93b14c, refreshed upstream main
261fc257d17c3eab0f673be31c408fd9fdc2171a, and the ODF text modules
src/formats/odf/mod.rs and src/formats/odf/text.rs. The reviewed main range
contains no production-code or manifest delta beyond the release; no
dependency update or source import is required.

Adopted local delta: worker code 5; exact ODT mimetype and content identity;
balanced well-formed XML; required office:text content; rejection of hidden
text, tracked changes, annotations, conditional text, external references,
encrypted manifests, scripts/forms/objects, missing internal href/src targets,
and unsupported `text:note` elements that the pinned parser omits; the
existing worker input/output/time/concurrency/sanitizer bounds; and path-free
stable error mapping.

Evidence is recorded in the synthetic public omission fixture
test-corpus/odt/public-research-memo.odt, the positive minimal.odt fixture, and
negative fixtures for hidden or tracked content, external references, active
forms, missing images, malformed content, encryption, wrong identity, and
missing content. The MCP integration test exercises the real worker and
asserts the provider identity, schema version, visible sentinels, and absence
of paths, HTML, and remote URLs; the omission oracle now fails closed for
`text:note`. ODT is enabled for this narrow contract; strict EPUB is now enabled under
its own EPUB 3 completeness and Linux-memory gate. RTF and broader format
promotion remain deferred until each format has its own resource and platform
evidence.

## ODP audit decision

This iteration selected strict ODP as the next source-aligned additive route
after live inspection of the exact AnyDoc v0.2.4 release and current main.
AnyDoc exposes ODP through its public Format and to_markdown_bytes APIs; the
relevant parser is src/formats/odf/mod.rs, whose presentation path extracts
slide text, tables, grouped shapes, images, and speaker notes. Current main
261fc257d17c3eab0f673be31c408fd9fdc2171a is README-only after the v0.2.4
release, so no dependency update or upstream-main import is justified.

The local delta adds worker code 7, exact ODP mimetype/content identity,
balanced XML, a required presentation body with at least one page, local asset
existence checks, and fail-closed active, external, hidden, encrypted,
malformed, and archive-limit behavior. AnyDoc output remains private to the
worker boundary; layout fidelity, animations, source coordinates, embedded
object execution, and external fetching are not claimed.

The public fixture is test-corpus/odp/public-presentation.odp, copied from the
MIT-licensed AnyDoc v0.2.4 fixture at tests/fixtures/odp/pres.odp. Its hash and
the hashes of deterministic derivatives are recorded in the corpus manifests.
The private worker converts the real fixture on Darwin and emits the expected
Markdown markers. The MCP route is enabled only on Linux, where the worker
address-space ceiling is enforceable. This is an additive local adapter
contract, not a source merge.

## CSV audit decision

The prior reconciled adversarial review identified CSV as a high-value lane for
bank, general-ledger, payroll, and 1099 exports, while flagging AnyDoc issue
#104, materialized row storage, delimiter/header inference, and skip-on-error
behavior. Live inspection of AnyDoc v0.2.4 and current main confirmed those
risks. No AnyDoc CSV code was copied and no new parser dependency was added.

Adopted local delta: worker code 6; a sibling module implements strict UTF-8
(with optional BOM), deterministic delimiter sniffing, RFC-4180-style quoting,
equal-width rows, row/column/field/output bounds, and Markdown escaping. The
provider record is local-csv with source firecrawl/anydoc to show the reviewed
lineage without falsely claiming that AnyDoc performed the conversion. The
route is enabled only on Linux, where the existing worker address-space ceiling
is enforceable; classification remains available on all platforms.

Evidence is the synthetic non-PII fixture test-corpus/csv/public-bank-export.csv,
the parser unit tests, and the Linux-target MCP integration test. The upstream
AnyDoc CSV and RTF frontends remain explicitly unexposed. This slice is not a
license or dependency update and must not be broadened to permissive row
skipping, inferred encoding, or unconstrained materialization.

## EPUB audit decision

The source-backed review identified EPUB as a high-value long-form use case for
technical manuals, legal/research publications, regulations, and policy books.
AnyDoc v0.2.4 exposes the EPUB frontend through its public `Format::Epub` and
`to_markdown_bytes` APIs; refreshed upstream `main` is README-only after the
release, so no dependency update or source import is justified.

The local route closes the pinned frontend's partial-success behavior with exact
stored-first OCF `mimetype`, one container rootfile, OPF/manifest/spine
resolution, all declared spine-target checks, XHTML/body validation, navigation
agreement, local-reference existence, active/external/hidden-content rejection,
encryption rejection, archive limits, and stable incomplete-conversion mapping.
Worker code 8 keeps the provider-neutral protocol explicit. The route is enabled
only on Linux, where the worker address-space ceiling is enforceable; EPUB 2,
layout fidelity, source coordinates, embedded-object execution, and external
fetching remain out of contract.

The evaluator and deterministic fixtures live in
`crates/pdf-inspector-skillkit/src/document.rs`; the tracked synthetic public
corpus, generator, and executable oracle live in `test-corpus/epub/` and
`scripts/build-epub-corpus.py`. The real worker test proves chapter-order output,
provider identity, path/HTML/URL absence, and fail-closed negative fixtures on
Linux. The SOL and MiniMax sidecars returned no usable memo in this cycle, so no
specialist approval is inferred; promotion rests on live source inspection and
reproducible local evidence. No upstream source was copied, no Cargo dependency
changed, and PDFs remain on the dedicated pdf-inspector route.

## Deferred-format review and promotion gate

The ODP sidecar review attempt in this iteration produced no usable SOL or
MiniMax memo (SOL timed out and the MiniMax transport closed), so no specialist
approval is inferred. The disposition below is based on direct inspection of
the refreshed AnyDoc release/main mirrors, the local contract, and reproducible
fixture and worker evidence.

- **ODP:** promoted as a separate strict local contract with exact identity,
  visible complete slides, local assets, active/external/hidden rejection,
  malformed/encrypted/archive limits, and Linux-only MCP enablement while the
  worker address-space ceiling is enforceable.
- **EPUB:** promoted as a Linux-memory-gated strict EPUB 3 route. Local
  preflight rejects partial-success cases, requires all-spine completeness and
  navigation order, and blocks external/active/hidden/encrypted/archive-abuse
  inputs; filesystem, cross-platform, and broader hostile-input evidence remain
  open gates.
- **CSV:** the upstream parser remains unexposed because it materializes input,
  infers delimiter/header structure, and skips malformed records. The local
  strict CSV lane is already separate, bounded, and Linux-memory-gated.
- **RTF:** remains deferred because control words, codepages, group recovery,
  and embedded binary objects require an independent completeness and safety
  model.

The following upstream behavior must not be inherited by any future lane:
log-only content omission, success on partial conversion, permissive malformed
input recovery presented as complete output, external destinations treated as
ordinary content, and silent dropping of unknown or unsupported structures.
Cross-host memory, filesystem, and additional hostile-input evidence remain
open promotion dependencies for broader formats.

## Maintenance procedure

Refresh both sibling mirrors before each dependency or integration review. Record
the observed release, default-branch commit, and audit date here. Review release
notes and commit ranges against the local source map, then run the isolated
compatibility and fixture gates before changing `Cargo.toml` or `Cargo.lock`.

The public repository remains dependency-first: use a released crate where its
API and behavior satisfy the local contract; preserve an exact revision only
when a release is insufficient; never copy upstream source into this tree
without an explicit provenance, license, and maintenance decision.
