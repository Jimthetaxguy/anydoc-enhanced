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
//! reports a page on which a run showing the same bytes, or the same text
//! as its font reads it, starts again near an earlier one, in whatever
//! font: within a tenth of its size, or, for a run of two glyphs or more,
//! which cannot start again so near itself, a third, as a shadow does. A
//! run that starts where a longer or shorter one did, the one beginning the
//! other, repeats it too, as a second paint split in two strings does; so
//! does a string a `TJ` array shows again after stepping back, as an
//! overstrike does.
//!
//! **Word gaps judged against the wrong space.** On the same pages, text
//! shown in a subset font whose differences name the space at another code
//! than 32 is checked for gaps pdf-inspector judges otherwise than its open
//! fix would (open upstream #532; see `word_gaps`).
//!
//! **Text drawn through a form.** A form XObject without `/Resources` of
//! its own draws with its invoker's, as renderers read the specification,
//! but pdf-inspector gives it none (open upstream #312): a form it draws is
//! never read. pdf-inspector finds a page's forms among the page's own
//! resources alone, not those it inherits, and a form's fonts and forms
//! among the form's own alone, where pdfium looks a category they lack up
//! in the page's. It also starts every form with no font, so text a form
//! shows before setting a font of its own, in the font it was drawn with,
//! is read byte by byte, as Windows-1252, UTF-16, or UTF-8 has it. A page
//! is reported when it shows text in a form pdf-inspector does not reach,
//! or text read byte by byte that its font reads otherwise.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};

use lopdf::{content::Content, Dictionary, Document, Object, ObjectId, Stream};

use crate::glyph_words::{Glyph, GlyphFonts, GlyphWords, KeptWords, ShownWord};
use crate::optional_content::Layers;
use crate::repeated_lines::EdgeRun;
use crate::vertical_text::Reading;
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
/// Bytes of text a reader does not see, or sees otherwise, noted a page for
/// each check, at most.
const MAX_HIDDEN_TEXT: usize = 64 << 10;
/// Bytes of such text kept across a document, to look for in the Markdown.
/// A page whose text does not fit, like one whose text overran its own
/// limit, is reported without it.
const MAX_NOTED_TEXT: usize = 4 << 20;
/// Repeated runs recorded per page.
const MAX_REPEATS_PER_PAGE: usize = 16;
/// Placed run starts kept per document, for the table check.
const MAX_PLACED_RUNS: usize = 400_000;
/// How near a run must start again to repeat one, as a share of its size:
/// a run of one glyph, and of more; and at least.
const REPEAT_SHARE: f64 = 0.1;
const REPEAT_SHARE_GLYPHS: f64 = 0.35;
const MIN_REPEAT_DISTANCE: f64 = 0.3;
/// The side, in points, of the squares run starts are filed in for runs
/// that begin one another; runs whose reach spans more squares than
/// `MAX_REPEAT_SQUARES` on a side are matched whole only.
const REPEAT_SQUARE: f64 = 4.0;
const MAX_REPEAT_SQUARES: i64 = 16;
/// The least text, in bytes, that a run beginning another must show.
const MIN_BEGINNING_BYTES: usize = 4;
/// Least step back between two strings of a `TJ` array, in thousandths of
/// the font size, that paints the second over the first: past kerning, and
/// no more than the narrowest glyph's width.
const REWIND_TRAVEL: f64 = 180.0;

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
    /// The render mode as pdf-inspector reads text by it: it takes each
    /// text object to start in mode 0.
    read_mode: i64,
    /// Whether `Tf` has set a font; text shown before shows nothing.
    font: bool,
    /// Whether pdf-inspector reads the text here at all: not in a form it
    /// does not find where the page or form drawing it names it (see
    /// [`Resources`]).
    reached: bool,
    /// Whether pdf-inspector reads the text here byte by byte, with no
    /// font: in a form, until the form sets a font it finds among the
    /// form's own resources.
    raw: bool,
    /// How the font in force reads its codes, when the page is read for
    /// forms.
    decoded: Option<Decoded>,
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
    /// Whether a layer a reader hides holds what is drawn here, as a form
    /// in such a layer does.
    hidden: bool,
    /// How pdf-inspector reads the font in force, where it finds no map for
    /// it (see `cjk_fonts`), when the page is read for it.
    cjk: Option<crate::cjk_fonts::Unmapped>,
    /// Whether the fill is white, as pdf-inspector judges it: in a form,
    /// it skips text filled white.
    white: bool,
    /// Whether the font in force writes vertically (see `vertical_text`),
    /// when the page is read for it.
    vertical: bool,
}

impl State {
    const START: State = State {
        ctm: IDENTITY,
        render_mode: 0,
        read_mode: 0,
        font: false,
        reached: true,
        raw: false,
        decoded: None,
        gaps: None,
        glyph_font: None,
        size: 0.0,
        char_spacing: 0.0,
        word_spacing: 0.0,
        horizontal_scale: 1.0,
        leading: 0.0,
        rise: 0.0,
        hidden: false,
        cjk: None,
        white: false,
        vertical: false,
    };
}

/// How a font reads its codes.
#[derive(Clone, Copy, Debug)]
enum Decoded {
    /// Through the font's own maps (see `GlyphFonts`), two bytes a code or
    /// one.
    Glyphs { font: usize, two_bytes: bool },
    /// Two bytes a code, in maps the scan does not read.
    TwoBytes,
}

/// Where the visible runs of one page start, by a hash of their bytes and
/// one of their text as the font reads it. The font is left out:
/// pdf-inspector keeps the text of every paint, whichever font object draws
/// it, so a second paint in an identical font object, or another font,
/// repeats the text all the same.
#[derive(Default)]
struct Runs {
    starts: HashMap<u64, Vec<[f64; 2]>>,
    /// What the runs long enough to begin another show, and where each
    /// starts, filed by square.
    shown: Vec<Shows>,
    squares: HashMap<(i64, i64), Vec<Filed>>,
    noted: usize,
    /// Where repeated runs start, with their size.
    repeats: Vec<Repeat>,
    /// Where every placed run starts, whatever its bytes, and whether it
    /// is plain.
    placed: Vec<([f64; 2], bool)>,
}

/// Where a run starts, and where what it shows is kept.
type Filed = ([f64; 2], usize);

/// What a run shows: its bytes, and its text when its font reads it.
#[derive(Clone, Debug)]
struct Shows {
    bytes: Vec<u8>,
    text: Option<String>,
}

impl Shows {
    /// Whether one of two runs shows the beginning of what the other does,
    /// by their text where both fonts read it, else by their bytes.
    fn begins(&self, other: &Shows) -> bool {
        let (one, another): (&[u8], &[u8]) = match (&self.text, &other.text) {
            (Some(one), Some(another)) => (one.as_bytes(), another.as_bytes()),
            _ => (&self.bytes, &other.bytes),
        };
        let (shorter, longer) = if one.len() <= another.len() {
            (one, another)
        } else {
            (another, one)
        };
        shorter.len() >= MIN_BEGINNING_BYTES && longer.starts_with(shorter)
    }

    /// How many glyphs it shows, or its bytes where its font does not say.
    fn glyphs(&self) -> usize {
        self.text
            .as_ref()
            .map_or(self.bytes.len(), |text| text.chars().count())
    }
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

/// Where a repeated run starts, in user space, its size, and its text when
/// its font reads it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Repeat {
    pub(crate) at: [f64; 2],
    pub(crate) size: f64,
    pub(crate) text: Option<String>,
}

impl Runs {
    fn note(&mut self, shows: Shows, at: [f64; 2], size: f64) {
        if self.repeats.len() >= MAX_REPEATS_PER_PAGE || self.noted >= MAX_RUNS_PER_PAGE {
            return;
        }
        let share = if shows.glyphs() >= 2 {
            REPEAT_SHARE_GLYPHS
        } else {
            REPEAT_SHARE
        };
        let near = (share * size).max(MIN_REPEAT_DISTANCE);
        let within =
            |start: &[f64; 2]| (start[0] - at[0]).abs() <= near && (start[1] - at[1]).abs() <= near;
        let key = |tag: u8, shown: &[u8]| {
            let mut hasher = DefaultHasher::new();
            (tag, shown).hash(&mut hasher);
            hasher.finish()
        };
        let mut keys = vec![key(b'b', &shows.bytes)];
        keys.extend(shows.text.as_ref().map(|text| key(b't', text.as_bytes())));
        let mut repeated = false;
        for key in keys {
            match self.starts.entry(key) {
                Entry::Occupied(mut entry) => {
                    repeated |= entry.get().iter().any(within);
                    entry.get_mut().push(at);
                }
                Entry::Vacant(entry) => {
                    entry.insert(vec![at]);
                }
            }
        }
        if repeated {
            self.repeat(at, size, &shows);
            return;
        }
        self.noted += 1;
        // A run beginning one that starts near it repeats it.
        let square = |value: f64| (value / REPEAT_SQUARE).floor() as i64;
        let reach = (near / REPEAT_SQUARE).ceil() as i64;
        if shows.bytes.len() < MIN_BEGINNING_BYTES || reach > MAX_REPEAT_SQUARES {
            return;
        }
        let (column, row) = (square(at[0]), square(at[1]));
        for x in column - reach..=column + reach {
            for y in row - reach..=row + reach {
                let begun = self.squares.get(&(x, y)).is_some_and(|filed| {
                    filed
                        .iter()
                        .any(|(start, other)| within(start) && shows.begins(&self.shown[*other]))
                });
                if begun {
                    self.repeat(at, size, &shows);
                    return;
                }
            }
        }
        self.squares
            .entry((column, row))
            .or_default()
            .push((at, self.shown.len()));
        self.shown.push(shows);
    }

    fn repeat(&mut self, at: [f64; 2], size: f64, shows: &Shows) {
        if self.repeats.len() < MAX_REPEATS_PER_PAGE {
            self.repeats.push(Repeat {
                at,
                size,
                text: shows.text.clone(),
            });
        }
    }
}

/// The string a `TJ` array shows again right after stepping back, as an
/// overstrike paints it over itself.
fn overstruck(text: Option<&Object>) -> Option<&[u8]> {
    let Some(Object::Array(elements)) = text else {
        return None;
    };
    let (mut last, mut travel): (Option<&[u8]>, f64) = (None, 0.0);
    for element in elements {
        match element {
            Object::Integer(offset) => travel += *offset as f64,
            Object::Real(offset) => travel += f64::from(*offset),
            Object::String(bytes, _) if !bytes.is_empty() => {
                if last == Some(bytes.as_slice())
                    && travel >= REWIND_TRAVEL
                    && bytes.iter().any(|&byte| byte != b' ')
                {
                    return Some(bytes);
                }
                (last, travel) = (Some(bytes.as_slice()), 0.0);
            }
            _ => {}
        }
    }
    None
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
    /// the spacing is taken back. With the pen unknown, a run placed anew
    /// decides nothing.
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
            None => false,
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

/// The strings a text-showing operand holds: a string, or an array's.
fn strings(text: Option<&Object>) -> impl Iterator<Item = &[u8]> {
    let (one, many) = match text {
        Some(Object::String(bytes, _)) => (Some(bytes.as_slice()), &[][..]),
        Some(Object::Array(parts)) => (None, parts.as_slice()),
        _ => (None, &[][..]),
    };
    one.into_iter()
        .chain(many.iter().filter_map(|part| match part {
            Object::String(bytes, _) => Some(bytes.as_slice()),
            _ => None,
        }))
}

/// A string as pdf-inspector reads it without a font: UTF-16 after a
/// byte-order mark, or where nulls make over a quarter of an even string of
/// four bytes or more and the UTF-16 reads as text; UTF-8 where the bytes
/// past ASCII form it; otherwise byte by byte, as Windows-1252 has them.
fn read_without_font(bytes: &[u8]) -> String {
    let utf16 = |bytes: &[u8]| {
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    };
    if let [0xFE, 0xFF, rest @ ..] = bytes {
        return utf16(rest);
    }
    let nulls = bytes.iter().filter(|&&byte| byte == 0).count();
    if bytes.len() >= 4 && bytes.len().is_multiple_of(2) && 4 * nulls > bytes.len() {
        let text = utf16(bytes);
        if reads_as_text(&text) {
            return text;
        }
    }
    if bytes.iter().any(|&byte| byte > 0x7F) {
        if let Ok(text) = std::str::from_utf8(bytes) {
            return text.to_string();
        }
    }
    bytes.iter().map(|&byte| windows_1252(byte)).collect()
}

/// A byte as Windows-1252 reads it; the five codes it leaves undefined, and
/// the others outside 0x80 to 0x9F, as Latin-1 does.
fn windows_1252(byte: u8) -> char {
    const HIGH: [char; 32] = [
        '\u{20AC}', '\u{81}', '\u{201A}', '\u{0192}', '\u{201E}', '\u{2026}', '\u{2020}',
        '\u{2021}', '\u{02C6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{8D}',
        '\u{017D}', '\u{8F}', '\u{90}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2022}',
        '\u{2013}', '\u{2014}', '\u{02DC}', '\u{2122}', '\u{0161}', '\u{203A}', '\u{0153}',
        '\u{9D}', '\u{017E}', '\u{0178}',
    ];
    match byte {
        0x80..=0x9F => HIGH[usize::from(byte - 0x80)],
        _ => char::from(byte),
    }
}

/// Whether text read as UTF-16 reads as text, by pdf-inspector's score of
/// it: ten for each common English word, one for each letter (with CJK
/// ideographs and kana), digit, and two for each space, less two for each
/// other character and six for a control or replacement character, less
/// fifteen more for over fifteen letters without a common word.
fn reads_as_text(text: &str) -> bool {
    const COMMON: [&str; 22] = [
        "the", "and", "of", "to", "in", "a", "is", "that", "for", "with", "on", "as", "by", "from",
        "this", "be", "are", "at", "or", "not", "it", "our",
    ];
    let (mut score, mut letters, mut common) = (0i64, 0i64, 0i64);
    let mut word = String::new();
    for character in text.chars().chain([' ']) {
        if character.is_ascii_alphabetic() {
            letters += 1;
            score += 1;
            word.push(character.to_ascii_lowercase());
            continue;
        }
        if COMMON.contains(&word.as_str()) {
            common += 1;
            score += 10;
        }
        word.clear();
        score += match character {
            ' ' => 2,
            '0'..='9' => 1,
            '\u{4E00}'..='\u{9FFF}'
            | '\u{3040}'..='\u{30FF}'
            | '\u{3400}'..='\u{4DBF}'
            | '\u{F900}'..='\u{FAFF}' => {
                letters += 1;
                1
            }
            '\u{FFFD}' => -6,
            character if character.is_control() => -6,
            _ => -2,
        };
    }
    // The space closing the text scores nothing.
    score -= 2;
    if letters > 15 && common == 0 {
        score -= 15;
    }
    score > 0
}

/// Text without the control characters pdf-inspector drops from what it
/// reads, all below the space but tab and line ends.
fn without_controls(text: &str) -> String {
    text.chars()
        .filter(|&character| character >= ' ' || matches!(character, '\t' | '\n' | '\r'))
        .collect()
}

/// What one page's content shows.
#[derive(Default)]
struct PageText {
    hidden_bytes: u64,
    visible_bytes: u64,
    /// The strings shown on the page so far.
    shows: u64,
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
    /// Whether the page is read for text drawn through forms, and whether
    /// such text pdf-inspector misses or misreads.
    forms: bool,
    form_text_unread: bool,
    /// The runs pdf-inspector reads, for the running-header check's gate,
    /// when the page is read for repeats.
    edges: Option<Vec<EdgeRun>>,
    /// The document's layers, and the text pdf-inspector reads in layers a
    /// reader hides, when the page is read for repeats and the document has
    /// layers.
    layers: Option<std::rc::Rc<Layers>>,
    hidden_text: Option<Noted>,
    /// Text a viewer never paints, in render mode 3, that pdf-inspector
    /// reads as shown, when the page is read for repeats. Each string noted
    /// here, and in `hidden_text`, carries the edge run it is read in.
    invisible_text: Option<Noted>,
    /// Text shown in a font pdf-inspector reads without its collection's
    /// map, as the font says it (see `cjk_fonts`), and whether any of it
    /// reads otherwise with no sign, when the page is read for repeats;
    /// and text in a font it may read so, as it would read it.
    cjk_text: Option<Noted>,
    cjk_read: Option<Noted>,
    cjk_misread: bool,
    cjk_evidence: Option<Noted>,
    cjk_fonts: crate::cjk_fonts::CjkFonts,
    /// Strings shown in fonts that write vertically, when the page is read
    /// for repeats, and the bytes of their text kept.
    vertical: Option<Vec<crate::vertical_text::VerticalRun>>,
    vertical_bytes: usize,
}

/// Texts noted on a page, run by run, the bytes they hold, and whether a
/// text was left out for want of room.
#[derive(Default)]
struct Noted {
    notes: Vec<Note>,
    bytes: usize,
    last: Option<Last>,
    overflowed: bool,
}

/// A run of noted text, the first and last of the page's edge runs (see
/// `PageText::note_edge`) it is read in, where the scan kept them, and
/// whether it starts on the page. pdf-inspector leaves out text set well
/// off the page where it reads as whole paragraphs, a neighbouring page's
/// on an imposed sheet, and keeps it otherwise.
struct Note {
    text: String,
    edges: Option<(usize, usize)>,
    on_page: bool,
}

/// Where the run noted last starts on an upright line, and the characters
/// shown from there.
#[derive(Clone, Copy)]
struct Last {
    start: Option<Start>,
    shown: usize,
}

/// Where an upright string starts on the page, and its size.
#[derive(Clone, Copy)]
struct Start {
    x: f64,
    y: f64,
    size: f64,
}

impl Start {
    /// Where a string shown under `text_matrix` starts, when its line is
    /// upright.
    fn of(state: State, text_matrix: [f64; 6]) -> Option<Start> {
        let matrix = multiply(text_matrix, state.ctm);
        let upright =
            matrix[1].abs() < 1e-6 && matrix[2].abs() < 1e-6 && matrix[0] > 0.0 && matrix[3] > 0.0;
        let start = Start {
            x: matrix[4],
            y: matrix[3] * state.rise + matrix[5],
            size: state.size.abs() * matrix[3],
        };
        (upright
            && start.size > 0.0
            && [start.x, start.y, start.size].iter().all(|v| v.is_finite()))
        .then_some(start)
    }
}

/// Text in fonts pdf-inspector finds no map for on a page: as it says it,
/// as far as can be told, and as pdf-inspector reads it with no sign; and
/// text in fonts it may find none for, as it would read it with none.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct CjkTexts {
    pub(crate) says: Vec<String>,
    pub(crate) read_as: Vec<String>,
    pub(crate) evidence: Vec<String>,
}

impl CjkTexts {
    fn bytes(&self) -> usize {
        self.says
            .iter()
            .chain(&self.read_as)
            .chain(&self.evidence)
            .map(String::len)
            .sum()
    }
}

/// The texts noted on a page as they are looked for in the Markdown, and
/// whether text starting on the page was left out for want of room: such a
/// page is reported without being looked for.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct PageTexts {
    pub(crate) texts: Vec<String>,
    pub(crate) overflowed: bool,
    /// Whether any of the texts starts on the page.
    on_page: bool,
}

impl PageTexts {
    fn is_empty(&self) -> bool {
        self.texts.is_empty() && !self.overflowed
    }

    fn bytes(&self) -> usize {
        self.texts.iter().map(String::len).sum()
    }

    /// Leave the texts out, the room to keep them spent.
    fn leave_out(&mut self) {
        self.overflowed |= self.on_page;
        self.texts = Vec::new();
    }
}

impl Noted {
    /// Note `text`, read in the page's edge run `edge`, without the control
    /// characters pdf-inspector drops: more of the run noted last when the
    /// text matrix was not just set, or was set on the run's line no
    /// further than a glyph past its characters, as text set glyph by glyph
    /// is; else a run of its own. Past `MAX_HIDDEN_TEXT` bytes a page, text
    /// is left out, and the page marked where it starts `on_page` with more
    /// than white space in it.
    fn note(
        &mut self,
        text: &str,
        placed: bool,
        start: Option<Start>,
        edge: Option<usize>,
        on_page: bool,
    ) {
        if self.overflowed {
            return;
        }
        let text = without_controls(text);
        if self.bytes + text.len() > MAX_HIDDEN_TEXT {
            self.overflowed = on_page && text.chars().any(|character| !character.is_whitespace());
            self.last = None;
            return;
        }
        self.bytes += text.len();
        let characters = text.chars().count();
        let goes_on = |from: Start, shown: usize, start: Start| {
            (start.y - from.y).abs() <= 0.3 * from.size
                && (start.size - from.size).abs() <= 0.5 * from.size
                && start.x > from.x
                && start.x - from.x <= from.size * (shown as f64 + 1.0)
        };
        let joined = self.last.is_some_and(|last| {
            !placed
                || last
                    .start
                    .zip(start)
                    .is_some_and(|(from, start)| goes_on(from, last.shown, start))
        });
        match self.notes.last_mut().filter(|_| joined) {
            Some(note) => {
                note.text.push_str(&text);
                note.edges = note.edges.zip(edge).map(|((first, _), last)| (first, last));
                note.on_page |= on_page;
            }
            None => self.notes.push(Note {
                text,
                edges: edge.map(|edge| (edge, edge)),
                on_page,
            }),
        }
        self.last = Some(match self.last.filter(|_| joined && !placed) {
            Some(last) => Last {
                shown: last.shown + characters,
                ..last
            },
            None => Last {
                start,
                shown: characters,
            },
        });
    }

    /// Text pdf-inspector reads that is not of the kind noted was shown:
    /// the run noted last goes on no further.
    fn interrupt(&mut self) {
        self.last = None;
    }

    /// The texts noted, as they are looked for in the Markdown: a run of
    /// `MIN_HIDDEN_CHARS` characters or more as it is, and as its line
    /// reads from its first character to its last, as glyphs shown out of
    /// order are read; and a shorter one, such as a "not" or a digit
    /// slipped into a line, with the text before it, and apart with the
    /// text after it, as pdf-inspector reads the page's lines (see
    /// `line_contexts`), where the scan read them.
    fn looked_for(self, edges: &[EdgeRun]) -> PageTexts {
        let characters = |note: &Note| crate::repeated_lines::bare(&note.text).chars().count();
        let short = |note: &Note| characters(note) < crate::MIN_HIDDEN_CHARS;
        let notes: Vec<Note> = self
            .notes
            .into_iter()
            .filter(|note| characters(note) > 0)
            .collect();
        let spans: Vec<(usize, usize, bool)> = notes
            .iter()
            .filter_map(|note| note.edges.map(|(first, last)| (first, last, short(note))))
            .collect();
        let mut read = crate::repeated_lines::line_contexts(edges, &spans).into_iter();
        let mut on_page = false;
        let mut texts = Vec::new();
        for note in notes {
            let lines = note.edges.and_then(|_| read.next()).unwrap_or_default();
            let found = texts.len();
            if !short(&note) {
                let bare = crate::repeated_lines::bare(&note.text);
                texts.extend(lines.into_iter().filter(|line| *line != bare));
                texts.push(note.text);
            } else {
                texts.extend(lines);
            }
            on_page |= texts.len() > found && note.on_page;
        }
        PageTexts {
            texts,
            overflowed: self.overflowed,
            on_page,
        }
    }
}

impl PageText {
    fn show(&mut self, state: State, bytes: &[u8]) {
        self.shows += 1;
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

    /// Note visible text a form shows, for the check of forms: text
    /// pdf-inspector does not reach, or reads byte by byte (see
    /// [`read_without_font`]) otherwise than its font does, the control
    /// characters it drops aside. Where the font does not say what a string
    /// reads as, codes of two bytes read byte by byte differ from it; codes
    /// of one may not.
    fn note_form_text(&mut self, state: State, text: Option<&Object>, bytes: &[u8]) {
        if !self.forms
            || self.form_text_unread
            || !state.font
            || state.read_mode == 3
            || !bytes.iter().any(|&byte| byte != b' ')
        {
            return;
        }
        self.form_text_unread = if !state.reached {
            true
        } else if !state.raw {
            false
        } else {
            match state.decoded {
                Some(Decoded::TwoBytes) => true,
                Some(Decoded::Glyphs { font, two_bytes }) => {
                    let (mut read, mut raw) = (String::new(), String::new());
                    let mut known = true;
                    for string in strings(text) {
                        match self.glyph_fonts.text(font, string) {
                            Some(text) => read.push_str(&text),
                            None => {
                                known = false;
                                break;
                            }
                        }
                        raw.push_str(&read_without_font(string));
                    }
                    if known {
                        without_controls(&read) != without_controls(&raw)
                    } else {
                        two_bytes
                    }
                }
                None => false,
            }
        };
    }

    /// Note a string for the words shown glyph by glyph: each glyph placed
    /// where the text matrix and the font's widths say, as pdf-inspector
    /// reads it; a string in a font whose glyphs are not read, which ends
    /// the word being shown unless it goes on with it; or one whose place
    /// is not known.
    fn note_glyphs(
        &mut self,
        state: State,
        text_matrix: [f64; 6],
        text: Option<&Object>,
        placed: bool,
    ) {
        let Some(words) = self.glyph_words.as_mut() else {
            return;
        };
        let matrix = multiply(text_matrix, state.ctm);
        let mut at = [
            matrix[2] * state.rise + matrix[4],
            matrix[3] * state.rise + matrix[5],
        ];
        let em = [matrix[0] * state.size, matrix[1] * state.size];
        // One text space unit along the baseline, horizontally scaled.
        let unit = [
            matrix[0] * state.horizontal_scale,
            matrix[1] * state.horizontal_scale,
        ];
        if !placed
            || !at
                .iter()
                .chain(&em)
                .chain(&unit)
                .all(|value| value.is_finite())
        {
            words.lose();
            return;
        }
        let Some(font) = state
            .glyph_font
            .filter(|_| state.font && state.read_mode != 3)
        else {
            words.interrupt(at);
            return;
        };
        let elements = match text {
            Some(Object::Array(elements)) => elements.as_slice(),
            Some(text) => std::slice::from_ref(text),
            None => &[],
        };
        let two_byte = self.glyph_fonts.two_byte(font);
        // Whether the glyph shown last left the pen where the font's widths
        // do not say.
        let (mut first, mut unplaced) = (true, false);
        for element in elements {
            let bytes = match element {
                Object::String(bytes, _) => bytes,
                Object::Integer(offset) => {
                    let travel = -(*offset as f64) / 1000.0 * state.size;
                    at = [at[0] + travel * unit[0], at[1] + travel * unit[1]];
                    continue;
                }
                Object::Real(offset) => {
                    let travel = -f64::from(*offset) / 1000.0 * state.size;
                    at = [at[0] + travel * unit[0], at[1] + travel * unit[1]];
                    continue;
                }
                _ => continue,
            };
            for code in bytes.chunks_exact(if two_byte { 2 } else { 1 }) {
                if unplaced {
                    words.lose();
                    return;
                }
                let code = match code {
                    [high, low] => u16::from_be_bytes([*high, *low]),
                    [byte] => u16::from(*byte),
                    _ => continue,
                };
                let (reading, width) = self.glyph_fonts.glyph(font, code);
                // The pen moves by the width at the font size, the character
                // spacing, and after code 32 the word spacing.
                let travel = width.map(|width| {
                    width * state.size
                        + state.char_spacing
                        + if code == 32 { state.word_spacing } else { 0.0 }
                });
                words.glyph(Glyph {
                    font,
                    reading,
                    at,
                    em,
                    advance: travel
                        .filter(|_| state.size != 0.0)
                        .map(|travel| travel * state.horizontal_scale / state.size),
                    first,
                });
                first = false;
                match travel {
                    Some(travel) => at = [at[0] + travel * unit[0], at[1] + travel * unit[1]],
                    None => unplaced = true,
                }
            }
        }
    }

    /// Note text pdf-inspector reads that a reader does not see, in a layer
    /// it hides or, when `invisible`, painted in render mode 3, run by run
    /// (see `Noted::note`), with the edge run `edge` it is read in: its text
    /// where its font can be read. Text pdf-inspector reads that a reader
    /// sees, when `unseen` is not set, ends the run noted last.
    #[allow(clippy::too_many_arguments)]
    fn note_unseen(
        &mut self,
        state: State,
        text_matrix: [f64; 6],
        bytes: &[u8],
        placed: bool,
        edge: Option<usize>,
        unseen: bool,
        invisible: bool,
        page_box: [f64; 4],
    ) {
        let noted = if invisible {
            self.invisible_text.as_mut()
        } else {
            self.hidden_text.as_mut()
        };
        let Some(noted) = noted.filter(|noted| !noted.overflowed) else {
            return;
        };
        if !unseen {
            noted.interrupt();
            return;
        }
        let Some(text) = state
            .glyph_font
            .and_then(|font| self.glyph_fonts.text(font, bytes))
        else {
            return;
        };
        let on_page = starts_on_page(state, text_matrix, page_box);
        noted.note(&text, placed, Start::of(state, text_matrix), edge, on_page);
    }

    /// Note a string pdf-inspector reads, in a font it finds no map for,
    /// when `cjk` says how it reads it (see `cjk_fonts`): what the string
    /// says, as far as can be told, and what pdf-inspector reads it as,
    /// where it reads it with no sign; and whether it reads it otherwise so.
    /// A string in another font ends the runs noted last.
    fn note_unmapped(
        &mut self,
        bytes: &[u8],
        placed: bool,
        cjk: Option<crate::cjk_fonts::Unmapped>,
    ) {
        let (Some(says), Some(read), Some(evidence)) = (
            self.cjk_text.as_mut(),
            self.cjk_read.as_mut(),
            self.cjk_evidence.as_mut(),
        ) else {
            return;
        };
        let Some(font) = cjk else {
            says.interrupt();
            read.interrupt();
            evidence.interrupt();
            return;
        };
        // A font not judged may have a map: its text is looked for only as
        // pdf-inspector reads it with none.
        if !font.judged {
            says.interrupt();
            read.interrupt();
            match crate::cjk_fonts::read_as(font, bytes) {
                Some(text) => evidence.note(&text, placed, None, None, true),
                None => evidence.interrupt(),
            }
            return;
        }
        evidence.interrupt();
        self.cjk_misread |= crate::cjk_fonts::misread(font, bytes);
        says.note(
            &crate::cjk_fonts::says(font, bytes).unwrap_or_default(),
            placed,
            None,
            None,
            true,
        );
        match crate::cjk_fonts::read_as(font, bytes) {
            Some(text) => read.note(&text, placed, None, None, true),
            None => read.interrupt(),
        }
    }

    /// Note a string shown in a font that writes vertically: more of the
    /// run before when it was the page's last string and the text matrix
    /// was not set since, as its glyphs go on down its column; else a run
    /// of its own where its line is upright and it starts on the page, as
    /// no column set off the page is seen. Its text is kept to
    /// `MAX_HIDDEN_TEXT` bytes a page, and past them is not read.
    fn note_vertical(
        &mut self,
        state: State,
        text_matrix: [f64; 6],
        bytes: &[u8],
        placed: bool,
        page_box: [f64; 4],
    ) {
        let show = self.shows;
        let room = MAX_HIDDEN_TEXT.saturating_sub(self.vertical_bytes);
        let text = state
            .glyph_font
            .and_then(|font| self.glyph_fonts.text(font, bytes))
            .filter(|text| text.len() <= room);
        let Some(runs) = self
            .vertical
            .as_mut()
            .filter(|runs| runs.len() < MAX_RUNS_PER_PAGE)
        else {
            return;
        };
        let glyphs = (bytes.len() / 2) as f64;
        if let Some(last) = runs
            .last_mut()
            .filter(|last| !placed && last.show + 1 == show)
        {
            last.bottom -= glyphs * last.size;
            last.text = last.text.take().zip(text).map(|(mut before, text)| {
                self.vertical_bytes += text.len();
                before.push_str(&text);
                before
            });
            last.show = show;
            return;
        }
        let Some(start) = Start::of(state, text_matrix).filter(|start| {
            (page_box[0]..=page_box[2]).contains(&start.x)
                && (page_box[1]..=page_box[3]).contains(&start.y)
        }) else {
            return;
        };
        if let Some(text) = &text {
            self.vertical_bytes += text.len();
        }
        runs.push(crate::vertical_text::VerticalRun {
            x: start.x,
            top: start.y,
            bottom: start.y - glyphs * start.size,
            size: start.size,
            text,
            show,
        });
    }

    /// Note a run pdf-inspector reads, for the running-header check's gate
    /// and to read unseen text beside what it stands with: where it starts,
    /// when the text matrix was just set, else as more of the run before;
    /// with its text, where its font can be read. The run the string is
    /// noted in, where it is.
    fn note_edge(
        &mut self,
        state: State,
        text_matrix: [f64; 6],
        bytes: &[u8],
        placed: bool,
    ) -> Option<usize> {
        let room = self
            .edges
            .as_ref()
            .is_some_and(|edges| edges.len() < MAX_RUNS_PER_PAGE);
        if !(room && state.font && state.reached && state.read_mode != 3) {
            return None;
        }
        let text = state
            .glyph_font
            .and_then(|font| self.glyph_fonts.text(font, bytes));
        let edges = self.edges.as_mut()?;
        if !placed {
            if let Some(last) = edges.last_mut() {
                last.text = last.text.take().zip(text).map(|(mut before, text)| {
                    before.push_str(&text);
                    before
                });
                return Some(edges.len() - 1);
            }
        }
        let matrix = multiply(text_matrix, state.ctm);
        let at = [
            matrix[2] * state.rise + matrix[4],
            matrix[3] * state.rise + matrix[5],
        ];
        let size = state.size.abs() * matrix[2].hypot(matrix[3]);
        if !(at.iter().all(|value| value.is_finite()) && size.is_finite()) {
            return None;
        }
        edges.push(EdgeRun {
            y: at[1] as f32,
            x: at[0] as f32,
            direction: [matrix[0] as f32, matrix[1] as f32],
            size: size as f32,
            text,
        });
        Some(edges.len() - 1)
    }

    /// Note a visible run pdf-inspector reads whose start the text matrix
    /// says, and the string it strikes over itself, if any.
    fn note_run(
        &mut self,
        state: State,
        text_matrix: [f64; 6],
        bytes: &[u8],
        plain: bool,
        overstruck: Option<&[u8]>,
    ) {
        if self.runs.is_none()
            || !state.font
            || !state.reached
            || matches!(state.read_mode, 3 | 7)
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
        if !at.iter().all(|value| value.is_finite()) || !size.is_finite() {
            return;
        }
        // A run of one byte is matched by its bytes alone: text read through
        // the font adds nothing but its cost for a page shown glyph by glyph.
        let mut shows = |bytes: &[u8]| Shows {
            bytes: bytes.to_vec(),
            text: state
                .glyph_font
                .filter(|_| bytes.len() >= 2)
                .and_then(|font| self.glyph_fonts.text(font, bytes)),
        };
        let run = shows(bytes);
        let struck = overstruck.map(shows);
        let Some(runs) = self.runs.as_mut() else {
            return;
        };
        if runs.placed.len() < MAX_RUNS_PER_PAGE {
            runs.placed.push((at, plain));
        }
        if let Some(struck) = struck {
            runs.repeat(at, size, &struck);
        }
        runs.note(run, at, size);
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

    /// Whether the page is a scan with a text layer: images, drawn or set
    /// inline, cover it, and most of its text paints nothing. The layer
    /// describes what the scan shows; a page drawn over a background image
    /// shows its text.
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
    /// Pages showing text through a form without resources of its own that
    /// pdf-inspector misses or misreads.
    pub(crate) forms_unread: Vec<u32>,
    /// Where the visible runs placed on the pages read for repeats start,
    /// up to `MAX_PLACED_RUNS`.
    pub(crate) placed: Vec<Placed>,
    /// Words shown glyph by glyph in fonts that paint their spaces anywhere
    /// in the document, on the pages read for repeats (see `KeptWords`).
    pub(crate) glyph_words: Vec<ShownWord>,
    /// Form field values pdf-inspector misreads or never writes, when the
    /// pages are read for repeats (see `form_fields`).
    pub(crate) form_values: Vec<crate::form_fields::FormValue>,
    /// Text annotations show that pdf-inspector never reads, when the pages
    /// are read for repeats (see `annotations`).
    pub(crate) annotation_texts: Vec<crate::annotations::AnnotationText>,
    /// Whether the document is a dynamic XFA form, whose content
    /// pdf-inspector never reads, when the whole document is read.
    pub(crate) xfa_dynamic: bool,
    /// The files the document embeds, which pdf-inspector never reads, and
    /// whether it is a portfolio of them, when the whole document is read.
    pub(crate) embedded_files: (usize, bool),
    /// The runs at each page's edges, for the running-header check's gate,
    /// when every page pdf-inspector reads, to `MAX_REPEAT_PAGES`, is read
    /// for repeats; else `None`.
    pub(crate) edges: Option<Vec<(u32, Vec<EdgeRun>)>>,
    /// Text pdf-inspector reads in layers a reader hides (see
    /// `optional_content`), by page, on the pages read for repeats.
    pub(crate) hidden_layer_texts: Vec<(u32, PageTexts)>,
    /// Text a viewer never paints, in render mode 3, that pdf-inspector
    /// reads as shown (upstream issue #572), by page, on the pages read for
    /// repeats but a scan's with its text layer.
    pub(crate) invisible_texts: Vec<(u32, PageTexts)>,
    /// The pages, among those read for repeats, whose text in a font
    /// pdf-inspector finds no map for (upstream issue #573) reads otherwise
    /// with no sign; and that text as it says it and as pdf-inspector reads
    /// it, by page.
    pub(crate) cjk_pages: Vec<u32>,
    pub(crate) cjk_texts: Vec<(u32, CjkTexts)>,
    /// What the Markdown must show of the columns of text in vertical
    /// writing (upstream issue #575), right to left, by page, on the pages
    /// read for repeats (see `vertical_text::readings`).
    pub(crate) vertical_readings: Vec<(u32, Vec<crate::vertical_text::Reading>)>,
    /// The page from which the checks of what pages paint stopped, their
    /// limits spent, where they did.
    pub(crate) unchecked_from: Option<u32>,
}

/// Scan a document's pages. Pages in `layer_skip` are not checked for an
/// invisible layer; pages are checked for text painted twice only when
/// `twice_skip` is given, and not those in it. Pages outside `only`, when
/// it is given, are not scanned. A document that does not load reports
/// none.
#[cfg(test)]
pub(crate) fn scan(
    buffer: &[u8],
    layer_skip: &HashSet<u32>,
    twice_skip: Option<&HashSet<u32>>,
    only: Option<&HashSet<u32>>,
) -> Findings {
    scan_document(
        buffer,
        layer_skip,
        twice_skip,
        only,
        twice_skip.is_some(),
        None,
    )
}

/// Whether the document holds form fields. pdf-inspector reads their values
/// among a page's lines, where the page scan does not see them, so the
/// running-header gate cannot tell what it would drop with them.
fn has_form_fields(document: &Document) -> bool {
    fn resolved<'a>(document: &'a Document, object: &'a Object) -> Option<&'a Object> {
        document.dereference(object).ok().map(|(_, object)| object)
    }
    document
        .trailer
        .get(b"Root")
        .ok()
        .and_then(|root| resolved(document, root))
        .and_then(|root| root.as_dict().ok())
        .and_then(|root| root.get(b"AcroForm").ok())
        .and_then(|form| resolved(document, form))
        .and_then(|form| form.as_dict().ok())
        .and_then(|form| form.get(b"Fields").ok())
        .and_then(|fields| resolved(document, fields))
        .and_then(|fields| fields.as_array().ok())
        .is_some_and(|fields| !fields.is_empty())
}

/// As `scan`, and, when `whole` is set, reading what the document holds
/// beside its pages that pdf-inspector never reads: dynamic XFA, and the
/// files it embeds.
pub(crate) fn scan_document(
    buffer: &[u8],
    layer_skip: &HashSet<u32>,
    twice_skip: Option<&HashSet<u32>>,
    only: Option<&HashSet<u32>>,
    whole: bool,
    tables: Option<crate::repeated_lines::Tables<'_>>,
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
    let mut cjk_fonts = crate::cjk_fonts::CjkFonts::default();
    let mut found = Findings::default();
    let layers = Layers::new(&document).map(std::rc::Rc::new);
    if twice_skip.is_some() {
        found.form_values = crate::form_fields::misread(&document);
        found.annotation_texts = crate::annotations::unread(&document, only, layers.as_deref());
    }
    if whole {
        found.xfa_dynamic = crate::form_fields::dynamic_xfa(&document);
        found.embedded_files = crate::form_fields::embedded_files(&document);
    }
    // Words shown glyph by glyph, kept while their fonts may yet be seen
    // painting their spaces.
    let mut glyph_words = KeptWords::default();
    // The runs at the edges of the pages read, while every page is: a page
    // not read for repeats, as one needing OCR is not, leaves the gate
    // nothing to go by.
    let mut edges = twice_skip
        .filter(|skip| skip.is_empty() && !has_form_fields(&document))
        .map(|_| Vec::new());
    // Room left for the text kept to look for in the Markdown: a page's
    // text that does not fit is left out, and the page reported.
    let mut noted_room = MAX_NOTED_TEXT;
    let mut fits = |bytes: usize| match noted_room.checked_sub(bytes) {
        Some(room) => {
            noted_room = room;
            true
        }
        None => false,
    };
    for (&number, &page_id) in &document.get_pages() {
        if only.is_some_and(|only| !only.contains(&number)) {
            continue;
        }
        let check_layer = !layer_skip.contains(&number);
        let check_twice = repeats && twice_skip.is_some_and(|skip| !skip.contains(&number));
        if !check_twice {
            edges = None;
        }
        // Forms without resources are checked on every page of a full run.
        let check_forms = repeats && twice_skip.is_some();
        if !check_layer && !check_twice && !check_forms {
            continue;
        }
        let budgets = Budgets {
            layer: &mut layer_budget,
            repeat: &mut repeat_budget,
            gap_fonts: &mut gap_fonts,
            glyph_fonts: &mut glyph_fonts,
            cjk_fonts: &mut cjk_fonts,
        };
        let checks = Checks {
            layer: check_layer,
            twice: check_twice,
            forms: check_forms,
            tables,
        };
        match scan_page(&document, page_id, checks, budgets, layers.as_ref()) {
            Ok(page) => {
                if page.hidden_layer {
                    found.hidden_layer.push(number);
                }
                if page.form_text_unread {
                    found.forms_unread.push(number);
                }
                if page.gaps_misread {
                    found.gaps_misread.push(number);
                }
                glyph_words.page(number, page.glyph_words, page.glyph_spaces);
                let mut hidden = page.hidden_text;
                if !fits(hidden.bytes()) {
                    hidden.leave_out();
                }
                if !hidden.is_empty() {
                    found.hidden_layer_texts.push((number, hidden));
                }
                let mut invisible = page.invisible_text;
                if !fits(invisible.bytes()) {
                    invisible.leave_out();
                }
                if !invisible.is_empty() {
                    found.invisible_texts.push((number, invisible));
                }
                if page.cjk_misread {
                    found.cjk_pages.push(number);
                }
                // Text left out cannot tell the page read right; text that
                // may read right is looked for only where it fits.
                if (page.cjk_misread || !page.cjk_text.evidence.is_empty())
                    && fits(page.cjk_text.bytes())
                {
                    found.cjk_texts.push((number, page.cjk_text));
                }
                let mut readings = page.vertical_readings;
                let read: usize = readings
                    .iter()
                    .map(|reading| match reading {
                        Reading::Alone(text) => text.len(),
                        Reading::Pair(texts, _) => texts
                            .as_ref()
                            .map_or(0, |(right, left)| right.len() + left.len()),
                    })
                    .sum();
                // Columns side by side whose text is left out cannot be told
                // read right; one alone is taken to.
                if !fits(read) {
                    readings.retain(|reading| matches!(reading, Reading::Pair(..)));
                    for reading in &mut readings {
                        if let Reading::Pair(texts, _) = reading {
                            *texts = None;
                        }
                    }
                }
                if !readings.is_empty() {
                    found.vertical_readings.push((number, readings));
                }
                if let Some(edges) = edges
                    .as_mut()
                    .filter(|edges| edges.len() < crate::repeated_lines::MAX_REPEAT_PAGES)
                {
                    edges.push((number, page.edges));
                }
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
            Err(Exhausted) if repeat_budget.spent() => {
                repeats = false;
                edges = None;
                found.unchecked_from.get_or_insert(number);
            }
            Err(Exhausted) => {
                edges = None;
                found.unchecked_from.get_or_insert(number);
                break;
            }
        }
    }
    found.glyph_words = glyph_words.finish();
    found.edges = edges;
    found
}

/// The limits a page is read under: the layer check's when the page is
/// read for it, the repeat check's otherwise; and the fonts read so far.
struct Budgets<'a> {
    layer: &'a mut Budget,
    repeat: &'a mut Budget,
    gap_fonts: &'a mut GapFonts,
    glyph_fonts: &'a mut GlyphFonts,
    cjk_fonts: &'a mut crate::cjk_fonts::CjkFonts,
}

/// What a page is read for: an invisible layer, text painted twice (with
/// word gaps and words shown glyph by glyph), and forms without resources.
#[derive(Clone, Copy)]
struct Checks<'a> {
    layer: bool,
    twice: bool,
    forms: bool,
    /// The Markdown's table rows, set aside from the lines at a page's
    /// edges the running-header gate reads.
    tables: Option<crate::repeated_lines::Tables<'a>>,
}

/// What the scan found on one page.
#[derive(Default)]
struct PageFindings {
    hidden_layer: bool,
    gaps_misread: bool,
    form_text_unread: bool,
    /// Runs painted again over themselves, measured from the visible box.
    repeats: Vec<Repeat>,
    /// Where placed runs start, measured from the visible box, and whether
    /// they are plain.
    placed: Vec<([f64; 2], bool)>,
    /// Words shown glyph by glyph, with their font and widest gap, and the
    /// fonts seen painting their spaces.
    glyph_words: Vec<(String, usize, f64)>,
    glyph_spaces: HashSet<usize>,
    /// The runs at the page's edges, measured from the visible box, when
    /// it is read for repeats.
    edges: Vec<EdgeRun>,
    /// Text pdf-inspector reads in layers a reader hides.
    hidden_text: PageTexts,
    /// Text a viewer never paints that pdf-inspector reads, on a page that
    /// is not a scan with its text layer.
    invisible_text: PageTexts,
    /// Text in fonts pdf-inspector finds no map for, and whether any reads
    /// otherwise with no sign.
    cjk_text: CjkTexts,
    cjk_misread: bool,
    /// What the Markdown must show of the columns of text in vertical
    /// writing, right to left.
    vertical_readings: Vec<crate::vertical_text::Reading>,
}

fn scan_page(
    document: &Document,
    page_id: ObjectId,
    checks: Checks<'_>,
    budgets: Budgets<'_>,
    layers: Option<&std::rc::Rc<Layers>>,
) -> Result<PageFindings, Exhausted> {
    let Some(page_box) = page_box(document, page_id) else {
        return Ok(PageFindings::default());
    };
    let scopes = page_resources(document, page_id);
    let check_twice = checks.twice;
    // A scan is an image XObject; for the layer check, a page that binds
    // none, directly or through its forms, is not read. Nor is a page that
    // binds no form read for forms.
    let mut seen = HashSet::new();
    let check_layer = checks.layer
        && scopes
            .iter()
            .any(|scope| binds_image(document, scope.dictionary, 0, &mut seen));
    let check_forms = checks.forms
        && scopes
            .iter()
            .any(|scope| binds_form(document, scope.dictionary));
    if !check_layer && !check_twice && !check_forms {
        return Ok(PageFindings::default());
    }
    let Budgets {
        layer,
        repeat,
        gap_fonts,
        glyph_fonts,
        cjk_fonts,
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
    let content = without_comments(&content);
    let mut page = PageText {
        runs: check_twice.then(Runs::default),
        edges: check_twice.then(Vec::new),
        layers: layers.cloned(),
        hidden_text: (check_twice && layers.is_some()).then(Noted::default),
        invisible_text: check_twice.then(Noted::default),
        cjk_text: check_twice.then(Noted::default),
        cjk_read: check_twice.then(Noted::default),
        cjk_misread: false,
        cjk_evidence: check_twice.then(Noted::default),
        cjk_fonts: std::mem::take(cjk_fonts),
        vertical: check_twice.then(Vec::new),
        gap_fonts: std::mem::take(gap_fonts),
        glyph_words: check_twice.then(GlyphWords::default),
        glyph_fonts: std::mem::take(glyph_fonts),
        forms: check_forms,
        ..PageText::default()
    };
    page.save();
    let mut forms = Vec::new();
    let resources = Resources {
        form: None,
        own: true,
        page: &scopes,
    };
    let executed = execute(
        document,
        &content,
        resources,
        State::START,
        page_box,
        &mut page,
        &mut forms,
        budget,
    );
    *gap_fonts = std::mem::take(&mut page.gap_fonts);
    *glyph_fonts = std::mem::take(&mut page.glyph_fonts);
    *cjk_fonts = std::mem::take(&mut page.cjk_fonts);
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
    let (glyph_words, glyph_spaces) = page
        .glyph_words
        .take()
        .map(GlyphWords::finish)
        .unwrap_or_default();
    let runs = page.edges.take().unwrap_or_default();
    let hidden_text = page
        .hidden_text
        .take()
        .map(|noted| noted.looked_for(&runs))
        .unwrap_or_default();
    // A scan with its text layer is reported for OCR, which reads what the
    // scan shows: the layer's invisible text is not looked for.
    let hidden_layer = checks.layer && page.is_hidden_layer(page_box);
    let invisible_text = page
        .invisible_text
        .take()
        .filter(|_| !hidden_layer)
        .map(|noted| noted.looked_for(&runs))
        .unwrap_or_default();
    let edges = crate::repeated_lines::edge_runs(
        runs.into_iter()
            .map(|run| EdgeRun {
                x: run.x - page_box[0] as f32,
                y: run.y - page_box[1] as f32,
                ..run
            })
            .collect(),
        checks.tables,
    );
    Ok(PageFindings {
        edges,
        hidden_text,
        invisible_text,
        cjk_text: CjkTexts {
            says: page
                .cjk_text
                .take()
                .map(|noted| noted.notes.into_iter().map(|note| note.text).collect())
                .unwrap_or_default(),
            read_as: page
                .cjk_read
                .take()
                .map(|noted| noted.notes.into_iter().map(|note| note.text).collect())
                .unwrap_or_default(),
            evidence: page
                .cjk_evidence
                .take()
                .map(|noted| noted.notes.into_iter().map(|note| note.text).collect())
                .unwrap_or_default(),
        },
        cjk_misread: page.cjk_misread,
        vertical_readings: page
            .vertical
            .take()
            .map(|runs| crate::vertical_text::readings(&runs))
            .unwrap_or_default(),
        hidden_layer,
        gaps_misread: page.gaps_misread,
        form_text_unread: page.form_text_unread,
        placed,
        glyph_words,
        glyph_spaces,
        repeats: page
            .runs
            .map(|runs| {
                runs.repeats
                    .into_iter()
                    .map(|repeat| Repeat {
                        at: [repeat.at[0] - page_box[0], repeat.at[1] - page_box[1]],
                        ..repeat
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
    resources: Resources<'a, '_>,
    start: State,
    page_box: [f64; 4],
    page: &mut PageText,
    forms: &mut Vec<ObjectId>,
    budget: &mut Budget,
) -> Result<(), Exhausted> {
    // pdf-inspector reads nothing of a stream of more operators than it
    // reads; the rest are charged before they are decoded, as decoding
    // holds them all at once.
    let operators = crate::content_ops::operators(
        content,
        crate::content_ops::MAX_READ_OPERATORS.saturating_add(1),
    );
    if operators > crate::content_ops::MAX_READ_OPERATORS {
        return Ok(());
    }
    budget.take_operations(operators)?;
    let Ok(content) = Content::decode(content) else {
        return Ok(());
    };
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
    // How far the pen has travelled since the text was last placed, in
    // unscaled text space units, while every run shown since is in a font
    // whose widths the scan reads.
    let mut travelled: Option<f64> = Some(0.0);
    // The marked-content spans open.
    let mut marked = Marked::default();
    for operation in &content.operations {
        let operands = &operation.operands;
        let operator = operation.operator.as_str();
        // `'` and `"` move to the next line before they show text; with no
        // leading set, pdf-inspector moves by 1.2 times the font size.
        if matches!(operator, "T*" | "'" | "\"") {
            let leading = if state.leading != 0.0 {
                state.leading
            } else {
                1.2 * state.size
            };
            line_matrix = multiply([1.0, 0.0, 0.0, 1.0, 0.0, -leading], line_matrix);
            text_matrix = line_matrix;
            placed = true;
        }
        if matches!(operator, "T*" | "'" | "\"" | "Td" | "TD" | "Tm" | "BT") {
            travelled = Some(0.0);
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
            // The fill colour, as pdf-inspector reads it: white past 0.95 in
            // gray and RGB, and under 0.05 in CMYK.
            "g" => {
                if let Some(gray) = operands.first().and_then(|gray| number(document, gray)) {
                    state.white = gray > 0.95;
                }
            }
            "rg" if operands.len() >= 3 => {
                state.white = operands[..3]
                    .iter()
                    .all(|value| number(document, value).unwrap_or(0.0) > 0.95);
            }
            "k" if operands.len() >= 4 => {
                state.white = operands[..4]
                    .iter()
                    .all(|value| number(document, value).unwrap_or(1.0) < 0.05);
            }
            "sc" | "scn" => {
                let values: Vec<f64> = operands
                    .iter()
                    .filter_map(|value| number(document, value))
                    .collect();
                state.white = match values.as_slice() {
                    [_, _, _] => values.iter().all(|value| *value > 0.95),
                    [_, _, _, _] => values.iter().all(|value| *value < 0.05),
                    _ => false,
                };
            }
            "Tr" => {
                // pdf-inspector takes the first operand, cut to a whole
                // number, and skips mode 3 alone; a viewer takes the last,
                // cut the same way, and only as one of the eight modes.
                if let Some(mode) = operands.first().and_then(|mode| number(document, mode)) {
                    state.read_mode = mode as i64;
                }
                if let Some(mode) = operands
                    .last()
                    .and_then(|mode| number(document, mode))
                    .map(|mode| mode as i64)
                    .filter(|mode| (0..=7).contains(mode))
                {
                    state.render_mode = mode;
                }
            }
            "BMC" => marked.open(false, false),
            "BDC" => {
                let actual_text = operands
                    .get(1)
                    .and_then(|properties| match properties {
                        Object::Dictionary(properties) => Some(properties),
                        Object::Reference(id) => document.get_dictionary(*id).ok(),
                        _ => None,
                    })
                    .is_some_and(gives_actual_text);
                // A span marked /OC names its layer, or a membership
                // dictionary, among the resources' properties, or writes
                // the dictionary in place.
                let optional = operands
                    .first()
                    .and_then(|tag| tag.as_name().ok())
                    .is_some_and(|tag| tag == b"OC");
                let hidden = optional
                    && page
                        .layers
                        .as_ref()
                        .is_some_and(|layers| match operands.get(1) {
                            Some(Object::Name(name)) => resources
                                .find(document, b"Properties", name)
                                .is_some_and(|(properties, _)| layers.hides(document, properties)),
                            Some(Object::Dictionary(properties)) => {
                                layers.hides_written(document, properties)
                            }
                            Some(properties) => layers.hides(document, properties),
                            None => false,
                        });
                marked.open(actual_text, hidden);
            }
            "EMC" => marked.close(),
            "Tf" => {
                if let [name, size] = operands.as_slice() {
                    state.font = name.as_name().is_ok();
                    let found = name
                        .as_name()
                        .ok()
                        .and_then(|name| resources.find(document, b"Font", name))
                        .and_then(|(font, read)| Some((dictionary(document, font)?, read)));
                    let resolved = found.map(|(font, _)| font);
                    // pdf-inspector reads text by the fonts it finds (see
                    // `Resources`), and byte by byte where it finds none.
                    let read = found.is_some_and(|(_, read)| read);
                    // Word gaps are checked with the repeats, on the pages
                    // read for them, in text pdf-inspector reads.
                    let judged = read && state.reached;
                    state.gaps = resolved
                        .filter(|_| page.runs.is_some() && judged)
                        .and_then(|font| page.gap_fonts.font(document, font));
                    state.glyph_font = resolved
                        .filter(|_| page.glyph_words.is_some() && judged)
                        .and_then(|font| page.glyph_fonts.font(document, font));
                    state.raw = !read;
                    state.cjk = resolved
                        .filter(|_| judged && page.cjk_text.is_some())
                        .and_then(|font| page.cjk_fonts.font(document, font));
                    state.vertical = judged
                        && page.vertical.is_some()
                        && resolved.is_some_and(crate::vertical_text::writes_vertically);
                    state.decoded = resolved.filter(|_| page.forms).and_then(|font| {
                        let two_bytes = font
                            .get(b"Subtype")
                            .and_then(Object::as_name)
                            .is_ok_and(|subtype| subtype == b"Type0");
                        match page.glyph_fonts.font(document, font) {
                            Some(font) => Some(Decoded::Glyphs { font, two_bytes }),
                            None => two_bytes.then_some(Decoded::TwoBytes),
                        }
                    });
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
                // A browser writes each run of text as a text object of its
                // own, which ends the words shown in it.
                if let Some(words) = page.glyph_words.as_mut() {
                    words.end_word();
                }
                text_matrix = IDENTITY;
                line_matrix = IDENTITY;
                placed = true;
                in_text = true;
                pending = None;
                // pdf-inspector takes each of a page's text objects to start
                // in mode 0; in a form, the mode goes on as set.
                if forms.is_empty() {
                    state.read_mode = 0;
                }
            }
            "ET" => {
                page.text_object_ended();
                if let Some(words) = page.glyph_words.as_mut() {
                    words.end_word();
                }
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
                // What pdf-inspector reads: text it reaches, in a font, in a
                // text object, but in mode 3 as it takes the mode, and, in a
                // form, text filled white.
                let white =
                    !forms.is_empty() && state.white && matches!(state.read_mode, 0 | 4 | 7);
                if in_text && state.font && state.reached && state.read_mode != 3 && !white {
                    let edge = page.note_edge(state, text_matrix, &bytes, placed);
                    let hidden = state.hidden || marked.hidden > 0;
                    page.note_unseen(
                        state,
                        text_matrix,
                        &bytes,
                        placed,
                        edge,
                        hidden,
                        false,
                        page_box,
                    );
                    // Mode 3 paints nothing; pdf-inspector reads it where it
                    // takes the text object to start in mode 0 (#572).
                    let invisible = state.render_mode == 3;
                    page.note_unseen(
                        state,
                        text_matrix,
                        &bytes,
                        placed,
                        edge,
                        invisible,
                        true,
                        page_box,
                    );
                    page.note_unmapped(&bytes, placed, state.cjk);
                    if state.vertical {
                        page.note_vertical(state, text_matrix, &bytes, placed, page_box);
                    }
                }
                if in_text && !forms.is_empty() {
                    page.note_form_text(state, text, &bytes);
                }
                // Inside a span giving the text its glyphs stand for,
                // pdf-inspector reads that text and not the glyphs.
                let glyphs_read = marked.actual_text == 0;
                page.note_glyphs(state, text_matrix, text, placed && in_text && glyphs_read);
                let before = travelled;
                travelled = None;
                if let Some(text) = text.filter(|_| in_text && glyphs_read && !page.gaps_misread) {
                    // A run that shows glyphs decides for the string before.
                    if shows_glyphs(text) {
                        if let Some(waiting) = pending.take() {
                            page.gaps_misread = waiting.taken_back(state, text_matrix, text);
                        }
                    }
                    // pdf-inspector reads all but mode 3 text.
                    if let Some(font) = state.gaps.filter(|_| state.font && state.read_mode != 3) {
                        let matrix = multiply(text_matrix, state.ctm);
                        let shown = Shown {
                            size: state.size as f32,
                            char_spacing: state.char_spacing as f32,
                            word_spacing: state.word_spacing as f32,
                            horizontal: matrix[0].abs() >= matrix[1].abs(),
                        };
                        let judged = page.gap_fonts.judge(font, text, shown);
                        page.gaps_misread |= judged.misjudged;
                        let advance = f64::from(judged.advance);
                        travelled = before.map(|travelled| travelled + advance);
                        if let Some(candidate) = judged.candidate {
                            let unit = [
                                matrix[0] * state.horizontal_scale,
                                matrix[1] * state.horizontal_scale,
                            ];
                            let origin = [matrix[4], matrix[5]];
                            pending = Some(Pending {
                                candidate,
                                origin,
                                unit,
                                pen: travelled.map(|travelled| {
                                    [
                                        origin[0] + travelled * unit[0],
                                        origin[1] + travelled * unit[1],
                                    ]
                                }),
                                moved: false,
                            });
                        }
                    }
                }
                // After a run, the next starts where it ended, which the
                // glyph widths decide.
                if placed {
                    page.note_run(state, text_matrix, &bytes, plain(text), overstruck(text));
                }
                placed = false;
            }
            "BI" => page.draw_image(state.ctm, page_box),
            "sh" => page.painted(),
            "Do" => {
                // What a form or an image paints continues no string.
                pending = None;
                if let Some(words) = page.glyph_words.as_mut() {
                    words.lose();
                }
                let Some(name) = operands.first().and_then(|name| name.as_name().ok()) else {
                    continue;
                };
                let Some((id, stream, read)) = xobject(document, resources, name) else {
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
                        // A form without resources of its own, where
                        // `/Resources` is missing or no dictionary, draws
                        // with its invoker's, which pdf-inspector does not
                        // read.
                        let form_resources = match stream
                            .dict
                            .get(b"Resources")
                            .ok()
                            .and_then(|resources| dictionary(document, resources))
                        {
                            Some(own) => Resources {
                                form: Some(own),
                                own: true,
                                page: resources.page,
                            },
                            None => Resources {
                                own: false,
                                ..resources
                            },
                        };
                        // pdf-inspector reads a form's text from no font and
                        // no spacing, with the fonts of its own resources,
                        // and reads it at all only where it finds the form.
                        let hidden = state.hidden
                            || marked.hidden > 0
                            || page
                                .layers
                                .as_ref()
                                .is_some_and(|layers| layers.hide(document, &stream.dict));
                        let inner = State {
                            ctm: multiply(form_matrix, state.ctm),
                            reached: state.reached && read,
                            hidden,
                            raw: true,
                            gaps: None,
                            glyph_font: None,
                            char_spacing: 0.0,
                            word_spacing: 0.0,
                            leading: 0.0,
                            ..state
                        };
                        // A form runs in a saved state of its own.
                        let depth = page.clip_levels.len();
                        page.save();
                        forms.push(id);
                        let result = execute(
                            document,
                            &bytes,
                            form_resources,
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

/// A page's content with each comment, from a `%` outside a string to the
/// end of its line, read as a space, as pdf-inspector reads a page's
/// content before decoding it: lopdf stops at a comment between an
/// operator's operands. A form's content it decodes as it stands.
fn without_comments(content: &[u8]) -> Vec<u8> {
    if !content.contains(&b'%') {
        return content.to_vec();
    }
    let mut kept = Vec::with_capacity(content.len());
    // How deep in parentheses a literal string is, and whether a hex string
    // is open.
    let (mut depth, mut hex) = (0usize, false);
    let mut index = 0;
    while index < content.len() {
        let byte = content[index];
        match byte {
            b'\\' if depth > 0 => {
                kept.push(byte);
                if let Some(&next) = content.get(index + 1) {
                    kept.push(next);
                    index += 1;
                }
            }
            b'(' if !hex => {
                depth += 1;
                kept.push(byte);
            }
            b')' if !hex && depth > 0 => {
                depth -= 1;
                kept.push(byte);
            }
            b'<' if depth == 0 && !hex => {
                hex = true;
                kept.push(byte);
            }
            b'>' if hex => {
                hex = false;
                kept.push(byte);
            }
            b'%' if depth == 0 && !hex => {
                while index < content.len() && !matches!(content[index], b'\n' | b'\r') {
                    index += 1;
                }
                kept.push(b' ');
                continue;
            }
            _ => kept.push(byte),
        }
        index += 1;
    }
    kept
}

/// The marked-content spans open in a content stream: whether each gives
/// the text its glyphs stand for (see `gives_actual_text`), which
/// pdf-inspector reads in place of the glyphs, and whether each is in a
/// layer a reader hides; with how many of each are open, so a string asks
/// whether it is in one at once however many spans a stream leaves open.
#[derive(Default)]
struct Marked {
    open: Vec<(bool, bool)>,
    actual_text: usize,
    hidden: usize,
}

impl Marked {
    fn open(&mut self, actual_text: bool, hidden: bool) {
        self.open.push((actual_text, hidden));
        self.actual_text += usize::from(actual_text);
        self.hidden += usize::from(hidden);
    }

    fn close(&mut self) {
        if let Some((actual_text, hidden)) = self.open.pop() {
            self.actual_text -= usize::from(actual_text);
            self.hidden -= usize::from(hidden);
        }
    }
}

/// Whether a marked-content span's properties give the text its glyphs
/// stand for, as pdf-inspector reads it: an `/ActualText` string that
/// decodes without the replacement character.
fn gives_actual_text(properties: &Dictionary) -> bool {
    let Ok(Object::String(bytes, _)) = properties.get(b"ActualText") else {
        return false;
    };
    let text = match bytes.as_slice() {
        [0xFE, 0xFF, rest @ ..] => {
            let units: Vec<u16> = rest
                .chunks_exact(2)
                .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
                .collect();
            String::from_utf16_lossy(&units)
        }
        bytes => bytes.iter().map(|&byte| char::from(byte)).collect(),
    };
    !text.contains('\u{FFFD}')
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

/// Points past the page box that text pdf-inspector takes for the page's
/// may start.
const PAGE_BOX_TOLERANCE: f64 = 6.0;

/// Whether a string shown under `text_matrix` starts on the page, within
/// `PAGE_BOX_TOLERANCE` of its box. One whose start is unknown is taken to.
fn starts_on_page(state: State, text_matrix: [f64; 6], page_box: [f64; 4]) -> bool {
    let matrix = multiply(text_matrix, state.ctm);
    let at = [
        matrix[2] * state.rise + matrix[4],
        matrix[3] * state.rise + matrix[5],
    ];
    !at.iter().all(|value| value.is_finite())
        || ((page_box[0] - PAGE_BOX_TOLERANCE..=page_box[2] + PAGE_BOX_TOLERANCE).contains(&at[0])
            && (page_box[1] - PAGE_BOX_TOLERANCE..=page_box[3] + PAGE_BOX_TOLERANCE)
                .contains(&at[1]))
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

/// One of a page's resource dictionaries, and what pdf-inspector reads of
/// it.
#[derive(Clone, Copy)]
struct Scope<'a> {
    dictionary: &'a Dictionary,
    /// Whether pdf-inspector finds the fonts it binds: the page's own, and
    /// those it inherits by reference, as lopdf collects a page's fonts.
    fonts: bool,
    /// Whether pdf-inspector finds the XObjects it binds: the page's own
    /// alone.
    xobjects: bool,
}

/// Where a content stream finds what it names, as renderers look it up,
/// and whether pdf-inspector finds it there too. A form's own resources
/// hold its names, and pdfium looks a category they lack, such as `/Font`,
/// up in the page's; pdf-inspector reads a form's own resources alone, and
/// none for a form without them, which draws with its invoker's.
#[derive(Clone, Copy)]
struct Resources<'a, 'p> {
    /// The resources of the form being read: its own, or, for a form
    /// without, those of the form drawing it; none for the page's content
    /// and a form it draws that has none.
    form: Option<&'a Dictionary>,
    /// Whether pdf-inspector reads the names the stream looks up: the
    /// page's content, and a form with resources of its own.
    own: bool,
    /// The page's resource dictionaries, its own first, then those it
    /// inherits, nearest first.
    page: &'p [Scope<'a>],
}

impl<'a> Resources<'a, '_> {
    /// The resource of `category` named `name`, as a renderer finds it, and
    /// whether pdf-inspector finds it there too.
    fn find(
        &self,
        document: &'a Document,
        category: &[u8],
        name: &[u8],
    ) -> Option<(&'a Object, bool)> {
        let held = |resources: &'a Dictionary| {
            resources
                .get(category)
                .ok()
                .and_then(|held| dictionary(document, held))
        };
        if let Some(held) = self.form.and_then(held) {
            return held.get(name).ok().map(|entry| (entry, self.own));
        }
        let read = self.own && self.form.is_none();
        self.page.iter().find_map(|scope| {
            let entry = held(scope.dictionary)?.get(name).ok()?;
            let found = if category == b"Font" {
                scope.fonts
            } else {
                scope.xobjects
            };
            Some((entry, read && found))
        })
    }
}

/// The page's resource dictionaries, as renderers inherit them: its own,
/// then each ancestor's, nearest first, each with what pdf-inspector reads
/// of it.
fn page_resources(document: &Document, page_id: ObjectId) -> Vec<Scope<'_>> {
    // lopdf, which pdf-inspector finds a page's fonts with, takes the
    // page's own resources and those it inherits by reference.
    let lopdf: Vec<&Dictionary> = match document.get_page_resources(page_id) {
        Ok((own, inherited)) => own
            .into_iter()
            .chain(
                inherited
                    .into_iter()
                    .filter_map(|id| document.get_dictionary(id).ok()),
            )
            .collect(),
        Err(_) => Vec::new(),
    };
    let mut scopes: Vec<Scope> = Vec::new();
    let mut node = document.get_dictionary(page_id).ok();
    for depth in 0..MAX_PAGE_TREE_DEPTH {
        let Some(current) = node else {
            break;
        };
        if let Some(resources) = current
            .get(b"Resources")
            .ok()
            .and_then(|resources| dictionary(document, resources))
        {
            if !scopes
                .iter()
                .any(|scope| std::ptr::eq(scope.dictionary, resources))
            {
                scopes.push(Scope {
                    dictionary: resources,
                    fonts: lopdf.iter().any(|known| std::ptr::eq(*known, resources)),
                    xobjects: depth == 0,
                });
            }
        }
        node = current
            .get(b"Parent")
            .and_then(Object::as_reference)
            .ok()
            .and_then(|parent| document.get_dictionary(parent).ok());
    }
    scopes
}

/// The XObject named `name`, as a renderer finds it, with its object id and
/// whether pdf-inspector finds it there too.
fn xobject<'a>(
    document: &'a Document,
    resources: Resources<'a, '_>,
    name: &[u8],
) -> Option<(ObjectId, &'a Stream, bool)> {
    let (Object::Reference(id), read) = resources.find(document, b"XObject", name)? else {
        return None;
    };
    let stream = document.get_object(*id).and_then(Object::as_stream).ok()?;
    Some((*id, stream, read))
}

/// Whether a resource dictionary binds a form XObject.
fn binds_form(document: &Document, resources: &Dictionary) -> bool {
    resources
        .get(b"XObject")
        .ok()
        .and_then(|xobjects| dictionary(document, xobjects))
        .is_some_and(|xobjects| {
            xobjects.iter().any(|(_, entry)| {
                let Object::Reference(id) = entry else {
                    return false;
                };
                document
                    .get_object(*id)
                    .and_then(Object::as_stream)
                    .is_ok_and(|stream| {
                        stream
                            .dict
                            .get(b"Subtype")
                            .and_then(Object::as_name)
                            .is_ok_and(|subtype| subtype == b"Form")
                    })
            })
        })
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
        scan_pdf_with(page, form, font, "/Resources << /Font << /F1 4 0 R >> >>")
    }

    /// A one-page document whose `/F1` is the given font, and whose form
    /// has the given entries.
    fn scan_pdf_with(page: &str, form: &str, font: &str, form_entries: &str) -> Vec<u8> {
        scan_pdf_objects(page, form, font, form_entries, &[])
    }

    /// `scan_pdf_with`, with `extra` as objects 8, 9, and on.
    fn scan_pdf_objects(
        page: &str,
        form: &str,
        font: &str,
        form_entries: &str,
        extra: &[Vec<u8>],
    ) -> Vec<u8> {
        let pixels = vec![200u8; 64 * 64];
        let mut objects: Vec<Vec<u8>> = vec![
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
                &format!("/Type /XObject /Subtype /Form /BBox [0 0 612 792] {form_entries}"),
                form.as_bytes(),
            ),
        ];
        objects.extend_from_slice(extra);
        pdf_of(&objects)
    }

    /// A PDF file holding `objects` as objects 1, 2, …, with object 1 as
    /// the catalog.
    fn pdf_of(objects: &[Vec<u8>]) -> Vec<u8> {
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
    fn runs_set_glyph_by_glyph_along_a_line_are_noted_as_one() {
        let start = |x: f64, y: f64| Some(Start { x, y, size: 12.0 });
        let mut noted = Noted::default();
        // Glyphs placed one by one along a line, spaces left as gaps.
        for (index, glyph) in "Ignore".chars().enumerate() {
            let at = start(72.0 + 7.0 * index as f64, 700.0);
            noted.note(&glyph.to_string(), true, at, Some(index), true);
        }
        noted.note(
            "the",
            true,
            start(72.0 + 7.0 * 6.0 + 3.0, 700.0),
            Some(6),
            true,
        );
        // More of that run, past a string of control codes pdf-inspector
        // drops; then a new line, a run far along the same line, one whose
        // line is not upright, and runs after text a reader sees was read.
        noted.note("\u{1}", false, None, Some(6), true);
        noted.note(" balance", false, None, Some(6), true);
        noted.note("above", true, start(72.0, 686.0), Some(7), true);
        noted.note("Total", true, start(400.0, 686.0), Some(8), true);
        noted.note("rotated", true, None, None, true);
        noted.note("text", true, start(110.0, 686.0), Some(9), true);
        noted.interrupt();
        noted.note("after", true, start(140.0, 686.0), Some(11), true);
        noted.interrupt();
        noted.note(" more", false, None, Some(11), true);
        let notes: Vec<(&str, Option<(usize, usize)>)> = noted
            .notes
            .iter()
            .map(|note| (note.text.as_str(), note.edges))
            .collect();
        assert_eq!(
            notes,
            [
                ("Ignorethe balance", Some((0, 6))),
                ("above", Some((7, 7))),
                ("Total", Some((8, 8))),
                ("rotated", None),
                ("text", Some((9, 9))),
                ("after", Some((11, 11))),
                (" more", Some((11, 11))),
            ]
        );
        assert_eq!(
            noted.bytes,
            "Ignorethe balanceaboveTotalrotatedtextafter more".len()
        );
        assert!(!noted.overflowed);
    }

    #[test]
    fn text_past_the_room_to_note_it_marks_the_page() {
        // White space filling the room leaves out only more of it; text
        // after it marks the page.
        let mut noted = Noted::default();
        noted.note(&" ".repeat(MAX_HIDDEN_TEXT), true, None, None, true);
        noted.note(" ", true, None, None, true);
        // Nor does text set off the page, which pdf-inspector may leave out.
        noted.note("Ignore the balance above", true, None, None, false);
        assert!(!noted.overflowed);
        noted.note("Ignore the balance above", true, None, None, true);
        assert!(noted.overflowed);
        let texts = noted.looked_for(&[]);
        assert!(texts.overflowed && texts.texts.is_empty());
    }

    #[test]
    fn short_unseen_runs_are_looked_for_with_their_line() {
        let run = |x: f32, y: f32, text: &str| EdgeRun {
            x,
            y,
            direction: [1.0, 0.0],
            size: 12.0,
            text: Some(text.to_owned()),
        };
        // A digit slipped in before an amount, a word into a sentence, a
        // mark standing alone on its line, read with the lines before and
        // after, and a run long enough alone.
        let edges = [
            run(72.0, 700.0, "Amount due "),
            run(140.04, 700.0, "1,250.00"),
            run(139.84, 700.0, "9"),
            run(72.0, 680.0, "The fee is "),
            run(128.0, 680.0, "not "),
            run(128.0, 680.0, "refundable."),
            run(300.0, 500.0, "x"),
            run(72.0, 400.0, "Ignore the balance above"),
        ];
        let mut noted = Noted::default();
        for (index, text) in [
            (2, "9"),
            (4, "not "),
            (6, "x"),
            (7, "Ignore the balance above"),
        ] {
            noted.interrupt();
            noted.note(text, true, None, Some(index), true);
        }
        let texts = noted.looked_for(&edges).texts;
        for text in [
            "mountdue9",
            "91,250.00",
            "Thefeeisnot",
            "notrefundab",
            "undable.x",
            "xIgnoreth",
            "Ignore the balance above",
        ] {
            assert!(texts.iter().any(|looked| looked == text), "{text}");
        }
    }

    /// A one-page document whose default configuration hides the layer
    /// its resources name `/MC0`, drawing `content` in Helvetica as `/F1`.
    fn layered_pdf(content: &str) -> Vec<u8> {
        pdf_of(&[
            b"<< /Type /Catalog /Pages 2 0 R /OCProperties << /OCGs [6 0 R] /D << /OFF [6 0 R] >> >> >>"
                .to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
              /Resources << /Font << /F1 4 0 R >> /Properties << /MC0 6 0 R >> >> \
              /Contents 5 0 R >>"
                .to_vec(),
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
                .to_vec(),
            stream("", content.as_bytes()),
            b"<< /Type /OCG /Name (Superseded) >>".to_vec(),
        ])
    }

    /// The text the scan notes in hidden layers, by page.
    fn hidden_texts(pdf: &[u8]) -> Vec<(u32, Vec<String>)> {
        scan(pdf, &HashSet::new(), Some(&HashSet::new()), None)
            .hidden_layer_texts
            .into_iter()
            .map(|(page, texts)| (page, texts.texts))
            .collect()
    }

    #[test]
    fn text_in_spans_of_a_hidden_layer_is_noted_however_deep() {
        // A hidden span open around many spans of other kinds, and a
        // stream leaving spans open: the text in it is hidden, and the text
        // after the spans close shows.
        let inner = "/Span BMC ".repeat(50_000);
        let closes = "EMC ".repeat(50_001);
        let content = format!(
            "/OC /MC0 BDC {inner} BT /F1 10 Tf 72 680 Td (Ending balance 1,000.00 superseded) Tj ET \
             {closes} BT /F1 10 Tf 72 700 Td (Ending balance 2,000.00) Tj ET {inner}"
        );
        assert_eq!(
            hidden_texts(&layered_pdf(&content)),
            [(1, vec!["Ending balance 1,000.00 superseded".to_string()])]
        );
        // A membership dictionary written in the span itself.
        let written = "/OC << /Type /OCMD /OCGs [6 0 R] >> BDC \
                       BT /F1 10 Tf 72 680 Td (Draft figures) Tj ET EMC";
        assert_eq!(
            hidden_texts(&layered_pdf(written)),
            [(1, vec!["Draft figures".to_string()])]
        );
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
        // With no leading set, `T*` moves as pdf-inspector moves it, a line
        // down, where nothing is taken back.
        let content = "BT /F1 10 Tf 60 700 Td 0 1.05 (dt) \" T* (oday) Tj ET";
        assert!(gaps(&subset("26 /space", 0, 288), content).is_empty());
        // The pen is followed over a run shown after the string, so a run
        // placed far on decides nothing, as pdf-inspector reads it.
        let label =
            "BT /F1 10 Tf 60 700 Td 2 Tc (TOTAL\\032) Tj (DUE) Tj 0 Tc 260 0 Td (1,234.56) Tj ET";
        assert!(gaps(&subset("26 /space", 0, 288), label).is_empty());
    }

    #[test]
    fn words_shown_glyph_by_glyph_are_placed_by_their_widths() {
        // Capitals 600 units wide and a space of 250: at 10 points a glyph
        // moves the pen 6 units and a space 2.5.
        let widths: Vec<&str> = (32..=90)
            .map(|code| if code == 32 { "250" } else { "600" })
            .collect();
        let font = format!(
            "<< /Type /Font /Subtype /TrueType /BaseFont /ABCDEF+Serif /FirstChar 32 \
             /LastChar 90 /Widths [{}] /Encoding /WinAnsiEncoding >>",
            widths.join(" ")
        );
        let words = |content: &str| {
            let pdf = scan_pdf_in(content, "", &font);
            let mut words: Vec<(String, f64)> =
                scan(&pdf, &HashSet::new(), Some(&HashSet::new()), None)
                    .glyph_words
                    .into_iter()
                    .map(|word| (word.text, (word.gap * 1000.0).round() / 1000.0))
                    .collect();
            words.sort_by(|one, other| one.0.cmp(&other.0));
            words
        };
        // Strings as Skia writes them, each placed a unit past the widths
        // of the glyphs before: a word goes on in a string of several
        // glyphs, and a word in one string is read whole.
        let skia = "BT /F1 10 Tf 40 700 Td (TOT) Tj 19 0 Td (AL ) Tj 14.5 0 Td (DUE) Tj ET";
        assert_eq!(words(skia), vec![("TOTAL".to_string(), 0.1)]);
        // A label and its value as runs of their own, a fifth of an em
        // apart: the end of the label's text object ends it.
        let runs = "BT /F1 10 Tf 40 680 Td (PA) Tj 13 0 Td (ID) Tj ET \
                    BT /F1 10 Tf 67 680 Td (I) Tj 7 0 Td (N) Tj ET";
        // A `TJ` array's offsets move the glyphs after them.
        let offsets = "BT /F1 10 Tf 40 660 Td [(F) -100 (EE)] TJ 20 0 Td (S) Tj ET";
        assert_eq!(
            words(&format!("{skia} {runs} {offsets}")),
            [("FEES", 0.1), ("IN", 0.1), ("PAID", 0.1), ("TOTAL", 0.1)]
                .map(|(text, gap)| (text.to_string(), gap))
                .to_vec()
        );
        // Without a painted space, the font's words are not read.
        assert!(words(&format!("{runs} {offsets}")).is_empty());
    }

    #[test]
    fn text_is_read_as_pdf_inspector_reads_it() {
        let subset = "<< /Type /Font /Subtype /Type1 /BaseFont /ABCDEF+SubsetSans /FirstChar 26 \
             /LastChar 57 /Widths [288 556 556 556 556 556 0 556 556 556 556 556 556 556 556 556 \
             556 556 556 556 556 556 556 556 556 556 556 556 556 556 556 556] /Encoding << \
             /Type /Encoding /BaseEncoding /WinAnsiEncoding /Differences [26 /space] >> >>";
        let found = |page: &str, form: &str, form_entries: &str| {
            let pdf = scan_pdf_with(page, form, subset, form_entries);
            scan(&pdf, &HashSet::new(), Some(&HashSet::new()), None)
        };
        let own = "/Resources << /Font << /F1 4 0 R >> >>";
        let kerned = "[(8) -106 (5,000) -108 (.00)] TJ";
        // A comment between an operator's operands, which pdf-inspector
        // strips from a page's content, hides nothing after it.
        let commented = format!("BT /F1 % the body font\n10 Tf 60 700 Td {kerned} ET");
        assert_eq!(found(&commented, "", own).gaps_misread, vec![1]);
        // pdf-inspector starts each text object in render mode 0, so text
        // after `3 Tr` set outside one is read.
        let hidden_before = format!("3 Tr BT /F1 10 Tf 60 700 Td {kerned} ET");
        assert_eq!(found(&hidden_before, "", own).gaps_misread, vec![1]);
        // Glyphs in a span giving their text are not read.
        let span = format!(
            "/Span << /ActualText (85,000.00) >> BDC BT /F1 10 Tf 60 700 Td {kerned} ET EMC"
        );
        assert!(found(&span, "", own).gaps_misread.is_empty());
        // A form reads its text from its own fonts, with none carried in:
        // without resources, it finds none.
        let form = format!("BT 60 700 Td {kerned} ET");
        let page = "BT /F1 10 Tf ET /Fm1 Do";
        assert!(found(page, &form, "").gaps_misread.is_empty());
        let own_font = format!("BT /F1 10 Tf 60 700 Td {kerned} ET");
        assert_eq!(found(page, &own_font, own).gaps_misread, vec![1]);
        // Nor is text in a form pdf-inspector does not reach, which it
        // leaves out, read for its gaps.
        let nested = |outer: &str| {
            let pdf = nested_form_pdf_in(outer, subset, &own_font);
            scan(&pdf, &HashSet::new(), Some(&HashSet::new()), None).gaps_misread
        };
        assert!(nested("").is_empty());
        assert_eq!(
            nested("/Resources << /XObject << /Inner 7 0 R >> >>"),
            vec![1]
        );
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
        // A shadow a fifth of the size off, a second paint split in two
        // strings, and a `TJ` stepping back to show its string again.
        assert!(repeated(&format!("{} {}", line(74.2), line(72.0)), ""));
        assert!(repeated(
            &format!(
                "{} BT /F1 11 Tf 1 0 0 1 72.3 700 Tm (Total amount due: ) Tj ($1,234.56) Tj ET",
                line(72.0)
            ),
            ""
        ));
        assert!(repeated(
            "BT /F1 11 Tf 1 0 0 1 72 700 Tm [(Total due 1,234.56) 8309 (Total due 1,234.56)] TJ ET",
            ""
        ));
        assert!(repeated(
            "BT /F1 11 Tf 1 0 0 1 72 700 Tm [(T) 611 (T) (O) 778 (O)] TJ ET",
            ""
        ));
        // Kerning between two glyphs alike steps back too little.
        assert!(!repeated(
            "BT /F1 11 Tf 1 0 0 1 72 700 Tm [(te) 15 (l) 20 (l)] TJ ET",
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

    /// A one-page document drawing `/Outer`, a form with the given entries
    /// drawing `/Inner`, bound on the page with Helvetica, which shows the
    /// page's one line of text.
    fn nested_form_pdf(outer_entries: &str) -> Vec<u8> {
        nested_form_pdf_in(
            outer_entries,
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>",
            "BT /F1 10 Tf 72 700 Td (Box 1 Wages 85,000.00) Tj ET",
        )
    }

    /// `nested_form_pdf`, with `/F1` the given font, and `/Inner` showing
    /// `inner`.
    fn nested_form_pdf_in(outer_entries: &str, font: &str, inner: &str) -> Vec<u8> {
        let form = "/Type /XObject /Subtype /Form /BBox [0 0 612 792]";
        let objects: Vec<Vec<u8>> = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
              /Resources << /Font << /F1 4 0 R >> /XObject << /Outer 6 0 R /Inner 7 0 R >> >> \
              /Contents 5 0 R >>"
                .to_vec(),
            font.as_bytes().to_vec(),
            stream(
                "",
                b"BT /F1 12 Tf 72 740 Td (Payroll summary) Tj ET q /Outer Do Q",
            ),
            stream(&format!("{form} {outer_entries}"), b"q /Inner Do Q"),
            stream(
                &format!("{form} /Resources << /Font << /F1 4 0 R >> >>"),
                inner.as_bytes(),
            ),
        ];
        pdf_of(&objects)
    }

    /// A one-page document drawing `/Fm1`, a form with Helvetica of its own
    /// showing a line of the page, from resources (object 7) the page or
    /// its page tree node binds with the given entries.
    fn inherited_form_pdf(pages_entries: &str, page_entries: &str) -> Vec<u8> {
        pdf_of(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            format!("<< /Type /Pages /Kids [3 0 R] /Count 1 {pages_entries} >>").into_bytes(),
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] {page_entries} \
                 /Contents 5 0 R >>"
            )
            .into_bytes(),
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
                .to_vec(),
            stream(
                "",
                b"BT /F1 12 Tf 72 740 Td (Payroll summary) Tj ET q /Fm1 Do Q",
            ),
            stream(
                "/Type /XObject /Subtype /Form /BBox [0 0 612 792] \
                 /Resources << /Font << /F1 4 0 R >> >>",
                b"BT /F1 10 Tf 72 700 Td (Box 1 Wages 85,000.00) Tj ET",
            ),
            b"<< /Font << /F1 4 0 R >> /XObject << /Fm1 6 0 R >> >>".to_vec(),
        ])
    }

    #[test]
    fn text_drawn_through_forms_pdf_inspector_misses_is_found() {
        let unread =
            |pdf: &[u8]| scan(pdf, &HashSet::new(), Some(&HashSet::new()), None).forms_unread;
        // A form drawn by a form without resources of its own is not read,
        // nor by one whose `/Resources` is no dictionary, or lacks the
        // `/XObject` category pdfium then takes from the page; bound in the
        // drawing form's own resources, it is.
        for outer in [
            "",
            "/Resources null",
            "/Resources 99 0 R",
            "/Resources << /ProcSet [/PDF] >>",
        ] {
            assert_eq!(unread(&nested_form_pdf(outer)), vec![1], "{outer}");
        }
        assert!(unread(&nested_form_pdf(
            "/Resources << /XObject << /Inner 7 0 R >> >>"
        ))
        .is_empty());
        // pdf-inspector finds the page's forms in its own resources alone:
        // a form the page inherits, directly or by reference, is not read.
        let own = inherited_form_pdf("", "/Resources 7 0 R");
        assert!(unread(&own).is_empty());
        for pages in [
            "/Resources 7 0 R",
            "/Resources << /Font << /F1 4 0 R >> /XObject << /Fm1 6 0 R >> >>",
        ] {
            assert_eq!(unread(&inherited_form_pdf(pages, "")), vec![1], "{pages}");
        }
        // Only a full run with Markdown reads forms.
        let pdf = nested_form_pdf("");
        assert!(scan(&pdf, &HashSet::new(), None, None)
            .forms_unread
            .is_empty());
        // Text a form shows in the font it was drawn with is read byte by
        // byte: plain text reads the same, a space named at code 26 does not.
        let subset = "<< /Type /Font /Subtype /Type1 /BaseFont /ABCDEF+SubsetSans \
             /FirstChar 26 /LastChar 57 /Widths [278 556 556 556 556 556 556 556 556 556 556 \
             556 556 556 556 556 556 556 556 556 556 556 556 556 556 556 556 556 556 556 556 \
             556] /Encoding << /Type /Encoding /BaseEncoding /WinAnsiEncoding \
             /Differences [26 /space] >> >>";
        let found = |page: &str, form: &str, font: &str, entries: &str| {
            let pdf = scan_pdf_with(page, form, font, entries);
            scan(&pdf, &HashSet::new(), Some(&HashSet::new()), None).forms_unread
        };
        let page = "BT /F1 10 Tf ET /Fm1 Do";
        let plain = "BT 72 700 Td (Total deposits 85,000.00) Tj ET";
        let coded = "BT 72 700 Td (Total deposits\\03285,000.00) Tj ET";
        let helvetica =
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>";
        assert!(found(page, plain, helvetica, "").is_empty());
        assert_eq!(found(page, coded, subset, ""), vec![1]);
        assert_eq!(found(page, coded, subset, "/Resources << >>"), vec![1]);
        // A form setting the font from its own resources is read with it;
        // from resources without `/Font`, where pdfium takes the page's
        // font, it is read byte by byte.
        let own = "BT /F1 10 Tf 72 700 Td (Total deposits\\03285,000.00) Tj ET";
        assert!(found(page, own, subset, "/Resources << /Font << /F1 4 0 R >> >>").is_empty());
        assert_eq!(
            found(page, own, subset, "/Resources << /ProcSet [/PDF] >>"),
            vec![1]
        );
        // Bytes past ASCII read byte by byte as Windows-1252 has them, as the
        // font reads them too, are read right; a font that names another
        // glyph at such a code is not.
        let winansi = |differences: &str| {
            format!(
                "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding << /Type \
                 /Encoding /BaseEncoding /WinAnsiEncoding /Differences [{differences}] >> >>"
            )
        };
        let accented = "BT 72 700 Td (Soci\\351t\\351 holder\\222s 2024\\2262025) Tj ET";
        let names = winansi("146 /quoteright 150 /endash 233 /eacute");
        assert!(found(page, accented, &names, "").is_empty());
        let name = "BT 72 700 Td (Soci\\351t\\351 G\\351n\\351rale) Tj ET";
        assert_eq!(found(page, name, &winansi("233 /egrave"), ""), vec![1]);
    }

    #[test]
    fn two_byte_text_read_byte_by_byte_is_compared_with_its_font() {
        // A composite font whose codes are the text's code points, as UTF-16
        // writes them: pdf-inspector reads the bytes as UTF-16, as the font
        // does. Codes offset from the code points read otherwise.
        let font = "<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+UnicodeSans \
                    /Encoding /Identity-H /ToUnicode 8 0 R >>";
        // The codes from `first` read as the code points from U+0020.
        let found = |form: &str, first: &str| {
            let cmap = stream(
                "",
                format!(
                    "/CIDInit /ProcSet findresource begin 12 dict begin begincmap \
                     /CMapName /Custom def /CMapType 2 def 1 begincodespacerange <0000> <FFFF> \
                     endcodespacerange 1 beginbfrange <{first}> <00FF> <0020> endbfrange \
                     endcmap CMapName currentdict /CMap defineresource pop end end"
                )
                .as_bytes(),
            );
            let pdf = scan_pdf_objects("BT /F1 10 Tf ET /Fm1 Do", form, font, "", &[cmap]);
            scan(&pdf, &HashSet::new(), Some(&HashSet::new()), None).forms_unread
        };
        let unicode =
            "BT 72 700 Td <0054006F00740061006C00200031002C003200350030002E00300030> Tj ET";
        assert!(found(unicode, "0020").is_empty());
        // "Total" at codes 29 below its code points.
        let offset = "BT 72 700 Td <0037005200570044004F> Tj ET";
        assert_eq!(found(offset, "0003"), vec![1]);
    }

    #[test]
    fn strings_read_without_a_font_read_as_pdf_inspector_reads_them() {
        for (bytes, text) in [
            (&b"Total 1,250.00"[..], "Total 1,250.00"),
            (
                b"\x92\x96\x93\x97\xe9\xa7",
                "\u{2019}\u{2013}\u{201c}\u{2014}\u{e9}\u{a7}",
            ),
            (b"A\x81\x8dB", "A\u{81}\u{8d}B"),
            (b"\xc3\xa9t\xc3\xa9", "\u{e9}t\u{e9}"),
            (b"\xfe\xff\x00A\x00B", "AB"),
            (b"\x00T\x00o\x00t\x00a\x00l", "Total"),
            // Too short for UTF-16: the null is a control byte, dropped.
            (b"\x00-", "\u{0}-"),
            // UTF-16 that does not read as text is read byte by byte.
            (b"\x00\x13\x00\x14", "\u{0}\u{13}\u{0}\u{14}"),
        ] {
            assert_eq!(read_without_font(bytes), text, "{bytes:?}");
        }
        assert_eq!(without_controls("\u{0}-\u{1}\tA"), "-\tA");
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
