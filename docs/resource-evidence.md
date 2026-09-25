# AnyDoc worker resource evidence

**Observed:** 2026-08-28
**Reference host:** Darwin/arm64
**Build profile:** `cargo test --release`
**Provider:** AnyDoc 0.2.4
**Scope:** One public, non-PII positive fixture per enabled non-PDF lane

The evaluator is `worker_resource_evidence_covers_enabled_lanes` in
`crates/pdf-inspector-mcp/tests/document_tool.rs`. It launches the real
workspace worker with `ANYDOC_RESOURCE_EVIDENCE=1`, sends the same framed
worker protocol used by the production adapter, and records:

- `peak_rss_bytes`: the worker's `getrusage(RUSAGE_SELF).ru_maxrss`, normalized
  to bytes on Unix (Linux reports KiB and Darwin reports bytes).
- `response_bytes`: the complete framed worker response, including its 16-byte
  protocol header.
- `wall_clock_ms`: monotonic elapsed time from worker spawn through exit.

## Release observations

| Lane | Fixture | Peak RSS (bytes) | Response bytes | Wall time (ms) |
|---|---|---:|---:|---:|
| DOCX | `docx/public-fixture.docx` | 8,617,984 | 171 | 11 |
| PPTX | `pptx/public-walkthrough.pptx` | 8,896,512 | 274 | 3 |
| XLSX | `xlsx/public-workpaper.xlsx` | 8,749,056 | 320 | 2 |
| ODS | `ods/public-workpaper.ods` | 9,175,040 | 496 | 3 |
| ODT | `odt/minimal.odt` | 8,454,144 | 103 | 2 |
| ODP | `odp/public-presentation.odp` | 9,863,168 | 374 | 3 |
| EPUB | `epub/public-spine-order.epub` | 8,749,056 | 565 | 2 |

These are reproducible observations for the recorded host/profile, not
cross-platform performance guarantees. The evaluator asserts that every
worker returns a positive response, stays below the existing 8 MiB public
output envelope, and completes before the existing 15-second worker deadline.
The opt-in metadata is omitted from normal worker responses.

### Linux observations, 2026-09-24

The same evaluator on Linux x86-64 (release build, commit `37f8e46`) with the
1 GiB address-space ceiling and seccomp network denial active:

| Lane | Fixture | Peak RSS (bytes) | Response bytes | Wall time (ms) |
|---|---|---:|---:|---:|
| DOCX | `docx/public-fixture.docx` | 6,742,016 | 171 | 2 |
| PPTX | `pptx/public-walkthrough.pptx` | 6,795,264 | 274 | 2 |
| XLSX | `xlsx/public-workpaper.xlsx` | 6,881,280 | 320 | 2 |
| ODS | `ods/public-workpaper.ods` | 6,946,816 | 496 | 2 |
| ODT | `odt/minimal.odt` | 6,664,192 | 103 | 2 |
| ODP | `odp/public-presentation.odp` | 7,372,800 | 374 | 3 |
| EPUB | `epub/public-spine-order.epub` | 6,615,040 | 565 | 2 |

## PDF worker observations, 2026-09-24

PDF tools run in the same worker since this refresh (25-second deadline, four
in-flight slots, the same Linux ceiling and network filter). Measurements from
the release build on Linux x86-64:

| Input | Route | Result | Peak RSS |
|---|---|---|---:|
| 1 MB object-stream bomb inflating to 1 GiB | in-process, `pdf-inspector` 1.17.0 | converted | about 1,100 MiB |
| Same | in-process, `pdf-inspector` 1.24.0 | converted | about 19 MiB |
| Page-content bomb | in-process, `pdf-inspector` 1.24.0 (classify, Markdown, analyze) | converted | about 2,122 MiB |
| Same | worker | `resource_limit` in 0.6–1.7 s | worst process about 519 MiB |

The worker adds little on ordinary input. Median `classify_pdf` time goes from
4.3 to 7.7 ms on `source/sample-1.pdf` and from 55.8 to 65.6 ms on
`source/sample-2.pdf`. `pdf_to_markdown` on sample 2 goes from 408 to 417 ms.
A `batch_classify` over 24 files of 40 MiB lowers the server's high-water mark
from 1,015 MiB to 192 MiB, because each call takes a slot before it reads its
file.

## Preflight model observations, 2026-09-25

The EPUB, ODF, spreadsheet, and PDF checks added in review round four and
loops 8–11 run before conversion. Their bounds, measured with the release
build on Linux x86-64:

| Input | Before | After |
|---|---|---|
| EPUB stylesheet with 16,000 unterminated `@import` rules | 9.27 s, 2,208 MiB | 0.01 s, 13 MiB |
| Same with 64,000 | server killed at 32,000 | 0.03 s |
| 200,000 imports fanned out 400 times | 7.09 s | 0.17 s, `resource_limit` |
| 16,000 selectors matched against 16,000 elements | 3.54 s | 0.27 s |
| 1.39 million selectors in one sheet | 182 MiB | 61 MiB, `resource_limit` at the token cap |

The EPUB model caps tokens (500,000 per sheet), rules, imports, import depth,
sheet applications, and matching work (50 million steps). The ODF walker stops
at AnyDoc's depth bound, and the DOCX numbering reader at its depth and node
bounds. The
spreadsheet format check keeps at most 262,144 style and value pairs, which a
workbook cannot exceed within Excel's 64,000 formats.

The scanned-page check loads each PDF a second time with `lopdf`, decoding at
most 32 MiB per content stream and 128 MiB and 10 million operations per
document, with form nesting capped at 12. It adds 1.1 ms to `classify_pdf` on
`source/sample-1.pdf`, 4.5 ms on `source/sample-2.pdf`, and about 96 ms on a
42 MB file (230 to 327 ms), with peak memory unchanged (50 to 51 MiB). For the
invisible-layer check, a page that binds no image is not decoded.

In a full `pdf_to_markdown` run the same scan also reads every text page for
runs painted twice, under limits of its own (64 MiB and 4 million
operations; past them only that check stops). Decoding the content is its
cost: on `source/sample-2.pdf` the document loads in 9 ms and its 278,000
operations decode in 121 ms. Median `pdf_to_markdown` time goes from 36 to
48 ms on sample 1, 449 to 578 ms on sample 2, and 275 to 335 ms on sample 3;
the 42 MB file is unchanged (387 to 370 ms), and classification is not
affected.

## Boundary and interpretation

The document lanes enforce an 8 MiB Markdown cap and a 15-second deadline.
The PDF lane enforces a 128 MiB response cap and a 25-second deadline. Linux
additionally applies a 1 GiB `RLIMIT_AS` and a seccomp network-denial filter.
Darwin launches the worker under the macOS named `no-network` profile. Darwin
and other non-Linux hosts do not yet have an equivalent production memory
ceiling. On hosts without a worker sandbox, PDF tools parse in-process, as
they did before this refresh. Therefore this evidence:

- supports the current enabled-lane baseline and can detect gross release
  regressions;
- does not establish a hostile-input peak-memory budget;
- proves only the scoped worker-level no-network canaries on the platforms
  where they run;
- does not establish filesystem isolation or non-Linux process/memory
  containment;
- does not justify broadening the strict ODP or EPUB routes beyond their Linux memory-gated contracts, or enabling RTF, legacy, macro-enabled, CSV on non-Linux hosts, or source-coordinate behavior;

Run the release observation with:

```text
cargo test --release -p pdf-inspector-mcp --test document_tool worker_resource_evidence_covers_enabled_lanes --locked -- --exact --nocapture
```

## Upstream AnyDoc abuse observations

The evaluator `scripts/evaluate-upstream-abuse.py` runs the reviewed abuse
fixtures from a caller-supplied sibling `firecrawl-anydoc` mirror. The fixtures
are not copied into this repository and the evaluator emits only logical names,
stable result codes, and bounded measurements.

**Observed:** 2026-08-28 on Darwin/arm64, release worker, macOS `no-network`
profile.

| Fixture | Input bytes | Expected/actual | Peak RSS (bytes) | Wall time (ms) |
|---|---:|---|---:|---:|
| `deepxml--errors.docx` | 1,257 | `resource_limit` / `resource_limit` | 12,746,752 | 7 |
| `imagebomb--errors.docx` | 197,082 | `resource_limit` / `resource_limit` | 8,110,080 | 5 |
| `zipbomb--errors.docx` | 196,603 | `resource_limit` / `resource_limit` | 8,060,928 | 5 |
| `hugespan--errors.pptx` | 1,749 | `resource_limit` / `resource_limit` | 8,601,600 | 4 |
| `emptyrowrepeat--errors.ods` | 466 | `resource_limit` / `resource_limit` | 8,273,920 | 4 |
| `hugespan--errors.ods` | 465 | `resource_limit` / `resource_limit` | 8,355,840 | 4 |
| `hugerepeat--errors.ods` | 483 | `resource_limit` / `resource_limit` | 8,355,840 | 4 |

The run passed 7/7 cases with zero nonzero worker exits, protocol failures,
raw stderr, or timeout outcomes. These are initial Darwin hostile-resource
observations, not a universal memory budget or cross-platform proof; repeat the
evaluator after upstream revision changes and on Linux CI before promotion.

**Repeated:** 2026-09-24 on Linux x86-64 with the release worker from commit
`37f8e46` against AnyDoc `main` `261fc257`. The result was 7/7 `resource_limit`,
with peak RSS between 12,873,728 and 13,660,160 bytes and wall times of
2–9 ms.

Run it with a local mirror and release worker:

```text
python3 scripts/evaluate-upstream-abuse.py \
  --mirror /path/to/firecrawl-anydoc \
  --worker target/release/pdf-inspector-mcp
```

The instrumentation is internal and opt-in. It adds no public document fields,
no upstream source, no dependency, and no change to the default MCP response
contract.
