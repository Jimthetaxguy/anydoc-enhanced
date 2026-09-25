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
//!
//! **Word gaps judged against the wrong space.** On the same pages, text
//! shown in a subset font whose differences name the space at another code
//! than 32 is checked for gaps pdf-inspector judges otherwise than its open
//! fix would (open upstream #532; see `word_gaps`).

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};

use lopdf::{content::Content, Dictionary, Document, Object, ObjectId, Stream};

use crate::glyph_words::{Glyph, GlyphFonts, GlyphWords};
use crate::word_gaps::{leading_travel, shows_glyphs, Candidate, GapFont, GapFonts, Shown};

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
/// Repeated runs recorded per page.
const MAX_REPEATS_PER_PAGE: usize = 16;
/// Placed run starts kept per document, for the table check.
const MAX_PLACED_RUNS: usize = 400_000;
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
    /// The font, when pdf-inspector and its fix take different word-gap
    /// thresholds from it and the page is checked for them.
    gaps: Option<GapFont>,
    /// The font, when its glyph codes can be read and the page is checked
    /// for words shown glyph by glyph.
    glyph_font: Option<usize>,
    size: f64,
    char_spacing: f64,
    word_spacing: f64,
    /// Horizontal scaling, as a share.
    horizontal_scale: f64,
    leading: f64,
    rise: f64,
}

impl State {
    const START: State = State {
        ctm: IDENTITY,
        render_mode: 0,
        font: false,
        gaps: None,
        glyph_font: None,
        size: 0.0,
        char_spacing: 0.0,
        word_spacing: 0.0,
        horizontal_scale: 1.0,
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
    /// Where repeated runs start, with their size.
    repeats: Vec<Repeat>,
    /// Where every placed run starts, whatever its bytes, and whether it
    /// is plain.
    placed: Vec<([f64; 2], bool)>,
}

/// Where a visible run placed on a page starts, measured from the page's
/// visible box, and whether it is plain: one string, or strings no word
/// gap apart, which pdf-inspector reads as one item as the producer wrote
/// it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Placed {
    pub(crate) page: u32,
    pub(crate) at: [f64; 2],
    pub(crate) plain: bool,
}

/// Least pen travel between two strings of a `TJ` array, in thousandths of
/// the font size, that pdf-inspector may read as a word gap.
const WORD_GAP_TRAVEL: f64 = 80.0;

/// Whether a text-showing operand is plain (see [`Placed`]).
fn plain(text: Option<&Object>) -> bool {
    let Some(Object::Array(elements)) = text else {
        return true;
    };
    let (mut shown, mut travel) = (false, 0.0);
    for element in elements {
        match element {
            Object::Integer(offset) => travel -= *offset as f64,
            Object::Real(offset) => travel -= f64::from(*offset),
            Object::String(bytes, _) if !bytes.is_empty() => {
                if shown && travel >= WORD_GAP_TRAVEL {
                    return false;
                }
                (shown, travel) = (true, 0.0);
            }
            _ => {}
        }
    }
    true
}

/// Where a repeated run starts, in user space, and its size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Repeat {
    pub(crate) at: [f64; 2],
    pub(crate) size: f64,
}

impl Runs {
    fn note(&mut self, text: &[u8], at: [f64; 2], size: f64) {
        if self.repeats.len() >= MAX_REPEATS_PER_PAGE || self.noted >= MAX_RUNS_PER_PAGE {
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
                    self.repeats.push(Repeat { at, size });
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

/// A string whose spacing pdf-inspector and its fix read differently,
/// waiting for the next run of its text object to show whether the spacing
/// after it is taken back (see `word_gaps`): where it was shown, in device
/// space, the pen after it when where it started is known, and whether the
/// text has been placed anew since.
struct Pending {
    candidate: Candidate,
    origin: [f64; 2],
    /// One unscaled text space unit along its baseline.
    unit: [f64; 2],
    pen: Option<[f64; 2]>,
    moved: bool,
}

impl Pending {
    /// Whether `text`, the next run, shown at `text_matrix`, starts where
    /// the spacing is taken back. A run placed anew on the same baseline,
    /// with the pen unknown, may.
    fn taken_back(&self, state: State, text_matrix: [f64; 6], text: &Object) -> bool {
        let travel = f64::from(leading_travel(text, state.size as f32));
        if !self.moved {
            return self.candidate.taken_back(-travel as f32);
        }
        let matrix = multiply(text_matrix, state.ctm);
        let next = [
            matrix[4] + travel * state.horizontal_scale * matrix[0],
            matrix[5] + travel * state.horizontal_scale * matrix[1],
        ];
        let scale = self.unit[0] * self.unit[0] + self.unit[1] * self.unit[1];
        // A degenerate or unreadable baseline decides nothing.
        if scale.is_nan() || scale <= 0.0 {
            return false;
        }
        let from = self.pen.unwrap_or(self.origin);
        let (dx, dy) = (next[0] - from[0], next[1] - from[1]);
        let across = (dx * self.unit[1] - dy * self.unit[0]) / scale;
        if !self.candidate.on_line(across) {
            return false;
        }
        match self.pen {
            Some(_) => {
                let along = (dx * self.unit[0] + dy * self.unit[1]) / scale;
                self.candidate.taken_back(-along as f32)
            }
            None => true,
        }
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
    /// Whether text pdf-inspector reads has a gap it judges otherwise than
    /// its fix would, and the fonts read for that so far in the document.
    gaps_misread: bool,
    gap_fonts: GapFonts,
    /// The words shown glyph by glyph, when the page is read for them, and
    /// the fonts read for that so far in the document.
    glyph_words: Option<GlyphWords>,
    glyph_fonts: GlyphFonts,
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

    /// Note a string for the words shown glyph by glyph: one glyph placed
    /// where the text matrix says, as pdf-inspector reads it, or anything
    /// else, which ends the word being shown.
    fn note_glyph(&mut self, state: State, text_matrix: [f64; 6], bytes: &[u8], placed: bool) {
        let Some(words) = self.glyph_words.as_mut() else {
            return;
        };
        let matrix = multiply(text_matrix, state.ctm);
        let at = [
            matrix[2] * state.rise + matrix[4],
            matrix[3] * state.rise + matrix[5],
        ];
        let em = [matrix[0] * state.size, matrix[1] * state.size];
        let known = placed && at.iter().chain(&em).all(|value| value.is_finite());
        let font = state
            .glyph_font
            .filter(|_| state.font && state.render_mode != 3);
        match (font, known) {
            (Some(font), true) => match self.glyph_fonts.one_glyph(font, bytes) {
                Some(reading) => words.glyph(Glyph {
                    font,
                    reading,
                    at,
                    em,
                }),
                None => words.interrupt(Some(at), self.glyph_fonts.first_glyph(font, bytes)),
            },
            (None, true) => words.interrupt(Some(at), None),
            (_, false) => words.interrupt(None, None),
        }
    }

    /// Note a visible run whose start the text matrix says.
    fn note_run(&mut self, state: State, text_matrix: [f64; 6], bytes: &[u8], plain: bool) {
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
            if runs.placed.len() < MAX_RUNS_PER_PAGE {
                runs.placed.push((at, plain));
            }
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
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Findings {
    /// Pages whose text is mostly an invisible layer over images covering
    /// the page.
    pub(crate) hidden_layer: Vec<u32>,
    /// Pages that paint a visible run again over itself.
    pub(crate) painted_twice: Vec<u32>,
    /// Where those runs start again, by page, measured from the page's
    /// visible box, as pdf-inspector places its text.
    pub(crate) repeats: Vec<(u32, Repeat)>,
    /// Pages with a gap between glyphs that pdf-inspector judges against
    /// the wrong space width.
    pub(crate) gaps_misread: Vec<u32>,
    /// Where the visible runs placed on the pages read for repeats start,
    /// up to `MAX_PLACED_RUNS`.
    pub(crate) placed: Vec<Placed>,
    /// Words shown glyph by glyph in fonts that paint their spaces, on the
    /// pages read for repeats, with the page and how often, up to
    /// `MAX_GLYPH_WORDS`.
    pub(crate) glyph_words: Vec<(u32, String, u32)>,
}

/// Words shown glyph by glyph kept across a document.
const MAX_GLYPH_WORDS: usize = 65_536;

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
    let mut gap_fonts = GapFonts::default();
    let mut glyph_fonts = GlyphFonts::default();
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
            gap_fonts: &mut gap_fonts,
            glyph_fonts: &mut glyph_fonts,
        };
        match scan_page(&document, page_id, check_layer, check_twice, budgets) {
            Ok(page) => {
                if page.hidden_layer {
                    found.hidden_layer.push(number);
                }
                if page.gaps_misread {
                    found.gaps_misread.push(number);
                }
                let room = MAX_GLYPH_WORDS.saturating_sub(found.glyph_words.len());
                found.glyph_words.extend(
                    page.glyph_words
                        .into_iter()
                        .take(room)
                        .map(|(text, count)| (number, text, count)),
                );
                let room = MAX_PLACED_RUNS.saturating_sub(found.placed.len());
                found.placed.extend(
                    page.placed
                        .into_iter()
                        .take(room)
                        .map(|(at, plain)| Placed {
                            page: number,
                            at,
                            plain,
                        }),
                );
                if !page.repeats.is_empty() {
                    found.painted_twice.push(number);
                    found
                        .repeats
                        .extend(page.repeats.into_iter().map(|repeat| (number, repeat)));
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
/// read for it, the repeat check's otherwise; and the fonts read so far.
struct Budgets<'a> {
    layer: &'a mut Budget,
    repeat: &'a mut Budget,
    gap_fonts: &'a mut GapFonts,
    glyph_fonts: &'a mut GlyphFonts,
}

/// What the scan found on one page.
#[derive(Default)]
struct PageFindings {
    hidden_layer: bool,
    gaps_misread: bool,
    /// Runs painted again over themselves, measured from the visible box.
    repeats: Vec<Repeat>,
    /// Where placed runs start, measured from the visible box, and whether
    /// they are plain.
    placed: Vec<([f64; 2], bool)>,
    /// Words shown glyph by glyph in fonts that paint their spaces.
    glyph_words: Vec<(String, u32)>,
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
    let Budgets {
        layer,
        repeat,
        gap_fonts,
        glyph_fonts,
    } = budgets;
    let budget = if check_layer { layer } else { repeat };
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
        gap_fonts: std::mem::take(gap_fonts),
        glyph_words: check_twice.then(GlyphWords::default),
        glyph_fonts: std::mem::take(glyph_fonts),
        ..PageText::default()
    };
    page.save();
    let mut forms = Vec::new();
    let executed = execute(
        document,
        &content,
        &resources,
        State::START,
        page_box,
        &mut page,
        &mut forms,
        budget,
    );
    *gap_fonts = std::mem::take(&mut page.gap_fonts);
    *glyph_fonts = std::mem::take(&mut page.glyph_fonts);
    executed?;
    page.ended();
    let placed = page
        .runs
        .as_ref()
        .map(|runs| {
            runs.placed
                .iter()
                .map(|(at, plain)| ([at[0] - page_box[0], at[1] - page_box[1]], *plain))
                .collect()
        })
        .unwrap_or_default();
    let glyph_words = page
        .glyph_words
        .take()
        .map(GlyphWords::finish)
        .unwrap_or_default();
    Ok(PageFindings {
        hidden_layer: check_layer && page.is_hidden_layer(page_box),
        gaps_misread: page.gaps_misread,
        placed,
        glyph_words,
        repeats: page
            .runs
            .map(|runs| {
                runs.repeats
                    .into_iter()
                    .map(|repeat| Repeat {
                        at: [repeat.at[0] - page_box[0], repeat.at[1] - page_box[1]],
                        size: repeat.size,
                    })
                    .collect()
            })
            .unwrap_or_default(),
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
    // Whether a text object is open, the only place pdf-inspector reads
    // text, and a string there waiting on the next run.
    let mut in_text = false;
    let mut pending: Option<Pending> = None;
    for operation in &content.operations {
        let operands = &operation.operands;
        let operator = operation.operator.as_str();
        // `'` and `"` move to the next line before they show text.
        if matches!(operator, "T*" | "'" | "\"") {
            line_matrix = multiply([1.0, 0.0, 0.0, 1.0, 0.0, -state.leading], line_matrix);
            text_matrix = line_matrix;
            placed = true;
        }
        if matches!(operator, "T*" | "'" | "\"" | "Td" | "TD" | "Tm") {
            if let Some(pending) = pending.as_mut() {
                pending.moved = true;
            }
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
                    // Word gaps are checked with the repeats, on the pages
                    // read for them.
                    state.gaps = name
                        .as_name()
                        .ok()
                        .filter(|_| page.runs.is_some())
                        .and_then(|name| font(document, resources, name))
                        .and_then(|font| page.gap_fonts.font(document, font));
                    state.glyph_font = name
                        .as_name()
                        .ok()
                        .filter(|_| page.glyph_words.is_some())
                        .and_then(|name| font(document, resources, name))
                        .and_then(|font| page.glyph_fonts.font(document, font));
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
            "Tc" => {
                if let Some(spacing) = operands.first().and_then(|value| number(document, value)) {
                    state.char_spacing = spacing;
                }
            }
            "Tw" => {
                if let Some(spacing) = operands.first().and_then(|value| number(document, value)) {
                    state.word_spacing = spacing;
                }
            }
            "Tz" => {
                if let Some(scale) = operands.first().and_then(|value| number(document, value)) {
                    state.horizontal_scale = scale / 100.0;
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
                in_text = true;
                pending = None;
            }
            "ET" => {
                page.text_object_ended();
                placed = false;
                in_text = false;
                pending = None;
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
                // `"` sets the word and character spacing it shows with; out
                // of a text object pdf-inspector ignores it, spacing and all.
                if let [word, character, _] = operands.as_slice() {
                    if operator == "\"" && in_text {
                        if let (Some(word), Some(character)) =
                            (number(document, word), number(document, character))
                        {
                            state.word_spacing = word;
                            state.char_spacing = character;
                        }
                    }
                }
                let bytes = shown_bytes(text);
                page.show(state, &bytes);
                page.note_glyph(state, text_matrix, &bytes, placed && in_text);
                if let Some(text) = text.filter(|_| in_text && !page.gaps_misread) {
                    // A run that shows glyphs decides for the string before.
                    if shows_glyphs(text) {
                        if let Some(waiting) = pending.take() {
                            page.gaps_misread = waiting.taken_back(state, text_matrix, text);
                        }
                    }
                    // pdf-inspector reads all but mode 3 text.
                    if let Some(font) = state.gaps.filter(|_| state.font && state.render_mode != 3)
                    {
                        let matrix = multiply(text_matrix, state.ctm);
                        let shown = Shown {
                            size: state.size as f32,
                            char_spacing: state.char_spacing as f32,
                            word_spacing: state.word_spacing as f32,
                            horizontal: matrix[0].abs() >= matrix[1].abs(),
                        };
                        let judged = page.gap_fonts.judge(font, text, shown);
                        page.gaps_misread |= judged.misjudged;
                        if let Some(candidate) = judged.candidate {
                            let unit = [
                                matrix[0] * state.horizontal_scale,
                                matrix[1] * state.horizontal_scale,
                            ];
                            let origin = [matrix[4], matrix[5]];
                            let advance = f64::from(judged.advance);
                            pending = Some(Pending {
                                candidate,
                                origin,
                                unit,
                                pen: placed.then(|| {
                                    [origin[0] + advance * unit[0], origin[1] + advance * unit[1]]
                                }),
                                moved: false,
                            });
                        }
                    }
                }
                // After a run, the next starts where it ended, which the
                // glyph widths decide.
                if placed {
                    page.note_run(state, text_matrix, &bytes, plain(text));
                }
                placed = false;
            }
            "BI" => page.draw_image(state.ctm, page_box),
            "sh" => page.painted(),
            "Do" => {
                // What a form or an image paints continues no string.
                pending = None;
                if let Some(words) = page.glyph_words.as_mut() {
                    words.interrupt(None, None);
                }
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

/// The named font from the first resource dictionary that binds it.
fn font<'a>(
    document: &'a Document,
    resources: &[&'a Dictionary],
    name: &[u8],
) -> Option<&'a Dictionary> {
    for resources in resources {
        let Some(fonts) = resources
            .get(b"Font")
            .ok()
            .and_then(|fonts| dictionary(document, fonts))
        else {
            continue;
        };
        let Ok(entry) = fonts.get(name) else {
            continue;
        };
        return dictionary(document, entry);
    }
    None
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
        scan_pdf_in(
            page,
            form,
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>",
        )
    }

    /// A one-page document whose `/F1` is the given font.
    fn scan_pdf_in(page: &str, form: &str, font: &str) -> Vec<u8> {
        let pixels = vec![200u8; 64 * 64];
        let objects: Vec<Vec<u8>> = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
              /Resources << /Font << /F1 4 0 R >> /XObject << /Im1 6 0 R /Fm1 7 0 R >> >> \
              /Contents 5 0 R >>"
                .to_vec(),
            font.as_bytes().to_vec(),
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
    fn word_gaps_pdf_inspector_misjudges_are_found() {
        let gaps = |font: &str, content: &str| {
            let pdf = scan_pdf_in(content, "", font);
            scan(&pdf, &HashSet::new(), Some(&HashSet::new()), None).gaps_misread
        };
        let subset = |differences: &str, width_32: u32, width_26: u32| {
            let widths: Vec<String> = (1..=120)
                .map(|code| match code {
                    26 => width_26.to_string(),
                    32 => width_32.to_string(),
                    _ => "556".to_string(),
                })
                .collect();
            format!(
                "<< /Type /Font /Subtype /Type1 /BaseFont /ABCDEF+SubsetSans /FirstChar 1 \
                 /LastChar 120 /Widths [{}] /Encoding << /Type /Encoding \
                 /BaseEncoding /WinAnsiEncoding /Differences [{differences}] >> >>",
                widths.join(" ")
            )
        };
        let kerned = "BT /F1 10 Tf 60 700 Td [(8) -106 (5,000) -108 (.00)] TJ ET";
        let columns = "BT /F1 10 Tf 60 700 Td [(13,100.00) -300 (1,020.00)] TJ ET";
        // The space named at code 26 and code 32 unused: pdf-inspector
        // takes 250 units, and kerning between its threshold and the fix's
        // splits the amount.
        assert_eq!(gaps(&subset("26 /space", 0, 288), kerned), vec![1]);
        // Code 32 holding a wide glyph: a word gap under it is lost.
        assert_eq!(gaps(&subset("26 /space 32 /M", 833, 288), columns), vec![1]);
        // Gaps both thresholds judge alike.
        assert!(gaps(&subset("26 /space 32 /M", 833, 288), kerned).is_empty());
        assert!(gaps(&subset("26 /space", 0, 288), columns).is_empty());
        // The space where pdf-inspector reads it, as wide as its fallback,
        // or at two codes of different widths, which the fix leaves alone.
        assert!(gaps(&subset("26 /space", 288, 288), kerned).is_empty());
        assert!(gaps(&subset("26 /space", 0, 250), kerned).is_empty());
        assert!(gaps(&subset("26 /space 32 /space", 250, 288), kerned).is_empty());
        assert!(gaps(
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>",
            kerned
        )
        .is_empty());
        // Text pdf-inspector does not read.
        let hidden = "BT /F1 10 Tf 3 Tr 60 700 Td [(8) -106 (5,000) -108 (.00)] TJ ET";
        assert!(gaps(&subset("26 /space", 0, 288), hidden).is_empty());
        // Character spacing set with `"` inside a short string counts once
        // the next run takes it back: by the offset before its first glyph,
        // or by starting there on the same line (the string, 13.22 units
        // wide, ends 1.05 past where the run starts).
        let spaced = |next: &str| {
            let content = format!("BT /F1 10 Tf 12 TL 60 700 Td 0 1.05 (dt) \" {next} ET");
            gaps(&subset("26 /space", 0, 288), &content)
        };
        assert_eq!(spaced("[105 (oday)] TJ"), vec![1]);
        assert_eq!(spaced("12.17 0 Td (oday) Tj"), vec![1]);
        for next in ["(oday) Tj", "20 0 Td (oday) Tj", "0 -12 Td (oday) Tj", ""] {
            assert!(spaced(next).is_empty(), "{next}");
        }
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
