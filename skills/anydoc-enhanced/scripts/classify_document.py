#!/usr/bin/env python3
"""Classify a local document and report whether its generic route is enabled."""

from __future__ import annotations

import _mcp


def main() -> int:
    return _mcp.main(
        tool="classify_document",
        description=(
            "Detect a local file's kind. PDF results use the pdf-inspector route. "
            "Office results use the AnyDoc route. This command does not convert the file."
        ),
        examples=[
            "python3 skills/anydoc-enhanced/scripts/classify_document.py --path test-corpus/docx/public-fixture.docx",
            "python3 skills/anydoc-enhanced/scripts/classify_document.py --path test-corpus/source/sample-1.pdf",
        ],
        invoke=_mcp.run_classify_document,
    )


if __name__ == "__main__":
    raise SystemExit(main())
