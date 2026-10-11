"""Install helper for the anydoc-enhanced wheel.

``pip install`` places ``pdf-inspector-mcp`` on PATH. The agent skill files
ship as package data under ``skill/`` when the wheel is built, and as a
per-platform zip with the binary under ``bin/<target>/``.
"""

from pathlib import Path


def skill_directory() -> Path | None:
    """Return the bundled skill folder, when this install includes one."""
    candidate = Path(__file__).resolve().parent / "skill"
    if (candidate / "SKILL.md").is_file():
        return candidate
    return None
