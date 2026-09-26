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
const NUMBER: &[u8] = b"urn:oasis:names:tc:opendocument:xmlns:datastyle:1.0";
const STYLE: &[u8] = b"urn:oasis:names:tc:opendocument:xmlns:style:1.0";
const FO: &[u8] = b"urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0";

/// Number styles and cell styles a content part may define before a
/// check gives up telling a colour from other marks.
const MAX_STYLES: usize = 65_536;
/// Literal text and conditional maps kept per number style.
const MAX_STYLE_TEXT: usize = 1_024;
const MAX_STYLE_MAPS: usize = 16;

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
    /// A cell whose display text is empty, as a format that hides the value
    /// leaves it, while AnyDoc converts the typed value instead.
    pub(super) hidden_value: bool,
    /// A negative value displayed without a sign, which only its colour
    /// marked as negative. Decided by [`OdfWalk::decide_signs`] once the
    /// styles part is read.
    pub(super) sign_lost: bool,
    /// Negative values displayed with no sign and a digit other than zero,
    /// with the cell style each takes.
    unsigned: Vec<(Option<String>, f64)>,
    /// The number and cell styles the content part defines.
    styles: NumberStyles,
}

/// Unsigned negatives kept for the styles part; past this, the walk
/// assumes a colour marked one.
const MAX_UNSIGNED: usize = 65_536;

impl OdfWalk {
    /// Decide which unsigned negatives lost their sign, with the styles
    /// part's number and cell styles (`styles.xml`) added to the content
    /// part's. A cell whose styles are not known counts as lost.
    pub(super) fn decide_signs(&mut self, styles_part: Option<&[u8]>) -> Result<(), DocumentError> {
        if self.unsigned.is_empty() {
            return Ok(());
        }
        let mut styles = std::mem::take(&mut self.styles);
        if let Some(bytes) = styles_part {
            styles.merge(read_number_styles(bytes)?);
        }
        self.sign_lost |= self.unsigned.iter().any(|(style, number)| {
            styles
                .colour_only(style.as_deref(), *number)
                .unwrap_or(true)
        });
        self.unsigned.clear();
        Ok(())
    }
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
    /// A paragraph was written for the cell's display text.
    paragraph: bool,
    /// The typed value is a number below zero.
    negative: bool,
    /// The typed value is anything but a number equal to zero.
    nonzero: bool,
    /// The converted text shows a sign: a minus, a parenthesis, or `CR` or
    /// `DR` as a word.
    sign_shown: bool,
    /// The converted text shows a digit other than zero.
    digit_shown: bool,
    /// The typed number, and the cell's style (`table:style-name`).
    number: Option<f64>,
    style: Option<String>,
}

/// One number style (`number:number-style`, `number:currency-style`,
/// `number:percentage-style`) as far as a negative's marks go.
#[derive(Default, Debug)]
struct NumberStyle {
    /// A text colour (`style:text-properties/@fo:color`).
    colour: bool,
    /// Its literal text and currency symbols, without white space.
    literal: String,
    /// A minus, a parenthesis, or `CR` or `DR` as a word in that text.
    sign: bool,
    /// Conditional styles (`style:map`): the condition and the style.
    maps: Vec<(String, String)>,
}

/// The number styles a part defines and the cell styles naming them.
#[derive(Default, Debug)]
struct NumberStyles {
    styles: std::collections::HashMap<String, NumberStyle>,
    /// Each cell style's number style (`style:data-style-name`) and parent
    /// (`style:parent-style-name`).
    cells: std::collections::HashMap<String, (Option<String>, Option<String>)>,
    /// The number style being read, the stack depth it opened at, and
    /// whether its literal text is open.
    open: Option<(String, NumberStyle, usize)>,
    text_depth: Option<usize>,
}

/// Parent cell styles followed for a number style.
const MAX_CELL_STYLE_CHAIN: usize = 16;

impl NumberStyles {
    /// Add another part's styles; those already here win.
    fn merge(&mut self, other: NumberStyles) {
        for (name, style) in other.styles {
            self.styles.entry(name).or_insert(style);
        }
        for (name, cell) in other.cells {
            self.cells.entry(name).or_insert(cell);
        }
    }

    /// A cell style's number style, its own or its nearest parent's.
    fn data_style(&self, cell_style: &str) -> Option<&str> {
        let mut current = cell_style;
        for _ in 0..MAX_CELL_STYLE_CHAIN {
            let (data, parent) = self.cells.get(current)?;
            if let Some(data) = data {
                return Some(data);
            }
            current = parent.as_deref()?;
        }
        None
    }

    /// The number style a value takes: the first map whose condition holds,
    /// else the style itself.
    fn applied<'a>(&'a self, name: &'a str, value: f64) -> &'a str {
        let Some(style) = self.styles.get(name) else {
            return name;
        };
        style
            .maps
            .iter()
            .find(|(condition, _)| condition_holds(condition, value) == Some(true))
            .map_or(name, |(_, applied)| applied.as_str())
    }

    /// Whether a negative value in a cell of this style is marked by its
    /// colour alone: the style it takes has a colour and no sign, and shows
    /// the same text as the style a positive value takes. `None` when the
    /// styles are not known.
    fn colour_only(&self, cell_style: Option<&str>, value: f64) -> Option<bool> {
        let data = self.data_style(cell_style?)?;
        self.styles.get(data)?;
        let negative = self.applied(data, value);
        let positive = self.applied(data, value.abs());
        if negative == positive {
            return Some(false);
        }
        let (negative, positive) = (self.styles.get(negative)?, self.styles.get(positive)?);
        Some(negative.colour && !negative.sign && negative.literal == positive.literal)
    }
}

/// Whether a `style:map` condition, `value()` compared with a number, holds.
fn condition_holds(condition: &str, value: f64) -> Option<bool> {
    let rest = condition.trim().strip_prefix("value()")?.trim_start();
    let (operator, operand) =
        ["<=", ">=", "!=", "<>", "<", ">", "="]
            .iter()
            .find_map(|operator| {
                rest.strip_prefix(operator)
                    .map(|operand| (*operator, operand))
            })?;
    let operand: f64 = operand.trim().parse().ok()?;
    Some(match operator {
        "<=" => value <= operand,
        ">=" => value >= operand,
        "!=" | "<>" => value != operand,
        "<" => value < operand,
        ">" => value > operand,
        _ => value == operand,
    })
}

/// Characters a reader takes for a minus sign or accounting parentheses:
/// hyphen-minus, the minus sign, figure and en dashes, and small and
/// fullwidth forms. An em dash, which often stands for zero, is not one.
const SIGNS: [char; 10] = [
    '-', '(', ')', '\u{2212}', '\u{2012}', '\u{2013}', '\u{fe63}', '\u{ff0d}', '\u{ff08}',
    '\u{ff09}',
];

/// Whether text marks a value negative: a minus or parenthesis, or the
/// accounting words `CR` and `DR`, but not a currency code holding them
/// (`IDR`, `CRC`).
fn shows_sign(text: &str) -> bool {
    text.contains(SIGNS)
        || text
            .split(|character: char| !character.is_alphabetic())
            .any(|word| word.eq_ignore_ascii_case("CR") || word.eq_ignore_ascii_case("DR"))
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
    let mut styles = NumberStyles::default();
    let mut tables: Vec<TableColumns> = Vec::new();
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
                close_number_style(&mut styles, stack.len());
                if tables
                    .last()
                    .is_some_and(|table| table.depth == stack.len())
                {
                    tables.pop();
                }
                buffer.clear();
                continue;
            }
            quick_xml::events::Event::Text(text) => {
                style_text(&mut styles, &String::from_utf8_lossy(text.as_ref()));
                on_text(&mut stack, text.as_ref(), &mut walk);
                buffer.clear();
                continue;
            }
            quick_xml::events::Event::CData(text) => {
                style_text(&mut styles, &String::from_utf8_lossy(text.as_ref()));
                on_text(&mut stack, text.as_ref(), &mut walk);
                buffer.clear();
                continue;
            }
            quick_xml::events::Event::GeneralRef(reference) => {
                let text = super::anydoc_entity_text(&String::from_utf8_lossy(reference.as_ref()));
                style_text(&mut styles, &text);
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
        let attributes = resolved_attributes(&reader, &element);
        let cell_style = track_tables(
            &mut tables,
            &namespace,
            &local,
            &attributes,
            stack.len(),
            start,
        );
        read_number_style(
            &mut styles,
            &namespace,
            &local,
            &attributes,
            stack.len(),
            start,
        )?;
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
            let number = match attribute(&attributes, OFFICE, b"value-type") {
                Some("percentage" | "currency" | "float") => {
                    attribute(&attributes, OFFICE, b"value")
                        .and_then(|value| value.trim().parse::<f64>().ok())
                }
                _ => None,
            };
            // A string of white space shows as nothing, like a zero.
            let blank_string = attribute(&attributes, OFFICE, b"value-type") == Some("string")
                && attribute(&attributes, OFFICE, b"string-value")
                    .is_some_and(|text| text.trim().is_empty());
            open.cell = Some(CellState {
                formula: attributes.iter().any(|(_, name, _)| name == b"formula"),
                value_renders: value_renders(&attributes),
                negative: number.is_some_and(|number| number < 0.0),
                nonzero: number != Some(0.0) && !blank_string,
                number,
                style: cell_style,
                ..CellState::default()
            });
        }
        if start {
            stack.push(open);
        } else {
            finish(open, &mut walk);
            close_number_style(&mut styles, stack.len());
        }
        buffer.clear();
    }
    while let Some(closed) = stack.pop() {
        finish(closed, &mut walk);
    }
    walk.styles = styles;
    Ok(walk)
}

/// A table's column default cell styles (`table:default-cell-style-name`)
/// in runs, the row's, and the column the next cell takes.
struct TableColumns {
    depth: usize,
    columns: Vec<(u64, Option<String>)>,
    row_default: Option<String>,
    column: u64,
}

/// Column runs a table may declare before later ones are ignored.
const MAX_COLUMN_RUNS: usize = 16_384;

/// Follow tables, their columns, rows, and cells; for a cell, the cell style
/// it takes: its own, else its row's default, else its column's.
fn track_tables(
    tables: &mut Vec<TableColumns>,
    namespace: &[u8],
    local: &[u8],
    attributes: &Attributes,
    depth: usize,
    start: bool,
) -> Option<String> {
    if namespace != TABLE {
        return None;
    }
    let value = |name: &[u8]| attribute(attributes, TABLE, name);
    let repeated = |name: &[u8]| {
        value(name)
            .and_then(|count| count.trim().parse::<u64>().ok())
            .unwrap_or(1)
            .max(1)
    };
    match local {
        b"table" if start => tables.push(TableColumns {
            depth,
            columns: Vec::new(),
            row_default: None,
            column: 0,
        }),
        b"table-column" => {
            if let Some(table) = tables.last_mut() {
                if table.columns.len() < MAX_COLUMN_RUNS {
                    table.columns.push((
                        repeated(b"number-columns-repeated"),
                        value(b"default-cell-style-name").map(str::to_string),
                    ));
                }
            }
        }
        b"table-row" => {
            if let Some(table) = tables.last_mut() {
                table.column = 0;
                table.row_default = value(b"default-cell-style-name").map(str::to_string);
            }
        }
        b"table-cell" | b"covered-table-cell" => {
            let table = tables.last_mut()?;
            let column = table.column;
            table.column = column.saturating_add(repeated(b"number-columns-repeated"));
            if let Some(own) = value(b"style-name") {
                return Some(own.to_string());
            }
            if let Some(row) = &table.row_default {
                return Some(row.clone());
            }
            let mut first = 0u64;
            for (count, style) in &table.columns {
                if column < first.saturating_add(*count) {
                    return style.clone();
                }
                first = first.saturating_add(*count);
            }
        }
        _ => {}
    }
    None
}

/// Read what an element adds to the number styles: a number style opening
/// at `depth`, its colour, literal text, or maps, or a cell style naming
/// one.
fn read_number_style(
    styles: &mut NumberStyles,
    namespace: &[u8],
    local: &[u8],
    attributes: &Attributes,
    depth: usize,
    start: bool,
) -> Result<(), DocumentError> {
    let value = |bound: &[u8], name: &[u8]| attribute(attributes, bound, name);
    match (namespace, local) {
        (NUMBER, b"number-style" | b"currency-style" | b"percentage-style") if start => {
            if let Some(name) = value(STYLE, b"name") {
                styles.open = Some((name.to_string(), NumberStyle::default(), depth));
            }
        }
        (NUMBER, b"text" | b"currency-symbol") if start && styles.open.is_some() => {
            styles.text_depth = Some(depth);
        }
        (STYLE, b"text-properties") => {
            if let Some((_, style, _)) = styles.open.as_mut() {
                style.colour |= value(FO, b"color").is_some();
            }
        }
        (STYLE, b"map") => {
            if let (Some((_, style, _)), Some(condition), Some(applied)) = (
                styles.open.as_mut(),
                value(STYLE, b"condition"),
                value(STYLE, b"apply-style-name"),
            ) {
                if style.maps.len() < MAX_STYLE_MAPS {
                    style
                        .maps
                        .push((condition.to_string(), applied.to_string()));
                }
            }
        }
        (STYLE, b"style") if value(STYLE, b"family") == Some("table-cell") => {
            if let Some(name) = value(STYLE, b"name") {
                if styles.cells.len() >= MAX_STYLES {
                    return Err(DocumentError::ResourceLimit);
                }
                styles.cells.insert(
                    name.to_string(),
                    (
                        value(STYLE, b"data-style-name").map(str::to_string),
                        value(STYLE, b"parent-style-name").map(str::to_string),
                    ),
                );
            }
        }
        _ => {}
    }
    Ok(())
}

/// Read the number and cell styles a styles part defines.
fn read_number_styles(bytes: &[u8]) -> Result<NumberStyles, DocumentError> {
    let mut reader = quick_xml::NsReader::from_reader(Cursor::new(bytes));
    reader.config_mut().trim_text(false);
    reader.config_mut().check_end_names = false;
    let mut buffer = Vec::new();
    let mut styles = NumberStyles::default();
    let mut depth = 0usize;
    loop {
        let (namespace, event) = reader
            .read_resolved_event_into(&mut buffer)
            .map_err(|_| DocumentError::Malformed)?;
        let namespace = match namespace {
            ResolveResult::Bound(namespace) => namespace.as_ref().to_vec(),
            _ => Vec::new(),
        };
        match &event {
            quick_xml::events::Event::Start(element) | quick_xml::events::Event::Empty(element) => {
                let start = matches!(event, quick_xml::events::Event::Start(_));
                if depth >= MAX_DEPTH {
                    return Err(DocumentError::ResourceLimit);
                }
                let attributes = resolved_attributes(&reader, element);
                read_number_style(
                    &mut styles,
                    &namespace,
                    element.local_name().as_ref(),
                    &attributes,
                    depth,
                    start,
                )?;
                if start {
                    depth += 1;
                } else {
                    close_number_style(&mut styles, depth);
                }
            }
            quick_xml::events::Event::End(_) => {
                depth = depth.saturating_sub(1);
                close_number_style(&mut styles, depth);
            }
            quick_xml::events::Event::Text(text) => {
                style_text(&mut styles, &String::from_utf8_lossy(text.as_ref()));
            }
            quick_xml::events::Event::GeneralRef(reference) => {
                let text = super::anydoc_entity_text(&String::from_utf8_lossy(reference.as_ref()));
                style_text(&mut styles, &text);
            }
            quick_xml::events::Event::Eof => return Ok(styles),
            _ => {}
        }
        buffer.clear();
    }
}

/// An element's attributes, with their namespaces resolved.
fn resolved_attributes<R>(
    reader: &quick_xml::NsReader<R>,
    element: &quick_xml::events::BytesStart<'_>,
) -> Attributes {
    element
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
                .unwrap_or_else(|_| String::from_utf8_lossy(attribute.value.as_ref()).into_owned());
            (bound, name.as_ref().to_vec(), value)
        })
        .collect()
}

/// Close what the element ending at `depth` opened.
fn close_number_style(styles: &mut NumberStyles, depth: usize) {
    if styles.text_depth == Some(depth) {
        styles.text_depth = None;
    }
    if styles
        .open
        .as_ref()
        .is_some_and(|(_, _, opened)| *opened == depth)
    {
        if let Some((name, style, _)) = styles.open.take() {
            if styles.styles.len() < MAX_STYLES || styles.styles.contains_key(&name) {
                styles.styles.insert(name, style);
            }
        }
    }
}

/// Literal text inside an open number style's `number:text` or
/// `number:currency-symbol`.
fn style_text(styles: &mut NumberStyles, text: &str) {
    if styles.text_depth.is_none() {
        return;
    }
    if let Some((_, style, _)) = styles.open.as_mut() {
        style.sign |= shows_sign(text);
        for character in text.chars().filter(|character| !character.is_whitespace()) {
            if style.literal.len() >= MAX_STYLE_TEXT {
                break;
            }
            style.literal.push(character);
        }
    }
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
            if spreadsheet && namespace == OFFICE && local == b"annotation" {
                // Cell comments, which no conversion covers.
                Mode::Ignored
            } else if spreadsheet && namespace == DRAW {
                // A drawing anchored to the cell: text in it is lost.
                Mode::Skipped
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
            // Drawings over the grid are not converted: text in them is lost.
            (TABLE, b"shapes") => Mode::Skipped,
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
    if parent.mode == Mode::Cell && mode == Mode::Inline {
        if let Some(cell) = parent.cell.as_mut() {
            cell.paragraph = true;
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
        let text = String::from_utf8_lossy(text);
        cell.sign_shown |= shows_sign(&text);
        cell.digit_shown |= text.contains(|character: char| matches!(character, '1'..='9'));
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
        let fallback = !cell.rendered && !cell.structured && cell.value_renders;
        // A display paragraph, even an empty one, is the formula's cached
        // display: LibreOffice writes an empty one, and no value type, for
        // a formula returning an empty string (`=IF(A2="";"";A2*B2)`).
        if cell.formula && !(cell.rendered || fallback || cell.paragraph) {
            walk.uncached_formula = true;
        }
        // An empty display paragraph is what LibreOffice writes for a value
        // its format hides; AnyDoc converts the typed value in its place.
        // A hidden zero is the common "hide zeros" format and hides nothing.
        if cell.paragraph && fallback && cell.nonzero {
            walk.hidden_value = true;
        }
        // A negative shown without a sign lost it only where a colour was
        // its sole mark: not a zero, and not text of its own. The styles
        // decide, once the styles part is read.
        if cell.negative && cell.rendered && !cell.sign_shown && cell.digit_shown {
            if walk.unsigned.len() < MAX_UNSIGNED {
                walk.unsigned
                    .push((cell.style.clone(), cell.number.unwrap_or(-1.0)));
            } else {
                walk.sign_lost = true;
            }
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
        r#"xmlns:number="urn:oasis:names:tc:opendocument:xmlns:datastyle:1.0" "#,
        r#"xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" "#,
        r#"xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0" "#,
        r#"xmlns:xlink="http://www.w3.org/1999/xlink" xmlns:x="urn:x""#
    );

    fn walk(body: OdfBody, inner: &str) -> OdfWalk {
        walk_styled(body, "", inner)
    }

    fn walk_styled(body: OdfBody, styles: &str, inner: &str) -> OdfWalk {
        let element = std::str::from_utf8(body.element()).unwrap();
        let content = format!(
            r#"<office:document-content {NAMESPACES}><office:automatic-styles>{styles}</office:automatic-styles><office:body><office:{element}>{inner}</office:{element}></office:body></office:document-content>"#
        );
        let mut walk =
            walk_content(content.as_bytes(), body, body == OdfBody::Spreadsheet).expect("walk");
        walk.decide_signs(None).expect("signs");
        walk
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
            // An empty string result: an empty display and no value type.
            r#"<table:table-cell table:formula="of:=IF([.A2]=&quot;&quot;;&quot;&quot;;1)"><text:p/></table:table-cell>"#,
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
        // Comments, merged-away cells, and validation help are not content.
        for converted in [
            r#"<table:table><table:table-row><table:table-cell><office:annotation><text:p>Comment</text:p></office:annotation><text:p>A</text:p></table:table-cell><table:covered-table-cell><text:p>Merged away</text:p></table:covered-table-cell></table:table-row></table:table>"#,
            r#"<table:content-validations><table:content-validation><table:help-message><text:p>Help</text:p></table:help-message></table:content-validation></table:content-validations>"#,
            r#"<table:table><table:table-row><table:table-cell><draw:frame><draw:image/></draw:frame><text:p>A</text:p></table:table-cell></table:table-row></table:table>"#,
        ] {
            assert!(!dropped(converted), "{converted}");
        }
        // Text in a drawing over the grid, or anchored to a cell, is lost.
        for lost in [
            r#"<table:table><table:shapes><draw:frame><draw:text-box><text:p>Sheet note</text:p></draw:text-box></draw:frame></table:shapes><table:table-row><table:table-cell><text:p>A</text:p></table:table-cell></table:table-row></table:table>"#,
            r#"<table:table><table:table-row><table:table-cell><draw:frame><draw:text-box><text:p>Anchored</text:p></draw:text-box></draw:frame></table:table-cell></table:table-row></table:table>"#,
            r#"<table:table><table:table-row><table:table-cell><draw:custom-shape><text:p>Shape</text:p></draw:custom-shape></table:table-cell></table:table-row></table:table>"#,
            r#"<table:table><table:table-row><x:cell><text:p>Foreign</text:p></x:cell></table:table-row></table:table>"#,
        ] {
            assert!(dropped(lost), "{lost}");
        }
    }

    #[test]
    fn spreadsheet_values_a_format_hides_or_unsigns_are_found() {
        let cell = |attributes: &str, text: &str| {
            let body = if text.is_empty() {
                "<text:p/>".to_string()
            } else {
                format!("<text:p>{text}</text:p>")
            };
            walk(
                OdfBody::Spreadsheet,
                &format!(
                    r#"<table:table><table:table-row><table:table-cell {attributes}>{body}</table:table-cell></table:table-row></table:table>"#
                ),
            )
        };
        let float = |value: &str| format!(r#"office:value-type="float" office:value="{value}""#);
        // An empty display paragraph hides a value AnyDoc then converts.
        assert!(cell(&float("98765"), "").hidden_value);
        assert!(
            cell(
                r#"office:value-type="string" office:string-value="Adjusted basis""#,
                ""
            )
            .hidden_value
        );
        // A hidden zero, or a cell without a typed value, hides nothing.
        assert!(!cell(&float("0"), "").hidden_value);
        assert!(!cell("", "").hidden_value);
        assert!(!cell(&float("98765"), "98,765").hidden_value);
        // A cell with no display paragraph at all shows its value.
        let bare = walk(
            OdfBody::Spreadsheet,
            &format!(
                r#"<table:table><table:table-row><table:table-cell {}/></table:table-row></table:table>"#,
                float("12")
            ),
        );
        assert!(!bare.hidden_value);
        // A negative shown without a sign lost it to a colour.
        assert!(cell(&float("-1234.1"), "1234.10").sign_lost);
        for shown in ["-1234.10", "(1,234.10)", "\u{2212}1234.10", "1,234.10 CR"] {
            assert!(!cell(&float("-1234.1"), shown).sign_lost, "{shown}");
        }
        assert!(!cell(&float("1234.1"), "1234.10").sign_lost);
        // With no display text AnyDoc writes the typed value, sign and all.
        assert!(!cell(&float("-1234.1"), "").sign_lost);
        // A currency code holding `CR` or `DR` is no accounting marker, and
        // a value displayed as zero lost nothing.
        assert!(cell(&float("-25000"), "IDR 25,000").sign_lost);
        assert!(!cell(&float("-0.0000000000291"), "0.00").sign_lost);
        // A string of white space shows nothing to hide.
        let blank = cell(
            r#"table:formula="of:=IF(1;&quot; &quot;)" office:value-type="string" office:string-value=" ""#,
            "<text:s/>",
        );
        assert!(!blank.hidden_value && !blank.uncached_formula);

        // With the number styles LibreOffice writes, a colour marks the
        // negative only when the text is the positive style's text.
        let styled = |colour: &str, negative: &str, positive: &str, value: &str, shown: &str| {
            let styles = format!(
                r#"<number:number-style style:name="N1P0">{positive}<number:number number:decimal-places="2"/></number:number-style><number:number-style style:name="N1">{colour}{negative}<number:number number:decimal-places="2"/><style:map style:condition="value()&gt;=0" style:apply-style-name="N1P0"/></number:number-style><style:style style:name="ce1" style:family="table-cell" style:data-style-name="N1"/>"#
            );
            walk_styled(
                OdfBody::Spreadsheet,
                &styles,
                &format!(
                    r#"<table:table><table:table-row><table:table-cell table:style-name="ce1" {}><text:p>{shown}</text:p></table:table-cell></table:table-row></table:table>"#,
                    float(value)
                ),
            )
            .sign_lost
        };
        let red = r##"<style:text-properties fo:color="#ff0000"/>"##;
        let idr =
            "<number:currency-symbol>IDR</number:currency-symbol><number:text> </number:text>";
        assert!(styled(red, "", "", "-1234.1", "1,234.10"));
        assert!(styled(red, idr, idr, "-25000", "IDR 25,000.00"));
        assert!(!styled(
            red,
            "<number:text>Refund </number:text>",
            "<number:text>Balance due </number:text>",
            "-830",
            "Refund 830.00"
        ));
        assert!(!styled(
            red,
            "<number:text>\u{25bc}</number:text>",
            "<number:text>\u{25b2}</number:text>",
            "-3.1",
            "\u{25bc}3.10"
        ));
        assert!(!styled("", "", "", "-1234.1", "1,234.10"));
        // The style may come from the cell's column, and the number styles
        // from the styles part, as LibreOffice writes a named format.
        let styles_part = format!(
            r#"<office:document-styles {NAMESPACES}><office:styles><number:number-style style:name="N9P0"><number:text>Balance due $</number:text><number:number/></number:number-style><number:number-style style:name="N9P1"><number:text>Refund $</number:text><number:number/></number:number-style><number:number-style style:name="N9"><number:text>Even</number:text><style:map style:condition="value()&gt;0" style:apply-style-name="N9P0"/><style:map style:condition="value()&lt;0" style:apply-style-name="N9P1"/></number:number-style></office:styles></office:document-styles>"#
        );
        let content = format!(
            r#"<office:document-content {NAMESPACES}><office:automatic-styles><style:style style:name="ce1" style:family="table-cell" style:parent-style-name="Default" style:data-style-name="N9"/></office:automatic-styles><office:body><office:spreadsheet><table:table><table:table-column table:default-cell-style-name="Default"/><table:table-column table:default-cell-style-name="ce1"/><table:table-row><table:table-cell office:value-type="string"><text:p>Refund</text:p></table:table-cell><table:table-cell office:value-type="float" office:value="-830"><text:p>Refund $830</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#
        );
        let mut labelled = walk_content(content.as_bytes(), OdfBody::Spreadsheet, true).unwrap();
        labelled.decide_signs(Some(styles_part.as_bytes())).unwrap();
        assert!(!labelled.sign_lost);
        // Without the styles part the mark is unknown, and counts as lost.
        let mut unknown = walk_content(content.as_bytes(), OdfBody::Spreadsheet, true).unwrap();
        unknown.decide_signs(None).unwrap();
        assert!(unknown.sign_lost);
        assert_eq!(condition_holds("value()>=0", -1.0), Some(false));
        assert_eq!(condition_holds(" value() < 0 ", -1.0), Some(true));
        assert_eq!(condition_holds("cell-content()>0", 1.0), None);
    }
}
