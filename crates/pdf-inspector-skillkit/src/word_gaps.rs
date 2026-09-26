//! Gaps between glyphs that pdf-inspector 1.25.0 judges against the wrong
//! space width (open upstream #532).
//!
//! pdf-inspector reads the pen travel between glyphs as a word space by a
//! threshold it takes from the font's space width: 0.4 of it, and at least
//! 0.08 em. For a simple font it reads that width at code 32, or takes 250
//! units when code 32 has none. A subset font whose `/Differences` name the
//! space at another code, as producers numbering glyphs in order of first
//! use write them, leaves code 32 unused or holding another glyph, so the
//! threshold follows the wrong width; the open fix reads the width of the
//! code the differences name the space. Where the two thresholds differ, a
//! gap between them is a word space by one and not the other: kerning
//! inside an amount splits it ("8 5,000 .00"), a word space is lost
//! ("13,100.001,020.00"), or a tracked word comes apart letter by letter.
//!
//! A page is reported when it shows text in such a font with a gap the two
//! thresholds judge differently, as pdf-inspector judges gaps: each `TJ`
//! offset between two strings, over the tracking it reads from a run of
//! single glyphs, and the character spacing inside a string of two or three
//! glyphs once the next run takes the spacing after it back. Glyphs are
//! read as pdf-inspector reads them, by the font's ToUnicode map, then the
//! names its differences give, then as printable ASCII; where a glyph cannot
//! be read, each reading it could have is allowed for. A dependent sign's
//! placement and return, which pdf-inspector nets into one gap, are judged
//! as they stand.

use std::collections::HashMap;

use lopdf::{Dictionary, Document, Object};
use pdf_inspector::glyph_names::glyph_name_to_string;
use pdf_inspector::tounicode::ToUnicodeCMap;

/// Share of the space width that makes a word gap, and the least word gap,
/// in thousandths of the font size, as pdf-inspector 1.25.0 sets them.
const WORD_GAP_SHARE: f32 = 0.4;
const MIN_WORD_GAP: f32 = 80.0;
/// A `TJ` offset of this many word gaps ends pdf-inspector's sub-run.
const COLUMN_GAP_WORD_GAPS: f32 = 4.0;
/// Where pdf-inspector reads a run of single glyphs as tracked: its typical
/// gap from half a word gap to three (at most 450 thousandths), and from
/// 1.25 word gaps (at most 140) only in capitals.
const TRACKING_MIN: f32 = 0.5;
const TRACKING_MAX: f32 = 3.0;
const TRACKING_MAX_ABSOLUTE: f32 = 450.0;
const TRACKING_NEEDS_CAPITALS: f32 = 1.25;
const TRACKING_NEEDS_CAPITALS_MAX: f32 = 140.0;
/// Glyphs in the longest string whose character spacing pdf-inspector reads
/// as word gaps.
const MAX_BOUNDARY_GLYPHS: usize = 3;
/// A take-back covers at least this share of the spacing, and at most the
/// spacing and this kerning allowance, in em.
const TAKE_BACK_MIN: f32 = 0.7;
const KERN_ALLOWANCE_EM: f32 = 0.12;
/// How far off a string's baseline, in em, the next run may start and still
/// take the spacing after it back.
const SAME_LINE_EM: f64 = 0.2;
/// The space width pdf-inspector takes for a font whose code 32 has none.
const FALLBACK_SPACE_WIDTH: u16 = 250;
/// Codes a `/Widths` array can give; past them pdf-inspector's codes wrap.
const MAX_WIDTHS: usize = 1 << 16;
/// Bytes a ToUnicode map may decode to.
const MAX_CMAP_BYTES: usize = 1 << 20;
/// Font array entries and ToUnicode bytes read per document.
const MAX_FONT_STEPS: usize = 16_000_000;

/// A font whose word gaps pdf-inspector 1.25.0 and its fix judge against
/// different thresholds, in thousandths of the font size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct GapFont {
    /// From the width pdf-inspector reads as the space.
    read: f32,
    /// From the width of the glyph the differences name the space.
    fixed: f32,
    /// Where the font's metrics are kept in [`GapFonts`].
    index: usize,
}

/// What the scan knows of such a font.
struct Metrics {
    widths: HashMap<u16, u16>,
    /// Text space units per glyph unit.
    scale: f32,
    /// What each one-byte code reads as, where the scan can tell.
    readings: Vec<Option<String>>,
}

/// Fonts read for their word gaps, by the address of their dictionary, and
/// the work spent reading them.
#[derive(Default)]
pub(crate) struct GapFonts {
    known: HashMap<usize, Option<GapFont>>,
    metrics: Vec<Metrics>,
    steps: usize,
}

/// The text state text is shown in.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Shown {
    pub(crate) size: f32,
    pub(crate) char_spacing: f32,
    pub(crate) word_spacing: f32,
    /// Whether the baseline reads along x.
    pub(crate) horizontal: bool,
}

/// What text shown in such a font holds.
#[derive(Debug, Default)]
pub(crate) struct Judged {
    /// A gap the two thresholds judge differently.
    pub(crate) misjudged: bool,
    /// The string ending the text, when the thresholds read its spacing
    /// differently and the next run decides whether it is taken back.
    pub(crate) candidate: Option<Candidate>,
    /// The pen's travel over the text, in unscaled text space units.
    pub(crate) advance: f32,
}

/// A short string whose character spacing one threshold reads as a word gap
/// and the other does not: pdf-inspector spaces it only if the next run
/// takes the spacing after it back.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Candidate {
    /// The spacing after its last glyph, in unscaled text space units.
    spacing: f32,
    pub(crate) size: f32,
}

impl Candidate {
    /// Whether pen travel of `back` units against the reading direction
    /// takes the spacing back: most of it, and no more than it and a kern.
    pub(crate) fn taken_back(&self, back: f32) -> bool {
        self.spacing > 0.0
            && back >= self.spacing * TAKE_BACK_MIN
            && back <= self.spacing + self.size.abs() * KERN_ALLOWANCE_EM
    }

    /// Whether the next run, starting `across` units off the string's
    /// baseline, can take the spacing back.
    pub(crate) fn on_line(&self, across: f64) -> bool {
        across.abs() <= f64::from(self.size.abs()) * SAME_LINE_EM
    }
}

impl GapFonts {
    /// The font, when pdf-inspector misjudges its word gaps. Past the
    /// document's work limit, fonts not yet read are taken as judged right.
    pub(crate) fn font(&mut self, document: &Document, font: &Dictionary) -> Option<GapFont> {
        let key = std::ptr::from_ref(font) as usize;
        if let Some(known) = self.known.get(&key) {
            return *known;
        }
        if self.steps > MAX_FONT_STEPS {
            return None;
        }
        let found = read_font(document, font, &mut self.steps).map(|(read, fixed, metrics)| {
            self.metrics.push(metrics);
            GapFont {
                read,
                fixed,
                index: self.metrics.len() - 1,
            }
        });
        self.known.insert(key, found);
        found
    }

    /// Judge `text`, a string or a `TJ` array, shown in `font`.
    pub(crate) fn judge(&self, font: GapFont, text: &Object, shown: Shown) -> Judged {
        let metrics = &self.metrics[font.index];
        match text {
            Object::String(raw, _) => Judged {
                misjudged: false,
                candidate: font.candidate(metrics, raw, shown),
                advance: metrics.advance(raw, shown),
            },
            Object::Array(elements) => {
                let mut judged = Judged {
                    misjudged: font.offsets_misjudged(metrics, elements, shown.horizontal),
                    ..Judged::default()
                };
                let last = elements.iter().rposition(shows);
                for (index, element) in elements.iter().enumerate() {
                    let raw = match element {
                        Object::String(raw, _) => raw,
                        element => {
                            if let Some(offset) = offset(element) {
                                judged.advance -= offset / 1000.0 * shown.size;
                            }
                            continue;
                        }
                    };
                    judged.advance += metrics.advance(raw, shown);
                    let Some(candidate) = font.candidate(metrics, raw, shown) else {
                        continue;
                    };
                    // Taken back by the offset after it, or, ending the
                    // array, by where the next run starts.
                    match elements.get(index + 1).and_then(offset) {
                        Some(back) if candidate.taken_back(back / 1000.0 * shown.size) => {
                            judged.misjudged = true;
                        }
                        _ if Some(index) == last => judged.candidate = Some(candidate),
                        _ => {}
                    }
                }
                judged
            }
            _ => Judged::default(),
        }
    }
}

impl GapFont {
    /// Whether a word gap by one threshold is none by the other.
    fn disagree(&self, travel: f32) -> bool {
        (travel > self.read) != (travel > self.fixed)
    }

    /// A string of two or three glyphs with a junction whose character
    /// spacing the thresholds judge differently, where a space can go.
    fn candidate(&self, metrics: &Metrics, raw: &[u8], shown: Shown) -> Option<Candidate> {
        if shown.size <= 0.0 || !(2..=MAX_BOUNDARY_GLYPHS).contains(&raw.len()) {
            return None;
        }
        // Word spacing counts after a space code whose glyph paints.
        let travel = |code: u8, paints: bool| {
            let word = if code == b' ' && paints {
                shown.word_spacing
            } else {
                0.0
            };
            (shown.char_spacing + word) / shown.size * 1000.0
        };
        let differs = raw.windows(2).any(|pair| {
            let (this, next) = (metrics.reading(pair[0]), metrics.reading(pair[1]));
            if let (Some(this), Some(next)) = (this, next) {
                if !takes_space(this.chars().last(), next.chars().next()) {
                    return false;
                }
            }
            let paints: &[bool] = match this {
                Some(text) => &[!text.chars().all(char::is_whitespace)],
                None => &[true, false],
            };
            paints
                .iter()
                .any(|&paints| self.disagree(travel(pair[0], paints)))
        });
        let last = raw[raw.len() - 1];
        differs.then_some(Candidate {
            spacing: shown.char_spacing
                + if last == b' ' {
                    shown.word_spacing
                } else {
                    0.0
                },
            size: shown.size,
        })
    }

    /// The offsets of a `TJ` array, judged under each threshold, over the
    /// tracking pdf-inspector reads from a run of single glyphs when it
    /// reads them as letters, and, for the widest tracking, as capitals.
    fn offsets_misjudged(&self, metrics: &Metrics, elements: &[Object], horizontal: bool) -> bool {
        let gaps = tracking_gaps(elements);
        let answers = match gaps {
            None => vec![(false, false)],
            Some(_) => match metrics.run_reading(elements) {
                Some(answer) => vec![answer],
                None => vec![(false, false), (true, false), (true, true)],
            },
        };
        answers.into_iter().any(|(letters, capitals)| {
            let bounds = |word_gap: f32| {
                let tracking = gaps
                    .as_deref()
                    .filter(|_| letters)
                    .and_then(|gaps| tracking(gaps, word_gap))
                    .filter(|(_, needs_capitals)| capitals || !needs_capitals)
                    .map(|(tracking, _)| tracking);
                match tracking {
                    Some(tracking) if horizontal => (word_gap + tracking, word_gap + tracking),
                    Some(tracking) => (
                        word_gap + tracking,
                        word_gap * COLUMN_GAP_WORD_GAPS + tracking,
                    ),
                    None => (word_gap, word_gap * COLUMN_GAP_WORD_GAPS),
                }
            };
            separations(elements, bounds(self.read), metrics, horizontal)
                != separations(elements, bounds(self.fixed), metrics, horizontal)
        })
    }
}

impl Metrics {
    fn reading(&self, code: u8) -> Option<&str> {
        self.readings[usize::from(code)].as_deref()
    }

    /// The pen's travel over a string, as pdf-inspector measures it.
    fn advance(&self, raw: &[u8], shown: Shown) -> f32 {
        let units: f32 = raw
            .iter()
            .map(|&code| f32::from(self.widths.get(&u16::from(code)).copied().unwrap_or(0)))
            .sum();
        let spaces = raw.iter().filter(|&&code| code == b' ').count();
        units * self.scale * shown.size
            + raw.len() as f32 * shown.char_spacing
            + spaces as f32 * shown.word_spacing
    }

    /// Whether a run's glyphs read as letters (no fewer letters and digits
    /// than punctuation) and as display glyphs throughout, when every glyph
    /// reads.
    fn run_reading(&self, elements: &[Object]) -> Option<(bool, bool)> {
        let (mut letters, mut punctuation, mut display) = (0usize, 0usize, true);
        for element in elements {
            let Object::String(raw, _) = element else {
                continue;
            };
            for &code in raw {
                for character in self.reading(code)?.chars() {
                    if character.is_alphanumeric() {
                        letters += 1;
                    } else if !character.is_whitespace() {
                        punctuation += 1;
                    }
                    display &= tracked_display_glyph(character);
                }
            }
        }
        Some((letters >= punctuation, display))
    }

    /// Whether a string's text ends in a space, when its last glyph reads.
    fn ends_in_space(&self, raw: &[u8]) -> bool {
        raw.last()
            .and_then(|&code| self.reading(code))
            .is_some_and(|text| text.ends_with(' '))
    }

    /// Whether a string's text starts with white space, when its first
    /// glyph reads.
    fn starts_with_space(&self, raw: &[u8]) -> bool {
        raw.first()
            .and_then(|&code| self.reading(code))
            .is_some_and(|text| text.starts_with(char::is_whitespace))
    }
}

/// Read a simple font as pdf-inspector 1.25.0 does (`parse_simple_font_widths`)
/// and as its fix does: the two thresholds, when they differ, and what the
/// scan needs of the font.
fn read_font(
    document: &Document,
    font: &Dictionary,
    steps: &mut usize,
) -> Option<(f32, f32, Metrics)> {
    let subtype = font.get(b"Subtype").ok()?.as_name().ok()?;
    if !matches!(subtype, b"Type1" | b"TrueType" | b"MMType1" | b"Type3") {
        return None;
    }
    let first = code_bound(document, font.get(b"FirstChar").ok()?)?;
    let last = code_bound(document, font.get(b"LastChar").ok()?)?;
    let values = resolved_array(document, font.get(b"Widths").ok()?)?;
    let mut widths: HashMap<u16, u16> = HashMap::new();
    let mut space: u16 = 0;
    for (index, value) in values.iter().take(MAX_WIDTHS).enumerate() {
        *steps += 1;
        let code = first.wrapping_add(index as u16);
        if code > last {
            break;
        }
        let value = match value {
            Object::Reference(id) => match document.get_object(*id) {
                Ok(value) => value,
                Err(_) => continue,
            },
            value => value,
        };
        let width = match value {
            Object::Integer(width) => *width as u16,
            Object::Real(width) => *width as u16,
            _ => continue,
        };
        if code == 32 {
            space = width;
        }
        widths.insert(code, width);
    }
    let names = differences(document, font, steps)?;
    let fixed = encoded_space_width(&names, &widths)?;
    let scale = match font
        .get(b"FontMatrix")
        .ok()
        .and_then(|matrix| resolved_array(document, matrix))
        .and_then(|matrix| matrix.first())
    {
        Some(Object::Real(scale)) => scale.abs(),
        Some(Object::Integer(scale)) => (*scale as f32).abs(),
        _ => 0.001,
    };
    if space == 0 {
        space = if !widths.is_empty() && (scale - 0.001).abs() > 0.0005 {
            let sum: u64 = widths.values().map(|&width| u64::from(width)).sum();
            ((sum as f32 / widths.len() as f32) * 0.45).max(1.0) as u16
        } else {
            FALLBACK_SPACE_WIDTH
        };
    }
    let threshold =
        |width: u16| (f32::from(width) * scale * 1000.0 * WORD_GAP_SHARE).max(MIN_WORD_GAP);
    let (read, fixed) = (threshold(space), threshold(fixed));
    if read == fixed {
        return None;
    }
    let readings = readings(document, font, &names, steps);
    Some((
        read,
        fixed,
        Metrics {
            widths,
            scale,
            readings,
        },
    ))
}

/// The name each code last takes in the font's `/Differences`, read as
/// pdf-inspector reads them.
fn differences(
    document: &Document,
    font: &Dictionary,
    steps: &mut usize,
) -> Option<HashMap<u8, Vec<u8>>> {
    let encoding = match font.get(b"Encoding").ok()? {
        Object::Reference(id) => document.get_dictionary(*id).ok()?,
        Object::Dictionary(encoding) => encoding,
        _ => return None,
    };
    let entries = resolved_array(document, encoding.get(b"Differences").ok()?)?;
    let mut names = HashMap::new();
    let mut code: u8 = 0;
    for entry in entries {
        *steps += 1;
        if *steps > MAX_FONT_STEPS {
            return None;
        }
        match entry {
            Object::Integer(value) => code = *value as u8,
            Object::Name(name) => {
                names.insert(code, name.clone());
                code = code.wrapping_add(1);
            }
            _ => {}
        }
    }
    Some(names)
}

/// The width the fix reads as the space: that of every code the
/// differences name the space, when they give one positive width.
fn encoded_space_width(names: &HashMap<u8, Vec<u8>>, widths: &HashMap<u16, u16>) -> Option<u16> {
    let mut found: Vec<u16> = names
        .iter()
        .filter(|(_, name)| names_space(name))
        .filter_map(|(code, _)| widths.get(&u16::from(*code)).copied())
        .filter(|width| *width > 0)
        .collect();
    found.sort_unstable();
    found.dedup();
    match found.as_slice() {
        [width] => Some(*width),
        _ => None,
    }
}

/// Whether a glyph name reads as the space: `space` or `spacehackarabic`,
/// `uni0020` (or `uniF020`, the Symbol convention), or `u0020` with up to
/// six digits, before any `.suffix`.
fn names_space(name: &[u8]) -> bool {
    let base = name.split(|&byte| byte == b'.').next().unwrap_or_default();
    if matches!(base, b"space" | b"spacehackarabic") {
        return true;
    }
    let value = |digits: &[u8]| {
        digits
            .iter()
            .all(u8::is_ascii_hexdigit)
            .then(|| std::str::from_utf8(digits).ok())
            .flatten()
            .and_then(|digits| u32::from_str_radix(digits, 16).ok())
    };
    if let Some(digits) = base.strip_prefix(b"uni") {
        return digits.len() == 4 && matches!(value(digits), Some(0x20 | 0xF020));
    }
    if let Some(digits) = base.strip_prefix(b"u") {
        return (4..=6).contains(&digits.len()) && value(digits) == Some(0x20);
    }
    false
}

/// What each one-byte code reads as: by a one-byte ToUnicode map, then the
/// name the differences give it, then, for a printable ASCII code they leave
/// alone, the byte itself. A map pdf-inspector reads otherwise leaves every
/// code unread.
fn readings(
    document: &Document,
    font: &Dictionary,
    names: &HashMap<u8, Vec<u8>>,
    steps: &mut usize,
) -> Vec<Option<String>> {
    let content = font
        .get(b"ToUnicode")
        .ok()
        .and_then(|value| document.dereference(value).ok())
        .and_then(|(_, value)| value.as_stream().ok())
        .and_then(|stream| stream.get_plain_content_with_limit(MAX_CMAP_BYTES).ok());
    let cmap = content.and_then(|content| {
        *steps += content.len();
        ToUnicodeCMap::parse(&content)
    });
    if cmap.as_ref().is_some_and(|cmap| cmap.code_byte_length != 1) {
        return vec![None; 256];
    }
    (0..=u8::MAX)
        .map(|code| {
            let mapped = cmap
                .as_ref()
                .and_then(|cmap| cmap.lookup(u16::from(code)))
                .filter(|text| !text.contains('\u{FFFD}'));
            if mapped.is_some() {
                return mapped;
            }
            match names.get(&code) {
                Some(name) => glyph_name_to_string(&String::from_utf8_lossy(name)),
                None => (0x20..=0x7E)
                    .contains(&code)
                    .then(|| char::from(code).to_string()),
            }
        })
        .collect()
}

/// `FirstChar` or `LastChar`: an integer, or a reference to one, as a code.
pub(crate) fn code_bound(document: &Document, value: &Object) -> Option<u16> {
    let value = match value {
        Object::Reference(id) => document.get_object(*id).ok()?,
        value => value,
    };
    match value {
        Object::Integer(value) => Some(*value as u16),
        _ => None,
    }
}

/// An array, or a reference to one.
pub(crate) fn resolved_array<'a>(
    document: &'a Document,
    value: &'a Object,
) -> Option<&'a [Object]> {
    match value {
        Object::Array(values) => Some(values),
        Object::Reference(id) => match document.get_object(*id) {
            Ok(Object::Array(values)) => Some(values),
            _ => None,
        },
        _ => None,
    }
}

/// A `TJ` offset, in thousandths of the font size.
fn offset(element: &Object) -> Option<f32> {
    match element {
        Object::Integer(offset) => Some(*offset as f32),
        Object::Real(offset) => Some(*offset),
        _ => None,
    }
}

/// Whether an element of a `TJ` array shows glyphs.
fn shows(element: &Object) -> bool {
    matches!(element, Object::String(raw, _) if !raw.is_empty())
}

/// Pen travel, in unscaled text space units, of the offsets a `TJ` array
/// makes before its first glyph, or none for a string.
pub(crate) fn leading_travel(text: &Object, size: f32) -> f32 {
    let Object::Array(elements) = text else {
        return 0.0;
    };
    elements
        .iter()
        .take_while(|element| !shows(element))
        .filter_map(offset)
        .map(|offset| -offset / 1000.0 * size)
        .sum()
}

/// Whether `text`, a string or a `TJ` array, shows any glyph.
pub(crate) fn shows_glyphs(text: &Object) -> bool {
    match text {
        Object::Array(elements) => elements.iter().any(shows),
        text => shows(text),
    }
}

/// How pdf-inspector's Markdown shows the text on either side of a junction
/// between two strings of a `TJ` array.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Junction {
    Joined,
    Spaced,
    /// In items of their own, which the line sets apart as columns.
    Apart,
}

/// How pdf-inspector shows each junction between two strings of a `TJ`
/// array, by a word gap and a sub-run-ending gap. A word gap, or a space the
/// text already has there, puts a space between. A sub-run-ending gap
/// starts an item of its own, which the line joins back without a space
/// before closing punctuation, sets apart as a column between digits, or on
/// a baseline not along x, and joins with a space otherwise. An offset
/// after the sub-run ends, or after a space, adds nothing.
fn separations(
    elements: &[Object],
    (word_gap, split_gap): (f32, f32),
    metrics: &Metrics,
    horizontal: bool,
) -> Vec<Junction> {
    let mut separations = Vec::new();
    let (mut started, mut text, mut spaced, mut split) = (false, false, false, false);
    let mut last: Option<u8> = None;
    for element in elements {
        if let Some(offset) = offset(element) {
            if !text {
                continue;
            }
            if -offset > split_gap {
                (text, split) = (false, true);
            } else if -offset > word_gap && !spaced {
                spaced = true;
            }
            continue;
        }
        let Object::String(raw, _) = element else {
            continue;
        };
        if raw.is_empty() {
            continue;
        }
        if started {
            let junction = if spaced || metrics.starts_with_space(raw) {
                Junction::Spaced
            } else if split {
                let before = last
                    .and_then(|code| metrics.reading(code))
                    .and_then(|text| text.chars().last());
                let after = metrics.reading(raw[0]).and_then(|text| text.chars().next());
                let digits = before.is_some_and(|character| character.is_ascii_digit())
                    && after.is_some_and(|character| character.is_ascii_digit());
                if after.is_some_and(|character| {
                    matches!(
                        character,
                        '.' | ',' | ';' | '!' | '?' | ')' | ']' | '}' | '\''
                    )
                }) {
                    Junction::Joined
                } else if digits || !horizontal {
                    Junction::Apart
                } else {
                    Junction::Spaced
                }
            } else {
                Junction::Joined
            };
            separations.push(junction);
        }
        started = true;
        text = true;
        spaced = metrics.ends_in_space(raw);
        split = false;
        last = raw.last().copied();
    }
    separations
}

/// The gaps between the glyphs of a `TJ` array showing one glyph per
/// string, one offset or none between each two, sorted, when there are two
/// or more and none closes up: the runs pdf-inspector reads tracking from.
fn tracking_gaps(elements: &[Object]) -> Option<Vec<f32>> {
    let mut gaps = Vec::new();
    let (mut strings, mut numbers, mut pending) = (0usize, 0usize, 0.0f32);
    for element in elements {
        if let Some(offset) = offset(element) {
            pending -= offset;
            numbers += 1;
            continue;
        }
        if !shows(element) {
            continue;
        }
        if !matches!(element, Object::String(raw, _) if raw.len() == 1) {
            return None;
        }
        if strings > 0 {
            if numbers > 1 {
                return None;
            }
            gaps.push(pending);
        }
        strings += 1;
        (numbers, pending) = (0, 0.0);
    }
    if gaps.len() < 2 || gaps.iter().any(|gap| *gap < 0.0) {
        return None;
    }
    gaps.sort_by(f32::total_cmp);
    Some(gaps)
}

/// The tracking pdf-inspector reads from a run's sorted gaps under a word
/// gap, and whether it reads it only in capitals.
fn tracking(sorted: &[f32], word_gap: f32) -> Option<(f32, bool)> {
    let seed = lower_median(sorted);
    let letter_gaps: Vec<f32> = sorted
        .iter()
        .copied()
        .filter(|gap| *gap <= seed + word_gap)
        .collect();
    let tracking = lower_median(&letter_gaps);
    let most = (word_gap * TRACKING_MAX).min(TRACKING_MAX_ABSOLUTE);
    if tracking < word_gap * TRACKING_MIN || tracking > most {
        return None;
    }
    let needs_capitals =
        tracking >= (word_gap * TRACKING_NEEDS_CAPITALS).min(TRACKING_NEEDS_CAPITALS_MAX);
    Some((tracking, needs_capitals))
}

/// The middle value, or the lower of the two middle values.
fn lower_median(sorted: &[f32]) -> f32 {
    sorted[(sorted.len() - 1) / 2]
}

/// Whether a word gap between two glyphs takes a space: not beside white
/// space, Han or Kana, or right-to-left text, nor before joining
/// punctuation.
fn takes_space(previous: Option<char>, next: Option<char>) -> bool {
    let (Some(previous), Some(next)) = (previous, next) else {
        return false;
    };
    if previous.is_whitespace() || next.is_whitespace() {
        return false;
    }
    if [previous, next]
        .iter()
        .any(|&character| spaceless(character) || right_to_left(character))
    {
        return false;
    }
    !matches!(
        next,
        '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '%' | '\u{2019}' | '\u{201D}' | '»'
    )
}

/// A glyph tracked display text is set in: a capital, a digit, Han or
/// Kana, white space, or title punctuation.
fn tracked_display_glyph(character: char) -> bool {
    character.is_uppercase()
        || character.is_numeric()
        || spaceless(character)
        || character.is_whitespace()
        || matches!(
            character,
            '.' | ','
                | ':'
                | ';'
                | '!'
                | '?'
                | '\''
                | '\u{2019}'
                | '"'
                | '\u{201C}'
                | '\u{201D}'
                | '-'
                | '\u{2010}'
                | '\u{2013}'
                | '\u{2014}'
                | '&'
                | '/'
                | '\u{00B7}'
                | '\u{2022}'
                | '\u{2026}'
        )
}

/// Han, Kana, and the CJK punctuation and forms set without spaces.
fn spaceless(character: char) -> bool {
    matches!(character,
        '\u{3000}'..='\u{303F}'
        | '\u{3040}'..='\u{309F}'
        | '\u{30A0}'..='\u{30FF}'
        | '\u{4E00}'..='\u{9FFF}'
        | '\u{F900}'..='\u{FAFF}'
        | '\u{FF00}'..='\u{FFEF}'
    )
}

/// Hebrew, Arabic, and the other right-to-left scripts pdf-inspector
/// orders itself.
fn right_to_left(character: char) -> bool {
    matches!(character,
        '\u{0590}'..='\u{08FF}'
        | '\u{FB1D}'..='\u{FB4F}'
        | '\u{FB50}'..='\u{FDFF}'
        | '\u{FE70}'..='\u{FEFF}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::StringFormat;

    fn subset(differences: &str, widths: &[(u16, u16)]) -> Dictionary {
        let mut values = vec![Object::Integer(556); 120];
        for &(code, width) in widths {
            values[usize::from(code) - 1] = Object::Integer(i64::from(width));
        }
        let mut encoding = Dictionary::new();
        let mut entries = Vec::new();
        for part in differences.split_whitespace() {
            entries.push(match part.strip_prefix('/') {
                Some(name) => Object::Name(name.as_bytes().to_vec()),
                None => Object::Integer(part.parse().expect("code")),
            });
        }
        encoding.set("Differences", Object::Array(entries));
        let mut font = Dictionary::new();
        font.set("Type", Object::Name(b"Font".to_vec()));
        font.set("Subtype", Object::Name(b"TrueType".to_vec()));
        font.set("FirstChar", Object::Integer(1));
        font.set("LastChar", Object::Integer(120));
        font.set("Widths", Object::Array(values));
        font.set("Encoding", Object::Dictionary(encoding));
        font
    }

    fn found(font: &Dictionary) -> Option<(GapFonts, GapFont)> {
        let mut fonts = GapFonts::default();
        let found = fonts.font(&Document::new(), font)?;
        Some((fonts, found))
    }

    /// Whether a font's thresholds are, to rounding, the ones given.
    fn reads(font: &Dictionary, read: f32, fixed: f32) -> bool {
        found(font).is_some_and(|(_, found)| {
            (found.read - read).abs() < 0.01 && (found.fixed - fixed).abs() < 0.01
        })
    }

    fn string(text: &str) -> Object {
        Object::String(text.as_bytes().to_vec(), StringFormat::Literal)
    }

    fn shown(size: f32, char_spacing: f32, word_spacing: f32) -> Shown {
        Shown {
            size,
            char_spacing,
            word_spacing,
            horizontal: true,
        }
    }

    #[test]
    fn thresholds_follow_pdf_inspector_and_its_fix() {
        // Code 32 unused: pdf-inspector takes 250 units, the fix 278.
        assert!(reads(
            &subset("26 /space", &[(26, 278), (32, 0)]),
            100.0,
            111.2
        ));
        // Code 32 holds a wide glyph.
        assert!(reads(
            &subset("26 /space 32 /M", &[(26, 278), (32, 833)]),
            333.2,
            111.2
        ));
        // A narrow space with code 32 unused: the fallback's threshold
        // against the least.
        assert!(reads(
            &subset("3 /space.alt", &[(3, 150), (32, 0)]),
            100.0,
            80.0
        ));
        // A space as wide as the fallback, a narrow space whose threshold
        // is the least either way, the space where pdf-inspector reads it,
        // or at two codes of different widths: no difference.
        for font in [
            subset("26 /space", &[(26, 250), (32, 0)]),
            subset("26 /space", &[(26, 150), (32, 180)]),
            subset("26 /space", &[(26, 278), (32, 278)]),
            subset("26 /space 32 /space", &[(26, 278), (32, 250)]),
            // A later name takes the code from the space.
            subset("26 /space 26 /a", &[(26, 278), (32, 0)]),
        ] {
            assert!(found(&font).is_none());
        }
        let mut composite = subset("26 /space", &[(26, 278), (32, 0)]);
        composite.set("Subtype", Object::Name(b"Type0".to_vec()));
        assert!(found(&composite).is_none());
    }

    #[test]
    fn space_names_read_as_pdf_inspector_reads_them() {
        for name in [
            "space",
            "space.alt",
            "spacehackarabic",
            "uni0020",
            "uniF020",
            "u0020",
            "u000020",
        ] {
            assert!(names_space(name.as_bytes()), "{name}");
        }
        for name in [
            "nbspace",
            "uni00A0",
            "u20",
            "space_space",
            "uni00200020",
            ".notdef",
        ] {
            assert!(!names_space(name.as_bytes()), "{name}");
        }
    }

    #[test]
    fn gaps_are_judged_as_pdf_inspector_judges_them() {
        let (fonts, font) = found(&subset("26 /space", &[(26, 278), (32, 0)])).expect("font");
        let judge = |text: &Object, shown: Shown| fonts.judge(font, text, shown);
        let array = |elements: Vec<Object>| Object::Array(elements);
        // Kerning inside an amount, between the thresholds, splits it by
        // one and not the other.
        let kerned = array(vec![
            string("8"),
            Object::Integer(-106),
            string("5,000"),
            Object::Integer(-108),
            string(".00"),
        ]);
        assert!(judge(&kerned, shown(10.0, 0.0, 0.0)).misjudged);
        // Gaps both judge alike, and an offset with no text after it.
        let spaced = array(vec![
            string("Total"),
            Object::Integer(-250),
            string("due"),
            Object::Integer(-40),
            string("12.00"),
            Object::Integer(-106),
        ]);
        assert!(!judge(&spaced, shown(10.0, 0.0, 0.0)).misjudged);
        // A tracked title keeps its letters together under both; read as
        // capitals, it stays together however wide the tracking.
        let tracked = |letters: &str, gaps: &[i64]| {
            let mut elements = Vec::new();
            for (index, letter) in letters.chars().enumerate() {
                elements.push(string(&letter.to_string()));
                if let Some(gap) = gaps.get(index) {
                    elements.push(Object::Integer(*gap));
                }
            }
            array(elements)
        };
        let title = tracked("VALLEY", &[-250; 5]);
        assert!(!judge(&title, shown(10.0, 0.0, 0.0)).misjudged);
        // Over the tracking, a gap between the thresholds ends a word by
        // one: in capitals, which keep their tracking, and not in lower
        // case, which does not.
        let capitals = tracked("TAXDU", &[-250, -250, -355, -250]);
        assert!(judge(&capitals, shown(10.0, 0.0, 0.0)).misjudged);
        let lower = tracked("taxdu", &[-250, -250, -355, -250]);
        assert!(!judge(&lower, shown(10.0, 0.0, 0.0)).misjudged);
        // Character spacing inside a short string, between the thresholds:
        // spaced once the offset after it takes the spacing back, or left
        // for the next run to decide when it ends the array.
        let dt = |after: Option<i64>| {
            let mut elements = vec![string("dt")];
            elements.extend(after.map(Object::Integer));
            elements.push(string("o"));
            array(elements)
        };
        assert!(judge(&dt(Some(105)), shown(10.0, 1.05, 0.0)).misjudged);
        assert!(!judge(&dt(Some(-40)), shown(10.0, 1.05, 0.0)).misjudged);
        assert!(!judge(&dt(None), shown(10.0, 1.05, 0.0)).misjudged);
        let single = judge(&string("dt"), shown(10.0, 1.05, 0.0));
        assert!(!single.misjudged);
        let candidate = single.candidate.expect("candidate");
        assert!(candidate.taken_back(1.0) && !candidate.taken_back(0.0));
        assert!(candidate.on_line(1.5) && !candidate.on_line(12.0));
        // Spacing both judge alike, a longer string, and a junction before
        // joining punctuation take no space.
        for (text, char_spacing) in [("dt", 0.5), ("send", 1.05), ("d.", 1.05)] {
            assert!(judge(&string(text), shown(10.0, char_spacing, 0.0))
                .candidate
                .is_none());
        }
        // Word spacing after a space code that paints nothing.
        assert!(judge(&string("a b"), shown(10.0, 0.0, 1.05))
            .candidate
            .is_none());
    }
}
