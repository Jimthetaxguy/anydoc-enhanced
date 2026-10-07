# AnyDoc multi-format worker implementation — 2026-08-28

status: complete

ODS follow-up completed on 2026-08-28: strict ODS routing, preflight, public positive and abuse fixtures, and MCP integration coverage were added without dependency updates or upstream source vendoring.
branch: agent/codex-align-firecrawl-20260828

Implemented the released Firecrawl dependency alignment, provider-neutral document contract, version-2 ADW1 stdin/stdout worker mode with explicit format negotiation, input/output/time/in-flight bounds, private worker directory, Unix process-group cleanup, Linux address-space ceiling, typed generic error envelopes, Markdown egress sanitizer, three additive generic MCP tools, and public DOCX/PPTX/ODS plus synthetic XLSX fixtures with end-to-end MCP integration tests. Strict PPTX accepts visible, non-macro `.pptx` only; every declared slide must resolve to well-formed XML with a shape tree, while hidden, external, active, macro-enabled, slideshow, malformed, and incomplete variants fail closed. Strict XLSX accepts visible, cached-value `.xlsx` only; hidden, external, active, macro-enabled, binary, legacy, malformed, and incomplete variants fail closed. Strict ODS accepts exact visible, non-active `.ods` with cached/displayed values only; hidden, external, encrypted, active, malformed, and uncached-formula packages fail closed.

Upstream inputs: sibling mirrors `firecrawl-pdf-inspector` and `firecrawl-anydoc`; production dependencies `pdf-inspector = 1.17.0`, `anydoc = 0.2.4`, `lopdf = 0.42.0`.

Agents consulted: SOL source review ranked strict PPTX for tax/control-process walkthroughs and identified AnyDoc's log-only skipped-slide limitation; MiniMax proposed CSV for tax exports but its memory-materialization risk kept CSV deferred. Neither agent edited the repository.

Known limitations: AnyDoc recoverable skipped-part diagnostics are not exposed as typed partial results; process-memory enforcement is platform-dependent (Linux is active; macOS/non-Linux promotion remains gated); measured resource reports and broader hostile-format promotion remain gated.

Verification completed: cargo fmt check, locked metadata with declared Rust 1.88 MSRV, duplicate-dependency inspection, workspace check, 62 tests, strict Clippy, release build, cargo audit, public hygiene, fixture archive/hash validation, and final release MCP stdio smoke for ODS capability and conversion. cargo-deny is unavailable locally and remains a CI gate.
