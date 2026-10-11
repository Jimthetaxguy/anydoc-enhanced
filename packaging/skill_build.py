"""PEP 517 hook that copies the skill into the wheel, then calls maturin.

The wheel installs the compiled ``pdf-inspector-mcp`` binary and the skill
files. Building the wheel from an sdist still needs a Rust toolchain.
Installing a release wheel does not.
"""

from __future__ import annotations

import shutil
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "skills" / "anydoc-enhanced"
DEST = ROOT / "python" / "anydoc_enhanced" / "skill"


def _stage() -> None:
    if DEST.exists():
        shutil.rmtree(DEST)
    shutil.copytree(
        SOURCE,
        DEST,
        ignore=shutil.ignore_patterns("tests", "bin", "__pycache__", "*.pyc"),
    )


def _maturin():
    import maturin

    return maturin


def get_requires_for_build_wheel(config_settings=None):
    return _maturin().get_requires_for_build_wheel(config_settings)


def get_requires_for_build_sdist(config_settings=None):
    try:
        return _maturin().get_requires_for_build_sdist(config_settings)
    except AttributeError:
        return []


def prepare_metadata_for_build_wheel(metadata_directory, config_settings=None):
    _stage()
    return _maturin().prepare_metadata_for_build_wheel(metadata_directory, config_settings)


def build_wheel(wheel_directory, config_settings=None, metadata_directory=None):
    _stage()
    return _maturin().build_wheel(wheel_directory, config_settings, metadata_directory)


def build_sdist(sdist_directory, config_settings=None):
    return _maturin().build_sdist(sdist_directory, config_settings)


if __name__ == "__main__":
    # `maturin build` does not call the PEP 517 hooks. Stage first so the
    # wheel still contains SKILL.md and the scripts.
    _stage()
