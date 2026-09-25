

## Adversarial derivatives

The following synthetic derivatives exercise the strict local boundary through
the production MCP worker:

- `missing-slide.pptx` removes the declared slide part and must return
  `incomplete_conversion`.
- `active-content.pptx` adds an embedded OLE marker and must return
  `active_content_disabled`.

Both are derived from the public inheritance fixture and contain no identifying
metadata or executable payload. Their hashes are recorded in the corpus index.

Two synthetic decks from `scripts/build-anydoc-hardening-corpus.py` check that
the slide AnyDoc converts is the slide the boundary checked, and must return
`incomplete_conversion`:

- `fragment-slide-target.pptx` names its slide with a target whose fragment
  hides a `..` from AnyDoc's resolver.
- `case-variant-presentation-rels.pptx` adds a decoy
  `PPT/_rels/presentation.xml.rels` beside the part AnyDoc reads.

`section-list.pptx`, from the same generator, is a clean deck with a PowerPoint
section list and must convert: section entries (`p14:sldId`) are not slides.
