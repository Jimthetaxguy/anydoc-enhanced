#!/usr/bin/env python3
"""Split a local SEC 10-K or 10-Q PDF by item. OCR is not performed."""

from __future__ import annotations

import _mcp


def main() -> int:
    return _mcp.main(
        tool="split_sec_filing",
        description=(
            "Split a local SEC 10-K or 10-Q PDF into sections by item number. "
            "A fully scanned PDF exits 3 instead of returning an empty split."
        ),
        examples=[
            "python3 skills/anydoc-enhanced/scripts/split_sec_filing.py --path test-corpus/source/sample-1.pdf",
        ],
        invoke=_mcp.run_pdf_domain,
    )


if __name__ == "__main__":
    raise SystemExit(main())
