# Contributing to anydoc-enhanced

Thanks for your interest. This project is small and pragmatic; contributions
that improve real-PDF coverage, fix domain-parser edge cases, or extend the
MCP surface are all welcome.

## Quick start

```bash
git clone https://github.com/Jimthetaxguy/anydoc-enhanced
cd anydoc-enhanced
cargo build --workspace --locked
cargo test --workspace --locked
```

## Validate against your own PDFs

The validation runner exercises the domain tools (tax / irc / sec) against
real input files:

```bash
cargo run --example validate_domain -- tax /path/to/your.pdf
cargo run --example validate_domain -- irc /path/to/title-26.pdf
cargo run --example validate_domain -- sec /path/to/10-K.pdf
```

## Code standards

- Format: `cargo fmt --all`
- Lint:   `cargo clippy --workspace --all-targets --locked -- -D warnings`
- Test:   `cargo test --workspace --locked`
- Skill:  `PDF_INSPECTOR_MCP_BIN=target/release/pdf-inspector-mcp python3 skills/anydoc-enhanced/tests/test_scripts.py`
- Candidate-text hygiene: `bash scripts/check-public-hygiene.sh`

All four must pass before a PR is merged.

## PR checklist

- [ ] Tests pass (`cargo test --workspace --locked`)
- [ ] Clippy clean (`cargo clippy --workspace --all-targets --locked -- -D warnings`)
- [ ] Candidate-text hygiene check passes (`bash scripts/check-public-hygiene.sh`)
- [ ] No PII or personal data in test fixtures, commit messages, or logs
- [ ] CHANGELOG.md updated under `[Unreleased]` if the change is user-facing

## Test fixtures

Never commit a real personal or financial PDF — no W-2s, 1099s, K-1s, bank
statements, offer letters, or anything with a name, SSN, account number, or
dollar amount tied to a real person. Use fully synthetic fixtures or public
documents with a recorded redistribution basis. Anonymized or redacted private
exports are not accepted because residual metadata and re-identification risk
are difficult to prove absent.

When in doubt, leave the fixture out. Validate it locally without recording its
path, contents, or metadata in source, tests, logs, commits, or pull requests.
