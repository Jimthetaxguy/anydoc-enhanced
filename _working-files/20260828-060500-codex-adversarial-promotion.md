# AnyDoc adversarial promotion evidence

- **Status:** complete for this promotion-evidence cycle; changes remain uncommitted and unpushed
- **Branch:** agent/codex-align-firecrawl-20260828
- **Scope:** preserve the strict DOCX/PPTX/XLSX/ODS/ODT lanes while adding public adversarial fixture coverage and closing the malformed DOCX main-part gap; defer EPUB, ODP, CSV, and RTF.
- **Reviews:** SOL and MiniMax both deferred all four broader candidates. SOL identified EPUB as first future candidate after promotion gates; MiniMax highlighted EPUB skip-and-succeed, permissive XML recovery, ODP unknown-shape omission, CSV materialization/guessing, and RTF recovery/object boundaries.
- **Fixtures:** added synthetic non-PII active/external/incomplete variants for DOCX, PPTX, XLSX, and ODS; SHA-256 values are recorded in `test-corpus/README.md`.
- **Tests:** added one production MCP adversarial matrix, one DOCX external-relationship containment test, and a malformed DOCX regression fixture; the local preflight now rejects unbalanced `word/document.xml`.
- **Documentation:** recorded the decision and gate in the upstream provenance, drift audit, roadmap, and format fixture notes.
- **Verification:** formatter, locked metadata/tree/check/test/clippy/release build, cargo audit, and public hygiene pass; the workspace has 73 passing tests. Cargo audit retains only the documented unmaintained `ttf-parser` and yanked `chacha20` warnings; cargo-deny remains unavailable on this host.
- **Next dependency:** expanded structural completeness-oracle coverage, measured resource/no-network/end-to-end cancellation evidence before any broader format enablement.
- **Follow-on hardening:** the worker now observes reviewed AnyDoc `log::warn!` prefixes for omission/recovery and returns stable `incomplete_conversion`; known public markers are covered, observed ODT `text:note` omission fails closed, and Unix process-group reaping is regression-tested; unobserved silent omissions and end-to-end cancellation remain false-negative boundaries.
- **Authorization boundary:** no upstream source vendoring, dependency update, commit, push, destructive cleanup, or broader format enablement.
