# ODP fixture

public-presentation.odp is copied from the MIT-licensed Firecrawl AnyDoc v0.2.4
test corpus:

- Source repository: https://github.com/firecrawl/anydoc
- Source revision: 42bf1c5ecdde9eb0d96d6bd75a9e6698cf93b14c
- Source path: tests/fixtures/odp/pres.odp
- Local SHA-256: 6b5e859ad2591be8f1cbc0246d7e757fbc0ffa06e8ccd022a3e1612f67169df1

The deck contains two synthetic slides with title/body text, a table, grouped
shape text, a local image, and speaker notes. Its metadata contains no author,
account, client, URL, or identifying document content.

The strict ODP route requires the presentation mimetype and content.xml,
balanced XML, at least one slide, visible content, local referenced assets, and
no active or external content. The route uses AnyDoc v0.2.4 only after this
local preflight and the existing bounded worker.

## Adversarial derivatives

build-odp-corpus.py derives these deterministic packages from the public
fixture:

| Fixture | SHA-256 | Expected result |
|---|---|---|
| active-content.odp | c08e35649edaa4b9c750971382d8f1303585779ed64bcf76f61ab1214dae81c2 | active_content_disabled |
| archive-amplification.odp | 544972d3b1e60657cd204ecbea2515bc84ea819293dd4ca050239568eae36b2e | resource_limit |
| encrypted.odp | 52a9822b28d44dc98cd52432bd4b6a463b9fa124f64db333185be65b0331e3c1 | encrypted |
| external-reference.odp | b702616012350f96fc5a47b7d9b184c6d338f2bf5dd333f6fa8a9e3e86acdf32 | incomplete_conversion |
| hidden-content.odp | 911b1e6bee78415f05ed6cdebd56056c4e61257dce64e0b0f08bec0424a65989 | incomplete_conversion |
| malformed-content.odp | 06636472d0604bb6a2e6b29a8d8bf1a5e920cfc13e817e969643b7ad69db0ddb | malformed |
| missing-asset.odp | 053fc5a1808b29495b8f8f40011a99cb1070b5f4480df0baf65f8b8171383fba | incomplete_conversion |
| wrong-mimetype.odp | 2ddb8844bd62327b0db0656e830cf218bfa123ccba220fdac234dccd97950158 | malformed |

`linked-frame.odp` comes from `scripts/build-anydoc-hardening-corpus.py`: a
slide frame wrapped in `draw:a`, which AnyDoc's shape walk skips, so it must
return `incomplete_conversion` (SHA-256
`79bd10da4aea4f66a75a2564bf90fbbca40fc6d3020b0991885eeb88eb8244c7`).

The binary hashes are recorded here and in the top-level corpus manifest. No
upstream source code is vendored and no new Cargo dependency was added.
