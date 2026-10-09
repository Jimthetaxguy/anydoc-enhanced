#!/usr/bin/env python3
"""Parse IRC sections from a local Title 26 PDF. OCR is not performed."""

from __future__ import annotations

import _mcp


def main() -> int:
    return _mcp.main(
        tool="parse_irc_sections",
        description=(
            "Parse Internal Revenue Code sections from a local Title 26 PDF. "
            "A fully scanned PDF exits 3 instead of returning an empty section list."
        ),
        examples=[
            "python3 skills/anydoc-enhanced/scripts/parse_irc_sections.py --path test-corpus/source/sample-1.pdf",
        ],
        invoke=_mcp.run_pdf_domain,
    )


if __name__ == "__main__":
    raise SystemExit(main())
