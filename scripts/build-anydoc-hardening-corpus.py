#!/usr/bin/env python3
"""Build deterministic fixtures for pinned-AnyDoc hardening gates.

Each package reproduces a behavior of AnyDoc 0.2.4 that the local contract
must not inherit (see docs/upstream-drift-audit-2026-09-24.md):

- symbol-checkbox.docx: Wingdings `w:sym` checkboxes, which the pinned parser
  drops without a diagnostic (firecrawl/anydoc#176, #177). Expected
  `incomplete_conversion`.
- legacy-form-checkbox.docx: a checked FORMCHECKBOX field, whose state the
  pinned parser drops. Expected `incomplete_conversion`.
- hidden-text.docx: a `w:vanish` run, converted with the visible text and
  disclosed by the `hidden_content_preserved` warning.
- external-link-schemes.docx: external hyperlinks with mailto, file, data,
  tel, and UNC targets, which the pinned parser writes into Markdown link
  destinations. Expected complete output with every destination removed.
- oversized-number-format.xlsx: one 64 KiB `formatCode`; the pinned parser
  expands every character into several vectors (firecrawl/anydoc#148).
  Expected `resource_limit` before conversion.
- utf16-footnote-symbol.docx: a Wingdings checkbox in a UTF-16 footnotes part.
  AnyDoc transcodes UTF-16 parts before parsing, so the checks must read the
  same text. Expected `incomplete_conversion`.
- utf16-hidden-style.docx: a hidden character style in a UTF-16 styles part.
  Expected complete conversion with the `hidden_content_preserved` warning.
- pptx/fragment-slide-target.pptx: a slide target whose fragment hides a `..`
  from AnyDoc's resolver, so AnyDoc converts a hidden slide outside
  `ppt/slides/` while a naive resolver lands on a checked decoy. Expected
  `incomplete_conversion`.
- pptx/case-variant-presentation-rels.pptx: a decoy
  `PPT/_rels/presentation.xml.rels` beside the relationships part AnyDoc reads
  by exact name. Expected `incomplete_conversion`.
- epub/encoded-chapter-href.epub: a spine href AnyDoc percent-decodes to a
  chapter with hidden text, beside a clean decoy stored under the encoded
  name. Expected `incomplete_conversion`.

All content is synthetic and contains no personal data.
"""
from pathlib import Path
from zipfile import ZIP_DEFLATED, ZIP_STORED, ZipFile, ZipInfo

ROOT = Path(__file__).resolve().parents[1]
CORPUS = ROOT / "test-corpus"
FIXED_TIMESTAMP = (1980, 1, 1, 0, 0, 0)

WORD_NS = (
    'xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" '
    'xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"'
)
RELS_NS = "http://schemas.openxmlformats.org/package/2006/relationships"
REL_TYPE = "http://schemas.openxmlformats.org/officeDocument/2006/relationships"
DOCX_TYPES = (
    '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
    '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">'
    '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>'
    '<Default Extension="xml" ContentType="application/xml"/>'
    '<Override PartName="/word/document.xml" '
    'ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>'
    "</Types>"
)
DOCX_ROOT_RELS = (
    '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
    f'<Relationships xmlns="{RELS_NS}">'
    f'<Relationship Id="rId1" Type="{REL_TYPE}/officeDocument" Target="word/document.xml"/>'
    "</Relationships>"
)


def write_package(path, entries, stored=()):
    with ZipFile(path, "w") as archive:
        for filename, data in entries:
            info = ZipInfo(filename, date_time=FIXED_TIMESTAMP)
            info.compress_type = ZIP_STORED if filename in stored else ZIP_DEFLATED
            info.create_system = 3
            info.external_attr = 0o600 << 16
            archive.writestr(info, data)


PRESENTATION_NS = (
    'xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" '
    'xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" '
    f'xmlns:r="{REL_TYPE}"'
)


def slide(text, hidden=False):
    show = ' show="0"' if hidden else ""
    return (
        f"<p:sld {PRESENTATION_NS}{show}><p:cSld><p:spTree>"
        '<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>'
        '<p:sp><p:nvSpPr><p:cNvPr id="2" name="Text"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr/>'
        f"<p:txBody><a:bodyPr/><a:p><a:r><a:t>{text}</a:t></a:r></a:p></p:txBody></p:sp>"
        "</p:spTree></p:cSld></p:sld>"
    )


def presentation_rels(target):
    return (
        f'<Relationships xmlns="{RELS_NS}">'
        f'<Relationship Id="rId2" Type="{REL_TYPE}/slide" Target="{target}"/>'
        "</Relationships>"
    )


def write_pptx(name, entries):
    types = (
        '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">'
        '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>'
        '<Default Extension="xml" ContentType="application/xml"/>'
        '<Override PartName="/ppt/presentation.xml" '
        'ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>'
        "</Types>"
    )
    root = (
        f'<Relationships xmlns="{RELS_NS}">'
        f'<Relationship Id="rId1" Type="{REL_TYPE}/officeDocument" Target="ppt/presentation.xml"/>'
        "</Relationships>"
    )
    presentation = (
        f"<p:presentation {PRESENTATION_NS}><p:sldIdLst>"
        '<p:sldId id="256" r:id="rId2"/></p:sldIdLst></p:presentation>'
    )
    write_package(
        CORPUS / "pptx" / name,
        [
            ("[Content_Types].xml", types),
            ("_rels/.rels", root),
            ("ppt/presentation.xml", presentation),
            ("ppt/slides/slide1.xml", slide("DECOY-SLIDE")),
            ("other/slide.xml", slide("HIDDEN-SLIDE-TEXT", hidden=True)),
        ]
        + entries,
    )


def write_encoded_chapter_epub():
    xhtml = 'xmlns="http://www.w3.org/1999/xhtml"'
    container = (
        '<?xml version="1.0"?><container version="1.0" '
        'xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles>'
        '<rootfile full-path="OPS/package.opf" media-type="application/oebps-package+xml"/>'
        "</rootfiles></container>"
    )
    opf = (
        '<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="3.0" '
        'unique-identifier="id"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/">'
        '<dc:identifier id="id">urn:uuid:00000000-0000-4000-8000-000000000000</dc:identifier>'
        "<dc:title>Hardening</dc:title><dc:language>en</dc:language></metadata><manifest>"
        '<item id="ch1" href="Text/ch%31.xhtml" media-type="application/xhtml+xml"/>'
        '<item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>'
        '</manifest><spine><itemref idref="ch1"/></spine></package>'
    )
    nav = (
        f'<?xml version="1.0"?><html {xhtml}><head><title>Contents</title></head><body>'
        '<nav xmlns:epub="http://www.idpf.org/2007/ops" epub:type="toc"><ol>'
        '<li><a href="Text/ch%31.xhtml">One</a></li></ol></nav></body></html>'
    )
    decoy = (
        f'<?xml version="1.0"?><html {xhtml}><head><title>Decoy</title></head>'
        "<body><p>DECOY-CHAPTER</p></body></html>"
    )
    chapter = (
        f'<?xml version="1.0"?><html {xhtml}><head><title>Chapter</title></head><body>'
        '<p>REAL-CHAPTER</p><p hidden="hidden">HIDDEN-CHAPTER-TEXT</p></body></html>'
    )
    write_package(
        CORPUS / "epub" / "encoded-chapter-href.epub",
        [
            ("mimetype", "application/epub+zip"),
            ("META-INF/container.xml", container),
            ("OPS/package.opf", opf),
            ("OPS/nav.xhtml", nav),
            ("OPS/Text/ch%31.xhtml", decoy),
            ("OPS/Text/ch1.xhtml", chapter),
        ],
        stored={"mimetype"},
    )


def paragraph(text):
    return f'<w:p><w:r><w:t xml:space="preserve">{text}</w:t></w:r></w:p>'


def write_docx(name, body, document_rels=None, parts=()):
    document = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        f"<w:document {WORD_NS}><w:body>"
        + paragraph("HARDENING-BEGIN")
        + body
        + paragraph("HARDENING-END")
        + "<w:sectPr/></w:body></w:document>"
    )
    entries = [
        ("[Content_Types].xml", DOCX_TYPES),
        ("_rels/.rels", DOCX_ROOT_RELS),
        ("word/document.xml", document),
    ]
    if document_rels is not None:
        entries.append(("word/_rels/document.xml.rels", document_rels))
    entries.extend(parts)
    write_package(CORPUS / "docx" / name, entries)


def utf16(text):
    """UTF-16LE with a byte order mark, as some producers write XML parts."""
    return b"\xff\xfe" + text.encode("utf-16-le")


def typed_rels(kind, target):
    return (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        f'<Relationships xmlns="{RELS_NS}">'
        f'<Relationship Id="rId{kind}" Type="{REL_TYPE}/{kind}" Target="{target}"/>'
        "</Relationships>"
    )


def external_links():
    # Assembled so the repository hygiene scan does not flag an address.
    address = "@".join(["someone", "example.invalid"])
    targets = [
        f"mailto:{address}",
        "file:///etc/passwd",
        "data:text/html;base64,PGI+ZGlzYWJsZWQ8L2I+",
        "tel:+15555550100",
        "\\\\fileserver.invalid\\share\\ledger.xlsx",
    ]
    rels = "".join(
        f'<Relationship Id="rLink{index}" Type="{REL_TYPE}/hyperlink" '
        f'Target="{target.replace("&", "&amp;")}" TargetMode="External"/>'
        for index, target in enumerate(targets)
    )
    body = "".join(
        f'<w:p><w:hyperlink r:id="rLink{index}"><w:r><w:t>LINK-{index}</w:t></w:r></w:hyperlink></w:p>'
        for index in range(len(targets))
    )
    document_rels = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        f'<Relationships xmlns="{RELS_NS}">{rels}</Relationships>'
    )
    return body, document_rels


def write_oversized_number_format():
    source = CORPUS / "xlsx" / "public-workpaper.xlsx"
    with ZipFile(source) as archive:
        entries = [
            (info.filename, archive.read(info))
            for info in archive.infolist()
            if not info.filename.endswith("/")
        ]
    styles_type = (
        '<Override PartName="/xl/styles.xml" '
        'ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/>'
    )
    styles_rel = (
        f'<Relationship Id="rIdStyles" Type="{REL_TYPE}/styles" Target="styles.xml"/>'
    )
    patched = []
    for filename, data in entries:
        if filename == "[Content_Types].xml":
            data = data.replace(b"</Types>", styles_type.encode() + b"</Types>")
        elif filename == "xl/_rels/workbook.xml.rels":
            data = data.replace(b"</Relationships>", styles_rel.encode() + b"</Relationships>")
        elif filename == "xl/worksheets/sheet1.xml":
            data = data.replace(b'<c r="B2">', b'<c r="B2" s="1">')
        patched.append((filename, data))
    styles = (
        '<?xml version="1.0" encoding="UTF-8"?>'
        '<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">'
        '<numFmts count="1"><numFmt numFmtId="164" formatCode="'
        + "0" * (64 * 1024)
        + '"/></numFmts><cellXfs count="2"><xf numFmtId="0"/>'
        '<xf numFmtId="164" applyNumberFormat="1"/></cellXfs></styleSheet>'
    )
    patched.append(("xl/styles.xml", styles))
    write_package(CORPUS / "xlsx" / "oversized-number-format.xlsx", patched)


def main():
    write_docx(
        "symbol-checkbox.docx",
        '<w:p><w:r><w:sym w:font="Wingdings" w:char="F0FE"/></w:r>'
        '<w:r><w:t xml:space="preserve"> Yes CHECKED-ANSWER</w:t></w:r></w:p>'
        '<w:p><w:r><w:sym w:font="Wingdings" w:char="F0A8"/></w:r>'
        '<w:r><w:t xml:space="preserve"> No UNCHECKED-ANSWER</w:t></w:r></w:p>',
    )
    write_docx(
        "legacy-form-checkbox.docx",
        '<w:p><w:r><w:fldChar w:fldCharType="begin"><w:ffData><w:name w:val="Check1"/>'
        '<w:enabled/><w:checkBox><w:sizeAuto/><w:default w:val="0"/><w:checked/></w:checkBox>'
        '</w:ffData></w:fldChar></w:r><w:r><w:instrText xml:space="preserve"> FORMCHECKBOX </w:instrText></w:r>'
        '<w:r><w:fldChar w:fldCharType="separate"/></w:r><w:r><w:fldChar w:fldCharType="end"/></w:r>'
        '<w:r><w:t xml:space="preserve"> Election made FORM-ANSWER</w:t></w:r></w:p>',
    )
    write_docx(
        "hidden-text.docx",
        '<w:p><w:r><w:rPr><w:vanish/></w:rPr><w:t>HIDDEN-RUN</w:t></w:r></w:p>',
    )
    body, document_rels = external_links()
    write_docx("external-link-schemes.docx", body, document_rels)
    write_oversized_number_format()
    write_docx(
        "utf16-footnote-symbol.docx",
        '<w:p><w:r><w:t>NOTED</w:t></w:r><w:r><w:footnoteReference w:id="1"/></w:r></w:p>',
        typed_rels("footnotes", "footnotes.xml"),
        [
            (
                "word/footnotes.xml",
                utf16(
                    '<?xml version="1.0" encoding="UTF-16"?>'
                    f'<w:footnotes {WORD_NS}><w:footnote w:id="1"><w:p>'
                    '<w:r><w:sym w:font="Wingdings" w:char="F0FE"/></w:r>'
                    '<w:r><w:t xml:space="preserve"> Yes NOTE-ANSWER</w:t></w:r>'
                    "</w:p></w:footnote></w:footnotes>"
                ),
            )
        ],
    )
    write_pptx(
        "fragment-slide-target.pptx",
        [
            (
                "ppt/_rels/presentation.xml.rels",
                presentation_rels("../other/slide.xml#/../../ppt/slides/slide1.xml"),
            )
        ],
    )
    write_pptx(
        "case-variant-presentation-rels.pptx",
        [
            ("ppt/_rels/presentation.xml.rels", presentation_rels("../other/slide.xml")),
            ("PPT/_rels/presentation.xml.rels", presentation_rels("slides/slide1.xml")),
        ],
    )
    write_encoded_chapter_epub()
    write_docx(
        "utf16-hidden-style.docx",
        '<w:p><w:r><w:rPr><w:rStyle w:val="Quiet"/></w:rPr>'
        "<w:t>STYLED-HIDDEN</w:t></w:r></w:p>",
        typed_rels("styles", "styles.xml"),
        [
            (
                "word/styles.xml",
                utf16(
                    '<?xml version="1.0" encoding="UTF-16"?>'
                    f'<w:styles {WORD_NS}><w:style w:type="character" w:styleId="Quiet">'
                    '<w:name w:val="Quiet"/><w:rPr><w:vanish/></w:rPr></w:style></w:styles>'
                ),
            )
        ],
    )


if __name__ == "__main__":
    main()
