# AnyDoc upstream abuse evaluator and iteration protocol

**Status:** complete
**Date:** 2026-08-28
**Branch:** `agent/codex-align-firecrawl-20260828`

## Implemented

Added `scripts/evaluate-upstream-abuse.py`, a non-vendoring evaluator that
accepts a local Firecrawl AnyDoc mirror and the release worker. Darwin uses the
named `no-network` profile; Linux uses the worker native seccomp path. The
bounded subprocess records logical fixture names, stable error codes, input and
response sizes, peak RSS, wall time, and timeout status without emitting raw
parser logs or machine paths.

Recorded the evaluator and its disposition in
`docs/resource-evidence.md` and `docs/upstream-provenance.md`. Added the
SOL-to-MiniMax-to-reconciliation-to-implementation-to-gatekeeper cycle to
`docs/iterative-improvement-roadmap.md`.

## Evidence

The reviewed upstream abuse corpus passed 7/7 on Darwin/arm64:
`deepxml--errors.docx`, `imagebomb--errors.docx`, `zipbomb--errors.docx`,
`hugespan--errors.pptx`, `emptyrowrepeat--errors.ods`, `hugespan--errors.ods`,
and `hugerepeat--errors.ods` all returned `resource_limit` with zero nonzero
worker exits and no timeout or protocol failure.

Verification passed: Python compile, evaluator run, Rust formatting, 85
workspace tests, public hygiene, and diff check. The branch remains dirty,
uncommitted, and unpushed by design.

## Specialist status and next gate

Prior SOL recommendations and direct corpus evidence support completing hostile-
resource evaluation before any new format promotion. A fresh SOL dispatch was
shut down after repeated observation timeouts without a result. MiniMax returned
`Transport closed`; no result was fabricated. EPUB, ODP, CSV, and RTF remain
gated. Next dependencies are broader hostile-resource cases, filesystem
isolation, non-Linux memory containment, and cross-host runtime evidence.
