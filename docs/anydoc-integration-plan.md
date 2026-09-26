# Firecrawl AnyDoc integration plan

**Evidence date:** 2026-08-28; refreshed 2026-09-24 (see [`upstream-drift-audit-2026-09-24.md`](upstream-drift-audit-2026-09-24.md))
**Plan status:** parser convergence complete; bounded worker, contract, DOCX happy path, strict PPTX, strict XLSX, strict ODS, strict ODT, Linux-memory-gated strict CSV, Linux-memory-gated strict ODP, and Linux-memory-gated strict EPUB slices implemented; broader formats remain gated
**Target:** additive multi-format document support without changing the 13
existing PDF/domain MCP tools plus additive generic document tools

## Decision

Integrate Firecrawl AnyDoc as an exact-version Rust dependency behind the
skillkit boundary. Keep every PDF on the existing PDF adapter. Expose
allowlisted non-PDF formats through a supervised, killable worker built from
the same Rust workspace.

No non-PDF format is enabled merely because `AnyDoc::to_markdown` returns
`Ok`. The current worker enables DOCX, exact `.pptx`, exact `.xlsx`, exact `.ods`, exact `.odt`, exact `.odp`, and strict EPUB through AnyDoc; strict CSV uses a separate local adapter and CSV, ODP, and EPUB are enabled only on Linux where the worker address-space ceiling is enforceable. PPTX requires every declared slide to resolve to a well-formed shape tree and fails closed for hidden slides, external relationships, active content, and incomplete packages. XLSX uses a local cached-value-only policy and fails closed for hidden content, external links, active content, macro-enabled/binary/legacy containers, malformed packages, and uncached formulas. Broader parser paths remain disabled until completeness is observable through a typed signal or the conversion fails closed.

The package preflight reads each package the way the pinned parser does. It decodes every XML part as AnyDoc's `to_utf8` decodes it, and resolves every reference with a port of `package::path::resolve`. It reads the parts AnyDoc reads by their exact names, and requires the officeDocument relationship to name the checked part. A check that saw a different document than the parser would otherwise let unchecked content through as complete output; the 2026-09-24 refresh reproduced five such packages and added them to the hardening corpus.

Do not use the Node CLI, `npx`, Python bindings, WASM wrapper, Docker, or the
hosted Firecrawl Parse API. The local library path is offline and does not need
an API key. The hosted API uploads documents and is a different privacy and
authorization boundary.

The upstream AnyDoc CSV and RTF paths remain unexposed because issue #104 documents materialization and memory-exhaustion risk. The local strict CSV adapter does not use AnyDoc's CSV parser: it enforces UTF-8, delimiter, quoting, row/column/field/output, and equal-width-row limits before producing Markdown, and it is enabled only on Linux with an address-space ceiling.

## Verified upstream baseline

| Property | Observed evidence |
|---|---|
| Repository | [`firecrawl/anydoc`](https://github.com/firecrawl/anydoc) |
| Version and revision | `v0.2.4` at signed commit `42bf1c5ecdde9eb0d96d6bd75a9e6698cf93b14c` |
| License | MIT |
| Core runtime | Rust 2024; minimum supported Rust `1.88` |
| Public Rust API | `to_markdown`, `to_markdown_bytes`, `to_document`, and content/extension format detection |
| Local network behavior | No HTTP client, telemetry, update checker, API-key lookup, external service, or ML model in the resolved local runtime |
| Formats | Word, PowerPoint, Excel, OpenDocument, RTF, EPUB, CSV, and PDF variants |
| PDF behavior | Declares `pdf-inspector 1.14.2` compatibility; this workspace converges that requirement to released `1.25.0`; PDFs bypass AnyDoc's shared document model; image-only PDFs require OCR |
| Safety controls | Fixed archive, decompression, XML, table, binary-record, and retained-asset limits |
| Upstream tests | 294 root-crate tests passed and 1 was ignored at the pinned revision; checked-in fixtures and fuzz targets exist, but fuzz targets are not exercised by upstream CI |

The upstream `Document` model is not a suitable MCP contract. It lacks Serde
schemas, provider/version metadata, stable warnings, page/slide/source
coordinates, and a PDF representation. Embedded assets contain raw bytes and
must not be serialized through MCP by default.

Upstream benchmark results are directional only: the 100-document corpus is
not redistributable, PDFs are excluded, and the quality judge is not locally
reproducible. This repository needs its own public acceptance corpus.

## Why the initial sidecar assumption changed

The earlier QA review correctly asked for isolation if AnyDoc were a different
runtime. Live inspection shows that it is a native Rust library, so a remote
service or language sidecar would add needless supply-chain and deployment
surface.

Process isolation is still required at the MCP execution boundary. Tokio
cannot abort a `spawn_blocking` closure after it starts; dropping the join
handle only stops waiting for it. A response timeout therefore does not prove
that a pathological parser stopped consuming CPU or memory. The worker below
is a small Rust binary using the same library, not an external service. It uses
versioned format negotiation, private working-directory state, process-group
cleanup on Unix, a Linux address-space ceiling plus seccomp network-denial
filter, and Darwin named `no-network` profile; platform-specific hostile-input
promotion remains a separate gate.

## Target architecture

```text
                                  existing 13 tools
                                         |
                                         v
                                +------------------+
                                |    PDF adapter   |
                                +--------+---------+
                                         |
                                         v
MCP stdio  ->  document service/router  ->  pdf-inspector =1.25.0
                     |
                     | allowlisted non-PDF input bytes
                     v
             supervised anydoc-worker
             (workspace Rust binary)
                     |
                     v
               AnyDoc adapter
               anydoc =0.2.4
                     |
                     v
          completeness + output sanitizer
```

Both adapters live behind `pdf-inspector-skillkit`. Production MCP conversion
uses the worker for non-PDF parsing; contract tests may invoke the real adapter
in-process. The worker receives document bytes on stdin, never a shell command
or user path, and returns Markdown on bounded stdout.

## Deterministic routing policy

Routing uses content signatures first. An extension may resolve a
signature-less format only when that format is enabled. There is no
try-every-parser chain.

| Input | Route | Initial status | Reason |
|---|---|---|---|
| PDF | Existing PDF adapter | Enabled | Preserves classification, page/OCR diagnostics, regions, and domain helpers |
| DOC/DOCX/DOCM | AnyDoc worker | DOCX enabled; DOC/DOCM disabled | Exact DOCX package only; macros are not executed |
| PPT/PPS/POT/PPTX/PPTM/PPSX/PPSM | AnyDoc worker | Exact `.pptx` enabled; other presentation variants disabled | Every declared slide must resolve; hidden, external, active, malformed, and incomplete packages fail closed |
| XLS/XLSX/XLSM/XLSB | AnyDoc worker | Exact `.xlsx` enabled; XLS/XLSM/XLSB disabled | Cached values only; hidden, external, active, incomplete, binary, and legacy variants fail closed |
| ODT | AnyDoc worker | Exact ODT enabled | Visible text only; exact ODF identity, balanced XML, hidden/tracked/external/encrypted/active/missing-asset preflight; unsupported optional parts remain incomplete |
| ODS | AnyDoc worker | Exact `.ods` enabled | Exact mimetype/content identity, visible non-active tables, cached/displayed values, and fail-closed hidden/external/encrypted/active/uncached checks |
| ODP | AnyDoc worker | Linux-memory-gated strict ODP | Exact presentation identity, visible pages, local assets, active/external/hidden rejection, and real worker evidence; layout/source coordinates remain out of contract |
| EPUB | AnyDoc worker | Linux-memory-gated strict | Exact EPUB 3 OCF/container/OPF/spine preflight, complete navigation order, local-resource checks, active/external/hidden/encrypted rejection, tracked corpus, executable oracle, and real-parser chapter-order/omission evidence; public worker/MCP route is enabled only where the worker address-space ceiling is enforceable |
| CSV | Local strict adapter | Linux-memory-gated | Valid UTF-8, deterministic delimiter sniffing, RFC-4180-style quoting, equal-width rows, bounded fields/output, Markdown escaping, synthetic public fixture, and parser/MCP tests; upstream AnyDoc CSV behavior is not inherited |
| RTF | None | Disabled | Upstream #104 memory-exhaustion risk |
| Image-only or scanned PDF | None | Typed `ocr_required` error | No silent upload or hosted OCR fallback |
| Unknown, encrypted, oversized, or disallowed input | None | Typed hard error | Never fall back after a security or permission failure |

No existing PDF tool changes its route. PDFs continue to use the dedicated PDF
tools and are never sent through AnyDoc by the generic document route.

Strict ODS now adds an exact package identity gate, well-formed content check, visible-table requirement, cached/displayed formula policy, and rejection of external references, encrypted manifests, active objects/scripts, and hidden table content. An AnyDoc success value is not evidence of a complete conversion. At the
pinned version, EPUB chapters, presentation slides, spreadsheet sheets, and
archive parts can be skipped while the call still succeeds. Upstream log text
is neither a public warning schema nor safe to forward. The worker now installs
a private sink logger and maps the reviewed AnyDoc v0.2.4 omission and
malformed-recovery prefixes to stable `incomplete_conversion` without retaining
raw messages; known public fixtures now have structural marker oracles, while
unobserved silent omissions still require additional cases. A format stays disabled
unless an audited, typed completeness mechanism covers
every recoverable skip path; any detected skip becomes `incomplete_conversion`.

Strict ODT now adds an exact mimetype/content identity gate, balanced
XML validation, required office:text content, manifest encryption detection,
and rejection of hidden text, tracked changes, annotations, conditional text,
external references, active forms/scripts/objects, missing internal href/src
targets, and unsupported `text:note` content that AnyDoc 0.2.4 omits. The
public synthetic ODT corpus includes one positive fixture and negative cases for
each exercised boundary; the real MCP worker test asserts
provider identity, visible output sentinels, and path/HTML/URL absence. ODT is
enabled for this narrow visible-text contract. Strict ODP is now an additive Linux-memory-gated presentation route, and strict EPUB is now an additive Linux-memory-gated EPUB 3 route, each with its own package and completeness preflight.
Initial Darwin/arm64 release-mode resource observations for each enabled lane are
recorded in [`docs/resource-evidence.md`](resource-evidence.md); this is baseline
evidence, not closure of cross-host memory, filesystem, hostile-resource, or
non-Linux containment gates. The worker-level network canaries are scoped evidence
on Darwin, with Linux enforcement covered by the target build and CI containment
job.

### Strict ODP

Strict ODP uses AnyDoc v0.2.4 only after a local preflight. The route requires
the exact presentation mimetype, content.xml, balanced XML, an
office:presentation body with at least one page, local referenced assets, and
no active, external, or hidden presentation content. Encryption, malformed
packages, wrong identity, missing assets, and archive amplification map to
typed failures. Worker code 7 keeps the provider-neutral protocol explicit.

The public fixture is copied from the MIT-licensed AnyDoc v0.2.4 ODP fixture;
the repository records its hash and deterministic derivatives. The real worker
path is covered on Darwin, while public MCP conversion is enabled only on
Linux where the process address-space ceiling is enforceable. There is no
dependency update, vendored source, PDF route change, layout-fidelity claim,
or source-coordinate claim.

Encrypted OOXML commonly uses an OLE container with `EncryptedPackage` and
`EncryptionInfo` streams, for which AnyDoc content detection can return
unknown. Run an explicit encryption preflight before extension fallback so the
router returns `encrypted` rather than `unsupported` or selecting a parser by
filename alone.

## Provider-neutral contract

Keep upstream types private. Define local, serializable types with an explicit
wire schema version:

- `DocumentKind`: stable format family and optional container variant.
- `DocumentProvider`: name, exact version, source revision/checksum, and local
  execution class.
- `DocumentCapabilities`: enabled formats, disabled reason, OCR availability,
  structured-model availability, diagnostic availability, and input/output
  limits.
- `DocumentClassification`: detected kind, confidence/diagnostics when the
  provider actually supplies them, and selected route.
- `DocumentContent`: Markdown, byte count, provider chain, warnings, and
  `schema_version`.
- `DocumentWarning`: typed diagnostics only. Do not turn AnyDoc's unstable log
  text into a public schema. Local sanitizer warnings such as
  `remote_asset_removed` are allowed because this repository owns them.
- `DocumentError`: stable codes for not found, not regular file, too large,
  unsupported, disabled format, malformed, encrypted, resource limit,
  incomplete conversion, timeout, worker failure, output too large, and OCR
  required.

Provenance records the full provider chain. A PDF result identifies
`pdf-inspector`; an office result identifies `anydoc`. Do not claim page,
slide, sheet, or source-offset provenance that upstream does not expose.

## Operational controls

- Canonicalize the input and require a regular file before reading it.
- Retain the current 50 MiB input cap unless corpus measurements justify a
  smaller per-format cap.
- Send bytes to the worker through stdin so it has no ambient input-path
  authority.
- Spawn with an argv array, never a shell; clear the environment and add only
  the minimum runtime variables.
- Install a sink logger before invoking AnyDoc. Upstream log messages can
  contain document-controlled sheet names, URLs, internal part names, cell
  values, and formatting values; never forward or persist them.
- Map upstream errors by audited variant to constant local error codes. Never
  include upstream `part`, `detail`, `Display`, or debug strings in stdout,
  stderr, telemetry, or MCP responses.
- Keep stdout reserved for the versioned IPC envelope and stderr for the
  worker's own bounded, path-free structured events.
- Stop reading and kill the worker if stdout exceeds 8 MiB. Return
  `output_too_large`; never silently truncate document content.
- Kill and reap the worker/process group when the 15-second worker deadline expires,
  on protocol/output failure, or after caller cancellation. The cancellation supervisor
  retains the in-flight permit until cleanup completes; the outer MCP document-tool
  timeout remains 30 seconds.
- Establish an OS memory ceiling from measured public/adversarial fixtures,
  with an absolute ceiling of 1 GiB. A platform without an enforceable memory
  boundary cannot enable untrusted CSV or RTF.
- Put a global semaphore and aggregate byte/memory budget above worker launch.
  Per-process limits alone allow concurrent workers to exhaust the host.
- Treat the worker as crash and resource containment, not a complete hostile-input
  sandbox. Close inherited file descriptors and use an empty private working
  directory. Linux installs the address-space ceiling and seccomp network-denial
  filter; macOS uses the named `no-network` profile. Filesystem isolation and
  non-Linux memory ceilings are not yet universal; if a required control cannot
  be enforced, narrow the supported threat model and leave untrusted inputs
  disabled.
- Do not expose embedded asset bytes in the first MCP schema.
- Parse returned Markdown into an AST, remove raw HTML, and neutralize remote,
  `file:`, and `data:` image destinations before returning content. Parser
  no-network behavior does not prevent a rendering client from fetching a
  preserved image URL.
- Treat any observable skipped chapter, slide, sheet, relationship, or package
  part as `incomplete_conversion`; never return partial content as success.
  A character-level omission that leaves the text usable, such as a dropped
  DOCX non-breaking hyphen, is returned with `completeness: partial` and a
  named warning, never as `complete`.
- Do not log paths, document text, embedded URLs, or filenames. Use an opaque
  request identifier.
- Verify the resolved production graph contains no HTTP client or two
  `pdf-inspector` sources.

## Dependency-ordered implementation gates

### Gate 0 — dependency, CI, and public baseline

Land the work described in
[`dependency-pr-review-2026-08-22.md`](dependency-pr-review-2026-08-22.md):

- consolidate the current direct dependency updates;
- update `crossbeam-epoch` beyond `RUSTSEC-2026-0204`;
- make cargo-deny actually check advisories and sources;
- replace home-directory fixture discovery with tracked public fixtures;
- remove machine-specific paths and potentially identifying pilot references
  from the tracked tree;
- record public fixture provenance.

**Exit evidence:** full Rust gates, RustSec scan, secret/PII scan, and a clean
tracked tree.

**Candidate-branch status:** the code and CI changes are present on this branch,
but Gate 0 is not satisfied for the public default branch until the branch is
pushed, its new CI jobs pass on that exact head, and the change is merged.

### Gate 1 — parser convergence

Replace the old Git-source `pdf-inspector 0.1.0` with exact
`pdf-inspector = "=1.17.0"`, then add exact `anydoc = "=0.2.4"`. Commit the
Cargo lockfile checksums and record the signed upstream revisions in
`THIRD_PARTY.md`.

Update skillkit calls for the current PDF API before adding document tools.
Declare and test the workspace MSRV required by AnyDoc (`1.88`). Resolve or
explicitly track the transitive `ttf-parser 0.25.1` unmaintained advisory with
an owner, upstream reference, and review trigger; parser convergence alone
does not remove that warning.

Run all resolution/build gates with `--locked`. Keep `unknown-git = "deny"`,
require revision-pinned Git sources, and delete source allowlist entries when
convergence removes the corresponding Git dependencies.

**Exit evidence:** `cargo metadata` and `cargo tree -d` show exactly one
`pdf-inspector 1.17.0`; all 13 existing tools compile and their public PDF
fixture outputs remain compatible.

### Gate 2 — contract plus unchanged PDF adapter

The local contract and router are implemented in `src/document.rs`. Wrap the existing PDF functions as the PDF
provider without changing current MCP names or schemas. Add contract tests for
error mapping, provenance, capability truthfulness, PDF route selection, and
encrypted OOXML preflight before extension-based routing.

**Exit evidence:** existing PDF and domain tests remain green; new router tests
prove every PDF takes the PDF adapter and every hard failure prevents fallback.

### Gate 3 — supervised AnyDoc worker

The workspace-built worker and real AnyDoc adapter are implemented for DOCX, exact PPTX, exact XLSX, exact ODS, exact ODT, exact ODP, and strict EPUB through AnyDoc.
The local strict CSV adapter and the AnyDoc ODP/EPUB adapters use the same bounded worker boundary and are enabled only on Linux with enforceable address-space containment. RTF and other office formats remain disabled. The worker uses raw stdin bytes, bounded
stdout/stderr, typed exit mapping, process-group cleanup, timeout termination,
versioned format negotiation, a global concurrency budget, an enforceable Linux
address-space ceiling plus seccomp network-denial filter, and Darwin named
`no-network` profile; filesystem isolation and non-Linux memory containment
remain promotion gates. Resolve the exact sibling worker executable; never search
`PATH`.

The worker must suppress all upstream logs, map upstream errors without their
document-controlled fields, fail on observable incomplete conversions, and run
the Markdown/HTML egress sanitizer before returning content. For PPTX, the local preflight closes the pinned parser's recoverable slide-skip path by validating every declared slide and rejecting any omission before conversion. If another pinned AnyDoc code path has a recoverable skip path without a typed completeness signal, that parser path remains disabled until upstream adds one or a minimal audited patch exposes it.

Tests must exercise the real AnyDoc library with public fixtures. Fakes are
permitted only in test files for worker failure injection; they do not satisfy
the integration gate. The Darwin worker-level network canary passes locally, and
Linux network denial is exercised by the target-specific implementation and CI
containment job. Hostile-input support still requires filesystem isolation, memory
evidence, and resource measurements; a killable subprocess alone is not a sandbox.

**Exit evidence:** real conversion tests, malformed/encrypted/incomplete/
resource-limit tests, remote-image and raw-HTML sanitizer tests, output-cap
tests, worker-kill tests, aggregate-concurrency tests, and scoped no-network
dependency/runtime checks pass. The upstream AnyDoc CSV/RTF paths remain explicitly disabled; the local strict CSV adapter is separately enabled only on Linux with enforceable process-memory containment.

### Gate 4 — small backward-compatible MCP surface

Add only:

- `document_capabilities`
- `classify_document`
- `document_to_markdown`

`document_capabilities` reports compiled and runtime availability, disabled
formats, provider versions, limits, and diagnostic gaps. `classify_document`
detects kind and route; it must not imply OCR/classification detail that the
provider cannot produce. `document_to_markdown` returns the local contract.

**Exit evidence:** the exact old 13-name set remains present, the three new
names are additive, schemas are versioned, and an MCP stdio smoke test calls a
real non-PDF fixture through the worker.

### Gate 5 — public corpus and promotion policy

Create at least one synthetic or redistributable fixture for every enabled
parser/container path, with source/generator, redistribution basis, SHA-256,
transformation, and metadata-scrub status in the manifest. Legacy DOC, PPT, and
XLS; OOXML DOCX/PPTX/XLSX; XLSB; and each ODF container take different code
paths and cannot share one family-level acceptance fixture. Advertise only the
extensions whose exact route has positive and adversarial coverage.

For every enabled family, require:

- correct deterministic kind detection;
- non-empty Markdown containing fixture-specific sentinel content;
- stable table/list/heading assertions relevant to that format;
- typed failures for malformed, encrypted, oversized, and disallowed inputs;
- no path or document-content leakage in logs/errors;
- no outbound network activity;
- no remote image destinations or raw HTML capable of renderer egress;
- a hard incomplete-conversion failure for every exercised skip path;
- enforced response, output, and worker resource bounds;
- successful rollback to the PDF-only build.

Published upstream benchmark scores are not acceptance evidence for this gate.

### Gate 6 — documentation and rollout

Update README, CONTEXT, THIRD_PARTY, contributor guidance, and MCP
examples. New generic tools may become available only after Gates 0–5 pass.
PDF routing never changes by default. RTF still requires closure of upstream #104 or a separately proven OS-sandbox policy; CSV remains limited to the local strict adapter and Linux memory boundary until cross-host evidence is available.

## Planned file map

| File | Change |
|---|---|
| `Cargo.toml` | Exact AnyDoc/PDF versions, workspace MSRV policy |
| `crates/pdf-inspector-skillkit/Cargo.toml` | AnyDoc dependency |
| `crates/pdf-inspector-skillkit/src/document/mod.rs` | Public local contract entry point |
| `crates/pdf-inspector-skillkit/src/document/model.rs` | Serializable contract and provenance |
| `crates/pdf-inspector-skillkit/src/document/error.rs` | Stable error taxonomy |
| `crates/pdf-inspector-skillkit/src/document/pdf.rs` | Existing PDF adapter |
| `crates/pdf-inspector-skillkit/src/document/anydoc.rs` | Private AnyDoc type/error mapping |
| `crates/pdf-inspector-skillkit/src/document/router.rs` | Deterministic content-based route policy |
| `crates/pdf-inspector-mcp/Cargo.toml` | Explicit `[[bin]]` declaration for the worker |
| `crates/pdf-inspector-mcp/src/bin/anydoc_worker.rs` | Versioned stdin/stdout worker binary |
| `crates/pdf-inspector-mcp/src/main.rs` | Supervisor, sibling-binary resolution, and three additive MCP tools |
| `crates/pdf-inspector-skillkit/src/document/sanitize.rs` | Markdown AST and raw-HTML egress sanitizer |
| `crates/pdf-inspector-skillkit/tests/document_integration.rs` | Real public-format contract tests |
| `test-corpus/` | Manifest-listed multi-format fixtures and golden Markdown assertions |

The precise module split may be collapsed if individual files remain small,
but ownership boundaries and public types must remain unchanged.

## Verification commands

```bash
cargo fmt --all -- --check
cargo metadata --locked --format-version 1
cargo check --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build --workspace --release --locked
cargo audit
cargo deny --locked check advisories licenses bans sources
cargo tree -d
```

Also run an MCP stdio initialization/tool-list/call smoke, a worker termination
test, fixture hash validation, Gitleaks against the tree and reachable history,
and a tracked-text scan for home paths, emails, credentials, and identifier
patterns. Do not claim the integration ready from a compile-only check.

## Rollback

Rollback is additive and explicit:

1. Disable/remove the three generic MCP registrations.
2. Remove the worker binary and AnyDoc adapter dependency.
3. Retain the converged, verified PDF dependency only if its legacy regression
   suite remains green; otherwise restore the previous lockfile and revision.
4. Re-run the exact 13-tool stdio smoke and public PDF corpus.

No migration changes stored documents or external state, so rollback does not
require data conversion.

## Non-goals

- Replacing or renaming the existing 13 tools.
- Sending documents to the hosted Firecrawl Parse API.
- OCR, image understanding, macro execution, or embedded-object extraction.
- Exposing AnyDoc's raw `Document` model or asset bytes through MCP.
- Silent parser fallback, silent truncation, or version drift.
