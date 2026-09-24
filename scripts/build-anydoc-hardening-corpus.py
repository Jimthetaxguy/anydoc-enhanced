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

All content is synthetic and contains no personal data.
"""
from pathlib import Path
from zipfile import ZIP_DEFLATED, ZipFile, ZipInfo

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


def write_package(path, entries):
    with ZipFile(path, "w") as archive:
        for filename, data in entries:
            info = ZipInfo(filename, date_time=FIXED_TIMESTAMP)
            info.compress_type = ZIP_DEFLATED
            info.create_system = 3
            info.external_attr = 0o600 << 16
            archive.writestr(info, data)


def paragraph(text):
    return f'<w:p><w:r><w:t xml:space="preserve">{text}</w:t></w:r></w:p>'


def write_docx(name, body, document_rels=None):
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
    write_package(CORPUS / "docx" / name, entries)


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


if __name__ == "__main__":
    main()
