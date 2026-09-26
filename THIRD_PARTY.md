# Third-party dependencies

This project is dual-licensed under **MIT OR Apache-2.0**. Every transitive
dependency is permissively licensed and compatible with that choice.

## Direct upstream

### pdf-inspector

- **Source:** https://github.com/firecrawl/pdf-inspector
- **Package:** crates.io `pdf-inspector 1.25.0`
- **Registry package:** crates.io `1.25.0`, checksum `4c7bbb3b63dec43a1c849508e0da95833af6185649f121e41fc3cbd2d3b2e639`
- **Upstream Git context:** no `v1.25.0` tag; the package matches `main` @ `2dbd16b3ea4ea943af2802bd72e45dd264eacb9f` (release of #592 and #593)
- **License:** MIT
- **Transitive core dep:** `lopdf 0.45.0` from crates.io (MIT), checksum `bfffda0fe1ab0157e1a13c14bebd3f28671f2fccb7922f0722ec53926e6922d3`

### anydoc

- **Source:** https://github.com/firecrawl/anydoc
- **Package:** crates.io `anydoc 0.2.4`
- **Upstream release:** `v0.2.4` @ `42bf1c5ecdde9eb0d96d6bd75a9e6698cf93b14c`
- **License:** MIT
- **Integration status:** Resolved in the workspace for parser convergence; bounded DOCX, exact `.pptx`, exact `.xlsx`, exact `.ods`, exact `.odt`, exact `.odp`, and strict EPUB runtime paths are exposed through the local provider-neutral contract. Macro-enabled, binary, legacy, hidden, externally linked, encrypted, active, and incomplete spreadsheet variants remain disabled; ODT is limited to visible, well-formed, exact-mimetype text packages; EPUB is limited to strict EPUB 3 packages with complete, local, inactive spine content.

### Upgrade checklist

1. Inspect the official upstream tag or commit without editing this workspace
2. Record and verify the new immutable revision or registry checksum
3. Update workspace `Cargo.toml` and `Cargo.lock` deliberately
4. Run the locked build, regression corpus, and security gates
5. Update this file and `docs/upstream-provenance.md` with the new evidence

### Rollback

Revert the dependency and lockfile changes, then rerun the locked verification gates.

## Full dependency license audit

Generated with `cargo license --json` (cargo-license 0.7.0) on 2026-09-26. The
208-package workspace graph (206 external packages plus 2 workspace packages)
resolves to:

| License set | Crate count | Notes |
|---|---:|---|
| `Apache-2.0 OR MIT` | 138 | Bulk of the Rust ecosystem |
| `MIT` | 34 | Includes `anydoc`, `lopdf`, `pdf-inspector`, and `pulldown-cmark` |
| `Apache-2.0 OR Apache-2.0 WITH LLVM-exception OR MIT` | 14 | wasm/wit toolchain crates |
| `MIT OR Unlicense` | 6 | Permissive dual-license choice |
| `Apache-2.0 OR MIT OR Zlib` | 3 | Permissive multi-license choice |
| `Apache-2.0` | 3 | `rmcp`, `rmcp-macros`, and `zopfli` |
| `BSD-3-Clause` | 2 | `alloc-no-stdlib` and `alloc-stdlib`, through `brotli-decompressor` (from `lopdf` 0.45.0) |
| `Zlib` | 2 | Permissive license |
| `(Apache-2.0 OR MIT) AND BSD-3-Clause` | 1 | `encoding_rs`; compatible conjunctive terms |
| `(Apache-2.0 OR MIT) AND Unicode-3.0` | 1 | Compatible conjunctive terms |
| `0BSD OR Apache-2.0 OR MIT` | 1 | Permissive multi-license choice |
| `BSD-3-Clause OR MIT` | 1 | `brotli-decompressor`; this project selects MIT |
| `Apache-2.0 OR BSL-1.0` | 1 | `ryu`; this project selects Apache-2.0 |
| `Apache-2.0 OR LGPL-2.1-or-later OR MIT` | 1 | `r-efi`; this project selects MIT |

**Result:** no resolved package requires GPL, AGPL, LGPL, SSPL, BUSL, or
proprietary licensing. The graph passes the repository's cargo-deny license
policy, whose allow list includes BSD-3-Clause. `r-efi` offers LGPL-or-later or
MIT, and this project selects MIT; `ryu` offers Apache-2.0 or BSL-1.0, and this
project selects Apache-2.0.

To re-run the audit:

```bash
cargo install cargo-license
cargo license --json
```
