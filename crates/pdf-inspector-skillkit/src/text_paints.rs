//! What pdf-inspector 1.24.0 misreads in the text a page paints.
//!
//! **An invisible layer over a scan.** A scanned page made searchable
//! carries its words as invisible text (render mode 3, or 7, which only adds
//! to the clip) behind the page image. When the page also shows a little
//! real text, such as a header or a Bates number, pdf-inspector classifies
//! it as text, extracts the visible text alone, and reports no page for OCR:
//! it flags only a page whose every text operator is invisible. Upstream
//! pull requests #479 and #501 are open against this. The scan finds the
//! page it misses: images cover at least half of it, and invisible text
//! carries most of the bytes it shows as text. Clip-only text through which
//! an image or a shading is then painted, as in a heading filled with a
//! picture or a gradient, is visible, as pdf-inspector's own scan counts it.
//! The layer is never read into any output, since what it says need not be
//! what the page shows; the page is reported as needing OCR instead.
//!
//! **Text painted twice.** A producer that paints a run again over itself,
//! for emphasis, as an overprint, or as a replayed row, shows it once, but
//! pdf-inspector keeps every paint, so its Markdown repeats the text:
//! "TToottaall", or "84.19 84.19" (open upstream #317, #377). The scan notes
//! where each visible run starts when its position was just set, and
//! reports a page on which a run with the same bytes starts again within a
//! tenth of its size of an earlier one, in whatever font.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};

use lopdf::{content::Content, Dictionary, Document, Object, ObjectId, Stream};

/// Bytes any one content stream may decode to.
const MAX_STREAM_BYTES: usize = 32 << 20;
/// Decoded content bytes and operations scanned per document. Past either,
/// the scan stops and reports the pages it has already found.
const MAX_CONTENT_BYTES: usize = 128 << 20;
const MAX_OPERATIONS: usize = 10_000_000;
/// The same for pages read only for the repeat check, which reads every
/// text page: past either, that check stops, and the layer check, which
/// reads pages with images, goes on under its own limits.
const MAX_REPEAT_CONTENT_BYTES: usize = 64 << 20;
const MAX_REPEAT_OPERATIONS: usize = 4_000_000;
/// Form XObjects executing inside one another.
const MAX_FORM_DEPTH: usize = 12;
/// Graphics states saved (`q`) and not yet restored, per content stream.
const MAX_SAVED_STATES: usize = 256;
/// Bytes an object stream may decode to while the document loads, as
/// pdf-inspector bounds them.
const MAX_OBJECT_STREAM_BYTES: usize = 8 << 20;
/// Invisible text a page must carry to count as a layer, in string bytes.
const MIN_LAYER_BYTES: u64 = 32;
/// Share of the page that images must cover.
const COVERED_SHARE: f64 = 0.5;
/// Parent links followed to find an inherited page attribute.
const MAX_PAGE_TREE_DEPTH: usize = 32;
/// Runs noted per page for the repeat check.
const MAX_RUNS_PER_PAGE: usize = 100_000;
/// How near a run must start again to repeat one, as a share of its size,
/// and at least.
const REPEAT_SHARE: f64 = 0.1;
const MIN_REPEAT_DISTANCE: f64 = 0.3;

const IDENTITY: [f64; 6] = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

/// The scan's work limits ran out.
struct Exhausted;

struct Budget {
    bytes: usize,
    operations: usize,
    max_bytes: usize,
    max_operations: usize,
}

impl Budget {
    fn new(max_bytes: usize, max_operations: usize) -> Self {
        Budget {
            bytes: 0,
            operations: 0,
            max_bytes,
            max_operations,
        }
    }

    fn spent(&self) -> bool {
        self.bytes > self.max_bytes || self.operations > self.max_operations
    }

    fn take_bytes(&mut self, bytes: usize) -> Result<(), Exhausted> {
        self.bytes = self.bytes.saturating_add(bytes);
        if self.bytes > self.max_bytes {
            return Err(Exhausted);
        }
        Ok(())
    }

    fn take_operations(&mut self, operations: usize) -> Result<(), Exhausted> {
        self.operations = self.operations.saturating_add(operations);
        if self.operations > self.max_operations {
            return Err(Exhausted);
        }
        Ok(())
    }
}

/// The graphics state the scan follows, with the text state in it.
#[derive(Clone, Copy)]
struct State {
    ctm: [f64; 6],
    render_mode: i64,
    /// Whether `Tf` has set a font; text shown before shows nothing.
    font: bool,
    size: f64,
    leading: f64,
    rise: f64,
}

impl State {
    const START: State = State {
        ctm: IDENTITY,
        render_mode: 0,
        font: false,
        size: 0.0,
        leading: 0.0,
        rise: 0.0,
    };
}

/// Where the visible runs of one page start, by a hash of their bytes. The
/// font is left out: pdf-inspector keeps the text of every paint, whichever
/// font object draws it, so a second paint in an identical font object, or
/// another font, repeats the text all the same.
#[derive(Default)]
struct Runs {
    starts: HashMap<u64, Vec<[f64; 2]>>,
    noted: usize,
    repeated: bool,
}

impl Runs {
    fn note(&mut self, text: &[u8], at: [f64; 2], size: f64) {
        if self.repeated || self.noted >= MAX_RUNS_PER_PAGE {
            return;
        }
        let near = (REPEAT_SHARE * size).max(MIN_REPEAT_DISTANCE);
        let mut hasher = DefaultHasher::new();
        text.hash(&mut hasher);
        match self.starts.entry(hasher.finish()) {
            Entry::Occupied(mut entry) => {
                if entry.get().iter().any(|start| {
                    (start[0] - at[0]).abs() <= near && (start[1] - at[1]).abs() <= near
                }) {
                    self.repeated = true;
                    return;
                }
                entry.get_mut().push(at);
            }
            Entry::Vacant(entry) => {
                entry.insert(vec![at]);
            }
        }
        self.noted += 1;
    }
}

/// The bytes a text-showing operand holds: a string, or an array's strings.
fn shown_bytes(text: Option<&Object>) -> Vec<u8> {
    match text {
        Some(Object::String(bytes, _)) => bytes.clone(),
        Some(Object::Array(parts)) => parts
            .iter()
            .filter_map(|part| match part {
                Object::String(bytes, _) => Some(bytes.as_slice()),
                _ => None,
            })
            .flatten()
            .copied()
            .collect(),
        _ => Vec::new(),
    }
}

/// What one page's content shows.
#[derive(Default)]
struct PageText {
    hidden_bytes: u64,
    visible_bytes: u64,
    /// Area of the page the drawn images cover, clipped to the page box,
    /// counting overlaps more than once.
    image_area: f64,
    /// Clip-only (mode 7) text of the open text object, and, by graphics
    /// state level, that whose clip is in force: hidden unless an image or
    /// a shading is painted through it before its level is restored.
    clip_open: u64,
    clip_levels: Vec<u64>,
    /// The runs noted for the repeat check, when it is made.
    runs: Option<Runs>,
}

impl PageText {
    fn show(&mut self, state: State, bytes: &[u8]) {
        let bytes = bytes.len() as u64;
        match state.render_mode {
            // Mode 3 paints nothing.
            3 => self.hidden_bytes += bytes,
            // Mode 7 adds the glyphs to the clip once the text object ends.
            7 => self.clip_open += bytes,
            _ => self.visible_bytes += bytes,
        }
    }

    fn text_object_ended(&mut self) {
        let open = std::mem::take(&mut self.clip_open);
        match self.clip_levels.last_mut() {
            Some(level) => *level += open,
            None => self.hidden_bytes += open,
        }
    }

    fn save(&mut self) {
        self.clip_levels.push(0);
    }

    /// Restore a level: its clip-only text with nothing painted through it
    /// stays hidden.
    fn restore(&mut self) {
        if let Some(level) = self.clip_levels.pop() {
            self.hidden_bytes += level;
        }
    }

    /// Restore the levels past `depth`.
    fn restore_to(&mut self, depth: usize) {
        while self.clip_levels.len() > depth {
            self.restore();
        }
    }

    /// An image or a shading was painted through the clips in force: their
    /// clip-only text shows it.
    fn painted(&mut self) {
        for level in &mut self.clip_levels {
            self.visible_bytes += std::mem::take(level);
        }
    }

    /// The content ended: what was never painted through stays hidden.
    fn ended(&mut self) {
        self.text_object_ended();
        self.restore_to(0);
    }

    /// Note a visible run whose start the text matrix says.
    fn note_run(&mut self, state: State, text_matrix: [f64; 6], bytes: &[u8]) {
        let Some(runs) = self.runs.as_mut() else {
            return;
        };
        if !state.font
            || matches!(state.render_mode, 3 | 7)
            || !bytes.iter().any(|&byte| byte != b' ')
        {
            return;
        }
        let matrix = multiply(text_matrix, state.ctm);
        let at = [
            matrix[2] * state.rise + matrix[4],
            matrix[3] * state.rise + matrix[5],
        ];
        let size = state.size.abs() * matrix[2].hypot(matrix[3]);
        if at.iter().all(|value| value.is_finite()) && size.is_finite() {
            runs.note(bytes, at, size);
        }
    }

    fn draw_image(&mut self, ctm: [f64; 6], page_box: [f64; 4]) {
        // An image fills the unit square of its user space.
        let corners = [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)].map(|(x, y)| {
            (
                ctm[0] * x + ctm[2] * y + ctm[4],
                ctm[1] * x + ctm[3] * y + ctm[5],
            )
        });
        let (mut left, mut right) = (f64::INFINITY, f64::NEG_INFINITY);
        let (mut bottom, mut top) = (f64::INFINITY, f64::NEG_INFINITY);
        for (x, y) in corners {
            left = left.min(x);
            right = right.max(x);
            bottom = bottom.min(y);
            top = top.max(y);
        }
        let width = (right.min(page_box[2]) - left.max(page_box[0])).max(0.0);
        let height = (top.min(page_box[3]) - bottom.max(page_box[1])).max(0.0);
        let area = width * height;
        if area.is_finite() {
            self.image_area += area;
        }
        self.painted();
    }

    fn is_hidden_layer(&self, page_box: [f64; 4]) -> bool {
        let page_area = (page_box[2] - page_box[0]) * (page_box[3] - page_box[1]);
        page_area > 0.0
            && self.image_area >= COVERED_SHARE * page_area
            && self.hidden_bytes >= MIN_LAYER_BYTES
            && self.hidden_bytes > self.visible_bytes
    }
}

/// The 1-indexed pages the scan reports.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Findings {
    /// Pages whose text is mostly an invisible layer over images covering
    /// the page.
    pub(crate) hidden_layer: Vec<u32>,
    /// Pages that paint a visible run again over itself.
    pub(crate) painted_twice: Vec<u32>,
}

/// Scan a document's pages. Pages in `layer_skip` are not checked for an
/// invisible layer; pages are checked for text painted twice only when
/// `twice_skip` is given, and not those in it. Pages outside `only`, when
/// it is given, are not scanned. A document that does not load reports
/// none.
pub(crate) fn scan(
    buffer: &[u8],
    layer_skip: &HashSet<u32>,
    twice_skip: Option<&HashSet<u32>>,
    only: Option<&HashSet<u32>>,
) -> Findings {
    let options = lopdf::LoadOptions {
        max_decompressed_size: Some(MAX_OBJECT_STREAM_BYTES),
        ..Default::default()
    };
    let Ok(document) = Document::load_mem_with_options(buffer, options) else {
        return Findings::default();
    };
    let mut layer_budget = Budget::new(MAX_CONTENT_BYTES, MAX_OPERATIONS);
    let mut repeat_budget = Budget::new(MAX_REPEAT_CONTENT_BYTES, MAX_REPEAT_OPERATIONS);
    let mut repeats = twice_skip.is_some();
    let mut found = Findings::default();
    for (&number, &page_id) in &document.get_pages() {
        if only.is_some_and(|only| !only.contains(&number)) {
            continue;
        }
        let check_layer = !layer_skip.contains(&number);
        let check_twice = repeats && twice_skip.is_some_and(|skip| !skip.contains(&number));
        if !check_layer && !check_twice {
            continue;
        }
        let budgets = Budgets {
            layer: &mut layer_budget,
            repeat: &mut repeat_budget,
        };
        match scan_page(&document, page_id, check_layer, check_twice, budgets) {
            Ok(page) => {
                if page.hidden_layer {
                    found.hidden_layer.push(number);
                }
                if page.painted_twice {
                    found.painted_twice.push(number);
                }
            }
            // The repeat check's limits ran out on a page read for it alone:
            // that check stops, and the layer check goes on.
            Err(Exhausted) if repeat_budget.spent() => repeats = false,
            Err(Exhausted) => break,
        }
    }
    found
}

/// The limits a page is read under: the layer check's when the page is
/// read for it, the repeat check's otherwise.
struct Budgets<'a> {
    layer: &'a mut Budget,
    repeat: &'a mut Budget,
}

/// What the scan found on one page.
#[derive(Default)]
struct PageFindings {
    hidden_layer: bool,
    painted_twice: bool,
}

fn scan_page(
    document: &Document,
    page_id: ObjectId,
    check_layer: bool,
    check_twice: bool,
    budgets: Budgets<'_>,
) -> Result<PageFindings, Exhausted> {
    let Some(page_box) = page_box(document, page_id) else {
        return Ok(PageFindings::default());
    };
    let resources = page_resources(document, page_id);
    // A scan is an image XObject; for the layer check, a page that binds
    // none, directly or through its forms, is not read.
    let mut seen = HashSet::new();
    let check_layer = check_layer
        && resources
            .iter()
            .any(|dictionary| binds_image(document, dictionary, 0, &mut seen));
    if !check_layer && !check_twice {
        return Ok(PageFindings::default());
    }
    let budget = if check_layer {
        budgets.layer
    } else {
        budgets.repeat
    };
    let mut content = Vec::new();
    for id in document.get_page_contents(page_id) {
        let Ok(stream) = document.get_object(id).and_then(Object::as_stream) else {
            continue;
        };
        let Ok(bytes) = stream.decompressed_content_with_limit(MAX_STREAM_BYTES) else {
            continue;
        };
        budget.take_bytes(bytes.len())?;
        content.extend_from_slice(&bytes);
        // Streams are concatenated as if one, separated by white space.
        content.push(b'\n');
    }
    let mut page = PageText {
        runs: check_twice.then(Runs::default),
        ..PageText::default()
    };
    page.save();
    let mut forms = Vec::new();
    execute(
        document,
        &content,
        &resources,
        State::START,
        page_box,
        &mut page,
        &mut forms,
        budget,
    )?;
    page.ended();
    Ok(PageFindings {
        hidden_layer: check_layer && page.is_hidden_layer(page_box),
        painted_twice: page.runs.is_some_and(|runs| runs.repeated),
    })
}

#[allow(clippy::too_many_arguments)]
fn execute<'a>(
    document: &'a Document,
    content: &[u8],
    resources: &[&'a Dictionary],
    start: State,
    page_box: [f64; 4],
    page: &mut PageText,
    forms: &mut Vec<ObjectId>,
    budget: &mut Budget,
) -> Result<(), Exhausted> {
    let Ok(content) = Content::decode(content) else {
        return Ok(());
    };
    budget.take_operations(content.operations.len())?;
    let mut state = start;
    let mut saved: Vec<State> = Vec::new();
    // Saves past the cap, so their restores are matched too.
    let mut unsaved = 0usize;
    // The text matrix and line matrix, and whether the next run's start is
    // known: its position was just set.
    let mut text_matrix = IDENTITY;
    let mut line_matrix = IDENTITY;
    let mut placed = false;
    for operation in &content.operations {
        let operands = &operation.operands;
        let operator = operation.operator.as_str();
        // `'` and `"` move to the next line before they show text.
        if matches!(operator, "T*" | "'" | "\"") {
            line_matrix = multiply([1.0, 0.0, 0.0, 1.0, 0.0, -state.leading], line_matrix);
            text_matrix = line_matrix;
            placed = true;
        }
        match operator {
            "q" => {
                if saved.len() < MAX_SAVED_STATES {
                    saved.push(state);
                    page.save();
                } else {
                    unsaved += 1;
                }
            }
            "Q" => {
                if unsaved > 0 {
                    unsaved -= 1;
                } else if let Some(previous) = saved.pop() {
                    state = previous;
                    page.restore();
                }
            }
            "cm" => {
                if let Some(matrix) = matrix(document, operands) {
                    state.ctm = multiply(matrix, state.ctm);
                }
            }
            "Tr" => {
                if let Some(mode) = operands.first().and_then(|mode| integer(document, mode)) {
                    state.render_mode = mode;
                }
            }
            "Tf" => {
                if let [name, size] = operands.as_slice() {
                    state.font = name.as_name().is_ok();
                    if let Some(size) = number(document, size) {
                        state.size = size;
                    }
                }
            }
            "TL" => {
                if let Some(leading) = operands.first().and_then(|value| number(document, value)) {
                    state.leading = leading;
                }
            }
            "Ts" => {
                if let Some(rise) = operands.first().and_then(|value| number(document, value)) {
                    state.rise = rise;
                }
            }
            "BT" => {
                text_matrix = IDENTITY;
                line_matrix = IDENTITY;
                placed = true;
            }
            "ET" => {
                page.text_object_ended();
                placed = false;
            }
            "Tm" => {
                if let Some(matrix) = matrix(document, operands) {
                    text_matrix = matrix;
                    line_matrix = matrix;
                    placed = true;
                }
            }
            "Td" | "TD" => {
                if let [x, y] = operands.as_slice() {
                    if let (Some(x), Some(y)) = (number(document, x), number(document, y)) {
                        if operator == "TD" {
                            state.leading = -y;
                        }
                        line_matrix = multiply([1.0, 0.0, 0.0, 1.0, x, y], line_matrix);
                        text_matrix = line_matrix;
                        placed = true;
                    }
                }
            }
            "Tj" | "'" | "\"" | "TJ" => {
                let text = if operator == "TJ" {
                    operands.first()
                } else {
                    operands.last()
                };
                let bytes = shown_bytes(text);
                page.show(state, &bytes);
                // After a run, the next starts where it ended, which the
                // glyph widths decide.
                if placed {
                    page.note_run(state, text_matrix, &bytes);
                }
                placed = false;
            }
            "BI" => page.draw_image(state.ctm, page_box),
            "sh" => page.painted(),
            "Do" => {
                let Some(name) = operands.first().and_then(|name| name.as_name().ok()) else {
                    continue;
                };
                let Some((id, stream)) = xobject(document, resources, name) else {
                    continue;
                };
                match stream.dict.get(b"Subtype").and_then(Object::as_name) {
                    Ok(b"Image") => page.draw_image(state.ctm, page_box),
                    Ok(b"Form") => {
                        if forms.len() >= MAX_FORM_DEPTH || forms.contains(&id) {
                            continue;
                        }
                        let Ok(bytes) = stream.decompressed_content_with_limit(MAX_STREAM_BYTES)
                        else {
                            continue;
                        };
                        budget.take_bytes(bytes.len())?;
                        let form_matrix = stream
                            .dict
                            .get(b"Matrix")
                            .ok()
                            .and_then(|matrix| array(document, matrix))
                            .and_then(|values| matrix(document, values))
                            .unwrap_or(IDENTITY);
                        // A form without resources uses its invoker's.
                        let form_resources: Vec<&Dictionary> = match stream
                            .dict
                            .get(b"Resources")
                            .ok()
                            .and_then(|resources| dictionary(document, resources))
                        {
                            Some(own) => vec![own],
                            None => resources.to_vec(),
                        };
                        let inner = State {
                            ctm: multiply(form_matrix, state.ctm),
                            ..state
                        };
                        // A form runs in a saved state of its own.
                        let depth = page.clip_levels.len();
                        page.save();
                        forms.push(id);
                        let result = execute(
                            document,
                            &bytes,
                            &form_resources,
                            inner,
                            page_box,
                            page,
                            forms,
                            budget,
                        );
                        forms.pop();
                        page.restore_to(depth);
                        result?;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// The product of two PDF matrices `[a b c d e f]`, `first` applied first.
fn multiply(first: [f64; 6], then: [f64; 6]) -> [f64; 6] {
    [
        first[0] * then[0] + first[1] * then[2],
        first[0] * then[1] + first[1] * then[3],
        first[2] * then[0] + first[3] * then[2],
        first[2] * then[1] + first[3] * then[3],
        first[4] * then[0] + first[5] * then[2] + then[4],
        first[4] * then[1] + first[5] * then[3] + then[5],
    ]
}

fn number(document: &Document, object: &Object) -> Option<f64> {
    let (_, object) = document.dereference(object).ok()?;
    let value = match object {
        Object::Integer(value) => *value as f64,
        Object::Real(value) => f64::from(*value),
        _ => return None,
    };
    value.is_finite().then_some(value)
}

fn integer(document: &Document, object: &Object) -> Option<i64> {
    match document.dereference(object).ok()?.1 {
        Object::Integer(value) => Some(*value),
        _ => None,
    }
}

fn matrix(document: &Document, values: &[Object]) -> Option<[f64; 6]> {
    if values.len() != 6 {
        return None;
    }
    let mut matrix = [0.0; 6];
    for (slot, value) in matrix.iter_mut().zip(values) {
        *slot = number(document, value)?;
    }
    Some(matrix)
}

fn array<'a>(document: &'a Document, object: &'a Object) -> Option<&'a [Object]> {
    document
        .dereference(object)
        .ok()?
        .1
        .as_array()
        .ok()
        .map(Vec::as_slice)
}

fn dictionary<'a>(document: &'a Document, object: &'a Object) -> Option<&'a Dictionary> {
    document.dereference(object).ok()?.1.as_dict().ok()
}

/// The page's resource dictionaries in lookup order: its own, then those it
/// inherits.
fn page_resources(document: &Document, page_id: ObjectId) -> Vec<&Dictionary> {
    let Ok((own, inherited)) = document.get_page_resources(page_id) else {
        return Vec::new();
    };
    own.into_iter()
        .chain(
            inherited
                .into_iter()
                .filter_map(|id| document.get_dictionary(id).ok()),
        )
        .collect()
}

/// The named XObject from the first resource dictionary that binds it, with
/// its object id.
fn xobject<'a>(
    document: &'a Document,
    resources: &[&'a Dictionary],
    name: &[u8],
) -> Option<(ObjectId, &'a Stream)> {
    for resources in resources {
        let Some(xobjects) = resources
            .get(b"XObject")
            .ok()
            .and_then(|xobjects| dictionary(document, xobjects))
        else {
            continue;
        };
        let Ok(Object::Reference(id)) = xobjects.get(name) else {
            continue;
        };
        return document
            .get_object(*id)
            .and_then(Object::as_stream)
            .ok()
            .map(|stream| (*id, stream));
    }
    None
}

/// Whether a resource dictionary binds an image XObject, directly or in the
/// resources of a form it binds.
fn binds_image(
    document: &Document,
    resources: &Dictionary,
    depth: usize,
    seen: &mut HashSet<ObjectId>,
) -> bool {
    let Some(xobjects) = resources
        .get(b"XObject")
        .ok()
        .and_then(|xobjects| dictionary(document, xobjects))
    else {
        return false;
    };
    for (_, entry) in xobjects.iter() {
        let Object::Reference(id) = entry else {
            continue;
        };
        if !seen.insert(*id) {
            continue;
        }
        let Ok(stream) = document.get_object(*id).and_then(Object::as_stream) else {
            continue;
        };
        match stream.dict.get(b"Subtype").and_then(Object::as_name) {
            Ok(b"Image") => return true,
            Ok(b"Form") if depth < MAX_FORM_DEPTH => {
                let nested = stream
                    .dict
                    .get(b"Resources")
                    .ok()
                    .and_then(|resources| dictionary(document, resources));
                if nested.is_some_and(|nested| binds_image(document, nested, depth + 1, seen)) {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// The page's visible box in default user space: the crop box within the
/// media box, as `[left, bottom, right, top]`.
fn page_box(document: &Document, page_id: ObjectId) -> Option<[f64; 4]> {
    let media = inherited(document, page_id, b"MediaBox")
        .and_then(|value| rectangle(document, value))
        .unwrap_or([0.0, 0.0, 612.0, 792.0]);
    let visible = match inherited(document, page_id, b"CropBox")
        .and_then(|value| rectangle(document, value))
    {
        Some(crop) => [
            media[0].max(crop[0]),
            media[1].max(crop[1]),
            media[2].min(crop[2]),
            media[3].min(crop[3]),
        ],
        None => media,
    };
    (visible[2] > visible[0] && visible[3] > visible[1]).then_some(visible)
}

fn inherited<'a>(document: &'a Document, page_id: ObjectId, key: &[u8]) -> Option<&'a Object> {
    let mut node = document.get_dictionary(page_id).ok()?;
    for _ in 0..MAX_PAGE_TREE_DEPTH {
        if let Ok(value) = node.get(key) {
            return Some(value);
        }
        let parent = node.get(b"Parent").ok()?.as_reference().ok()?;
        node = document.get_dictionary(parent).ok()?;
    }
    None
}

fn rectangle(document: &Document, value: &Object) -> Option<[f64; 4]> {
    let values = array(document, value)?;
    if values.len() != 4 {
        return None;
    }
    let mut corners = [0.0; 4];
    for (slot, value) in corners.iter_mut().zip(values) {
        *slot = number(document, value)?;
    }
    Some([
        corners[0].min(corners[2]),
        corners[1].min(corners[3]),
        corners[0].max(corners[2]),
        corners[1].max(corners[3]),
    ])
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A one-page PDF drawing `page` as its content, with Helvetica as
    /// `/F1`, a 64-pixel gray image as `/Im1`, and a form holding `form` as
    /// `/Fm1`.
    pub(crate) fn scan_pdf(page: &str, form: &str) -> Vec<u8> {
        let pixels = vec![200u8; 64 * 64];
        let objects: Vec<Vec<u8>> = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
              /Resources << /Font << /F1 4 0 R >> /XObject << /Im1 6 0 R /Fm1 7 0 R >> >> \
              /Contents 5 0 R >>"
                .to_vec(),
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
                .to_vec(),
            stream("", page.as_bytes()),
            stream(
                "/Type /XObject /Subtype /Image /Width 64 /Height 64 \
                 /ColorSpace /DeviceGray /BitsPerComponent 8",
                &pixels,
            ),
            stream(
                "/Type /XObject /Subtype /Form /BBox [0 0 612 792] \
                 /Resources << /Font << /F1 4 0 R >> >>",
                form.as_bytes(),
            ),
        ];
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
            pdf.extend_from_slice(object);
            pdf.extend_from_slice(b"\nendobj\n");
        }
        let xref = pdf.len();
        pdf.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
        );
        for offset in offsets {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        pdf
    }

    fn stream(dictionary: &str, content: &[u8]) -> Vec<u8> {
        let mut object =
            format!("<< {dictionary} /Length {} >>\nstream\n", content.len()).into_bytes();
        object.extend_from_slice(content);
        object.extend_from_slice(b"\nendstream");
        object
    }

    /// Lines of a scanned return, shown in render mode `mode`.
    pub(crate) fn text_layer(mode: u8) -> String {
        let lines = [
            "Form 1040 Individual Income Tax Return",
            "Wages, salaries, tips 85000",
            "Taxable interest 1250",
            "Total income 86250",
        ];
        let shown: String = lines
            .iter()
            .enumerate()
            .map(|(index, line)| format!("1 0 0 1 72 {} Tm ({line}) Tj\n", 700 - 20 * index))
            .collect();
        format!("BT {mode} Tr /F1 11 Tf\n{shown}ET")
    }

    pub(crate) const SCAN: &str = "q 612 0 0 792 0 0 cm /Im1 Do Q";
    pub(crate) const STAMP: &str =
        "BT /F1 9 Tf 1 0 0 1 72 770 Tm (CONFIDENTIAL - CLIENT COPY) Tj ET \
         BT /F1 9 Tf 1 0 0 1 480 20 Tm (BATES-000123) Tj ET";

    /// The pages the layer check reports.
    fn layer_pages(pdf: &[u8], skip: &HashSet<u32>, only: Option<&HashSet<u32>>) -> Vec<u32> {
        scan(pdf, skip, None, only).hidden_layer
    }

    fn flagged(page: &str, form: &str) -> bool {
        layer_pages(&scan_pdf(page, form), &HashSet::new(), None) == [1]
    }

    /// Whether the repeat check reports the page.
    fn repeated(page: &str, form: &str) -> bool {
        let pdf = scan_pdf(page, form);
        let found = scan(&pdf, &HashSet::from([1]), Some(&HashSet::new()), None);
        assert!(found.hidden_layer.is_empty());
        found.painted_twice == [1]
    }

    #[test]
    fn scans_with_an_invisible_layer_and_visible_chrome_are_found() {
        // The layer in a form, as ocrmypdf writes it, or in the page.
        assert!(flagged(
            &format!("{SCAN} q /Fm1 Do Q {STAMP}"),
            &text_layer(3)
        ));
        assert!(flagged(&format!("{SCAN} {} {STAMP}", text_layer(3)), ""));
        // Clip-only text paints nothing either.
        assert!(flagged(&format!("{SCAN} {} {STAMP}", text_layer(7)), ""));
        // The mode is inherited by a form and restored with the state.
        assert!(flagged(
            &format!("{SCAN} q BT 3 Tr ET /Fm1 Do Q {STAMP}"),
            &text_layer(3).replace("3 Tr", "")
        ));
        assert!(!flagged(
            &format!(
                "{SCAN} q BT 3 Tr ET Q {} {STAMP}",
                text_layer(0).replace("0 Tr", "")
            ),
            ""
        ));
    }

    #[test]
    fn pages_that_show_their_text_are_not_found() {
        // Visible text over a full-page image, as a letterhead or form.
        assert!(!flagged(&format!("{SCAN} {}", text_layer(0)), ""));
        // Invisible text without an image covering the page.
        assert!(!flagged(&text_layer(3), ""));
        assert!(!flagged(
            &format!("q 100 0 0 50 0 0 cm /Im1 Do Q {}", text_layer(3)),
            ""
        ));
        // More visible text than invisible.
        let visible = format!("{} {}", text_layer(0), text_layer(0));
        assert!(!flagged(&format!("{SCAN} {} {visible}", text_layer(3)), ""));
        // A page skipped or outside the pages asked for is not scanned.
        let pdf = scan_pdf(&format!("{SCAN} {} {STAMP}", text_layer(3)), "");
        assert!(layer_pages(&pdf, &HashSet::from([1]), None).is_empty());
        assert!(layer_pages(&pdf, &HashSet::new(), Some(&HashSet::from([2]))).is_empty());
        assert!(layer_pages(b"not a pdf", &HashSet::new(), None).is_empty());
    }

    #[test]
    fn clip_only_text_painted_through_is_visible() {
        // A heading filled with an image or a gradient: the image or the
        // shading is painted through the text's clip.
        let heading = text_layer(7);
        assert!(!flagged(
            &format!("{SCAN} q {heading} {SCAN} Q {STAMP}"),
            ""
        ));
        assert!(!flagged(
            &format!("{SCAN} q {heading} /Sh1 sh Q {STAMP}"),
            ""
        ));
        // Painted before the text, or after its level is restored, the
        // image shows through nothing.
        assert!(flagged(&format!("{SCAN} q {heading} Q {SCAN} {STAMP}"), ""));
        // A form paints through the clip it inherits.
        assert!(!flagged(
            &format!("{SCAN} q {heading} /Fm1 Do Q {STAMP}"),
            "/Sh1 sh"
        ));
    }

    #[test]
    fn text_painted_twice_is_found() {
        let line =
            |x: f64| format!("BT /F1 11 Tf 1 0 0 1 {x} 700 Tm (Total amount due: $1,234.56) Tj ET");
        // The same run again at the same place, or a fraction of a point
        // off, as for emphasis.
        assert!(repeated(&format!("{} {}", line(72.0), line(72.0)), ""));
        assert!(repeated(&format!("{} {}", line(72.0), line(72.3)), ""));
        // Glyph by glyph, each placed twice.
        let glyphs: String = [("T", 72.0), ("o", 79.33), ("t", 86.0)]
            .iter()
            .map(|(glyph, x)| format!("BT /F1 12 Tf 1 0 0 1 {x} 700 Tm ({glyph}) Tj ET "))
            .collect();
        assert!(repeated(&format!("{glyphs}{glyphs}"), ""));
        // In another font object, as a producer's per-run fonts are.
        assert!(repeated(
            &format!("{} {}", line(72.0), line(72.0).replace("/F1", "/F2")),
            ""
        ));
        // Within one text object, placed by Td, and in a form drawn twice.
        assert!(repeated(
            "BT /F1 9 Tf 40 698 Td (84.19) Tj 0 0 Td (84.19) Tj ET",
            ""
        ));
        assert!(repeated("/Fm1 Do /Fm1 Do", &line(72.0)));
        // Different places or text, text a run leaves unplaced, and
        // invisible text are not repeats.
        assert!(!repeated(&format!("{} {}", line(72.0), line(90.0)), ""));
        assert!(!repeated(
            &format!("{} {}", line(72.0), line(72.0).replace("1,234", "1,235")),
            ""
        ));
        assert!(!repeated(
            "BT /F1 11 Tf 1 0 0 1 72 700 Tm (A) Tj (A) Tj ET",
            ""
        ));
        assert!(!repeated(
            &format!(
                "{} {}",
                line(72.0),
                line(72.0).replace("/F1 11 Tf", "3 Tr /F1 11 Tf")
            ),
            ""
        ));
        // Adjacent narrow glyphs are a tenth of their size apart or more.
        assert!(!repeated(
            "BT /F1 12 Tf 1 0 0 1 72 700 Tm (l) Tj 1 0 0 1 74.66 700 Tm (l) Tj ET",
            ""
        ));
        // A page checked only for the layer is not checked for repeats.
        let pdf = scan_pdf(&format!("{} {}", line(72.0), line(72.0)), "");
        assert!(scan(&pdf, &HashSet::new(), None, None)
            .painted_twice
            .is_empty());
        assert!(scan(&pdf, &HashSet::new(), Some(&HashSet::from([1])), None)
            .painted_twice
            .is_empty());
    }

    #[test]
    fn scanning_is_bounded() {
        // A form that draws itself stops at the first repeat.
        let pdf = scan_pdf(
            &format!("{SCAN} /Fm1 Do {STAMP}"),
            &format!("{} /Fm1 Do", text_layer(3)),
        );
        assert_eq!(layer_pages(&pdf, &HashSet::new(), None), [1]);
        // Unbalanced saves and restores keep the state they can.
        let deep = "q ".repeat(MAX_SAVED_STATES + 10);
        let restores = "Q ".repeat(MAX_SAVED_STATES + 20);
        assert!(flagged(
            &format!("{SCAN} {deep}{restores}{} {STAMP}", text_layer(3)),
            ""
        ));
        let mut budget = Budget::new(MAX_CONTENT_BYTES, MAX_OPERATIONS);
        budget.bytes = MAX_CONTENT_BYTES;
        assert!(budget.take_bytes(1).is_err());
        budget.operations = MAX_OPERATIONS;
        assert!(budget.take_operations(1).is_err());
    }
}
