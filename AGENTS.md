# AGENTS.md

Contributor contract for anydoc-enhanced. How to call the tools is in [skills/anydoc-enhanced/SKILL.md](skills/anydoc-enhanced/SKILL.md). Leave that skill unchanged unless the task is the skill itself.

This repository has no UI, so it has no `DESIGN.md`. [CONTEXT.md](CONTEXT.md) is a glossary. [README.md](README.md) is the user-facing overview. [CONTRIBUTING.md](CONTRIBUTING.md) is the PR checklist.

## Layout

Virtual Cargo workspace (`Cargo.toml`, `resolver = "2"`) plus a maturin wheel. There is no root package.

| Path | Role |
|---|---|
| `crates/pdf-inspector-skillkit` | Library. PDF facade, document preflight, and domain modules `tax`, `irc`, `sec`, and `sweet`. Examples live in `examples/`. Integration test: `tests/integration_tests.rs`. |
| `crates/pdf-inspector-mcp` | Binary `pdf-inspector-mcp`. The MCP server and the bounded worker are this same executable. Tests: `tests/document_tool.rs`, `tests/pdf_tools.rs`, `tests/provenance.rs`. |
| `pyproject.toml` | Wheel build. `version` is dynamic and comes from the MCP crate. |
| `packaging/skill_build.py` | PEP 517 backend `skill_build`. Copies `skills/anydoc-enhanced` into `python/anydoc_enhanced/skill/` before maturin sees it. That copy is gitignored. The copy skips `tests/` and `bin/`. |
| `python/anydoc_enhanced/` | Import package. `skill_directory()` returns the bundled skill when the wheel included one. |
| `skills/anydoc-enhanced/` | Agent skill. Python scripts speak JSON-RPC to the binary. They do not parse documents. |
| `test-corpus/` | Public fixtures. Manifest: [test-corpus/README.md](test-corpus/README.md). |
| `scripts/` | `check-public-hygiene.sh`, corpus generators, `package_skill.py`. |
| `docs/` | Handoff, integration plan, upstream drift audits, resource evidence. |
| `.github/workflows/ci.yml` | Hygiene, fmt, clippy, test, release build, MSRV, containment, skill wheel, cargo-deny. |
| `.github/workflows/release.yml` | Per-target wheels and skill zips. |

Both crates are edition 2021, version `0.1.0`, `rust-version = "1.88"`, `publish = false`, license `MIT OR Apache-2.0`.

Pins in the workspace `Cargo.toml`, resolved in `Cargo.lock`:

- `pdf-inspector` `=1.25.2` (lockfile `1.25.2`)
- `anydoc` `=0.2.4`
- `lopdf` `0.45` (lockfile `0.45.0`)

`README.md` and the `[Unreleased]` section of [CHANGELOG.md](CHANGELOG.md) still say pdf-inspector 1.25.0 in places. The manifest and lockfile are 1.25.2.

The dev profile compiles dependency packages at `opt-level = 2` so integration tests stay inside the server's 30-second tool timeout. Workspace crates themselves stay at the default dev opt level.

## Toolchain

- Cargo, with `Cargo.lock`. Pass `--locked` on build, test, clippy, and check. Minimum Rust is 1.88. There is no `rust-toolchain` file. The CI test job installs stable via `dtolnay/rust-toolchain` with components `rustfmt` and `clippy`. The `msrv` job installs toolchain `1.88`.
- Python `requires-python = ">=3.10"`. CI uses Python 3.12. There is no uv, poetry, or npm project.
- Wheel builds need `maturin>=1.15,<2`, from `[build-system]` in `pyproject.toml`.

## Setup

```bash
cargo build --workspace --locked
```

That needs Rust 1.88 or newer. Install the binary with:

```bash
cargo install --locked --path crates/pdf-inspector-mcp
```

## Verify

These match `.github/workflows/ci.yml`:

```bash
bash scripts/check-public-hygiene.sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build --workspace --release --locked
cargo check --workspace --all-targets --locked
```

`cargo test --workspace --locked` is the Rust test gate. It includes `document_tool`. CI also runs that file on `ubuntu-latest` and `macos-latest`:

```bash
cargo test -p pdf-inspector-mcp --test document_tool --locked
```

`cargo check --workspace --all-targets --locked` on Rust 1.88 is the MSRV job. Strict CSV, ODP, and EPUB are enabled only on Linux, where the worker can enforce an address-space ceiling. The tests encode that split. PDF, DOCX, PPTX, XLSX, ODS, and ODT run on macOS and Linux. Windows builds are not published.

`cargo test` does not run the Python skill tests. Those need a built binary. Local form, from [CONTRIBUTING.md](CONTRIBUTING.md) and [README.md](README.md):

```bash
PDF_INSPECTOR_MCP_BIN=target/release/pdf-inspector-mcp python3 skills/anydoc-enhanced/tests/test_scripts.py
```

With `PDF_INSPECTOR_MCP_BIN` unset, `skills/anydoc-enhanced/tests/test_scripts.py` uses `target/release/pdf-inspector-mcp` and then `target/debug/pdf-inspector-mcp`. The CI skill job installs the wheel into a venv and points `PDF_INSPECTOR_MCP_BIN` at that venv's binary.

The `deny` job uses `EmbarkStudios/cargo-deny-action@v2`, which execs `cargo-deny --log-level warn --manifest-path ./Cargo.toml --all-features --locked check advisories licenses bans sources`. [deny.toml](deny.toml) documents the shorter local form `cargo deny check advisories licenses bans sources`. `cargo-deny` is not a workspace dependency.

Local helpers, not CI gates. This is a virtual workspace, so pass `-p`:

```bash
cargo run -p pdf-inspector-skillkit --locked --example validate_domain -- tax path/to.pdf
cargo run -p pdf-inspector-skillkit --locked --example validate_domain -- irc path/to.pdf
cargo run -p pdf-inspector-skillkit --locked --example validate_domain -- sec path/to.pdf
cargo run -p pdf-inspector-skillkit --locked --example sweet_review_demo -- demo_1040_w2_schedule_c
```

`validate_domain` takes `tax`, `irc`, or `sec` and a PDF path. The Sweet example defaults to `demo_1040_w2_schedule_c`.

## Wheel build

`pyproject.toml` sets `build-backend = "skill_build"`, `backend-path = ["packaging"]`, and maturin `bindings = "bin"`. The manifest path is `crates/pdf-inspector-mcp/Cargo.toml`. The import name is `anydoc_enhanced`. Maturin is `locked = true` and `profile = "release"`.

```bash
pip install "maturin>=1.15,<2"
python3 packaging/skill_build.py
maturin build --release --locked --out dist
```

`maturin build` does not call the PEP 517 hook, so the `skill_build.py` step has to run first. `pip wheel .` does call the hook and stages the skill itself. The Linux CI skill job adds `--strip`. Wheels land in `dist/`, which is gitignored.

A skill zip from an already built binary:

```bash
python3 scripts/package_skill.py \
  --binary target/release/pdf-inspector-mcp \
  --target "$(python3 -c 'import json,subprocess; print(json.loads(subprocess.check_output(["target/release/pdf-inspector-mcp","--provenance"]))["target"])')" \
  --dist dist
```

The zip extracts to `anydoc-enhanced/` and includes `SKILL.md`, `scripts/`, `references/`, and `bin/<target>/pdf-inspector-mcp`. Tests stay in git and are not in the zip.

## Release

`.github/workflows/release.yml` builds four targets. A push of a tag matching `v*` publishes a GitHub Release. `workflow_dispatch` builds the same artifacts and uploads workflow artifacts only. It does not create a release.

| Runner | Target | Extra build flags |
|---|---|---|
| `ubuntu-latest` | `x86_64-unknown-linux-musl` | `maturin[zig]`, `--zig --strip --compatibility manylinux_2_17` |
| `ubuntu-24.04-arm` | `aarch64-unknown-linux-musl` | same as the other Linux target |
| `macos-14` | `aarch64-apple-darwin` | `CARGO_PROFILE_RELEASE_STRIP=symbols`, ad-hoc `codesign`, wheel rebuilt so the signature survives |
| `macos-14` | `x86_64-apple-darwin` | same as the other macOS target |

Each package job runs `python3 packaging/skill_build.py`, then `maturin build --release --locked --target <triple> --out dist`, then `python3 scripts/package_skill.py --binary <binary> --target <triple> --dist dist`. The tag job runs `python3 scripts/package_skill.py --checksums --dist dist` and uploads the wheels, zips, and `dist/SHA256SUMS`.

There is no hosted deploy. The server speaks JSON-RPC over local stdio. Installing a release wheel, or unzipping a skill zip, does not need Rust.

## Test corpus

[test-corpus/README.md](test-corpus/README.md) is the fixture manifest: provenance, SHA-256, and expected outcome. Directories:

- `source/` — public U.S. Code Title 26 PDFs `sample-1.pdf`, `sample-2.pdf`, `sample-3.pdf`, plus `sample-4.README.txt` and `sample-5.README.txt` as empty slots
- `scanned/` — page renders and image-only PDFs from `scripts/build-scanned-corpus.py`
- `ground-truth/` — extracted text for samples 1–3, and placeholder files for samples 4 and 5
- `results/` — `summary.json` (OCR metrics). Regenerated `*.txt` outputs are gitignored
- `docx/`, `pptx/`, `xlsx/` (including `xlsx/source/`), `ods/`, `odt/`, `odp/`, `csv/`, `epub/` — public and synthetic format fixtures, including the hardening corpus from `scripts/build-anydoc-hardening-corpus.py`

`test-corpus/private/` and `test-corpus/local/` are gitignored.

## Conventions

- Format with `cargo fmt --all` before a PR. There is no repo `rustfmt.toml`.
- Clippy is `-D warnings` with `--workspace --all-targets --locked`.
- User-facing changes get an entry under `[Unreleased]` in [CHANGELOG.md](CHANGELOG.md).
- Dependencies come from crates.io. [deny.toml](deny.toml) sets `allow-git = []`.
- License is MIT OR Apache-2.0, at the user's option: [LICENSE-MIT](LICENSE-MIT), [LICENSE-APACHE](LICENSE-APACHE). The third-party inventory is [THIRD_PARTY.md](THIRD_PARTY.md).
- `PDF_INSPECTOR_MCP_LOG` may be `off`, `error`, `warn`, `info`, `debug`, or `trace`. The server ignores raw `RUST_LOG`.
- The skill does not set `ANYDOC_WORKER_BIN`. The binary re-executes itself as the worker. CI and the release workflow set `ANYDOC_ENHANCED_GIT_COMMIT` to the commit being built so `--provenance` can report it.

## Guardrails

- Commit public or fully synthetic fixtures only, with the redistribution basis recorded in [test-corpus/README.md](test-corpus/README.md). Personal and financial documents stay out of the tree, including redacted private exports.
- `bash scripts/check-public-hygiene.sh` has to pass. It scans the index and candidate files for private identifiers.
- Keep secrets, credentials, and private filesystem paths out of source, docs, tests, logs, and pull requests.
- No network fetch and no OCR service by default.
- Do not vendor Firecrawl parser source. Keep the crates.io pins above.
