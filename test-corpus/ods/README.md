# ODS fixture

public-workpaper.ods is copied from the MIT-licensed Firecrawl AnyDoc v0.2.4
test corpus:

- Source repository: https://github.com/firecrawl/anydoc
- Source revision: 42bf1c5ecdde9eb0d96d6bd75a9e6698cf93b14c
- Source path: tests/fixtures/ods/sheet.ods
- Local SHA-256: e2c092eb2173b9c7ea8dd2c42e8dd7ef284cf37de40e206f3c8e43571de88454

The archive contains two synthetic sheets (Values and Merged Grid), typed
percentage/currency/date/time/boolean values, and a merged cell span. It has no
author, account, client, URL, external relationship, macro, or identifying
document content. Its benign LibreOffice Configurations2 entries are retained
as fixture provenance and are not executable document content.

The strict ODS route expects complete Markdown, rejects hidden or externally
referenced content, and reads cached/displayed values without evaluating
formulas.

## Adversarial derivatives

The strict route also has synthetic derivatives for the promotion gate:

- `external-reference.ods` contains an external xlink and must return
  `incomplete_conversion`.
- `active-content.ods` contains a script package marker and must return
  `active_content_disabled`.
- `missing-table.ods` has valid ODS identity but no spreadsheet table and must
  return `incomplete_conversion`.

They are derived from the public fixture, contain no identifying metadata, and
their hashes are recorded in the corpus index.

## Number-format and drawing fixtures

`scripts/build-anydoc-hardening-corpus.py` generates three more synthetic
spreadsheets, each of which must return `incomplete_conversion`:

- `negative-sign-by-colour.ods`: a negative value displayed without a sign.
- `format-hidden-value.ods`: a value displayed as an empty paragraph, which
  AnyDoc would replace with the typed value.
- `cell-anchored-text-box.ods`: a text box anchored to a cell.

Their hashes are recorded in the corpus index.
