#!/usr/bin/env python3
"""Build the deterministic, synthetic EPUB containment corpus."""

from __future__ import annotations

import base64
from pathlib import Path
from zipfile import ZIP_DEFLATED, ZIP_STORED, ZipFile, ZipInfo

ROOT = Path(__file__).resolve().parents[1] / "test-corpus" / "epub"
MIMETYPE = b"application/epub+zip"
CONTAINER = (
    b"<?xml version=\"1.0\"?><container "
    b"xmlns=\"urn:oasis:names:tc:opendocument:xmlns:container\">"
    b"<rootfiles><rootfile full-path=\"OPS/package.opf\" "
    b"media-type=\"application/oebps-package+xml\"/></rootfiles></container>"
)
LOGO = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk"
    "+A8AAQUBAScY42YAAAAASUVORK5CYII="
)

CHAPTERS = {
    1: (
        "<?xml version=\"1.0\"?><html xmlns=\"http://www.w3.org/1999/xhtml\">"
        "<head><title>Scope</title></head><body>"
        "<p>EPUB-C01-BEGIN</p><h1>EPUB-H1-SCOPE</h1>"
        "<p>Public synthetic extraction fixture.</p>"
        "<p>EPUB-C01-END</p></body></html>"
    ).encode(),
    2: (
        "<?xml version=\"1.0\"?><html xmlns=\"http://www.w3.org/1999/xhtml\">"
        "<head><title>Evidence</title></head><body>"
        "<p id=\"section-two\">EPUB-C02-BEGIN</p><h1>EPUB-H1-EVIDENCE</h1>"
        "<ol><li>EPUB-LIST-01</li><li>EPUB-LIST-02</li></ol>"
        "<table><tr><th>EPUB-TABLE-FIELD</th><th>Value</th></tr>"
        "<tr><td>Sample</td><td>EPUB-TABLE-42</td></tr></table>"
        "<p>EPUB-C02-END</p></body></html>"
    ).encode(),
    3: (
        "<?xml version=\"1.0\"?><html xmlns=\"http://www.w3.org/1999/xhtml\">"
        "<head><title>Conclusion</title></head><body>"
        "<p>EPUB-C03-BEGIN</p><h1>EPUB-H1-CONCLUSION</h1>"
        "<p><a href=\"chapter-2.xhtml#section-two\">EPUB-INTERNAL-LINK</a>"
        "</p><p><img src=\"../images/logo.png\" alt=\"EPUB-IMAGE-ALT\"/></p>"
        "<p>EPUB-C03-END</p></body></html>"
    ).encode(),
}

def opf(*, include_nav: bool = True, missing_resource: bool = False) -> bytes:
    items = [
        "<item id=\"ch1\" href=\"Text/chapter-1.xhtml\" media-type=\"application/xhtml+xml\"/>",
        "<item id=\"ch2\" href=\"Text/chapter-2.xhtml\" media-type=\"application/xhtml+xml\"/>",
        "<item id=\"ch3\" href=\"Text/chapter-3.xhtml\" media-type=\"application/xhtml+xml\"/>",
        "<item id=\"logo\" href=\"images/logo.png\" media-type=\"image/png\"/>",
    ]
    if include_nav:
        items.append(
            "<item id=\"nav\" href=\"nav.xhtml\" media-type=\"application/xhtml+xml\" "
            "properties=\"nav\"/>"
        )
    if missing_resource:
        items.append(
            "<item id=\"style\" href=\"Styles/missing.css\" media-type=\"text/css\"/>"
        )
    manifest = "".join(items)
    return (
        "<?xml version=\"1.0\"?><package xmlns=\"http://www.idpf.org/2007/opf\" "
        "xmlns:dc=\"http://purl.org/dc/elements/1.1/\" version=\"3.0\">"
        "<metadata><dc:title>Public EPUB Extraction Corpus</dc:title>"
        "<dc:language>en</dc:language></metadata><manifest>"
        + manifest
        + "</manifest><spine><itemref idref=\"ch1\"/><itemref idref=\"ch2\"/>"
        "<itemref idref=\"ch3\"/></spine></package>"
    ).encode()

def nav(order: list[int]) -> bytes:
    links = "".join(
        f"<li><a href=\"Text/chapter-{number}.xhtml#EPUB-C0{number}-BEGIN\">"
        f"Chapter {number}</a></li>"
        for number in order
    )
    return (
        "<?xml version=\"1.0\"?><html xmlns=\"http://www.w3.org/1999/xhtml\" "
        "xmlns:epub=\"http://www.idpf.org/2007/ops\"><head><title>Contents</title>"
        "</head><body><nav epub:type=\"toc\"><ol>"
        + links
        + "</ol></nav></body></html>"
    ).encode()

def write_entry(archive: ZipFile, name: str, data: bytes, compression: int) -> None:
    info = ZipInfo(name, (1980, 1, 1, 0, 0, 0))
    info.compress_type = compression
    info.external_attr = 0o600 << 16
    archive.writestr(info, data)

def write_package(
    name: str,
    *,
    chapter_one: bytes = CHAPTERS[1],
    chapter_two: bytes | None = CHAPTERS[2],
    chapter_three: bytes | None = CHAPTERS[3],
    nav_order: list[int] | None = [1, 2, 3],
    missing_resource: bool = False,
    extra: dict[str, bytes] | None = None,
    chapter_order: list[int] = [1, 2, 3],
    compression: int = ZIP_DEFLATED,
) -> None:
    chapters = {1: chapter_one, 2: chapter_two, 3: chapter_three}
    path = ROOT / name
    with ZipFile(path, "w") as archive:
        write_entry(archive, "mimetype", MIMETYPE, ZIP_STORED)
        write_entry(archive, "META-INF/container.xml", CONTAINER, compression)
        write_entry(archive, "OPS/package.opf", opf(missing_resource=missing_resource), compression)
        if nav_order is not None:
            write_entry(archive, "OPS/nav.xhtml", nav(nav_order), compression)
        for number in chapter_order:
            if chapters[number] is not None:
                write_entry(
                    archive,
                    f"OPS/Text/chapter-{number}.xhtml",
                    chapters[number],
                    compression,
                )
        write_entry(archive, "OPS/images/logo.png", LOGO, ZIP_STORED)
        for entry, data in (extra or {}).items():
            write_entry(archive, entry, data, compression)

def main() -> None:
    ROOT.mkdir(parents=True, exist_ok=True)
    write_package("public-spine-order.epub", chapter_order=[3, 1, 2])
    write_package("missing-spine-chapter.epub", chapter_two=None)
    write_package(
        "malformed-spine-chapter.epub",
        chapter_two=b"<html xmlns=\"http://www.w3.org/1999/xhtml\"><body><h1>Unclosed",
    )
    write_package("nav-spine-mismatch.epub", nav_order=[3, 2, 1])
    write_package("missing-local-resource.epub", missing_resource=True)
    write_package(
        "external-reference.epub",
        chapter_one=(
            "<?xml version=\"1.0\"?><html xmlns=\"http://www.w3.org/1999/xhtml\">"
            "<head/><body><h1>External</h1><p><a href=\"https://example.invalid/reference\">"
            "EPUB-EXTERNAL-REFERENCE</a></p></body></html>"
        ).encode(),
    )
    write_package(
        "active-content.epub",
        chapter_one=(
            "<?xml version=\"1.0\"?><html xmlns=\"http://www.w3.org/1999/xhtml\">"
            "<head/><body><h1>Active</h1><script>alert(1)</script></body></html>"
        ).encode(),
    )
    write_package(
        "hidden-content.epub",
        chapter_one=(
            "<?xml version=\"1.0\"?><html xmlns=\"http://www.w3.org/1999/xhtml\">"
            "<head/><body><h1>Hidden</h1><p style=\"visibility:hidden\">Hidden text</p>"
            "</body></html>"
        ).encode(),
    )
    write_package(
        "encrypted.epub",
        extra={"META-INF/encryption.xml": b"<encryption/>"},
    )
    write_package(
        "archive-amplification.epub",
        extra={"OPS/data/repeat.bin": b"A" * (17 * 1024 * 1024)},
    )

if __name__ == "__main__":
    main()
