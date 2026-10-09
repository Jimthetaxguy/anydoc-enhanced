#!/usr/bin/env python3
"""Build a per-platform skill zip around an already compiled pdf-inspector-mcp.

The zip extracts to ``anydoc-enhanced/`` so it can be unpacked into
``.claude/skills/`` or another skills directory. It contains SKILL.md,
scripts/, references/, and ``bin/<target>/pdf-inspector-mcp``. Tests are
left in the git tree and are not part of the drop-in folder.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import stat
import subprocess
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SKILL = ROOT / "skills" / "anydoc-enhanced"
ZIP_ROOT = "anydoc-enhanced"


def provenance_of(binary: Path) -> dict:
    completed = subprocess.run(
        [str(binary), "--provenance"],
        check=False,
        capture_output=True,
        timeout=30,
    )
    if completed.returncode != 0:
        raise SystemExit("pdf-inspector-mcp --provenance failed")
    payload = json.loads(completed.stdout.decode("utf-8"))
    if not isinstance(payload, dict) or "target" not in payload:
        raise SystemExit("pdf-inspector-mcp --provenance did not return a build object")
    return payload


def copy_tree(archive: zipfile.ZipFile, source: Path, destination: str) -> None:
    for path in sorted(source.rglob("*")):
        if not path.is_file():
            continue
        if "__pycache__" in path.parts or path.suffix == ".pyc":
            continue
        relative = path.relative_to(source).as_posix()
        write_file(archive, path, f"{destination}/{relative}", executable=False)


def write_file(archive: zipfile.ZipFile, source: Path, arcname: str, executable: bool) -> None:
    data = source.read_bytes()
    info = zipfile.ZipInfo(arcname)
    info.compress_type = zipfile.ZIP_DEFLATED
    info.create_system = 3
    mode = 0o100755 if executable else 0o100644
    info.external_attr = mode << 16
    archive.writestr(info, data)


def package(binary: Path, target: str, dist: Path, sign_macos: bool) -> Path:
    identity = provenance_of(binary)
    if identity["target"] != target:
        raise SystemExit(
            f"binary target {identity['target']} does not match requested {target}"
        )
    if sign_macos:
        subprocess.run(
            ["codesign", "--force", "--sign", "-", str(binary)],
            check=True,
        )
    dist.mkdir(parents=True, exist_ok=True)
    zip_path = dist / f"anydoc-enhanced-{target}.zip"
    with zipfile.ZipFile(zip_path, "w") as archive:
        write_file(archive, SKILL / "SKILL.md", f"{ZIP_ROOT}/SKILL.md", executable=False)
        copy_tree(archive, SKILL / "scripts", f"{ZIP_ROOT}/scripts")
        copy_tree(archive, SKILL / "references", f"{ZIP_ROOT}/references")
        binary_name = "pdf-inspector-mcp.exe" if target.endswith("windows-msvc") else "pdf-inspector-mcp"
        arc_binary = f"{ZIP_ROOT}/bin/{target}/{binary_name}"
        write_file(archive, binary, arc_binary, executable=True)
        manifest = json.dumps(identity, indent=2).encode("utf-8") + b"\n"
        info = zipfile.ZipInfo(f"{ZIP_ROOT}/bin/{target}/provenance.json")
        info.compress_type = zipfile.ZIP_DEFLATED
        info.create_system = 3
        info.external_attr = 0o100644 << 16
        archive.writestr(info, manifest)
    digest = hashlib.sha256(zip_path.read_bytes()).hexdigest()
    checksum = dist / f"{zip_path.name}.sha256"
    checksum.write_text(f"{digest}  {zip_path.name}\n", encoding="utf-8")
    print(zip_path)
    return zip_path


def write_checksums(dist: Path) -> None:
    lines = []
    for path in sorted(dist.iterdir()):
        if path.name == "SHA256SUMS" or path.suffix == ".sha256":
            continue
        if not path.is_file():
            continue
        if path.suffix not in {".whl", ".zip"}:
            continue
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        lines.append(f"{digest}  {path.name}")
    text = "\n".join(lines) + ("\n" if lines else "")
    (dist / "SHA256SUMS").write_text(text, encoding="utf-8")
    print(dist / "SHA256SUMS")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, help="Compiled pdf-inspector-mcp to bundle")
    parser.add_argument("--target", help="Rust target triple, matched against --provenance")
    parser.add_argument("--dist", type=Path, default=ROOT / "dist")
    parser.add_argument(
        "--sign-macos",
        action="store_true",
        help="Ad-hoc codesign the binary before zipping. This does not satisfy Gatekeeper.",
    )
    parser.add_argument(
        "--checksums",
        action="store_true",
        help="Write dist/SHA256SUMS for wheels and zips already in --dist",
    )
    args = parser.parse_args()
    if args.checksums:
        write_checksums(args.dist)
        return
    if args.binary is None or args.target is None:
        raise SystemExit("--binary and --target are required unless --checksums is set")
    binary = args.binary.resolve()
    if not binary.is_file() or not (binary.stat().st_mode & stat.S_IXUSR):
        raise SystemExit(f"not an executable binary: {binary.name}")
    package(binary, args.target, args.dist, args.sign_macos)


if __name__ == "__main__":
    main()
