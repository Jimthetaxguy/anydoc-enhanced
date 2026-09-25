//! A model of AnyDoc 0.2.4's OpenDocument walkers (`formats::odf`), run over
//! a content part before conversion.
//!
//! AnyDoc converts an ODF body by walking known elements in known positions
//! and silently skips everything else: a frame anchored to the page in a
//! text document, a `text:numbered-paragraph`, a slide shape wrapped in a
//! hyperlink. This module follows the same walk without building a tree and
//! reports paragraph text in a position the walk skips, which a reader
//! shows and the conversion would lose. Elements that hold no displayed
//! text (declarations, tracked deletions, index templates, comments,
//! slide headers and footers, drawings over a spreadsheet's grid) are
//! ignored. The walk also mirrors how a spreadsheet cell renders
//! (`table::cell_blocks` and `value_text`), so a formula whose cached value
//! AnyDoc cannot render is reported.

use std::io::Cursor;

use quick_xml::name::ResolveResult;

use super::DocumentError;

const OFFICE: &[u8] = b"urn:oasis:names:tc:opendocument:xmlns:office:1.0";
const TEXT: &[u8] = b"urn:oasis:names:tc:opendocument:xmlns:text:1.0";
const TABLE: &[u8] = b"urn:oasis:names:tc:opendocument:xmlns:table:1.0";
const DRAW: &[u8] = b"urn:oasis:names:tc:opendocument:xmlns:drawing:1.0";
const PRESENTATION: &[u8] = b"urn:oasis:names:tc:opendocument:xmlns:presentation:1.0";
const SVG_COMPATIBLE: &[u8] = b"urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0";
const ANIMATION: &[u8] = b"urn:oasis:names:tc:opendocument:xmlns:animation:1.0";
const FORM: &[u8] = b"urn:oasis:names:tc:opendocument:xmlns:form:1.0";

/// Nesting depth of a content part, as AnyDoc's parser allows.
const MAX_DEPTH: usize = 256;

/// The body a package's lane converts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OdfBody {
    Text,
    Spreadsheet,
    Presentation,
}

impl OdfBody {
    fn element(self) -> &'static [u8] {
        match self {
            OdfBody::Text => b"text",
            OdfBody::Spreadsheet => b"spreadsheet",
            OdfBody::Presentation => b"presentation",
        }
    }
}

/// What the walk found.
#[derive(Debug, Default)]
pub(super) struct OdfWalk {
    /// Paragraph text in a position AnyDoc skips.
    pub(super) dropped_text: bool,
    /// A formula cell AnyDoc converts without a value.
    pub(super) uncached_formula: bool,
}

/// How AnyDoc treats an element's children.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// `office:document-content`: its first `office:body` is converted.
    Content,
    /// The chosen `office:body`: its first child of the lane's kind.
    Body,
    /// `parse_container`: each child through `parse_block_elem`.
    Container,
    /// A cell's `parse_container`, where drawings over the grid are
    /// ignored in a spreadsheet.
    Cell,
    /// `walk_inlines`: text converts, and so does any unknown child.
    Inline,
    /// `walk_frame` in running text: the first `draw:text-box` only.
    Frame,
    /// `text:note`: its first `text:note-body`.
    Note,
    /// `parse_list`: list items and headers.
    List,
    /// A list item: nested lists, else `parse_block_elem`.
    ListItem,
    /// `walk_rows`: row groups and rows.
    Table,
    /// `parse_row_cells`: cells.
    Row,
    /// `parse_spreadsheet`: tables.
    Spreadsheet,
    /// `parse_presentation`: pages.
    Presentation,
    /// `walk_shapes`: a page's or group's shapes.
    Page,
    /// A frame on a page (`walk_shapes`).
    PageFrame,
    /// A drawing shape, walked as a container when it holds a paragraph or
    /// list directly.
    Shape,
    /// `presentation:notes`: frames anywhere below, searched for their
    /// first text box.
    Notes,
    /// A frame found by that search.
    NotesFrame,
    /// Nothing below converts.
    Skipped,
    /// Nothing below converts, and nothing below is displayed body text.
    Ignored,
}

struct Open {
    mode: Mode,
    /// Text below is paragraph text AnyDoc skips.
    dropped_paragraph: bool,
    /// Element children seen (for a frame: text boxes seen; for a page
    /// frame: whether an image or object ended the walk).
    text_boxes: usize,
    broken: bool,
    /// For a shape: whether AnyDoc walks it, and whether text it would
    /// convert was seen before that was known.
    walked: bool,
    pending: bool,
    /// For a cell: the formula state.
    cell: Option<CellState>,
    first_child_taken: bool,
}

#[derive(Default)]
struct CellState {
    formula: bool,
    /// The typed value AnyDoc renders when the cell has no display text.
    value_renders: bool,
    /// Display text converted from the cell's paragraphs.
    rendered: bool,
    /// A list or nested table, which stops AnyDoc from falling back to the
    /// typed value.
    structured: bool,
}

impl Open {
    fn new(mode: Mode) -> Self {
        Open {
            mode,
            dropped_paragraph: false,
            text_boxes: 0,
            broken: false,
            walked: false,
            pending: false,
            cell: None,
            first_child_taken: false,
        }
    }
}

/// An element's attributes as (namespace, local name, value).
type Attributes = Vec<(Option<Vec<u8>>, Vec<u8>, String)>;

/// AnyDoc's `Element::attr`: the attribute in the element's vocabulary, else
/// an unqualified one with the same local name.
fn attribute<'a>(attributes: &'a Attributes, namespace: &[u8], local: &[u8]) -> Option<&'a str> {
    attributes
        .iter()
        .find(|(bound, name, _)| name == local && bound.as_deref() == Some(namespace))
        .or_else(|| {
            attributes
                .iter()
                .find(|(bound, name, _)| name == local && bound.is_none())
        })
        .map(|(_, _, value)| value.as_str())
}

/// Whether AnyDoc's `value_text` renders something for a cell.
fn value_renders(attributes: &Attributes) -> bool {
    let value = |name: &[u8]| attribute(attributes, OFFICE, name);
    let number = |name: &[u8]| value(name).is_some_and(|text| text.parse::<f64>().is_ok());
    match value(b"value-type") {
        Some("percentage" | "currency" | "float") => number(b"value"),
        Some("date") => value(b"date-value").is_some_and(|text| !text.is_empty()),
        Some("time") => value(b"time-value").is_some(),
        Some("boolean") => value(b"boolean-value").is_some(),
        Some("string") => value(b"string-value").is_some_and(|text| !text.is_empty()),
        _ => false,
    }
}

/// Elements that hold no displayed body text, wherever they appear.
fn ignored(namespace: &[u8], local: &[u8]) -> bool {
    match namespace {
        TEXT => {
            matches!(
                local,
                b"tracked-changes"
                    | b"variable-decls"
                    | b"sequence-decls"
                    | b"user-field-decls"
                    | b"dde-connection-decls"
                    | b"alphabetical-index-auto-mark-file"
                    | b"soft-page-break"
            ) || local.ends_with(b"-source")
        }
        TABLE => matches!(
            local,
            b"tracked-changes"
                | b"content-validations"
                | b"named-expressions"
                | b"database-ranges"
                | b"data-pilot-tables"
                | b"calculation-settings"
                | b"label-ranges"
                | b"dde-links"
                | b"consolidation"
                | b"table-column"
                | b"table-columns"
                | b"table-header-columns"
                | b"table-column-group"
                | b"table-source"
                | b"scenario"
                | b"title"
                | b"desc"
        ),
        OFFICE => matches!(
            local,
            b"annotation" | b"annotation-end" | b"forms" | b"scripts"
        ),
        SVG_COMPATIBLE | ANIMATION | FORM => true,
        _ => false,
    }
}

/// `parse_block_elem`: how a container treats a child. Returns the child's
/// mode, `Skipped` for one AnyDoc drops.
fn block_child(namespace: &[u8], local: &[u8]) -> Mode {
    if ignored(namespace, local) {
        return Mode::Ignored;
    }
    match (namespace, local) {
        (TEXT, b"h" | b"p") => Mode::Inline,
        (TEXT, b"list") => Mode::List,
        (
            TEXT,
            b"section"
            | b"index-body"
            | b"index-title"
            | b"table-of-content"
            | b"alphabetical-index"
            | b"bibliography"
            | b"illustration-index",
        ) => Mode::Container,
        (TABLE, b"table") => Mode::Table,
        _ => Mode::Skipped,
    }
}

/// Walk a content part's body as AnyDoc converts it for `body`.
pub(super) fn walk_content(
    bytes: &[u8],
    body: OdfBody,
    spreadsheet: bool,
) -> Result<OdfWalk, DocumentError> {
    let mut reader = quick_xml::NsReader::from_reader(Cursor::new(bytes));
    reader.config_mut().trim_text(false);
    reader.config_mut().check_end_names = false;
    let mut buffer = Vec::new();
    let mut walk = OdfWalk::default();
    let mut stack: Vec<Open> = Vec::new();
    let mut content_taken = false;
    loop {
        let (namespace, event) = reader
            .read_resolved_event_into(&mut buffer)
            .map_err(|_| DocumentError::Malformed)?;
        let namespace = match namespace {
            ResolveResult::Bound(namespace) => namespace.as_ref().to_vec(),
            _ => Vec::new(),
        };
        let (element, start) = match event {
            quick_xml::events::Event::Start(element) => (element, true),
            quick_xml::events::Event::Empty(element) => (element, false),
            quick_xml::events::Event::End(_) => {
                if let Some(closed) = stack.pop() {
                    finish(closed, &mut walk);
                }
                buffer.clear();
                continue;
            }
            quick_xml::events::Event::Text(text) => {
                on_text(&mut stack, text.as_ref(), &mut walk);
                buffer.clear();
                continue;
            }
            quick_xml::events::Event::CData(text) => {
                on_text(&mut stack, text.as_ref(), &mut walk);
                buffer.clear();
                continue;
            }
            quick_xml::events::Event::GeneralRef(reference) => {
                let text = super::anydoc_entity_text(&String::from_utf8_lossy(reference.as_ref()));
                on_text(&mut stack, text.as_bytes(), &mut walk);
                buffer.clear();
                continue;
            }
            quick_xml::events::Event::Eof => break,
            _ => {
                buffer.clear();
                continue;
            }
        };
        if stack.len() >= MAX_DEPTH {
            return Err(DocumentError::ResourceLimit);
        }
        let local = element.local_name().as_ref().to_vec();
        let attributes: Attributes = element
            .attributes()
            .flatten()
            .filter(|attribute| {
                let key = attribute.key.as_ref();
                key != b"xmlns" && !key.starts_with(b"xmlns:")
            })
            .map(|attribute| {
                let (bound, name) = reader.resolver().resolve_attribute(attribute.key);
                let bound = match bound {
                    ResolveResult::Bound(namespace) => Some(namespace.as_ref().to_vec()),
                    _ => None,
                };
                let value = attribute
                    .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                    .map(|value| value.into_owned())
                    .unwrap_or_else(|_| {
                        String::from_utf8_lossy(attribute.value.as_ref()).into_owned()
                    });
                (bound, name.as_ref().to_vec(), value)
            })
            .collect();
        let mut open = child(
            &mut stack,
            &namespace,
            &local,
            &attributes,
            body,
            spreadsheet,
            &mut content_taken,
        );
        if matches!(open.mode, Mode::Cell) && open.cell.is_none() {
            open.cell = Some(CellState {
                formula: attributes.iter().any(|(_, name, _)| name == b"formula"),
                value_renders: value_renders(&attributes),
                ..CellState::default()
            });
        }
        if start {
            stack.push(open);
        } else {
            finish(open, &mut walk);
        }
        buffer.clear();
    }
    while let Some(closed) = stack.pop() {
        finish(closed, &mut walk);
    }
    Ok(walk)
}

/// The state for a child element opening under the top of `stack`.
fn child(
    stack: &mut [Open],
    namespace: &[u8],
    local: &[u8],
    attributes: &Attributes,
    body: OdfBody,
    spreadsheet: bool,
    content_taken: &mut bool,
) -> Open {
    let Some(parent) = stack.last_mut() else {
        // Top level: the first `office:document-content` is converted.
        return if namespace == OFFICE && local == b"document-content" && !*content_taken {
            *content_taken = true;
            Open::new(Mode::Content)
        } else {
            Open::new(Mode::Ignored)
        };
    };
    let dropped_paragraph = parent.dropped_paragraph;
    let mode = match parent.mode {
        Mode::Ignored => Mode::Ignored,
        Mode::Skipped => {
            if ignored(namespace, local) {
                Mode::Ignored
            } else {
                Mode::Skipped
            }
        }
        Mode::Content => {
            if namespace == OFFICE && local == b"body" && !parent.first_child_taken {
                parent.first_child_taken = true;
                Mode::Body
            } else {
                Mode::Ignored
            }
        }
        Mode::Body => {
            if namespace == OFFICE && local == body.element() && !parent.first_child_taken {
                parent.first_child_taken = true;
                match body {
                    OdfBody::Text => Mode::Container,
                    OdfBody::Spreadsheet => Mode::Spreadsheet,
                    OdfBody::Presentation => Mode::Presentation,
                }
            } else {
                Mode::Skipped
            }
        }
        Mode::Container => block_child(namespace, local),
        Mode::Cell => {
            if spreadsheet && (namespace == DRAW || (namespace == OFFICE && local == b"annotation"))
            {
                // Drawings over the grid and cell comments.
                Mode::Ignored
            } else {
                block_child(namespace, local)
            }
        }
        Mode::ListItem => {
            if namespace == TEXT && local == b"list" {
                Mode::List
            } else {
                block_child(namespace, local)
            }
        }
        Mode::Inline => match (namespace, local) {
            (TEXT, b"note") => Mode::Note,
            (TEXT, b"annotation" | b"tracked-changes" | b"soft-page-break") => Mode::Ignored,
            (DRAW, b"frame") => Mode::Frame,
            _ => Mode::Inline,
        },
        Mode::Frame => {
            if namespace == DRAW && local == b"text-box" {
                parent.text_boxes += 1;
                if parent.text_boxes == 1 {
                    Mode::Container
                } else {
                    Mode::Skipped
                }
            } else if (namespace == DRAW && matches!(local, b"image" | b"object" | b"object-ole"))
                || ignored(namespace, local)
            {
                Mode::Ignored
            } else {
                Mode::Skipped
            }
        }
        Mode::Note => {
            if namespace == TEXT && local == b"note-body" && !parent.first_child_taken {
                parent.first_child_taken = true;
                Mode::Container
            } else {
                Mode::Ignored
            }
        }
        Mode::List => {
            if namespace == TEXT && matches!(local, b"list-item" | b"list-header") {
                Mode::ListItem
            } else if ignored(namespace, local) {
                Mode::Ignored
            } else {
                Mode::Skipped
            }
        }
        Mode::Table => match (namespace, local) {
            (TABLE, b"table-header-rows" | b"table-rows" | b"table-row-group") => Mode::Table,
            (TABLE, b"table-row") => Mode::Row,
            // Drawings over the grid are not converted; like a workbook's
            // drawings, they are outside what a spreadsheet conversion
            // covers.
            (TABLE, b"shapes") => Mode::Ignored,
            _ if ignored(namespace, local) => Mode::Ignored,
            _ => Mode::Skipped,
        },
        Mode::Row => match (namespace, local) {
            (TABLE, b"table-cell") => Mode::Cell,
            // Content a merge covers is hidden in LibreOffice too.
            (TABLE, b"covered-table-cell") => Mode::Ignored,
            _ if ignored(namespace, local) => Mode::Ignored,
            _ => Mode::Skipped,
        },
        Mode::Spreadsheet => match (namespace, local) {
            (TABLE, b"table") => Mode::Table,
            _ if ignored(namespace, local) => Mode::Ignored,
            _ => Mode::Skipped,
        },
        Mode::Presentation => match (namespace, local) {
            (DRAW, b"page") => Mode::Page,
            // Header, footer, and date declarations: slide chrome, which
            // AnyDoc omits on purpose.
            (PRESENTATION, _) => Mode::Ignored,
            _ if ignored(namespace, local) => Mode::Ignored,
            _ => Mode::Skipped,
        },
        Mode::Page => match (namespace, local) {
            (PRESENTATION, b"notes") => Mode::Notes,
            (DRAW, b"frame") => {
                let class = attribute(attributes, PRESENTATION, b"class").unwrap_or("");
                if matches!(class, "page-number" | "date-time" | "footer" | "header") {
                    Mode::Ignored
                } else {
                    Mode::PageFrame
                }
            }
            (DRAW, b"g") => Mode::Page,
            (
                DRAW,
                b"custom-shape" | b"rect" | b"ellipse" | b"polygon" | b"path" | b"line"
                | b"connector" | b"caption",
            ) => Mode::Shape,
            (DRAW, b"page-thumbnail") | (PRESENTATION, _) => Mode::Ignored,
            _ if namespace != DRAW => {
                if ignored(namespace, local) {
                    Mode::Ignored
                } else {
                    Mode::Skipped
                }
            }
            _ => Mode::Skipped,
        },
        Mode::PageFrame => {
            if namespace == DRAW && local == b"text-box" {
                parent.text_boxes += 1;
                // After an image, `walk_frame` still walks the frame's first
                // text box.
                if !parent.broken || parent.text_boxes == 1 {
                    Mode::Container
                } else {
                    Mode::Skipped
                }
            } else if namespace == TABLE && local == b"table" {
                if parent.broken {
                    Mode::Skipped
                } else {
                    Mode::Table
                }
            } else if namespace == DRAW && matches!(local, b"image" | b"object" | b"object-ole") {
                parent.broken = true;
                Mode::Ignored
            } else if ignored(namespace, local) {
                Mode::Ignored
            } else {
                Mode::Skipped
            }
        }
        Mode::Shape => {
            if namespace == TEXT && matches!(local, b"p" | b"list") {
                parent.walked = true;
            }
            block_child(namespace, local)
        }
        Mode::Notes | Mode::NotesFrame => {
            if namespace == DRAW && local == b"frame" {
                Mode::NotesFrame
            } else if parent.mode == Mode::NotesFrame
                && namespace == DRAW
                && local == b"text-box"
                && !parent.first_child_taken
            {
                parent.first_child_taken = true;
                Mode::Container
            } else if (namespace == DRAW && local == b"page-thumbnail") || ignored(namespace, local)
            {
                Mode::Ignored
            } else {
                Mode::Notes
            }
        }
    };
    // A list or table in a cell keeps AnyDoc from rendering the typed value.
    if parent.mode == Mode::Cell && matches!(mode, Mode::List | Mode::Table) {
        if let Some(cell) = parent.cell.as_mut() {
            cell.structured = true;
        }
    }
    let skipped_here = matches!(mode, Mode::Skipped | Mode::Notes);
    let paragraph = namespace == TEXT && matches!(local, b"p" | b"h");
    let mut open = Open::new(mode);
    open.dropped_paragraph =
        mode != Mode::Ignored && (dropped_paragraph || (skipped_here && paragraph));
    open
}

fn on_text(stack: &mut [Open], text: &[u8], walk: &mut OdfWalk) {
    if text.iter().all(u8::is_ascii_whitespace)
        || String::from_utf8_lossy(text)
            .chars()
            .all(char::is_whitespace)
    {
        return;
    }
    let Some(current) = stack.last() else {
        return;
    };
    if current.dropped_paragraph {
        walk.dropped_text = true;
        return;
    }
    if current.mode != Mode::Inline {
        return;
    }
    // Converted text: it renders its cell, and it waits on the nearest
    // shape AnyDoc has not yet chosen to walk.
    if let Some(cell) = stack.iter_mut().rev().find_map(|open| open.cell.as_mut()) {
        cell.rendered = true;
    }
    if let Some(shape) = stack.iter_mut().rev().find(|open| open.mode == Mode::Shape) {
        if !shape.walked {
            shape.pending = true;
        }
    }
}

fn finish(closed: Open, walk: &mut OdfWalk) {
    if closed.mode == Mode::Shape && !closed.walked && closed.pending {
        walk.dropped_text = true;
    }
    if let Some(cell) = closed.cell {
        let renders = cell.rendered || (!cell.structured && cell.value_renders);
        if cell.formula && !renders {
            walk.uncached_formula = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAMESPACES: &str = concat!(
        r#"xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" "#,
        r#"xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" "#,
        r#"xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" "#,
        r#"xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" "#,
        r#"xmlns:presentation="urn:oasis:names:tc:opendocument:xmlns:presentation:1.0" "#,
        r#"xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0" "#,
        r#"xmlns:anim="urn:oasis:names:tc:opendocument:xmlns:animation:1.0" "#,
        r#"xmlns:xlink="http://www.w3.org/1999/xlink" xmlns:x="urn:x""#
    );

    fn walk(body: OdfBody, inner: &str) -> OdfWalk {
        let element = std::str::from_utf8(body.element()).unwrap();
        let content = format!(
            r#"<office:document-content {NAMESPACES}><office:automatic-styles/><office:body><office:{element}>{inner}</office:{element}></office:body></office:document-content>"#
        );
        walk_content(content.as_bytes(), body, body == OdfBody::Spreadsheet).expect("walk")
    }

    fn text_dropped(inner: &str) -> bool {
        walk(OdfBody::Text, inner).dropped_text
    }

    fn slide_dropped(inner: &str) -> bool {
        walk(
            OdfBody::Presentation,
            &format!(r#"<draw:page draw:name="One">{inner}</draw:page>"#),
        )
        .dropped_text
    }

    #[test]
    fn text_documents_lose_only_what_the_walker_skips() {
        for converted in [
            "<text:h>Heading</text:h><text:p>Body</text:p>",
            "<text:list><text:list-item><text:p>Item</text:p><text:list><text:list-item><text:p>Nested</text:p></text:list-item></text:list></text:list-item></text:list>",
            "<text:section><text:p>Section</text:p></text:section>",
            "<table:table><table:table-column/><table:table-row><table:table-cell><text:p>Cell</text:p></table:table-cell></table:table-row></table:table>",
            "<text:p>Before<draw:frame><draw:text-box><text:p>Box</text:p></draw:text-box></draw:frame></text:p>",
            r#"<text:p><draw:frame><draw:image xlink:href="Pictures/a.png"><text:p/></draw:image><svg:title>Chart</svg:title><svg:desc>A chart</svg:desc></draw:frame></text:p>"#,
            "<text:table-of-content><text:table-of-content-source><text:index-title-template>Contents</text:index-title-template></text:table-of-content-source><text:index-body><text:index-title><text:p>Contents</text:p></text:index-title><text:p>Entry</text:p></text:index-body></text:table-of-content>",
            r#"<text:sequence-decls><text:sequence-decl text:name="Figure"/></text:sequence-decls><text:p>Body</text:p>"#,
            "<text:tracked-changes><text:changed-region><text:deletion><text:p>Deleted</text:p></text:deletion></text:changed-region></text:tracked-changes>",
            r#"<draw:frame text:anchor-type="page"><draw:image xlink:href="Pictures/a.png"/></draw:frame>"#,
            "<text:p>A<text:note><text:note-citation>1</text:note-citation><text:note-body><text:p>Note</text:p></text:note-body></text:note></text:p>",
        ] {
            assert!(!text_dropped(converted), "{converted}");
        }
        for dropped in [
            r#"<draw:frame text:anchor-type="page"><draw:text-box><text:p>Page box</text:p></draw:text-box></draw:frame>"#,
            "<text:numbered-paragraph><text:p>Numbered</text:p></text:numbered-paragraph>",
            "<text:p><draw:frame><draw:text-box><text:p>First</text:p></draw:text-box><draw:text-box><text:p>Second</text:p></draw:text-box></draw:frame></text:p>",
            "<text:user-index><text:index-body><text:p>Entry</text:p></text:index-body></text:user-index>",
            "<text:list><text:p>Stray</text:p></text:list>",
            r#"<draw:custom-shape text:anchor-type="page"><text:p>Shape text</text:p></draw:custom-shape>"#,
        ] {
            assert!(text_dropped(dropped), "{dropped}");
        }
    }

    #[test]
    fn only_the_first_body_of_the_lane_converts() {
        let content = format!(
            r#"<office:document-content {NAMESPACES}><office:body><office:text><text:p>First</text:p></office:text><office:text><text:p>Second</text:p></office:text></office:body></office:document-content>"#
        );
        assert!(
            walk_content(content.as_bytes(), OdfBody::Text, false)
                .unwrap()
                .dropped_text
        );
        let content = format!(
            r#"<office:document-content {NAMESPACES}><office:body><office:text><text:p>Body</text:p></office:text></office:body><office:body><office:text><text:p>Other</text:p></office:text></office:body></office:document-content>"#
        );
        assert!(
            !walk_content(content.as_bytes(), OdfBody::Text, false)
                .unwrap()
                .dropped_text
        );
    }

    #[test]
    fn presentations_lose_only_what_the_walker_skips() {
        for converted in [
            r#"<draw:frame presentation:class="title"><draw:text-box><text:p>Title</text:p></draw:text-box></draw:frame>"#,
            "<draw:custom-shape><text:p>Shape</text:p><draw:enhanced-geometry/></draw:custom-shape>",
            "<draw:g><draw:rect><text:list><text:list-item><text:p>Grouped</text:p></text:list-item></text:list></draw:rect></draw:g>",
            r#"<draw:frame><table:table><table:table-row><table:table-cell><text:p>Cell</text:p></table:table-cell></table:table-row></table:table><draw:image xlink:href="Pictures/t.png"/></draw:frame>"#,
            r#"<draw:frame><draw:image xlink:href="Pictures/a.png"/><draw:text-box><text:p>Caption</text:p></draw:text-box></draw:frame>"#,
            r#"<presentation:notes><draw:page-thumbnail/><draw:frame presentation:class="notes"><draw:text-box><text:p>Speaker note</text:p></draw:text-box></draw:frame></presentation:notes>"#,
            r#"<draw:frame presentation:class="footer"><draw:text-box><text:p>Footer</text:p></draw:text-box></draw:frame>"#,
            "<anim:par><anim:seq/></anim:par>",
        ] {
            assert!(!slide_dropped(converted), "{converted}");
        }
        for dropped in [
            "<draw:a><draw:frame><draw:text-box><text:p>Linked</text:p></draw:text-box></draw:frame></draw:a>",
            "<draw:circle><text:p>Circle</text:p></draw:circle>",
            "<draw:custom-shape><text:h>Heading only</text:h></draw:custom-shape>",
            "<presentation:notes><draw:custom-shape><text:p>Note in a shape</text:p></draw:custom-shape></presentation:notes>",
            r#"<draw:frame><draw:image xlink:href="Pictures/a.png"/><draw:text-box><text:p>First</text:p></draw:text-box><draw:text-box><text:p>Second</text:p></draw:text-box></draw:frame>"#,
        ] {
            assert!(slide_dropped(dropped), "{dropped}");
        }
    }

    #[test]
    fn spreadsheet_formulas_render_as_anydoc_renders_them() {
        let uncached = |cell: &str| {
            walk(
                OdfBody::Spreadsheet,
                &format!(
                    r#"<table:table table:name="S"><table:table-row><table:table-cell office:value-type="string"><text:p>Label</text:p></table:table-cell>{cell}</table:table-row></table:table>"#
                ),
            )
            .uncached_formula
        };
        for cached in [
            r#"<table:table-cell table:formula="of:=1+1" office:value-type="float" office:value="2"><text:p>2</text:p></table:table-cell>"#,
            r#"<table:table-cell table:formula="of:=1+1" office:value-type="float" office:value="2"/>"#,
            r#"<table:table-cell table:formula="of:=1+1" office:value-type="percentage" office:value="0.5"/>"#,
            r#"<table:table-cell table:formula="of:=NOW()" office:value-type="time" office:time-value="PT1H"/>"#,
            r#"<table:table-cell table:formula="of:=TRUE()" office:value-type="boolean" office:boolean-value="true"/>"#,
            r#"<table:table-cell table:formula="of:=&quot;a&quot;" office:value-type="string" office:string-value="a"/>"#,
            r#"<table:table-cell office:value-type="float" office:value="3"/>"#,
        ] {
            assert!(!uncached(cached), "{cached}");
        }
        for missing in [
            r#"<table:table-cell table:formula="of:=1+1"/>"#,
            r#"<table:table-cell table:formula="of:=1+1" office:value="2"/>"#,
            r#"<table:table-cell table:formula="of:=1+1" office:value-type="float" x:value="2"/>"#,
            r#"<table:table-cell table:formula="of:=1+1" office:value-type="string" office:value="2"/>"#,
            r#"<table:table-cell table:formula="of:=1+1" office:value-type="float" office:value="two"/>"#,
            r#"<table:table-cell table:formula="of:=TODAY()" office:value-type="date" office:date-value=""/>"#,
            r#"<table:table-cell table:formula="of:=1+1">2</table:table-cell>"#,
            r#"<table:table-cell table:formula="of:=1+1" office:value-type="float" office:value="2"><text:list/></table:table-cell>"#,
        ] {
            assert!(uncached(missing), "{missing}");
        }
    }

    #[test]
    fn spreadsheet_drawings_comments_and_merges_are_outside_the_grid() {
        let dropped = |table: &str| walk(OdfBody::Spreadsheet, table).dropped_text;
        for converted in [
            r#"<table:table><table:shapes><draw:frame><draw:text-box><text:p>Sheet note</text:p></draw:text-box></draw:frame></table:shapes><table:table-row><table:table-cell><text:p>A</text:p></table:table-cell></table:table-row></table:table>"#,
            r#"<table:table><table:table-row><table:table-cell><office:annotation><text:p>Comment</text:p></office:annotation><text:p>A</text:p></table:table-cell><table:covered-table-cell><text:p>Merged away</text:p></table:covered-table-cell></table:table-row></table:table>"#,
            r#"<table:table><table:table-row><table:table-cell><draw:frame><draw:text-box><text:p>Anchored</text:p></draw:text-box></draw:frame></table:table-cell></table:table-row></table:table>"#,
            r#"<table:content-validations><table:content-validation><table:help-message><text:p>Help</text:p></table:help-message></table:content-validation></table:content-validations>"#,
        ] {
            assert!(!dropped(converted), "{converted}");
        }
        assert!(dropped(
            r#"<table:table><table:table-row><x:cell><text:p>Foreign</text:p></x:cell></table:table-row></table:table>"#
        ));
    }
}
