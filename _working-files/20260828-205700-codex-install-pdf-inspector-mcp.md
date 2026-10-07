# Local MCP install: pdf-inspector-mcp

- status: complete
- repository: `anydoc-enhanced`
- branch: `agent/codex-align-firecrawl-20260828`
- source commit: `bd4730d433f2396bdfa75516380cd59040e98d36`
- installed binary: `/Users/jamespustorino/.cargo/bin/pdf-inspector-mcp`
- rollback backup: `/Users/jamespustorino/.cargo/bin/pdf-inspector-mcp.bak-20260828`
- install command: `cargo install --locked --path crates/pdf-inspector-mcp --force`
- host registration: existing Claude Code project-scoped `pdf-inspector` entry; no duplicate entry created
- registration status: pending approval in Claude Code
- verification: installed binary initialized over stdio, advertised 16 tools, and completed a `document_capabilities` call
- repository impact: no tracked files changed; existing untracked `_working-files/` preserved
- note: this local executable follows the open PR #22 head; reinstall after merge if the desired source becomes `origin/main`
