#!/usr/bin/env python3
"""Convert a local PDF to Markdown with pdf-inspector. OCR is not performed."""

from __future__ import annotations

import _mcp


def main() -> int:
    return _mcp.main(
        tool="pdf_to_markdown",
        description=(
            "Convert a local PDF to Markdown through pdf-inspector. "
            "Pages that need OCR are listed and left unread; text from other pages is kept. "
            "Exit 3 means OCR is required. PDFs are never sent through AnyDoc."
        ),
        examples=[
            "python3 skills/anydoc-enhanced/scripts/pdf_to_markdown.py --path test-corpus/source/sample-1.pdf",
            "python3 skills/anydoc-enhanced/scripts/pdf_to_markdown.py --path test-corpus/scanned/sample-1.pdf",
        ],
        invoke=_mcp.run_pdf_markdown,
    )


if __name__ == "__main__":
    raise SystemExit(main())
