#!/usr/bin/env python3
"""Classify a local PDF. OCR is not performed."""

from __future__ import annotations

import _mcp


def main() -> int:
    return _mcp.main(
        tool="classify_pdf",
        description=(
            "Classify a local PDF as TextBased, Scanned, ImageBased, or Mixed. "
            "pages_needing_ocr is 1-based and is a successful finding, not a failure."
        ),
        examples=[
            "python3 skills/anydoc-enhanced/scripts/classify_pdf.py --path test-corpus/source/sample-1.pdf",
            "python3 skills/anydoc-enhanced/scripts/classify_pdf.py --path test-corpus/scanned/sample-1.pdf",
        ],
        invoke=_mcp.run_pdf_report,
    )


if __name__ == "__main__":
    raise SystemExit(main())
