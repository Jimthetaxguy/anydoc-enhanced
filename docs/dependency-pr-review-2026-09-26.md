# Dependency PR review — 2026-09-26

Scope: the five Dependabot pull requests left open after
[#28](https://github.com/Jimthetaxguy/anydoc-enhanced/pull/28) merged:
[#23](https://github.com/Jimthetaxguy/anydoc-enhanced/pull/23) through
[#27](https://github.com/Jimthetaxguy/anydoc-enhanced/pull/27). All five were
opened between 2026-09-03 and 2026-09-10, against the `main` from before #28.

## Decisions

All five were closed. None fixes a security advisory: `cargo deny check`
reports no advisory against the versions `main` resolves today.

| PR | Update | Decision |
|---|---|---|
| [#27](https://github.com/Jimthetaxguy/anydoc-enhanced/pull/27) | rmcp 3.1.4 → 3.2.0 | Superseded. #28 moved rmcp to 3.4.1. |
| [#24](https://github.com/Jimthetaxguy/anydoc-enhanced/pull/24) | quick-xml 0.41.0 → 0.42.0 | Held until AnyDoc moves. AnyDoc 0.2.4 depends on quick-xml 0.41.0, and the package checks must read XML exactly as AnyDoc does, so both must use the same version. Moving only this crate would put two quick-xml versions in the build. The branch also carried hand edits to `document.rs` that conflict with #28. |
| [#23](https://github.com/Jimthetaxguy/anydoc-enhanced/pull/23) | libc 0.2.185 → 0.2.189 | Deferred. A patch-level lockfile update. |
| [#25](https://github.com/Jimthetaxguy/anydoc-enhanced/pull/25) | log 0.4.29 → 0.4.34 | Deferred. A patch-level lockfile update. |
| [#26](https://github.com/Jimthetaxguy/anydoc-enhanced/pull/26) | serde 1.0.228 → 1.0.229 | Deferred. A patch-level lockfile update. |

Closing a Dependabot pull request skips only that version. Dependabot proposes
the next release of each crate on its weekly run.

## Updates to check on

| Crate | On `main` | Next check | What to run |
|---|---|---|---|
| libc | 0.2.185 | Next dependency pass, or the next Dependabot PR | Full CI, including the worker containment jobs on Ubuntu and macOS: the worker's address-space limit, network denial, and process-group cleanup call libc. |
| log | 0.4.29 | Next dependency pass, or the next Dependabot PR | Full CI. AnyDoc logs through the same crate. |
| serde | 1.0.228 | Next dependency pass, or the next Dependabot PR | Full CI. `serde_core` and `serde_derive` move with it. |
| quick-xml | 0.41.0 | Only when an AnyDoc release moves to a newer quick-xml | Move AnyDoc and quick-xml together. Port the 0.42 API changes from #24's branch (`c5a2fa2`) onto the current `document.rs`. Re-run the package-check tests, which compare what the checks read with what AnyDoc converts. |

To take the three patch updates without waiting for Dependabot, run
`cargo update -p libc -p log -p serde` on a branch and open a pull request.
Its CI covers the Rust 1.88 check that #28 added.

Dependabot will propose quick-xml 0.43 when it is released. To stop that until
AnyDoc moves, add an `ignore` entry for quick-xml to `.github/dependabot.yml`.
