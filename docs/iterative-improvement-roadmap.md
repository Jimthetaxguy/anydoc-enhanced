# Iterative document-capability roadmap

**Baseline:** `agent/codex-align-firecrawl-20260828`
**Upstream:** Firecrawl `pdf-inspector 1.24.0` and AnyDoc `0.2.4`
**Status:** Stages 1-9, including the DOCX happy-path slice, strict PPTX, strict XLSX, strict ODS, strict ODT, Linux-memory-gated strict CSV, strict ODP, and strict EPUB slices, are implemented; the 2026-09-24 upstream refresh is complete; HTML/MHTML, RTF, and OCR remain gated.

## Objective

Expand the existing 13-tool PDF/domain MCP server into a safe, provider-neutral
document intelligence surface. Each capability must have a real upstream
primitive, a public non-PII evaluation corpus, typed completeness/error
behavior, bounded resource usage, and a reviewable handoff before it becomes
available through MCP.

## Priority sequence

| Stage | Capability | Primary use cases | Upstream basis | Gate |
|---|---|---|---|---|
| 0 | PDF alignment | Preserve existing PDF classification, extraction, tax, IRC, and SEC workflows while tracking Firecrawl improvements | `pdf-inspector 1.24.0` | Complete on this branch; rerun locked verification before merge |
| 1 | Contract and worker skeleton | One stable interface for multiple parsers; bounded conversion of untrusted bytes | AnyDoc `to_markdown_bytes`, `ConvertError`, local PDF facade | Implemented with versioned worker IPC, caps, sanitizer, process-group cleanup, and typed errors; full hostile-input promotion remains gated |
| 2 | DOCX | Tax workpapers, client correspondence, engagement letters, operating procedures, research documents | AnyDoc DOC/DOCX parser and shared Markdown renderer | Exact main-part XML preflight plus public happy/adversarial fixtures; broader completeness oracle and platform sandbox evidence remain |
| 3 | PPTX | Sales/procurement decks, process walkthroughs, post-mortems, training material | AnyDoc PPT/PPTX parser, notes and slide model | Exact `.pptx` only; declared-slide completeness, visible/active/external-content policy, stable output and resource budgets implemented; broader adversarial/platform gates remain |
| 4 | strict XLSX | Workpaper schedules, reconciliations, operating metrics, financial tables | AnyDoc Excel parser and table renderer | Exact `.xlsx` only; cached-value policy, hidden-content rejection, external-link rejection, macro/binary rejection, public happy/malformed fixtures, and preflight tests implemented |
| 5 | strict ODS | OpenDocument trial balances, reconciliations, depreciation schedules, and control workpapers | AnyDoc ODF table parser and bounded repeat/span expansion | Exact ODS only; mimetype/content identity, visible non-active tables, cached/displayed values, external/encrypted/active/uncached rejection, public fixture, and production MCP test implemented |
| 6 | strict ODT | Research memoranda, engagement letters, procedures, evidence narratives, and exported workpapers | AnyDoc ODF text parser and existing bounded worker | Exact ODT only; visible text, balanced XML, exact identity, hidden/tracked/external/encrypted/active/missing-asset rejection, public fixtures, and production MCP test implemented |
| 7 | strict CSV | Bank, general-ledger, payroll, and 1099 exports | AnyDoc CSV behavior review; local bounded tabular adapter | Strict UTF-8, deterministic delimiter sniffing, RFC-4180-style quoting, equal-width rows, bounded fields/output, Markdown escaping, public fixture, and MCP integration evidence; enabled only on Linux with address-space containment |
| 8 | strict ODP | OpenDocument presentations for tax/control walkthroughs, training material, and process reviews | AnyDoc ODF presentation parser and Markdown renderer | Exact ODP identity, visible presentation pages, local assets, no hidden/external/active content, public adversarial corpus, real worker evidence; enabled only on Linux with address-space containment |

| 9 | EPUB | Long-form technical/legal/research material | AnyDoc EPUB parser and archive limits | Strict EPUB 3 OCF/container/OPF/spine preflight, complete navigation order, local-resource checks, hostile corpus, real-parser chapter-order/omission evidence, and Linux-memory-gated worker route implemented; broader filesystem and cross-platform gates remain |
| 10 | HTML/MHTML | Saved web research and local reports | Sanitization layer and future provider adapter | Scripts, iframes, meta refresh, CSS imports, and external destinations neutralized |
| 11 | Opt-in local OCR | Scanned tax forms and image-only documents | Existing PDF OCR diagnostics; optional local OCR only | Explicit opt-in, per-page budgets, no hosted API, no fabricated text |

The upstream AnyDoc CSV and RTF paths remain disabled until separate resource and completeness reviews prove they are safe. The local strict CSV, ODP, and EPUB lanes are Linux-only additive routes because enforceable worker memory containment is required. HTML/MHTML, email formats, and legacy binary Office formats remain later investigations with separate threat models.

## Implemented first slice

The contract, worker boundary, sanitizer, DOCX happy path, strict PPTX path, strict XLSX path, strict ODS path, strict ODT path, strict CSV path, strict ODP path, and strict EPUB path are implemented on this branch and exposed through three additive MCP tools. The worker uses a version-2 `ADW1` stdin/stdout frame with an explicit format byte, a 50 MiB input cap,
an 8 MiB Markdown cap, a 15-second wall-time cap, private worker working directory,
process-group cleanup on Unix for timeout, protocol/output failure, and caller cancellation, Linux address-space ceiling plus seccomp network denial, Darwin named `no-network` profile, two in-flight permits retained through cleanup,
and a private fixed-prefix AnyDoc omission/recovery classifier that fails closed
without retaining raw log text.
The checked-in fixtures are synthetic and non-PII.

The iterative SOL review selected strict PPTX, followed by strict ODS and strict ODT after source-backed reviews. The selected ODS route adds exact package identity, visible-table and formula-cache preflight, active/external/encrypted rejection, and real AnyDoc MCP evidence. The selected ODT route adds exact text-package identity, balanced XML, hidden/tracked/external/encrypted/active/missing-asset rejection, and real AnyDoc MCP evidence. The current adversarial promotion slice adds public active, external, and incomplete-input fixtures for DOCX/PPTX/XLSX/ODS; initial release-mode resource observations and Unix end-to-end cancellation/reap evidence are now recorded. The strict EPUB evaluator now covers OCF identity, OPF/spine resolution, local references, navigation agreement, active/external/hidden content, encryption, and archive limits; the tracked public corpus now exercises complete, missing, malformed, navigation-mismatch, missing-resource, external, hidden, active, encrypted, and archive-amplification packages, while additional filesystem-isolation, cross-platform, and broader hostile-input gates remain active; the reviewed upstream abuse corpus is green for its scoped Darwin run, and worker-level network canaries are green. The current ODP cycle used live AnyDoc source inspection and recorded the SOL/MiniMax sidecars as unavailable when they produced no usable memo; no specialist approval is inferred.

Known remaining gates: the worker now converts reviewed AnyDoc omission/recovery
warning classes into stable `incomplete_conversion`, but additional silent omissions still
require structural or fixture oracles; process-memory enforcement is
platform-dependent (Linux address-space ceiling is active, while Darwin/non-Linux
promotion remains gated, with initial Darwin observations recorded in
[`docs/resource-evidence.md`](resource-evidence.md)); and hostile-input resource,
filesystem, non-Linux memory, and cross-host evidence are still required before
calling any format production-ready. The worker-level network canary is now green
for its scoped Darwin/Linux paths.

## Agent-assisted iteration cycle

Each new file-type or extraction capability follows this evidence chain:

1. **SOL source and use-case review:** inspect the exact Firecrawl release and
   current main, identify the highest-value user workflow, map parser behavior,
   and propose a public non-PII evaluation corpus.
2. **MiniMax adversarial review:** challenge completeness, egress, parser abuse,
   resource, privacy, and rollback assumptions; produce explicit no-go cases.
3. **Orchestrator reconciliation:** compare both memos against live code,
   mirrors, and dependency state; choose one bounded slice and record a
   correlation identifier, affected files, acceptance tests, and rollback edge.
4. **Implementation:** assign disjoint production-file ownership. The feature
   builder adds the provider-neutral adapter, real fixtures, and tests; the
   worker builder changes containment only when required. Reviewers do not edit
   production files.
5. **Gatekeeper verification:** run locked Rust checks, real-worker integration
   tests, fixture provenance and public-hygiene scans, advisory/license checks,
   and platform-specific evidence. A green conversion alone never promotes a
   format.
6. **Promotion or deferral:** update the provenance ledger, handoff, roadmap,
   and activity ledger. If either specialist is unavailable, record the missing
   review and keep the capability gated rather than infer approval.

No two agents may edit the same production files in parallel. Every accepted
slice must leave a reproducible fixture/evaluation artifact and a stated list
of changes that must not be merged.

## Stage ownership and handoffs

The orchestrator owns the integration branch, contract decisions, fixture
provenance, and final verification. Agents write only to their assigned
worktree or report; no two agents edit the same production files in parallel.

| Lane | Agent role | Bounded deliverable |
|---|---|---|
| Upstream monitor | SOL specialist | Release/main delta review, candidate primitives, and use-case/evaluation proposals |
| Adversarial reviewer | MiniMax specialist | Failure-mode review, disabled-list audit, and go/no-go objections |
| Contract builder | Implementation agent | Provider-neutral model, typed errors, capabilities, and schema tests |
| Worker builder | Implementation agent | Child-process IPC, timeout/kill/reap, byte/output limits, and bounded concurrency |
| Format builder | Implementation agent | One format adapter plus public fixtures and golden/evaluation cases |
| Gatekeeper | Verification agent | Locked checks, source/license/advisory scan, fixture report, and exact-head review |

All handoffs must include: upstream revision, files changed, tests run, known
limitations, fixture provenance, and an explicit disposition of unresolved
risk. Agent messages use a correlation identifier in the working record; the
orchestrator resolves conflicts rather than silently combining incompatible
contracts.

## First implementation slice: Stage 1 then DOCX

### Contract

Add a narrow local contract inside `pdf-inspector-skillkit`:

- `DocumentKind` for detected format family.
- `DocumentProvider` with provider name, package version, and source identity.
- `DocumentCapabilities` describing enabled formats and hard limits.
- `DocumentContent` with schema version, Markdown, byte count, provider chain,
  completeness, and typed warnings.
- `DocumentError` with stable codes for unsupported, encrypted, malformed,
  resource-limit, timeout, worker failure, output-too-large, incomplete, and
  OCR-required cases.

Keep upstream `Document` and raw asset bytes private. Do not expose page,
slide, sheet, or source offsets that the provider does not guarantee.

### Worker

Use a workspace-built child process for non-PDF parsing:

- Receive bytes through stdin and return a versioned IPC envelope on stdout.
- Resolve the sibling worker executable explicitly; never search `PATH`.
- Never invoke a shell, inherit ambient input paths, or forward parser logs.
- Apply input, output, wall-time, process-memory, in-flight, and aggregate
  budgets.
- Kill and reap the process group on timeout or output overflow.
- Map all upstream errors to local constant codes without document-controlled
  details in MCP responses.

The existing PDF tools remain on the PDF facade. AnyDoc PDF conversion is not
allowed to replace or bypass PDF classification and domain helpers.

### DOCX behavior

DOCX is enabled through the supervised worker with content-signature detection,
private worker state, and no execution or fetching of macros, OLE/DDE actions,
external relationships, or remote assets. Return complete Markdown only when all
required document parts are accounted for; a recoverable skip or malformed-XML
recovery must become a typed incomplete result or typed error. The worker
classifier covers reviewed AnyDoc warning sites without forwarding raw logs.

### Strict PPTX behavior

The current presentation lane enables only exact `.pptx` OOXML packages. It validates the presentation relationship graph, requires every declared slide to have well-formed XML and a shape tree, and rejects hidden slides, external relationships, embedded/active content, macro-enabled/slideshow variants, and incomplete conversion before returning Markdown. The real AnyDoc parser remains behind the supervised worker; slide-level source coordinates are not claimed.

### Strict XLSX behavior

The current Excel lane enables only exact `.xlsx` OOXML packages. The worker
negotiates the variant explicitly and preflight rejects macro-enabled, binary,
legacy, encrypted, malformed, hidden, externally linked, or uncached-formula
inputs. Formula cells use cached values only; this route never evaluates formulas.
The public fixture proves visible-sheet conversion, while negative/unit coverage
proves the fail-closed policy.

## Evaluation design

Every enabled format receives three fixture tiers:

1. Happy path: representative public, license-clean documents.
2. Adversarial: malformed XML/archive, encryption, external relationships,
   embedded objects, resource pressure, and parser-controlled log content.
3. Boundary: smallest valid file, largest permitted file, maximum output,
   and the relevant decompression/nesting limit.

Each fixture manifest records source URL, license/redistribution basis, SHA-256,
expected schema version, expected completeness, expected error code, and
expected sanitizer findings. No personal or client documents may enter the
repository.

The evaluation runner must execute the production worker path and fail on:

- Unexpected parser failure or panic.
- Missing or silently dropped required content.
- Any actionable URL, raw HTML, script, or external asset destination in the
  returned MCP content.
- Output truncation without a typed truncation/error signal.
- Wall-time, memory, decompression, output, or concurrency budget violation.
- Golden schema/output drift without an explicit versioned update.

The report card records fixture result, parser/provider identity, completeness,
sanitizer findings, peak/estimated resources, and wall time. Fuzzing and
parallel-call stress are release gates for the worker, not substitutes for
fixture expectations.

## CSV qualification slice

The reconciled implementation selected CSV for the next bounded additive lane
because bank, general-ledger, payroll, and 1099 exports are high-value inputs.
The local adapter intentionally does not call AnyDoc's CSV frontend: that
frontend materializes rows, guesses structure, and skips unreadable records.
The local contract instead requires valid UTF-8, deterministic delimiter
selection, RFC-4180-style quoting, equal-width rows, bounded fields and output,
and Markdown-structure escaping.

Worker code 6 routes the adapter through the existing private worker. The public
route is enabled only on Linux, where the worker address-space ceiling is
enforceable; other platforms recognize CSV but report it disabled. The public
fixture and MCP test are synthetic and non-PII. The upstream AnyDoc CSV path
must not be merged into this repository unless its materialization and
recovery behavior changes and a new independent review closes the resource and
completeness gates.

## ODP qualification slice

This cycle selected strict ODP as the next source-aligned capability because
AnyDoc v0.2.4 already has a public ODF presentation parser with slide text,
tables, grouped shapes, images, and speaker notes. The local route does not
treat AnyDoc success as proof of completeness.

The local contract adds worker code 7, exact presentation mimetype and
content.xml identity, balanced XML, a required office:presentation body with at
least one page, local-asset existence checks, and fail-closed active, external,
hidden, encrypted, malformed, and resource-limit handling. The real public
fixture is the MIT-licensed AnyDoc pres.odp fixture; deterministic derivatives
cover every rejection class listed above. The private worker proves the real
AnyDoc conversion on Darwin, while MCP promotion is enabled only on Linux
where the worker address-space ceiling is enforceable.

ODP non-goals are layout fidelity, source coordinates, animation semantics,
embedded-object execution, external fetching, and automatic fallback to PDF or
PPTX. No Cargo dependency or upstream source import was required.

## EPUB qualification slice

This cycle promotes strict EPUB 3 as the next additive route for long-form
technical, legal, research, regulation, and policy material. AnyDoc v0.2.4
provides the real parser and archive limits; refreshed upstream `main` is
README-only after the release, so the repository keeps the released dependency
and adds no vendored source.

The local preflight requires exact stored-first OCF identity, one OPF rootfile,
manifest and all-spine resolution, XHTML/body validity, navigation agreement,
local resource existence, and rejection of active, external, hidden, encrypted,
malformed, and archive-amplified packages. Worker code 8 uses the existing
versioned boundary, sanitizer, timeout, output, concurrency, and Linux
address-space controls. Public conversion is enabled only on Linux; EPUB 2,
layout/source-coordinate fidelity, embedded-object execution, and external
fetching remain out of contract.

The ten generator-backed fixtures and real-parser oracle prove chapter order,
provider identity, path/HTML/URL absence, and fail-closed negative outcomes.
Linux-only worker tests cover the route and hostile matrix; Darwin verifies
classification and containment preflight but intentionally reports the route as
disabled because it cannot enforce the worker memory ceiling.

## Upstream refresh cycle (2026-09-24)

This cycle refreshed the repository from the official Firecrawl
`pdf-inspector` and AnyDoc repositories and ran review loops until each round
came back clean. The evidence and dispositions are in
[`upstream-drift-audit-2026-09-24.md`](upstream-drift-audit-2026-09-24.md).

| Loop | Change | Evidence |
|---|---|---|
| 1 | `pdf-inspector` 1.17.0 to 1.24.0, `lopdf` 0.45.0 | Object-stream bomb peak RSS 1,100 MiB to 19 MiB; golden output differs by one line |
| 2 | Surface the new PDF signals (OCR reasons, layout, CMap gaps, provenance dates) and read the IRC structure pdf-inspector actually renders | IRC parsing goes from 0 to 14 sections and 284 provisions on the public corpus |
| 2b | Run PDF tools inside the bounded worker | A page-content bomb that still peaks at 2.1 GiB in-process returns `resource_limit`; batch high-water mark 1,015 MiB to 192 MiB |
| 3 | Refuse or disclose what the pinned AnyDoc drops or amplifies, from its open pull requests | Symbol and form checkboxes, hidden text, link-destination schemes, and number-format amplification fixtures |
| 4 | `rmcp` 3.4.1 with read-only tool annotations | Tool names and schemas unchanged |
| Review 1–2 | Make every preflight check read what AnyDoc reads: its transcoding, its reference resolution, its exact part names | Five decoy fixtures that converted as complete with no warning now fail closed or disclose |
| 6 | DOCX inline content: refuse ruby text and imported chunks; report dropped non-breaking hyphens as `partial` with a warning | Three fixtures; the DOCX lane emits `partial` for the first time |
| 7 | Optional region `frame` (`sheet` default, `display` for rendered-page boxes) from pdf-inspector 1.24 | A generated `/Rotate 90` page: each frame's rectangle finds the text only in that frame |
| Review 3 | Parse what the checks look for, in every lane, as AnyDoc reads it: namespace declarations, decoded values, CSS, drawing-page styles, walker positions, binary workbooks | 43 reproduced bypasses now fail closed or are disclosed; the Sections regression is fixed; a quadratic slide match is linear |

## Next slices

1. **DOCX inline-content oracle, remaining elements.** Ruby text, imported
   chunks, and non-breaking hyphens are handled. Elements AnyDoc 0.2.4's walker
   also skips, which are candidates to confirm with fixtures: `w:contentPart`
   ink, and date or page-number fields placed in the body (`w:pgNum`,
   `w:dayShort` and related elements). Master-document `w:subDoc` links
   reference files outside the package and are reported as external
   relationships.
2. **Next AnyDoc release.** Follow the adoption checklist in the drift audit:
   non-exhaustive `Format` arms, the four Wingdings codes from #177, and a
   re-check of the ported `to_utf8` and `path::resolve`.
3. **External hyperlinks outside DOCX.** PPTX, XLSX, and EPUB refuse any
   external relationship, including an ordinary hyperlink, while DOCX converts
   and removes the destination with a warning. Hyperlink relationships could
   follow the DOCX policy, since the sanitizer already removes their
   destinations. This is a policy change for the owners to decide.
4. **Non-Linux containment.** PDF and document parsing still lack a memory
   ceiling on macOS and fall back to in-process parsing where no sandbox
   exists; filesystem isolation remains open on every platform.

## Go/no-go rules

- Do not expose a new MCP tool until its stage’s contract, fixture, security,
  and resource gates are green.
- Do not update AnyDoc or `pdf-inspector` automatically; refresh sibling mirrors
  and repeat the provenance review first.
- Do not adopt current upstream `main` solely because it contains a newer
  package version; require a release or an explicitly pinned, reviewed commit.
- Do not enable hosted OCR, network fetching, the upstream CSV/RTF parsers, macro execution, or
  external-entity resolution.
- If a gate fails, preserve the current working capability and record the
  failure with the exact fixture, revision, and first violated invariant.

## Immediate next agent slice

The current implementation slice adds public adversarial fixtures and production-worker assertions for active, external, malformed, missing, and incomplete package cases across the enabled lanes. Strict EPUB now adds worker code 8, exact EPUB 3 identity, all-spine and navigation preflight, local-resource validation, active/external/hidden/encrypted/archive rejection, and Linux-memory-gated MCP conversion with public adversarial fixtures. The EPUB route is not a reason to broaden PDF routing, layout claims, embedded-resource execution, or external fetching. The next slice is RTF only after a fresh source review and independent resource/completeness evidence.
