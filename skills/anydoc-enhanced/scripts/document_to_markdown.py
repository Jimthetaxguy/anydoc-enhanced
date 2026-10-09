#!/usr/bin/env python3
"""Convert a local office document to Markdown. PDFs are refused."""

from __future__ import annotations

import _mcp


def main() -> int:
    return _mcp.main(
        tool="document_to_markdown",
        description=(
            "Convert a local DOCX, PPTX, XLSX, ODS, or ODT file to Markdown "
            "through the bounded AnyDoc worker. On Linux, strict CSV, ODP, and "
            "EPUB are also enabled. PDF inputs are refused; use pdf_to_markdown.py."
        ),
        examples=[
            "python3 skills/anydoc-enhanced/scripts/document_to_markdown.py --path test-corpus/docx/public-fixture.docx",
        ],
        invoke=_mcp.run_document_markdown,
    )


if __name__ == "__main__":
    raise SystemExit(main())
