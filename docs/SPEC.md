# PDF stack integration — capabilities & plan spec

<!-- STATUS: PARTIAL — 13-tool PDF/domain baseline plus bounded DOCX, strict PPTX, strict XLSX, strict ODS, strict ODT, strict ODP, Linux-memory-gated strict EPUB, and Linux-memory-gated strict CSV slices built; broader formats remain gated. -->

Companion docs:
- [`assessment.md`](assessment.md) — Part 1 install + API inventory + baseline benchmark
- [`anydoc-integration-plan.md`](anydoc-integration-plan.md) — current multi-format integration plan

> **Public repository.** Keep this specification repository-relative and free
> of PII, private corpus locations, credentials, and local agent configuration.

5. **Shoebox triage.** Batch-classify a folder of mixed tax documents;
   route scanned → OCR tier, text → extract; dedupe by **SHA-256 of
   file bytes** (exact dup) then first-page `TextItem` fingerprint
   (near-dup after re-scan).
6. **Form-field extraction.** Use **anchor-based region extraction**:
   locate known label text ("Wages, tips, other comp", "Interest
   income") via `TextItem` coordinates, then extract the rect
   below/right of the anchor. This is more robust than rigid bounding-
   box templates since tax form layouts vary by issuer (ADP vs
   QuickBooks vs IRS fillable). Output feeds TypedStepSchema in the
   `tax-domain` skill.
7. **Return-packet assembly** (write-side). Merge 1040 + schedules +
   attachments + supporting K-1s into one filing bundle with bookmarks
   per schedule.
8. **Redaction pass** (write-side). `replace_partial_text` to strip
   SSN / account numbers / DoB before sharing drafts externally or
   archiving to any shared location.
9. **Audit workpapers.** Generate provenance-cited PDF workpapers from
   extracted fields and LLM analysis.

### 4.3 SEC / financial research (`sec-extraction-toolkit`)

10. **Bulk filing ingestion.** Classify + extract + section-split
    10-K / 10-Q filings in parallel; feed `rag-vector-search`.
11. **Financial table reclassification.** Reclassify noisy markdown
    tables into structured rows (subtotals, bracketed negatives,
    continuation). Directly addresses the #1 baseline gap in
    [`assessment.md`](assessment.md) §6.
12. **Per-section splitters.** `Item 1A Risk Factors`, MD&A, etc., via
    markdown post-processing.

### 4.4 Career intelligence

13. **Offer-letter extraction.** Parties, base/bonus/equity, start
    date, clauses; feed the pipeline tracker.
14. **Resume variant generation** (write-side). Redact + regenerate
    shareable versions tuned per audience (stealth mode, full-detail,
    etc.).

### 4.5 Contracts & general business docs

15. **Contract key-fact extraction.** Parties, effective date,
    termination, governing law, assignment clauses.
16. **Template filling** (write-side). Fill boilerplate templates via
    `replace_text` for repeat engagements.


---

## 5. Architecture

### 5.1 Four-layer distribution (unchanged from plan)

```
Layer 4  Cross-agent skill mirrors   (Claude / Codex / OpenCode / Cursor / Gemini)
Layer 3  Rust MCP server             (pdf-inspector-mcp  ← rmcp)
Layer 2  Optional client-specific skill distribution (outside this repository)
Layer 1  Global CLI                  (pdf2md, detect-pdf, dump_ops — installed Part 1)
```

### 5.2 Read / write split

```
                       ┌───────────────────────────────────┐
                       │       pdf-inspector-mcp           │
                       │   (one binary, two tool groups)   │
                       └───────────────┬───────────────────┘
                                       │
                    ┌──────────────────┴───────────────────┐
                    ▼                                      ▼
           ┌────────────────────┐            ┌────────────────────────┐
           │  READ tools        │            │  WRITE tools (opt-in)  │
           │  (Part 2 ship)     │            │  (Part 2.5 or Part 3)  │
           ├────────────────────┤            ├────────────────────────┤
           │ classify_pdf       │            │ merge_pdfs             │
           │ pdf_to_markdown    │            │ replace_text           │
           │ analyze_layout     │            │ redact_pdf             │
           │ extract_text_regions│            │ recompress_pdf         │
           │ extract_table_rgns │            │                        │
           │ batch_classify     │            │                        │
           └────────┬───────────┘            └──────────┬─────────────┘
                    │                                   │
                    └──────────────┬────────────────────┘
                                   ▼
                        ┌────────────────────────┐
                        │ pdf-inspector-skillkit │
                        │ (facade crate)         │
                        └────────┬───────────────┘
                                 │
                ┌────────────────┴─────────────────┐
                ▼                                  ▼
     pdf-inspector crate                  lopdf crate (OPTIONAL — see §6.2)
     (git pin @ 2f23f07f)                 (only if write tools ship)
```

The facade crate wraps both in a single `PdfInfo` struct and a small set
of functions. MCP tool bodies never call lopdf directly — always through
the facade — so we can swap lopdf sourcing in exactly one file.

### 5.3 Part 3 — domain skills as post-processors

```
PDF ─► facade ─► primitives (TextItems, PdfRects, markdown)
                     │
                     ▼
               ┌─────────────────────────────┐
               │  domain post-processors     │
               │  (one module per pattern)   │
               │                             │
               │ • pdf-tax-forms             │  ← priority 2
               │ • pdf-financial-tables      │  ← priority 3
               │ • pdf-scanned-heuristics    │  ← priority 1
               │ • pdf-sec-filings           │  ← priority 4
               │ • pdf-contract-extract      │
               │ • pdf-receipt-extract  <LOCAL-ONLY>
               └─────────────┬───────────────┘
                             ▼
                 enriched, domain-typed output
```

No post-processor ever modifies upstream source. Each lands as:
- one module in `crates/pdf-inspector-skillkit/src/domain/`
- one optional client skill distributed outside this repository
- one MCP tool exposed by `pdf-inspector-mcp`
- a golden-file test with a fixture PDF

---

## 6. Sourcing strategy

### 6.1 Read-side — pdf-inspector

**Historical decision (superseded by the 2026-08-28 provenance audit):** `cargo git` pin by SHA.
- `pdf-inspector = { git = "https://github.com/firecrawl/pdf-inspector", rev = "2f23f07f…" }`
- Sibling mirror: logical name `firecrawl-pdf-inspector`; the public repository never vendors the source.
- Refresh the sibling mirror with `git fetch`; adopt a release or exact revision only after the tracked provenance review.

Rejected alternatives:
- Fork — violates upstream-preserving rule; adopts merge-conflict tax.
- Submodule with local path-patch — nothing gained over SHA pin; breaks clean `git pull`.

### 6.2 Write-side — lopdf (decision: DEFERRED)

Only taken if we ship the write-tool group. Options on the table, to be
decided when write tools are actually scoped:

| Option | Mechanism | Pros | Cons |
|---|---|---|---|
| **A. Match pdf-inspector's dep** | `lopdf = { git = "https://github.com/J-F-Liu/lopdf", rev = "7a05512d…" }` | Exactly what upstream tests against; guaranteed cargo dedup; one SHA to track | Only get features in J-F-Liu at that SHA |
| **B. firecrawl/lopdf at head** | `lopdf = { git = "https://github.com/firecrawl/lopdf", rev = "<sha>" }` | Claimed extra features (see §2.2) | Unverified; forks to separate crate version → cargo may build two lopdfs, pulling in ~double binary size and dedup risk; stale vs. J-F-Liu |
| **C. lopdf from crates.io** | `lopdf = "0.40"` | Stable release channel | Version may not align with pdf-inspector's pin; dedup risk |
| **D. `cargo vendor` offline snapshot** | `cargo vendor` + `.cargo/config.toml` | Fully offline reproducible | Large vendor dir; overkill if we don't actually need offline |

**Current parser graph:** the released PDF dependency resolves `lopdf 0.45.0` from crates.io. Write-side dependencies remain out of scope.

**Decision criterion:** we pick whichever option **(a)** works at the
same SHA pdf-inspector uses (so there's one lopdf in the dep graph, not
two), and **(b)** has the specific feature we want to ship a tool for,
verified by a compile + unit test.

Default path if we ship any write tool: **Option A** (J-F-Liu at
`7a05512d`). Only upgrade to B/C if a specific needed capability is
verified only in the other source.

### 6.3 Rejection of the "patch pdf-inspector's Cargo.toml" proposal

Leaves record because this was explicitly proposed and evaluated:

| Proposal aspect | Verdict |
|---|---|
| Clone `firecrawl/lopdf` into `third_party/` | OK in principle as research artifact, but not used in build graph |
| Edit `third_party/pdf-inspector/Cargo.toml` to point at it | **Rejected** — violates §6.1 no-edits rule and breaks `git pull --ff-only` |
| Rationale cited: "100% offline builds" | **Not compelling** — `cargo fetch --locked` + committed `Cargo.lock` already does this |
| Rationale cited: "reproducibility" | **Not compelling** — SHA pin delivers identical reproducibility without edits |
| Rationale cited: "ability to patch" | **Explicitly out of scope** — our upstream-preserving rule forbids patches; if we need to patch, we upstream a PR first |

---

## 7. MCP tool surface

Every tool: read-only input path is `canonicalize()`-d, 30 s timeout,
50 MB input cap, structured errors (no silent failure), narrow schemas
per `genui-tool-design` 7 Laws.

### 7.1 Read group (ships in Part 2)

| Tool | Input | Output | Maps to |
|---|---|---|---|
| `classify_pdf` | `{path, strategy?}` | `{pdf_type, confidence, page_count, pages_needing_ocr, has_encoding_issues, title}` | `detect_pdf*` |
| `pdf_to_markdown` | `{path, pages?, include_page_breaks?, raw?}` | `{markdown, pdf_type, warnings[]}` | `process_pdf*` |
| `analyze_layout` | `{path}` | `{tables[], columns[], headings[], complexity}` | `ProcessMode::Analyze` |
| `extract_text_regions` | `{path, regions: [{page (0-idx), rects: [[x1,y1,x2,y2]]}]}` | `[{page, texts[]}]` | `extract_text_in_regions_mem` |
| `extract_table_regions` | `{path, regions: [{page (0-idx), rects: [[x1,y1,x2,y2]]}]}` | `[{page, tables[]}]` | `extract_tables_in_regions_mem` |
| `batch_classify` | `{paths[]}` | `[{path, classification}]` (single response, array) | parallel loop over `detect_pdf` |

> **Coordinate convention:** Regions use top-left origin, PDF points,
> two-corner format `[x1, y1, x2, y2]` — matching upstream's
> `collect_text_in_region` convention. Pages are 0-indexed (converted to
> 1-indexed internally for lopdf). `PdfRect` fields in returned data use
> `{x, y, width, height, page}` form.

### 7.2 Write group (opt-in, Part 2.5 / Part 3)

All write tools require explicit `allow_write: true` client hint or a
distinct MCP server invocation. Never auto-enabled.

| Tool | Input | Output | Maps to |
|---|---|---|---|
| `merge_pdfs` | `{paths[], out, bookmarks?}` | `{out, page_count}` | lopdf merge |
| `replace_text` | `{path, out, replacements: [{find, replace, mode: "exact"\|"partial"}]}` | `{out, replacements_applied}` | lopdf `replace_*_text` |
| `redact_pdf` | `{path, out, patterns[]}` | `{out, redactions_count}` | `replace_partial_text` with regex |
| `recompress_pdf` | `{path, out, level?}` | `{out, before_bytes, after_bytes}` | `save_modern` / `save_with_options` |

> **Dropped:** `create_pdf_from_markdown` — lopdf is a low-level object
> model, not a layout engine. Rendering markdown to PDF requires a
> separate dependency (e.g. `genpdf`, `typst`, or `pandoc`). Revisit
> only if a concrete agent workflow demands generated PDFs.

### 7.3 Domain-layer tools (Part 3)

Each Part-3 skill may ship one MCP tool that composes the primitives
above with domain knowledge:

| Tool | Composes | Domain |
|---|---|---|
| `tax_form_extract` | classify → anchor-locate labels → extract_text_regions → redact(ssn) | tax |
| `sec_filing_split` | classify → pdf_to_markdown → section splitter | sec |
| `financial_table_normalize` | analyze_layout → table reclassifier | sec/accounting |
| `contract_key_facts` | pdf_to_markdown → NER pass | contracts/career |

---

## 8. Risks & mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| Upstream pdf-inspector breaking changes | Build breaks after `cargo update` | SHA-pinned; updates are deliberate; regression suite on bump |
| lopdf feature assumptions (§2.2) unverified | Write tools may not work on chosen fork | Ship per-feature compile + unit test before releasing the tool |
| Path edge cases (special chars) | Parse failure in the historical, non-reproducible 20-PDF sweep | `canonicalize()` at MCP boundary |
| Large PDFs blowing memory | OOM on >50 MB | Input-size cap at MCP boundary |
| Scanned PDFs misrouted | Downstream OCR skipped | `pages_needing_ocr` + chain to `PDF Tools MCP` on Scanned/Mixed |
| Table-detection gap vs OCR engines | Weak on heavily formatted tables | `pdf-financial-tables` post-processor (Part 3) |
| firecrawl/lopdf stale / "10 stars, no releases" | Maintenance risk if we ever depend on it | Default Option A (J-F-Liu) — only use firecrawl/lopdf if a specific needed feature verifies there and not in J-F-Liu |
| Cross-platform binary build | agents on other OSes get a different SHA | CI matrix once we ship beyond macOS |

---

## 9. Verification gates

Each part has a concrete gate before proceeding:

**Part 1 historical gate:** binaries were installed and the 20-PDF research
sweep completed, but its source set is not retained and it is not current
release evidence.

**Part 2.3a gate:** `cargo build` green on workspace skeleton + empty
MCP binary that prints its (empty) tool manifest.

**Part 2.3b gate:** each read-group tool passes its smoke test against the
tracked public corpus, invoked via MCP rather than CLI.

**Part 2.4 gate:** skill activates on test prompts in a fresh Claude
Code session ("classify this pdf"); `framework-health` PASSes on MCP
registry + skill index.

**Part 2.5 gate (only if write tools scoped):** each write tool has a
round-trip test — read a PDF, apply write, re-read, assert expected
change. Output validated by `detect-pdf` returning same page count
(no document corruption).

**Part 3 gate (per skill):** golden-file test on a fixture PDF; the
consumer project's CLAUDE.md references the new skill.

---

## 10. Open questions

1. **Do we actually need the write group?** If no concrete agent
   workflow in the next 30 days needs merge/replace/redact/recompress,
   defer to Part 3.5 or later. Read group alone covers the
   high-frequency use cases in §4.1–4.5.
2. **Which lopdf, if any?** Decision deferred per §6.2. Default Option A
   when forced to pick.
3. **Batch-classify concurrency.** Should `batch_classify` stream
   results or return all-at-once? Lean: stream, so large batches don't
   block.
4. **Which corpus to stratify for Part 3 evaluation?** Part 1 used a
   small unstratified sample; Part 3 needs a public stratified corpus
   (SEC / scanned / tax-form / mixed × 10 each). Sourcing TBD.
5. **Do we mirror skills into Codex / OpenCode / Cursor / Gemini
   simultaneously or Claude-first then propagate?** Lean: Claude-first
   for 1 week to iterate, then mirror when stable.
6. **MCP write-mode gating.** Do we want a separate MCP server binary
   for write tools (explicit install step) or one server with a
   capability flag? Lean: one server, explicit flag — fewer artifacts
   to maintain.

---

## 11. Out of scope

- Forking either pdf-inspector or lopdf.
- Editing `third_party/pdf-inspector/` in any way.
- We do not distribute or maintain our own Python or Node bindings.
  Consumers who need them may use upstream pdf-inspector's PyO3/napi
  bindings directly — that's outside this integration's surface.
- Replacing existing `PDF Tools MCP` (OCR / form-fill). pdf-inspector
  is the *pre-filter*; existing tools remain downstream for scanned /
  form-fill paths.
- Building a browser-based PDF viewer (use existing `pdf-viewer` skill).

---

## 12. Operational concerns

### 12.1 Binary install path

`pdf-inspector-mcp` installs to `~/.cargo/bin/pdf-inspector-mcp` via
`cargo install --path crates/pdf-inspector-mcp`. This is the same
directory as the upstream CLI binaries (`pdf2md`, `detect-pdf`).

### 12.2 MCP registration

For Claude Code, register the absolute binary path through the supported CLI:

```bash
claude mcp add pdf-inspector --scope user -- "$(command -v pdf-inspector-mcp)"
claude mcp get pdf-inspector
```

Project-scoped Claude Code configuration belongs in `<repo>/.mcp.json`; user
and local scopes must remain outside this public repository.

If the binary isn't built, the MCP client should surface a clear startup
error ("binary not found at …"). The skill `SKILL.md` documents the
install prerequisite so agents can self-diagnose.

### 12.3 Upstream release refresh procedure

1. Refresh the sibling mirrors for `firecrawl-pdf-inspector` and `firecrawl-anydoc` with `git fetch`.
2. Record each default-branch commit and latest release/tag in `docs/upstream-provenance.md`.
3. Prefer exact released versions in `Cargo.toml`; use a revision only when a release is insufficient.
4. Run the locked build, test, Clippy, audit, hygiene, and fixture gates.
5. Diff parser output against the prior public-fixture baseline and record the disposition.
6. If green, update the handoff and install the MCP binary.
7. Update `THIRD_PARTY.md` and the provenance manifest with the release, checksum, date, and rationale.

Rollback: restore the prior exact dependency versions and lockfile, then rerun the PDF-only regression suite.

### 12.4 Logging & debugging

Set the MCP crate's validated log level without enabling dependency payload
logging:

```bash
PDF_INSPECTOR_MCP_LOG=debug pdf-inspector-mcp
```

Accepted levels are `off`, `error`, `warn`, `info`, `debug`, and `trace`.
Raw `RUST_LOG` directives are ignored because protocol and parser dependency
events can contain caller-controlled arguments. The MCP server logs its own
path-free events to stderr; clients may capture and surface that stream.

---

## 13. Execution path recap

Sequence of next concrete steps (none executed yet beyond Part 1):

1. **Part 2.3a** — workspace skeleton (`Cargo.toml` + 2 crate stubs +
   `THIRD_PARTY.md`). Gate: `cargo build` green.
2. **Part 2.3b** — fill read-group MCP tools one at a time.
3. **Part 2.4** — plugin + skill + routing + master-index registration.
4. **Part 2.5** *(optional, gated by open question 1)* — write group.
5. **Part 2.6** — cross-agent mirror generation script.
6. **Part 2.7** — weekly upstream-monitor scheduled task.
7. **Part 3** — one domain skill at a time, priority order from plan.
