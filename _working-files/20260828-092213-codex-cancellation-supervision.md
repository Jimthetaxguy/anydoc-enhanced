# AnyDoc worker cancellation-safe supervision

Status: complete
Date: 2026-08-28
Scope: Close the worker lifecycle gap where cancellation of the caller future could leave a child process or descendant alive.

## Changes

- Moved worker child ownership, private temporary directory ownership, and the in-flight semaphore permit into a detached Tokio supervisor.
- Added a cancellation guard that signals the supervisor when the caller future is dropped.
- Kept the existing 15-second timeout, protocol/error mapping, process-group termination, and stable public error contract.
- Enabled kill_on_drop(true) as a direct-child backstop; normal timeout, protocol/output failure, and cancellation paths still use explicit process-group kill plus wait.
- Added a Unix skillkit regression with an isolated shell worker that records its own PID and a descendant PID, aborts the conversion future, and verifies both processes disappear.
- Updated the handoff, integration plan, roadmap, context glossary, and dated drift audit to state the cancellation and permit-retention guarantees.

## Verification

- cargo test -p pdf-inspector-skillkit --lib canceling_worker_reaps_process_group --locked - passed.
- cargo test --workspace --locked - passed: 3 MCP unit, 17 MCP integration, 60 skillkit, 6 integration, and doc tests.
- cargo clippy --workspace --all-targets --locked -- -D warnings - passed.
- cargo fmt --all -- --check - passed.
- bash scripts/check-public-hygiene.sh - passed.
- git diff --check - passed.
- git fetch origin; git rev-list --left-right --count HEAD...origin/main - 0 0.

## Remaining gates

- Filesystem isolation, non-Linux memory ceilings, broader hostile-input evidence, and cross-host containment remain open before enabling EPUB/ODP/CSV/RTF.
- No commit or push was performed.
