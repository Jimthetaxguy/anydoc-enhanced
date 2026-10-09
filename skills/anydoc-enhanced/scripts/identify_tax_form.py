#!/usr/bin/env python3
"""Identify a tax form in a local PDF. OCR is not performed."""

from __future__ import annotations

import _mcp


def main() -> int:
    return _mcp.main(
        tool="identify_tax_form",
        description=(
            "Identify a W-2, 1099, K-1, 1040, 1065, 1120, or schedule in a local PDF. "
            "A fully scanned PDF exits 3 and is not reported as an unknown form."
        ),
        examples=[
            "python3 skills/anydoc-enhanced/scripts/identify_tax_form.py --path test-corpus/source/sample-1.pdf",
        ],
        invoke=_mcp.run_pdf_domain,
    )


if __name__ == "__main__":
    raise SystemExit(main())
