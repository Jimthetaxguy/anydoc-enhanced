//! Words shown glyph by glyph, as browsers print text to PDF (open upstream
//! #531).
//!
//! Chromium's print to PDF shows each glyph of a line as a string of its
//! own, placed at the whole-pixel advance a hinting rasterizer laid it out
//! at, while the font's widths keep the unhinted advances; the word spaces
//! are painted as space glyphs of their own. A hinted glyph runs up to a
//! fifth of an em past its declared width, over pdf-inspector 1.24.0's
//! word-gap thresholds, so the Markdown splits words and amounts: "LIAB
//! ILITIES", "83, 476. 03". The painted spaces say where the words end: the
//! scan collects each word a font that paints its spaces shows glyph by
//! glyph. Where the Markdown holds such a word split by a space or a cell
//! edge, each page showing it is read again, as pdf-inspector places its
//! text, and reported when its own text splits it.
//!
//! Glyphs are read by the font's ToUnicode map, then the names its
//! differences give, then, for a simple font, as printable ASCII. Words are
//! made of ASCII letters and digits and the marks numbers and dates are
//! written with; any other character ends a word, and a glyph that cannot be
//! read drops the word it is in.

use std::collections::{HashMap, HashSet};

use lopdf::{Dictionary, Document, Object};
use pdf_inspector::glyph_names::glyph_name_to_string;
use pdf_inspector::tounicode::ToUnicodeCMap;

/// Bytes a ToUnicode map may decode to.
const MAX_CMAP_BYTES: usize = 1 << 20;
/// ToUnicode bytes and differences entries read per document.
const MAX_FONT_STEPS: usize = 16_000_000;
/// How far past the previous glyph's origin, in em along the baseline, the
/// next glyph of a word starts: the widest glyph's advance and its hinting.
const MAX_GLYPH_STEP_EM: f64 = 1.5;
/// How far off the previous glyph's baseline, in em, the next glyph of a
/// word may sit.
const MAX_BASELINE_SHIFT_EM: f64 = 0.2;
/// Words collected on a page, and bytes in one word.
const MAX_WORDS_PER_PAGE: usize = 4096;
const MAX_WORD_BYTES: usize = 64;

/// What stands between two items of a line in a page's text: neither a
/// space nor a character of a word, so a word ends there without being
/// split.
pub(crate) const ITEM_EDGE: char = '\u{1F}';

/// Whether a character belongs to a word the check reads: ASCII letters and
/// digits, and the marks amounts, dates, and times are written with.
fn word_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '.' | ',' | ':' | '/' | '-' | '%')
}

/// [`word_character`] for a byte of UTF-8 text.
fn word_byte(byte: u8) -> bool {
    word_character(char::from(byte)) && byte.is_ascii()
}

/// How the codes of one font read.
struct Decoder {
    /// Two bytes per code (a Type0 font with an identity encoding).
    two_byte: bool,
    cmap: Option<ToUnicodeCMap>,
    /// For a simple font, the name its differences give each code.
    names: HashMap<u8, String>,
    /// What the codes read so far read as.
    read: HashMap<u16, Option<String>>,
}

impl Decoder {
    /// What a code reads as, when the font says.
    fn text(&self, code: u16) -> Option<String> {
        let mapped = self
            .cmap
            .as_ref()
            .and_then(|cmap| cmap.lookup(code))
            .filter(|text| !text.contains('\u{FFFD}'));
        match mapped {
            Some(text) => Some(text),
            None if self.two_byte => None,
            None => match self.names.get(&(code as u8)) {
                Some(name) => glyph_name_to_string(name),
                None if (0x20..=0x7E).contains(&code) => {
                    char::from_u32(u32::from(code)).map(String::from)
                }
                None => None,
            },
        }
    }

    /// What a code reads as: one character, or `None` when the font does
    /// not say, or says more than one.
    fn read(&self, code: u16) -> Option<char> {
        let text = self.text(code)?;
        let mut characters = text.chars();
        match (characters.next(), characters.next()) {
            (Some(character), None) => Some(character),
            _ => None,
        }
    }
}

/// Fonts read to decode glyph codes, by the address of their dictionary,
/// and the work spent reading them.
#[derive(Default)]
pub(crate) struct GlyphFonts {
    known: HashMap<usize, Option<usize>>,
    decoders: Vec<Decoder>,
    steps: usize,
}

impl GlyphFonts {
    /// The font, when its codes can be read. Past the document's work
    /// limit, fonts not yet read are not.
    pub(crate) fn font(&mut self, document: &Document, font: &Dictionary) -> Option<usize> {
        let key = std::ptr::from_ref(font) as usize;
        if let Some(known) = self.known.get(&key) {
            return *known;
        }
        if self.steps > MAX_FONT_STEPS {
            return None;
        }
        let found = decoder(document, font, &mut self.steps).map(|decoder| {
            self.decoders.push(decoder);
            self.decoders.len() - 1
        });
        self.known.insert(key, found);
        found
    }

    /// What a string shown in `font` holds, when it is one glyph: `Some`
    /// with what the glyph reads as, if it can be read.
    pub(crate) fn one_glyph(&self, font: usize, bytes: &[u8]) -> Option<Option<char>> {
        let decoder = &self.decoders[font];
        let code = match (decoder.two_byte, bytes) {
            (false, [byte]) => u16::from(*byte),
            (true, [high, low]) => u16::from_be_bytes([*high, *low]),
            _ => return None,
        };
        Some(decoder.read(code))
    }

    /// What a string shown in `font` reads as, when every glyph of it can
    /// be read.
    pub(crate) fn text(&mut self, font: usize, bytes: &[u8]) -> Option<String> {
        let decoder = &mut self.decoders[font];
        let width = if decoder.two_byte { 2 } else { 1 };
        if !bytes.len().is_multiple_of(width) {
            return None;
        }
        let mut text = String::with_capacity(bytes.len());
        for code in bytes.chunks(width) {
            let code = match code {
                [byte] => u16::from(*byte),
                [high, low] => u16::from_be_bytes([*high, *low]),
                _ => return None,
            };
            if !decoder.read.contains_key(&code) {
                let reading = decoder.text(code);
                decoder.read.insert(code, reading);
            }
            text.push_str(decoder.read.get(&code)?.as_deref()?);
        }
        Some(text)
    }

    /// What the first glyph of a string shown in `font` reads as.
    pub(crate) fn first_glyph(&self, font: usize, bytes: &[u8]) -> Option<char> {
        let decoder = &self.decoders[font];
        let code = match (decoder.two_byte, bytes) {
            (false, [byte, ..]) => u16::from(*byte),
            (true, [high, low, ..]) => u16::from_be_bytes([*high, *low]),
            _ => return None,
        };
        decoder.read(code)
    }
}

fn decoder(document: &Document, font: &Dictionary, steps: &mut usize) -> Option<Decoder> {
    let subtype = font.get(b"Subtype").ok()?.as_name().ok()?;
    let two_byte = match subtype {
        b"Type0" => {
            let encoding = font.get(b"Encoding").ok()?.as_name().ok()?;
            if !matches!(encoding, b"Identity-H" | b"Identity-V") {
                return None;
            }
            true
        }
        b"Type1" | b"TrueType" | b"MMType1" | b"Type3" => false,
        _ => return None,
    };
    let cmap = font
        .get(b"ToUnicode")
        .ok()
        .and_then(|value| document.dereference(value).ok())
        .and_then(|(_, value)| value.as_stream().ok())
        .and_then(|stream| stream.get_plain_content_with_limit(MAX_CMAP_BYTES).ok())
        .and_then(|content| {
            *steps += content.len();
            ToUnicodeCMap::parse(&content)
        })
        .filter(|cmap| usize::from(cmap.code_byte_length) == if two_byte { 2 } else { 1 });
    if two_byte && cmap.is_none() {
        return None;
    }
    let names = if two_byte {
        HashMap::new()
    } else {
        differences(document, font, steps)
    };
    Some(Decoder {
        two_byte,
        cmap,
        names,
        read: HashMap::new(),
    })
}

/// The name each code last takes in a simple font's `/Differences`.
fn differences(document: &Document, font: &Dictionary, steps: &mut usize) -> HashMap<u8, String> {
    let mut names = HashMap::new();
    let encoding = match font.get(b"Encoding") {
        Ok(Object::Reference(id)) => document.get_dictionary(*id).ok(),
        Ok(Object::Dictionary(encoding)) => Some(encoding),
        _ => None,
    };
    let entries = encoding
        .and_then(|encoding| encoding.get(b"Differences").ok())
        .and_then(|entries| match entries {
            Object::Reference(id) => document.get_object(*id).ok(),
            entries => Some(entries),
        })
        .and_then(|entries| entries.as_array().ok());
    let mut code: u8 = 0;
    for entry in entries.into_iter().flatten() {
        *steps += 1;
        if *steps > MAX_FONT_STEPS {
            break;
        }
        match entry {
            Object::Integer(value) => code = *value as u8,
            Object::Name(name) => {
                names.insert(code, String::from_utf8_lossy(name).into_owned());
                code = code.wrapping_add(1);
            }
            _ => {}
        }
    }
    names
}

/// A glyph shown as a string of its own.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Glyph {
    /// The font it is shown in (see [`GlyphFonts`]).
    pub(crate) font: usize,
    /// What it reads as, if it can be read.
    pub(crate) reading: Option<char>,
    /// Where its origin is, in device space.
    pub(crate) at: [f64; 2],
    /// The baseline's direction, one em long, in device space.
    pub(crate) em: [f64; 2],
}

/// The word being shown.
struct Word {
    text: String,
    font: usize,
    last: [f64; 2],
    em: [f64; 2],
    glyphs: usize,
}

/// The words a page shows glyph by glyph, as the scan goes.
#[derive(Default)]
pub(crate) struct GlyphWords {
    current: Option<Word>,
    /// Fonts seen painting a space glyph right after a word's glyphs.
    painting_spaces: HashSet<usize>,
    /// Words of two glyphs or more, by text and font, with how often.
    words: HashMap<(String, usize), u32>,
}

impl GlyphWords {
    /// A glyph shown as a string of its own.
    pub(crate) fn glyph(&mut self, glyph: Glyph) {
        let follows = self
            .current
            .as_ref()
            .is_some_and(|word| word.font == glyph.font && follows(word.last, word.em, glyph.at));
        match glyph.reading {
            Some(' ') => {
                if follows {
                    self.painting_spaces.insert(glyph.font);
                }
                self.end_word();
            }
            Some(character) if word_character(character) => {
                if !follows {
                    self.end_word();
                }
                let word = self.current.get_or_insert_with(|| Word {
                    text: String::new(),
                    font: glyph.font,
                    last: glyph.at,
                    em: glyph.em,
                    glyphs: 0,
                });
                word.text.push(character);
                word.last = glyph.at;
                word.glyphs += 1;
                if word.text.len() > MAX_WORD_BYTES {
                    self.current = None;
                }
            }
            Some(_) => self.end_word(),
            None => self.current = None,
        }
    }

    /// Text shown otherwise than a glyph at a time: a string of several
    /// glyphs placed at `at`, whose first reads as `first`, or text or a
    /// form whose place is not known. A word the string goes on with, as a
    /// browser prints the small capitals of a word, is not read whole; one
    /// it does not go on with ends where it stands.
    pub(crate) fn interrupt(&mut self, at: Option<[f64; 2]>, first: Option<char>) {
        let ends = match (at, self.current.as_ref()) {
            (_, None) => return,
            (Some(at), Some(word)) => !follows(word.last, word.em, at) || first == Some(' '),
            (None, Some(_)) => false,
        };
        if ends {
            self.end_word();
        } else {
            self.current = None;
        }
    }

    fn end_word(&mut self) {
        let Some(word) = self.current.take() else {
            return;
        };
        if word.glyphs >= 2 && self.words.len() < MAX_WORDS_PER_PAGE {
            *self.words.entry((word.text, word.font)).or_default() += 1;
        }
    }

    /// The words shown in fonts that paint their spaces, with how often.
    pub(crate) fn finish(mut self) -> Vec<(String, u32)> {
        self.end_word();
        let mut words: HashMap<String, u32> = HashMap::new();
        for ((text, font), count) in self.words {
            if self.painting_spaces.contains(&font) {
                *words.entry(text).or_default() += count;
            }
        }
        words.into_iter().collect()
    }
}

/// Whether a glyph at `next` continues a word whose last glyph is at `last`:
/// on its baseline, and less than the widest advance on.
fn follows(last: [f64; 2], em: [f64; 2], next: [f64; 2]) -> bool {
    let length = em[0].hypot(em[1]);
    if length <= 0.0 || !length.is_finite() {
        return false;
    }
    let step = [next[0] - last[0], next[1] - last[1]];
    let along = (step[0] * em[0] + step[1] * em[1]) / (length * length);
    let across = (em[0] * step[1] - em[1] * step[0]) / (length * length);
    along > 0.0 && along <= MAX_GLYPH_STEP_EM && across.abs() <= MAX_BASELINE_SHIFT_EM
}

/// How often each of `patterns` stands whole in `text`, and how often split
/// by a space or a cell edge, where nothing of a word adjoins it; escapes
/// and emphasis marks are read through. A line end inside a word is where
/// the page wrapped it, which splits nothing.
fn counts(text: &str, patterns: &[&str]) -> (Vec<u32>, Vec<u32>) {
    let mut whole = vec![0u32; patterns.len()];
    let mut split = vec![0u32; patterns.len()];
    let mut squeezed: Vec<u8> = Vec::with_capacity(text.len());
    let mut separated: Vec<bool> = Vec::with_capacity(text.len() + 1);
    let mut wrapped: Vec<bool> = Vec::with_capacity(text.len());
    let (mut gap, mut line_end) = (true, false);
    for character in text.chars() {
        if character.is_whitespace() || character == '|' {
            gap = true;
            line_end |= matches!(character, '\n' | '\r');
            continue;
        }
        if matches!(character, '\\' | '*') {
            continue;
        }
        let mut bytes = [0u8; 4];
        for &byte in character.encode_utf8(&mut bytes).as_bytes() {
            squeezed.push(byte);
            separated.push(gap);
            wrapped.push(line_end);
            (gap, line_end) = (false, false);
        }
    }
    separated.push(true);
    let Ok(automaton) = aho_corasick::AhoCorasick::new(patterns) else {
        return (whole, split);
    };
    // A word ends at a separator, or where a character no word holds
    // stands next to it, as a quote mark or a parenthesis.
    let edge =
        |at: usize, next: Option<&u8>| separated[at] || next.is_none_or(|&byte| !word_byte(byte));
    for found in automaton.find_overlapping_iter(&squeezed) {
        let (start, end) = (found.start(), found.end());
        let before = start.checked_sub(1).and_then(|at| squeezed.get(at));
        if !edge(start, before) || !edge(end, squeezed.get(end)) {
            continue;
        }
        if wrapped[start + 1..end].iter().any(|&wrap| wrap) {
            continue;
        }
        // A space after a hyphen is where a paragraph joins the lines of a
        // compound the page wrapped ("self- employment").
        let gaps: Vec<usize> = (start + 1..end).filter(|&at| separated[at]).collect();
        if !gaps.is_empty() && gaps.iter().all(|&at| squeezed[at - 1] == b'-') {
            continue;
        }
        let index = found.pattern().as_usize();
        if !gaps.is_empty() {
            split[index] += 1;
        } else {
            whole[index] += 1;
        }
    }
    (whole, split)
}

/// The words among `words` (each with a page showing it and how often)
/// that the Markdown shows split somewhere, each with whether it also shows
/// it whole: which page splits them, the pages' own text tells (see
/// [`split_pages`]).
pub(crate) fn misread(markdown: &str, words: &[(u32, String, u32)]) -> HashMap<String, bool> {
    let patterns: Vec<&str> = words
        .iter()
        .map(|(_, text, _)| text.as_str())
        .collect::<HashSet<&str>>()
        .into_iter()
        .collect();
    if patterns.is_empty() {
        return HashMap::new();
    }
    let (whole, split) = counts(markdown, &patterns);
    patterns
        .iter()
        .enumerate()
        .filter(|&(index, _)| split[index] > 0)
        .map(|(index, text)| ((*text).to_string(), whole[index] > 0))
        .collect()
}

/// The pages showing a misread word, whose own text tells whether they
/// split it.
pub(crate) fn pages_to_read(
    misread: &HashMap<String, bool>,
    words: &[(u32, String, u32)],
) -> Vec<u32> {
    let mut pages: Vec<u32> = words
        .iter()
        .filter(|(_, text, _)| misread.contains_key(text))
        .map(|(page, _, _)| *page)
        .collect();
    pages.sort_unstable();
    pages.dedup();
    pages
}

/// The pages whose own text, as pdf-inspector places it (`page_text`),
/// shows a misread word split, and, where a page's text is not at hand,
/// those showing a word the Markdown never shows whole.
pub(crate) fn split_pages(
    misread: &HashMap<String, bool>,
    words: &[(u32, String, u32)],
    page_text: &HashMap<u32, String>,
) -> Vec<u32> {
    let mut shown: HashMap<u32, Vec<&str>> = HashMap::new();
    for (page, text, _) in words {
        if misread.contains_key(text) {
            shown.entry(*page).or_default().push(text);
        }
    }
    let mut named: Vec<u32> = shown
        .into_iter()
        .filter(|(page, texts)| match page_text.get(page) {
            Some(own) => counts(own, texts).1.iter().any(|&split| split > 0),
            None => texts.iter().any(|text| misread.get(*text) == Some(&false)),
        })
        .map(|(page, _)| page)
        .collect();
    named.sort_unstable();
    named
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(pairs: &[(u32, &str, u32)]) -> Vec<(u32, String, u32)> {
        pairs
            .iter()
            .map(|(page, text, count)| (*page, (*text).to_string(), *count))
            .collect()
    }

    /// The pages the Markdown names, where each word's pages are its own.
    fn split_pages(markdown: &str, shown: &[(u32, String, u32)]) -> Vec<u32> {
        super::split_pages(&misread(markdown, shown), shown, &HashMap::new())
    }

    #[test]
    fn words_the_markdown_splits_name_their_pages() {
        let shown = words(&[(1, "LIABILITIES", 1), (2, "EQUITY", 1), (3, "83,476.03", 1)]);
        assert_eq!(
            split_pages("|LIAB ILITIES|||\n|EQUITY|1|\n\nTotal 83, 476. 03", &shown),
            vec![1, 3]
        );
        // A word split over a cell edge, or across emphasis, is split too.
        assert_eq!(split_pages("|LIAB|ILITIES|", &shown), vec![1]);
        assert_eq!(
            split_pages("**LIAB ILITIES** and **EQ** **UITY**", &shown),
            vec![1, 2]
        );
        // A word several pages show names those whose own text splits it;
        // without their text, a word the Markdown also shows whole names
        // none.
        let shown = words(&[
            (2, "Limitations", 1),
            (7, "Limitations", 1),
            (18, "Limitations", 1),
        ]);
        let found = misread("Limitations; (Limitations) \"L imitations\"", &shown);
        assert_eq!(pages_to_read(&found, &shown), vec![2, 7, 18]);
        assert!(super::split_pages(&found, &shown, &HashMap::new()).is_empty());
        let own: HashMap<u32, String> = [
            (2, "Limitations"),
            (7, "Limitations"),
            (18, "(A) L imitations"),
        ]
        .into_iter()
        .map(|(page, text)| (page, text.to_string()))
        .collect();
        assert_eq!(super::split_pages(&found, &shown, &own), vec![18]);
    }

    #[test]
    fn whole_words_and_other_text_are_not_split() {
        let shown = words(&[(1, "LIABILITIES", 2), (1, "OF", 1)]);
        assert!(split_pages("LIABILITIES and **LIABILITIES**", &shown).is_empty());
        // Letters of a word inside other words, or across a word's edge,
        // are not the word split.
        assert!(split_pages("LIABILITIES LIABILITIES PROOF FILE", &shown).is_empty());
        assert!(split_pages("LIABILITIES LIABILITIES O FFICE", &shown).is_empty());
        // A quote mark or a parenthesis beside a word leaves it whole, and a
        // word wrapped at a line end is not split.
        assert!(split_pages("\"LIABILITIES\" (LIABILITIES) OF", &shown).is_empty());
        assert!(split_pages(
            "LIABILITIES and LIABI-\nLITIES",
            &words(&[(1, "LIABI-LITIES", 1)])
        )
        .is_empty());
        assert!(split_pages(
            "self-employment and self- employment",
            &words(&[(1, "self-employment", 2)])
        )
        .is_empty());
        // A split elsewhere in the Markdown names a page only when its own
        // text splits the word.
        let found = misread("LIABILITIES LIABILITIES LIAB ILITIES", &shown);
        let own: HashMap<u32, String> = [(1, "LIABILITIES and LIABILITIES".to_string())].into();
        assert!(super::split_pages(&found, &shown, &own).is_empty());
        // Items of a line meet at an edge, which ends a word and splits
        // none: a word split inside its item before an amount is split, and
        // one whose letters two items hold is not.
        let edge = |parts: &[&str]| parts.join(&ITEM_EDGE.to_string());
        let own: HashMap<u32, String> = [(1, edge(&["TOTAL LIAB ILITIES", "4,514"]))].into();
        assert_eq!(super::split_pages(&found, &shown, &own), vec![1]);
        let own: HashMap<u32, String> = [(1, edge(&["L", "IABILITIES", "4,514"]))].into();
        assert!(super::split_pages(&found, &shown, &own).is_empty());
    }

    #[test]
    fn glyphs_on_one_baseline_make_words_ended_by_painted_spaces() {
        let em = [8.0, 0.0];
        let mut page = GlyphWords::default();
        let mut x = 10.0;
        for character in "LIABILITIES ARE 1,120".chars() {
            page.glyph(Glyph {
                font: 0,
                reading: Some(character),
                at: [x, 100.0],
                em,
            });
            x += 6.0;
        }
        let mut found = page.finish();
        found.sort();
        assert_eq!(
            found,
            vec![
                ("1,120".to_string(), 1),
                ("ARE".to_string(), 1),
                ("LIABILITIES".to_string(), 1)
            ]
        );
        // A font that never paints a space gives no words, and a jump off
        // the baseline or far along it ends one.
        let mut page = GlyphWords::default();
        for (index, character) in "TOTAL".chars().enumerate() {
            page.glyph(Glyph {
                font: 1,
                reading: Some(character),
                at: [10.0 + 6.0 * index as f64, 100.0],
                em,
            });
        }
        assert!(page.finish().is_empty());
        // A word a string of several glyphs goes on with is not read
        // whole; one a string past it does not reach ends where it stands.
        let mut page = GlyphWords::default();
        for (x, character) in [(0.0, 'A'), (4.0, 'B'), (8.0, ' ')] {
            page.glyph(Glyph {
                font: 0,
                reading: Some(character),
                at: [x, 100.0],
                em,
            });
        }
        for (x, character) in [(10.0, 'E'), (16.0, 'F'), (40.0, ' ')] {
            if x == 40.0 {
                page.interrupt(Some([22.0, 100.0]), Some('F'));
            }
            page.glyph(Glyph {
                font: 0,
                reading: Some(character),
                at: [x, 100.0],
                em,
            });
        }
        for (x, character) in [(50.0, 'O'), (56.0, 'F')] {
            page.glyph(Glyph {
                font: 0,
                reading: Some(character),
                at: [x, 100.0],
                em,
            });
        }
        page.interrupt(Some([62.0, 100.0]), Some(' '));
        for (x, character) in [(90.0, 'T'), (96.0, 'O')] {
            page.glyph(Glyph {
                font: 0,
                reading: Some(character),
                at: [x, 100.0],
                em,
            });
        }
        page.interrupt(Some([300.0, 100.0]), Some('Z'));
        let mut found = page.finish();
        found.sort();
        assert_eq!(
            found,
            vec![
                ("AB".to_string(), 1),
                ("OF".to_string(), 1),
                ("TO".to_string(), 1)
            ]
        );
        assert!(!follows([10.0, 100.0], em, [16.0, 104.0]));
        assert!(!follows([10.0, 100.0], em, [30.0, 100.0]));
        assert!(!follows([10.0, 100.0], em, [4.0, 100.0]));
        assert!(follows([10.0, 100.0], em, [16.0, 100.5]));
    }
}
