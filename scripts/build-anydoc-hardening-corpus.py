#!/usr/bin/env python3
"""Build deterministic fixtures for pinned-AnyDoc hardening gates.

Each package reproduces a behavior of AnyDoc 0.2.4 that the local contract
must not inherit (see docs/upstream-drift-audit-2026-09-24.md):

- symbol-checkbox.docx: Wingdings `w:sym` checkboxes, which the pinned parser
  drops without a diagnostic. The open firecrawl/anydoc#177 renders only the
  F0FE and F06F Wingdings codes; F0A8 stays dropped. Expected
  `incomplete_conversion`.
- legacy-form-checkbox.docx: a checked FORMCHECKBOX field, whose state the
  pinned parser drops. Expected `incomplete_conversion`.
- hidden-text.docx: a `w:vanish` run, converted with the visible text and
  disclosed by the `hidden_content_preserved` warning.
- external-link-schemes.docx: external hyperlinks with mailto, file, data,
  tel, and UNC targets, which the pinned parser writes into Markdown link
  destinations. Expected complete output with every destination removed.
- oversized-number-format.xlsx: one 64 KiB `formatCode`; the pinned parser
  expands every character into several vectors (capped upstream only in the
  open firecrawl/anydoc#148).
  Expected `resource_limit` before conversion.
- utf16-footnote-symbol.docx: a Wingdings checkbox in a UTF-16 footnotes part.
  AnyDoc transcodes UTF-16 parts before parsing, so the checks must read the
  same text. Expected `incomplete_conversion`.
- utf16-hidden-style.docx: a hidden character style in a UTF-16 styles part.
  Expected complete conversion with the `hidden_content_preserved` warning.
- non-breaking-hyphen.docx: "FORM 1040" + `w:noBreakHyphen` + "SR"; the
  pinned parser drops the hyphen and joins the words. Expected a partial
  conversion with the `characters_omitted` warning.
- ruby-text.docx: ruby text, whose base text the pinned parser drops with the
  annotation. Expected `incomplete_conversion`.
- alt-chunk.docx: an imported HTML chunk (`w:altChunk`) that Word merges on
  opening and the pinned parser drops. Expected `incomplete_conversion`.
- pptx/fragment-slide-target.pptx: a slide target whose fragment hides a `..`
  from AnyDoc's resolver, so AnyDoc converts a hidden slide outside
  `ppt/slides/` while a naive resolver lands on a checked decoy. Expected
  `incomplete_conversion`.
- pptx/case-variant-presentation-rels.pptx: a decoy
  `PPT/_rels/presentation.xml.rels` beside the relationships part AnyDoc reads
  by exact name. Expected `incomplete_conversion`.
- docx/namespace-shadowed-main.docx: a root relationship whose `xmlns:Type`
  declaration shadowed its officeDocument type for a name-only reader, so a
  checked decoy stood in for the main part AnyDoc converts. Expected
  `malformed`.
- xlsx/binary-workbook.xlsx: `xl/workbook.xml` holding binary records, which
  AnyDoc reads with its XLSB reader. Expected `unsupported`.
- pptx/section-list.pptx: a clean deck with a PowerPoint section list, whose
  `p14:sldId` entries are not slides. Expected complete conversion.
- epub/linked-css-hidden.epub: a chapter whose linked stylesheet hides a
  paragraph with `visibility: hidden`, which AnyDoc converts as visible
  text. Expected `incomplete_conversion`.
- epub/encoded-chapter-href.epub: a spine href AnyDoc percent-decodes to a
  chapter with hidden text, beside a clean decoy stored under the encoded
  name. Expected `incomplete_conversion`.
- xlsx/xlsb-fallback-decoy.xlsx: a case-variant `XL/workbook.xml` decoy and
  no root relationships, so AnyDoc's exact lookup falls back to the binary
  `xl/workbook.bin`. Expected `malformed`.
- xlsx/unrendered-formula-cache.xlsx: a formula whose cached value does not
  parse as a number, which AnyDoc converts as an empty cell. Expected
  `incomplete_conversion`.
- pptx/relocated-notes.pptx: speaker notes with a hidden shape, stored
  outside `ppt/notesSlides/` and reached through the slide's notesSlide
  relationship. Expected `incomplete_conversion`.
- docx/cell-in-compatibility-block.docx: a table cell wrapped in
  `mc:AlternateContent`, which AnyDoc's row walker drops with its text.
  Expected `incomplete_conversion`.
- odt/page-anchored-frame.odt: a text box anchored to the page, which AnyDoc
  skips. Expected `incomplete_conversion`.
- ods/untyped-formula-value.ods: a formula cell whose cached value has no
  `office:value-type`, so AnyDoc renders nothing. Expected
  `incomplete_conversion`.
- odp/linked-frame.odp: a slide frame wrapped in `draw:a`, which AnyDoc's
  shape walk skips. Expected `incomplete_conversion`.
- epub/escaped-selector.epub: a linked stylesheet whose escaped class
  selector (`p.\73 ecret`) hides text with `visibility: hidden`. Expected
  `incomplete_conversion`.
- epub/web-address-in-text.epub: a chapter whose visible text mentions a web
  address. Expected complete output with the address sanitized.
- epub/display-none-omitted.epub: text a linked `display: none` rule hides,
  which AnyDoc omits as a reader does. Expected complete output without it.
- epub/list-text-outside-items.epub: text and a paragraph placed directly in
  an ordered list, which a reader shows and AnyDoc's list walker skips.
  Expected `incomplete_conversion`.
- epub/kindle-media-pair.epub: the common Kindle stylesheet pair, whose
  second rule inside `@media amzn-mobi` AnyDoc applies everywhere, dropping
  a paragraph readers show. Expected `incomplete_conversion`.
- xlsx/negative-sign-by-colour.xlsx: a negative amount whose format,
  `#,##0;[Red]#,##0`, marks it only in red; AnyDoc renders it unsigned.
  Expected `incomplete_conversion`.
- xlsx/red-parenthesized-negative.xlsx: the same amount under
  `#,##0;[Red]\\(#,##0\\)`, which keeps its parentheses. Expected
  complete conversion.
- xlsx/format-hidden-value.xlsx: an amount its format (`;;;`) hides.
  Expected `incomplete_conversion`.
- xlsx/locale-date-format.xlsx: a date under built-in format 31, which
  AnyDoc cannot resolve and renders as a serial number. Expected
  `incomplete_conversion`.
- xlsx/drawing-text-box.xlsx: a text box over the sheet, which AnyDoc never
  reads. Expected `incomplete_conversion`.
- ods/negative-sign-by-colour.ods: a negative value displayed without a
  sign, which only its colour marked. Expected `incomplete_conversion`.
- ods/format-hidden-value.ods: a value its format hides, displayed as an
  empty paragraph, which AnyDoc replaces with the typed value. Expected
  `incomplete_conversion`.
- ods/cell-anchored-text-box.ods: a text box anchored to a cell, which
  AnyDoc never reads. Expected `incomplete_conversion`.
- docx/shared-list-definition.docx: two list instances of one numbering
  definition; Word continues the count into the second, and AnyDoc restarts
  it. Expected `partial` completeness with the `list_numbering_differs`
  warning.

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


def write_pptx(name, entries, presentation_extra="", hidden_decoy=True):
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
        '<p:sldId id="256" r:id="rId2"/></p:sldIdLst>'
        f"{presentation_extra}</p:presentation>"
    )
    base = [
        ("[Content_Types].xml", types),
        ("_rels/.rels", root),
        ("ppt/presentation.xml", presentation),
        ("ppt/slides/slide1.xml", slide("DECOY-SLIDE" if hidden_decoy else "SECTION-SLIDE")),
    ]
    if hidden_decoy:
        base.append(("other/slide.xml", slide("HIDDEN-SLIDE-TEXT", hidden=True)))
    write_package(CORPUS / "pptx" / name, base + entries)


def write_epub(name, opf_manifest, spine, nav_href, parts):
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
        f"<dc:title>Hardening</dc:title><dc:language>en</dc:language></metadata><manifest>"
        f'{opf_manifest}<item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>'
        f"</manifest><spine>{spine}</spine></package>"
    )
    nav = (
        f'<?xml version="1.0"?><html {xhtml}><head><title>Contents</title></head><body>'
        '<nav xmlns:epub="http://www.idpf.org/2007/ops" epub:type="toc"><ol>'
        f'<li><a href="{nav_href}">One</a></li></ol></nav></body></html>'
    )
    write_package(
        CORPUS / "epub" / name,
        [
            ("mimetype", "application/epub+zip"),
            ("META-INF/container.xml", container),
            ("OPS/package.opf", opf),
            ("OPS/nav.xhtml", nav),
        ]
        + parts,
        stored={"mimetype"},
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


def write_docx(name, body, document_rels=None, parts=(), root_rels=DOCX_ROOT_RELS):
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
        ("_rels/.rels", root_rels),
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


def write_binary_workbook():
    source = CORPUS / "xlsx" / "public-workpaper.xlsx"
    with ZipFile(source) as archive:
        entries = [
            (info.filename, archive.read(info))
            for info in archive.infolist()
            if not info.filename.endswith("/")
        ]
    # BrtBeginBook and BrtEndBook records: a binary workbook stream.
    binary = bytes([0x83, 0x01, 0x00, 0x84, 0x01, 0x00])
    write_package(
        CORPUS / "xlsx" / "binary-workbook.xlsx",
        [
            (filename, binary if filename == "xl/workbook.xml" else data)
            for filename, data in entries
        ],
    )


def patched_workpaper(name, patch):
    """public-workpaper.xlsx with its entries rewritten by `patch`."""
    source = CORPUS / "xlsx" / "public-workpaper.xlsx"
    with ZipFile(source) as archive:
        entries = [
            (info.filename, archive.read(info))
            for info in archive.infolist()
            if not info.filename.endswith("/")
        ]
    write_package(CORPUS / "xlsx" / name, patch(entries))


def write_xlsb_fallback_decoy():
    def patch(entries):
        out = []
        for filename, data in entries:
            if filename == "_rels/.rels":
                continue
            if filename == "xl/workbook.xml":
                out.append(("XL/workbook.xml", data))
                continue
            out.append((filename, data))
        # BrtBeginBook and BrtEndBook records: a binary workbook stream.
        out.append(("xl/workbook.bin", bytes([0x83, 0x01, 0x00, 0x84, 0x01, 0x00])))
        return out

    patched_workpaper("xlsb-fallback-decoy.xlsx", patch)


def write_unrendered_formula_cache():
    def patch(entries):
        return [
            (
                filename,
                data.replace(b"<v>150000</v>", b"<v>RECEIPTS-TOTAL</v>")
                if filename == "xl/worksheets/sheet1.xml"
                else data,
            )
            for filename, data in entries
        ]

    patched_workpaper("unrendered-formula-cache.xlsx", patch)


SML_MAIN = "http://schemas.openxmlformats.org/spreadsheetml/2006/main"
OFFICE_RELS = "http://schemas.openxmlformats.org/officeDocument/2006/relationships"


def styled_workpaper(name, number_format_id, code, value, extra=()):
    """public-workpaper.xlsx with a styles part, and receipts cell B3 holding
    `value` under format `number_format_id` (defined as `code` when given)."""

    def patch(entries):
        custom = ""
        if code is not None:
            escaped = code.replace("&", "&amp;").replace('"', "&quot;")
            custom = (
                f'<numFmts count="1"><numFmt numFmtId="{number_format_id}" '
                f'formatCode="{escaped}"/></numFmts>'
            )
        styles = (
            '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
            f'<styleSheet xmlns="{SML_MAIN}">{custom}'
            f'<cellXfs count="2"><xf numFmtId="0"/><xf numFmtId="{number_format_id}" '
            'applyNumberFormat="1"/></cellXfs></styleSheet>'
        ).encode()
        out = []
        for filename, data in entries:
            if filename == "xl/worksheets/sheet1.xml":
                data = data.replace(
                    b'<c r="B3"><v>25000</v></c>', f'<c r="B3" s="1"><v>{value}</v></c>'.encode()
                ).replace(b"<v>150000</v>", f"<v>{125000 + value}</v>".encode())
            elif filename == "xl/_rels/workbook.xml.rels":
                data = data.replace(
                    b"</Relationships>",
                    f'<Relationship Id="rId3" Type="{OFFICE_RELS}/styles" '
                    'Target="styles.xml"/></Relationships>'.encode(),
                )
            elif filename == "[Content_Types].xml":
                data = data.replace(
                    b"</Types>",
                    b'<Override PartName="/xl/styles.xml" ContentType="application/'
                    b'vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/></Types>',
                )
            out.append((filename, data))
        out.append(("xl/styles.xml", styles))
        out.extend(extra)
        return out

    patched_workpaper(name, patch)


def write_drawing_text_box():
    drawing = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing" '
        'xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">'
        '<xdr:twoCellAnchor><xdr:from><xdr:col>3</xdr:col><xdr:colOff>0</xdr:colOff>'
        "<xdr:row>1</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>"
        "<xdr:to><xdr:col>6</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>4</xdr:row>"
        "<xdr:rowOff>0</xdr:rowOff></xdr:to>"
        '<xdr:sp macro="" textlink=""><xdr:nvSpPr><xdr:cNvPr id="2" name="TextBox 1"/>'
        '<xdr:cNvSpPr txBox="1"/></xdr:nvSpPr><xdr:spPr/><xdr:txBody><a:bodyPr/>'
        "<a:p><a:r><a:t>DRAWING-NOTE: receipts restated; see note 4</a:t></a:r></a:p>"
        "</xdr:txBody></xdr:sp><xdr:clientData/></xdr:twoCellAnchor></xdr:wsDr>"
    ).encode()
    sheet_rels = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
        f'<Relationship Id="rId1" Type="{OFFICE_RELS}/drawing" '
        'Target="../drawings/drawing1.xml"/></Relationships>'
    ).encode()

    def patch(entries):
        out = []
        for filename, data in entries:
            if filename == "xl/worksheets/sheet1.xml":
                data = data.replace(
                    b"</sheetData>",
                    f'</sheetData><drawing xmlns:r="{OFFICE_RELS}" r:id="rId1"/>'.encode(),
                )
            elif filename == "[Content_Types].xml":
                data = data.replace(
                    b"</Types>",
                    b'<Override PartName="/xl/drawings/drawing1.xml" ContentType="application/'
                    b'vnd.openxmlformats-officedocument.drawing+xml"/></Types>',
                )
            out.append((filename, data))
        out.append(("xl/worksheets/_rels/sheet1.xml.rels", sheet_rels))
        out.append(("xl/drawings/drawing1.xml", drawing))
        return out

    patched_workpaper("drawing-text-box.xlsx", patch)


def ods_sheet(cells):
    """An ODS body with one row of `cells`."""
    return (
        '<office:spreadsheet><table:table table:name="Sheet1"><table:table-row>'
        '<table:table-cell office:value-type="string"><text:p>Receipts</text:p></table:table-cell>'
        f"{cells}</table:table-row></table:table></office:spreadsheet>"
    )


ODF_NS = (
    'xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" '
    'xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" '
    'xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" '
    'xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" '
    'xmlns:presentation="urn:oasis:names:tc:opendocument:xmlns:presentation:1.0" '
    'xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0" '
    'xmlns:xlink="http://www.w3.org/1999/xlink"'
)


def write_odf(folder, name, mimetype, body):
    content = (
        '<?xml version="1.0" encoding="UTF-8"?>'
        f'<office:document-content {ODF_NS} office:version="1.3">'
        f"<office:body>{body}</office:body></office:document-content>"
    )
    manifest = (
        '<?xml version="1.0" encoding="UTF-8"?>'
        '<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0" '
        'manifest:version="1.3">'
        f'<manifest:file-entry manifest:full-path="/" manifest:media-type="{mimetype}"/>'
        '<manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/>'
        "</manifest:manifest>"
    )
    write_package(
        CORPUS / folder / name,
        [("mimetype", mimetype), ("META-INF/manifest.xml", manifest), ("content.xml", content)],
        stored={"mimetype"},
    )


def epub_chapter(head, body):
    xhtml = 'xmlns="http://www.w3.org/1999/xhtml"'
    return (
        f'<?xml version="1.0"?><html {xhtml}><head><title>Chapter</title>{head}</head>'
        f"<body>{body}</body></html>"
    )


def write_styled_epub(name, css, body):
    link = '<link rel="stylesheet" type="text/css" href="../Styles/main.css"/>'
    write_epub(
        name,
        '<item id="ch1" href="Text/ch1.xhtml" media-type="application/xhtml+xml"/>'
        '<item id="css" href="Styles/main.css" media-type="text/css"/>',
        '<itemref idref="ch1"/>',
        "Text/ch1.xhtml",
        [
            ("OPS/Text/ch1.xhtml", epub_chapter(link, body)),
            ("OPS/Styles/main.css", css),
        ],
    )


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
    xhtml = 'xmlns="http://www.w3.org/1999/xhtml"'
    write_epub(
        "linked-css-hidden.epub",
        '<item id="ch1" href="Text/ch1.xhtml" media-type="application/xhtml+xml"/>'
        '<item id="css" href="Styles/main.css" media-type="text/css"/>',
        '<itemref idref="ch1"/>',
        "Text/ch1.xhtml",
        [
            (
                "OPS/Text/ch1.xhtml",
                f'<?xml version="1.0"?><html {xhtml}><head><title>Chapter</title>'
                '<link rel="stylesheet" type="text/css" href="../Styles/main.css"/></head>'
                '<body><p>VISIBLE-CHAPTER</p><p class="note">HIDDEN-CSS-TEXT</p></body></html>',
            ),
            ("OPS/Styles/main.css", "p { margin: 0 }\n.note { visibility: hidden; }\n"),
        ],
    )
    write_pptx(
        "section-list.pptx",
        [("ppt/_rels/presentation.xml.rels", presentation_rels("slides/slide1.xml"))],
        presentation_extra=(
            '<p:extLst><p:ext uri="{521415D9-36F7-43E2-AB2F-B90AF26B5E84}">'
            '<p14:sectionLst xmlns:p14="http://schemas.microsoft.com/office/powerpoint/2010/main">'
            '<p14:section name="Opening" id="{00000000-0000-4000-8000-000000000001}">'
            '<p14:sldIdLst><p14:sldId id="256"/></p14:sldIdLst></p14:section>'
            "</p14:sectionLst></p:ext></p:extLst>"
        ),
        hidden_decoy=False,
    )
    write_docx(
        "namespace-shadowed-main.docx",
        paragraph("DECOY-MAIN"),
        parts=[
            (
                "word/other.xml",
                '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
                f"<w:document {WORD_NS}><w:body>"
                '<w:p><w:r><w:sym w:font="Wingdings" w:char="F0FE"/></w:r>'
                "<w:r><w:t xml:space=\"preserve\"> OTHER-MAIN-ANSWER</w:t></w:r></w:p>"
                "<w:sectPr/></w:body></w:document>",
            )
        ],
        root_rels=(
            '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
            f'<Relationships xmlns="{RELS_NS}">'
            '<Relationship xmlns:Type="urn:example:decoy" Id="rId1" '
            f'Type="{REL_TYPE}/officeDocument" Target="word/other.xml"/>'
            "</Relationships>"
        ),
    )
    write_binary_workbook()
    write_docx(
        "non-breaking-hyphen.docx",
        '<w:p><w:r><w:t xml:space="preserve">FORM 1040</w:t><w:noBreakHyphen/>'
        '<w:t xml:space="preserve">SR HYPHEN-MARKER</w:t></w:r></w:p>',
    )
    write_docx(
        "ruby-text.docx",
        "<w:p><w:r><w:ruby><w:rubyPr/><w:rt><w:r><w:t>RUBY-ANNOTATION</w:t></w:r></w:rt>"
        "<w:rubyBase><w:r><w:t>RUBY-BASE</w:t></w:r></w:rubyBase></w:ruby></w:r></w:p>",
    )
    write_docx(
        "alt-chunk.docx",
        '<w:altChunk r:id="rIdChunk"/>',
        (
            '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
            f'<Relationships xmlns="{RELS_NS}">'
            f'<Relationship Id="rIdChunk" Type="{REL_TYPE}/aFChunk" Target="chunk.html"/>'
            "</Relationships>"
        ),
        [("word/chunk.html", "<html><body><p>IMPORTED-CHUNK-TEXT</p></body></html>")],
    )
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
    write_xlsb_fallback_decoy()
    write_unrendered_formula_cache()
    write_pptx(
        "relocated-notes.pptx",
        [
            ("ppt/_rels/presentation.xml.rels", presentation_rels("slides/slide1.xml")),
            (
                "ppt/slides/_rels/slide1.xml.rels",
                f'<Relationships xmlns="{RELS_NS}">'
                f'<Relationship Id="rId3" Type="{REL_TYPE}/notesSlide" '
                'Target="../notes/notes1.xml"/></Relationships>',
            ),
            (
                "ppt/notes/notes1.xml",
                f"<p:notes {PRESENTATION_NS}><p:cSld><p:spTree>"
                '<p:sp><p:nvSpPr><p:cNvPr id="2" name="Hidden" hidden="1"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>'
                "<p:spPr/><p:txBody><a:bodyPr/><a:p><a:r><a:t>HIDDEN-NOTE-TEXT</a:t></a:r></a:p>"
                "</p:txBody></p:sp></p:spTree></p:cSld></p:notes>",
            ),
        ],
        hidden_decoy=False,
    )
    write_docx(
        "cell-in-compatibility-block.docx",
        '<w:tbl><w:tr><w:tc><w:p><w:r><w:t>VISIBLE-CELL</w:t></w:r></w:p></w:tc>'
        '<mc:AlternateContent xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006">'
        '<mc:Choice Requires="w"><w:tc><w:p><w:r><w:t>WRAPPED-CELL</w:t></w:r></w:p></w:tc>'
        "</mc:Choice></mc:AlternateContent></w:tr></w:tbl>",
    )
    write_odf(
        "odt",
        "page-anchored-frame.odt",
        "application/vnd.oasis.opendocument.text",
        "<office:text><text:p>VISIBLE-TEXT</text:p>"
        '<draw:frame text:anchor-type="page" svg:width="5cm" svg:height="2cm">'
        "<draw:text-box><text:p>PAGE-FRAME-TEXT</text:p></draw:text-box></draw:frame>"
        "</office:text>",
    )
    write_odf(
        "ods",
        "untyped-formula-value.ods",
        "application/vnd.oasis.opendocument.spreadsheet",
        '<office:spreadsheet><table:table table:name="Sheet1"><table:table-row>'
        '<table:table-cell office:value-type="string"><text:p>Total</text:p></table:table-cell>'
        '<table:table-cell table:formula="of:=40+2" office:value="42"/>'
        "</table:table-row></table:table></office:spreadsheet>",
    )
    write_odf(
        "odp",
        "linked-frame.odp",
        "application/vnd.oasis.opendocument.presentation",
        '<office:presentation><draw:page draw:name="Slide1">'
        '<draw:frame presentation:class="title"><draw:text-box><text:p>VISIBLE-TITLE</text:p>'
        "</draw:text-box></draw:frame><draw:a><draw:frame><draw:text-box>"
        "<text:p>LINKED-FRAME-TEXT</text:p></draw:text-box></draw:frame></draw:a>"
        "</draw:page></office:presentation>",
    )
    write_styled_epub(
        "escaped-selector.epub",
        "p.\\73 ecret { visibility: hidden }\n",
        '<p>VISIBLE-CHAPTER</p><p class="secret">HIDDEN-ESCAPED-TEXT</p>',
    )
    write_styled_epub(
        "web-address-in-text.epub",
        "p { margin: 0 }\n",
        "<p>VISIBLE-CHAPTER</p><p>Forms are at https://www.example.com/forms today.</p>",
    )
    write_styled_epub(
        "display-none-omitted.epub",
        ".gone { display: none }\n",
        '<p>VISIBLE-CHAPTER</p><p class="gone">OMITTED-LIKE-A-READER</p>',
    )
    write_styled_epub(
        "list-text-outside-items.epub",
        "p { margin: 0 }\n",
        "<p>VISIBLE-CHAPTER</p><ol><li>Line one</li>LOOSE-LIST-TEXT"
        "<p>PARAGRAPH-IN-LIST</p><li>Line two</li></ol>",
    )
    write_styled_epub(
        "kindle-media-pair.epub",
        ".mobi-only { display: none; }\n"
        "@media amzn-mobi {\n  .mobi-only { display: block; }\n"
        "  .kf8-only { display: none; }\n}\n",
        '<p>VISIBLE-CHAPTER</p><p class="kf8-only">SHOWN-IN-EPUB-READERS</p>'
        '<p class="mobi-only">Old Kindle fallback.</p>',
    )
    styled_workpaper("negative-sign-by-colour.xlsx", 164, "#,##0;[Red]#,##0", -25000)
    styled_workpaper(
        "red-parenthesized-negative.xlsx", 164, "#,##0;[Red]\\(#,##0\\)", -25000
    )
    styled_workpaper("format-hidden-value.xlsx", 164, ";;;", 25000)
    styled_workpaper("locale-date-format.xlsx", 31, None, 45762)
    write_drawing_text_box()
    numbered = lambda list_id, text: (
        f'<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="{list_id}"/></w:numPr>'
        f'</w:pPr><w:r><w:t xml:space="preserve">{text}</w:t></w:r></w:p>'
    )
    write_docx(
        "shared-list-definition.docx",
        numbered(1, "Enter wages.")
        + numbered(1, "Enter interest.")
        + paragraph("Attach the statement.")
        + numbered(2, "LIST-CONTINUES: enter dividends.")
        + numbered(2, "Add lines 1 through 3."),
        typed_rels("numbering", "numbering.xml"),
        [
            (
                "word/numbering.xml",
                '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
                f"<w:numbering {WORD_NS}>"
                '<w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:start w:val="1"/>'
                '<w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/></w:lvl></w:abstractNum>'
                '<w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>'
                '<w:num w:numId="2"><w:abstractNumId w:val="0"/></w:num>'
                "</w:numbering>",
            )
        ],
    )
    ods = "application/vnd.oasis.opendocument.spreadsheet"
    write_odf(
        "ods",
        "negative-sign-by-colour.ods",
        ods,
        ods_sheet(
            '<table:table-cell office:value-type="float" office:value="-1234.1">'
            "<text:p>1234.10</text:p></table:table-cell>"
        ),
    )
    write_odf(
        "ods",
        "format-hidden-value.ods",
        ods,
        ods_sheet(
            '<table:table-cell office:value-type="float" office:value="98765">'
            "<text:p/></table:table-cell>"
        ),
    )
    write_odf(
        "ods",
        "cell-anchored-text-box.ods",
        ods,
        ods_sheet(
            '<table:table-cell office:value-type="float" office:value="12">'
            '<draw:frame svg:width="4cm" svg:height="1cm"><draw:text-box>'
            "<text:p>CELL-NOTE: see workpaper 7</text:p></draw:text-box></draw:frame>"
            "<text:p>12</text:p></table:table-cell>"
        ),
    )


if __name__ == "__main__":
    main()
