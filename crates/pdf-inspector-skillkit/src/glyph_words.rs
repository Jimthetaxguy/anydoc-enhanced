//! Words shown glyph by glyph, as browsers print text to PDF (open upstream
//! #531).
//!
//! Chromium's print to PDF places each glyph of a line at the whole-pixel
//! advance a hinting rasterizer laid it out at, while the font's widths keep
//! the unhinted advances; the word spaces are painted as space glyphs. Its
//! PDF backend, Skia, starts a string at each glyph that its predecessor's
//! width does not place, so a line is a run of strings of one glyph or a
//! few, each placed anew, and a space often opens the string of the glyph
//! after it. A hinted glyph runs up to a fifth of an em past its declared
//! width, over pdf-inspector 1.25.0's word-gap thresholds, so the Markdown
//! splits words and amounts where two strings meet: "LIAB ILITIES", "83,
//! 476. 03". The painted spaces say where the words end: the scan places
//! each glyph by the font's widths and collects each word that a font
//! painting its spaces anywhere in the document shows across two strings or
//! more, where a string starts far enough past the glyph before it for
//! pdf-inspector to read a space there. Where the Markdown holds such a word
//! split by a space or a cell edge, the pages showing it are read again, as
//! pdf-inspector places their text, and reported when their own text splits
//! it, a page for each split the Markdown shows.
//!
//! Glyphs are read as pdf-inspector 1.25.0 reads them: by the font's
//! ToUnicode map, then the names its differences give, then, for a simple
//! font, by its encoding (see `Simple`); a composite font without a map, by
//! the code points its codes are where pdf-inspector reads them so. Words
//! are made of ASCII letters and digits and the marks numbers and dates are
//! written with, and a ligature glyph stands for its letters; any other
//! character ends a word, as does the end of a text object, since a browser
//! writes each run of text as one, and a glyph that cannot be read drops
//! the word it is in.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use aho_corasick::AhoCorasick;
use lopdf::{Dictionary, Document, Object};
use pdf_inspector::glyph_names::glyph_name_to_string;
use pdf_inspector::tounicode::ToUnicodeCMap;

use crate::word_gaps::{code_bound, resolved_array};

/// Bytes a ToUnicode map may decode to.
const MAX_CMAP_BYTES: usize = 1 << 20;
/// ToUnicode bytes, differences entries, and widths read per document.
const MAX_FONT_STEPS: usize = 16_000_000;
/// Widths a composite font's `/W` array may give.
const MAX_CID_WIDTHS: usize = 1 << 16;
/// How far past the previous glyph's origin, in em along the baseline, the
/// next glyph of a word starts in a font whose widths are not known: the
/// widest glyph's advance and its hinting.
const MAX_GLYPH_STEP_EM: f64 = 1.5;
/// How far past the previous glyph's advance, in em, the next glyph of a
/// word starts in a font whose widths are known: no further than
/// pdf-inspector joins two runs into one item, past which its text never
/// splits a word.
const MAX_GLYPH_GAP_EM: f64 = 0.5;
/// How far off the previous glyph's baseline, in em, the next glyph of a
/// word may sit.
const MAX_BASELINE_SHIFT_EM: f64 = 0.2;
/// The least step past a glyph's advance, in em, where two strings meet,
/// at which pdf-inspector 1.25.0 may read a word space: under its least
/// threshold, 0.08 em, and the floor it takes from a tracked run's gaps.
const MIN_WORD_GAP_EM: f64 = 0.04;
/// Words collected on a page, and bytes in one word.
const MAX_WORDS_PER_PAGE: usize = 4096;
const MAX_WORD_BYTES: usize = 64;
/// Characters a ligature glyph may stand for.
const MAX_LIGATURE_CHARACTERS: usize = 3;
/// Words shown glyph by glyph kept across a document, and kept for each
/// font not yet seen painting its spaces.
const MAX_GLYPH_WORDS: usize = 65_536;
const MAX_PENDING_WORDS: usize = 8_192;

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

/// What a glyph reads as, for the words the check reads.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Reading {
    /// A space.
    Space,
    /// Characters of a word: one, or the two or three a ligature stands
    /// for, with how many.
    Word([u8; MAX_LIGATURE_CHARACTERS], u8),
    /// A character no word holds, which ends a word.
    Other,
    /// Nothing the font says, or more than a ligature, which drops the
    /// word it is in.
    Unread,
}

impl Reading {
    /// What a glyph whose font reads it as `text` is for a word.
    fn of(text: Option<&str>) -> Reading {
        let Some(text) = text else {
            return Reading::Unread;
        };
        let mut characters = text.chars();
        match (characters.next(), characters.next()) {
            (None, _) => Reading::Unread,
            (Some(' '), None) => Reading::Space,
            (Some(character), None) if !word_character(character) => Reading::Other,
            _ if text.len() <= MAX_LIGATURE_CHARACTERS && text.chars().all(word_character) => {
                let mut letters = [0u8; MAX_LIGATURE_CHARACTERS];
                letters[..text.len()].copy_from_slice(text.as_bytes());
                Reading::Word(letters, text.len() as u8)
            }
            _ => Reading::Unread,
        }
    }
}

/// A font's glyph widths as pdf-inspector reads them, in em.
enum Widths {
    /// A simple font's, by code; a code its `/Widths` leaves out is none
    /// wide.
    Simple(Vec<f64>),
    /// A composite font's, by code, and the width its `/DW` gives the rest.
    Composite(HashMap<u16, f64>, f64),
}

impl Widths {
    fn of(&self, code: u16) -> f64 {
        match self {
            Widths::Simple(widths) => widths.get(usize::from(code)).copied().unwrap_or(0.0),
            Widths::Composite(widths, default) => widths.get(&code).copied().unwrap_or(*default),
        }
    }
}

/// What each code of a single-byte encoding reads as, where it names a
/// character.
type Table = [Option<char>; 256];

/// A predefined encoding pdf-inspector reads a font by, code by code, as
/// lopdf reads it, which is how pdf-inspector builds its own tables.
fn predefined(name: &[u8]) -> Option<&'static Table> {
    fn read(name: &[u8]) -> Table {
        let mut font = Dictionary::new();
        font.set("Type", Object::Name(b"Font".to_vec()));
        font.set("Subtype", Object::Name(b"Type1".to_vec()));
        font.set("BaseFont", Object::Name(b"Helvetica".to_vec()));
        font.set("Encoding", Object::Name(name.to_vec()));
        let mut table = [None; 256];
        if let Ok(encoding) = font.get_font_encoding(&Document::new()) {
            for (code, slot) in table.iter_mut().enumerate() {
                *slot = Document::decode_text(&encoding, &[code as u8])
                    .ok()
                    .and_then(|text| {
                        let mut characters = text.chars();
                        match (characters.next(), characters.next()) {
                            (Some(character), None) => Some(character),
                            _ => None,
                        }
                    });
            }
        }
        table
    }
    static STANDARD: LazyLock<Table> = LazyLock::new(|| read(b"StandardEncoding"));
    static WIN_ANSI: LazyLock<Table> = LazyLock::new(|| read(b"WinAnsiEncoding"));
    static MAC_ROMAN: LazyLock<Table> = LazyLock::new(|| read(b"MacRomanEncoding"));
    static MAC_EXPERT: LazyLock<Table> = LazyLock::new(|| read(b"MacExpertEncoding"));
    Some(match name {
        b"StandardEncoding" => &*STANDARD,
        b"WinAnsiEncoding" => &*WIN_ANSI,
        b"MacRomanEncoding" => &*MAC_ROMAN,
        b"MacExpertEncoding" => &*MAC_EXPERT,
        _ => return None,
    })
}

/// How pdf-inspector reads a code that a font's ToUnicode map and
/// differences leave.
enum Unnamed {
    /// In no way the check can tell: a code a composite font's map leaves,
    /// or a code of a font read through a built-in encoding of symbols.
    Unknown,
    /// Printable ASCII as itself, and past it in no way the check can tell:
    /// a simple font pdf-inspector reads through its embedded program.
    Ascii,
    /// By a simple font's encoding (see `Simple`).
    Simple(Simple),
    /// As the code point a composite font's code is: pdf-inspector's last
    /// reading of a font under `Identity-H` or `Identity-V` with no map,
    /// whose widths list codes that look like code points.
    CodePoints,
}

/// How pdf-inspector reads a simple font's codes that its ToUnicode map and
/// differences leave. A font with a ToUnicode map or a base encoding reads
/// them code by code: from 0x20 on by the base encoding, where it names a
/// character, else byte by byte; a control code as nothing. So does a
/// string one of whose codes the differences name. Any other string reads
/// as UTF-16 or UTF-8 where its bytes are so written (see
/// `read_as_unicode`), else as lopdf reads the font's encoding.
struct Simple {
    /// The base encoding the font's encoding dictionary names.
    base: Option<&'static Table>,
    /// How lopdf reads the font's encoding.
    lopdf: Lopdf,
    /// Whether a byte read byte by byte reads as Windows-1252 has it, not as
    /// Latin-1 does: as for any font but TeX's and those of symbols.
    cp1252: bool,
}

/// How lopdf reads a simple font's encoding, as pdf-inspector reads a
/// string by it.
enum Lopdf {
    /// What it reads each code as; a code it leaves out reads as nothing.
    Codes(Box<[String]>),
    /// Not at all: pdf-inspector reads each byte byte by byte, or, for a
    /// font named as symbols, as its own character, with bullets and a
    /// check mark where symbol fonts put them; a control code as nothing.
    Bytes { symbols: bool },
    /// Through a map the check does not read.
    Unknown,
}

impl Simple {
    /// What an unnamed code reads as: code by code where `by_code`, else as
    /// lopdf reads it; `None` where the check cannot tell.
    fn read(&self, byte: u8, by_code: bool) -> Option<String> {
        let bytewise = |byte: u8| {
            if self.cp1252 {
                crate::text_paints::windows_1252(byte)
            } else {
                char::from(byte)
            }
        };
        if by_code || self.base.is_some() {
            if byte < 0x20 {
                return Some(String::new());
            }
            let character = self
                .base
                .and_then(|table| table[usize::from(byte)])
                .unwrap_or_else(|| bytewise(byte));
            return Some(character.to_string());
        }
        match &self.lopdf {
            // A C1 control character lopdf gives, pdf-inspector reads as
            // Windows-1252 has its code.
            Lopdf::Codes(codes) => Some(
                codes[usize::from(byte)]
                    .chars()
                    .map(|character| match u8::try_from(u32::from(character)) {
                        Ok(code @ 0x80..=0x9F) if self.cp1252 => bytewise(code),
                        _ => character,
                    })
                    .collect(),
            ),
            Lopdf::Bytes { .. } if byte < 0x20 && !matches!(byte, b'\t' | b'\n' | b'\r') => {
                Some(String::new())
            }
            Lopdf::Bytes { symbols: true } => Some(
                match byte {
                    0xA1 | 0xA7 | 0xB7 => '\u{2022}',
                    0xFC => '\u{2713}',
                    _ => char::from(byte),
                }
                .to_string(),
            ),
            Lopdf::Bytes { symbols: false } => Some(bytewise(byte).to_string()),
            Lopdf::Unknown => None,
        }
    }
}

/// How the codes of one font read.
struct Decoder {
    /// Two bytes per code (a Type0 font).
    two_byte: bool,
    cmap: Option<ToUnicodeCMap>,
    /// For a simple font, the name its differences give each code.
    names: HashMap<u8, String>,
    /// How a code the map and names leave reads.
    unnamed: Unnamed,
    /// Its glyphs' widths, when it gives them and its glyphs advance along
    /// the baseline; and, for a simple font naming a standard font and giving
    /// none, each code's width as a viewer and pdf-inspector take it (see
    /// `standard_widths`), which places its strings.
    widths: Option<Widths>,
    standard: Option<Box<[f64]>>,
    /// What the codes read so far read as, code by code or not.
    read: HashMap<(u16, bool), Option<String>>,
    /// The codes shown so far: what each reads as for a word, and its width.
    glyphs: HashMap<u16, (Reading, Option<f64>)>,
}

impl Decoder {
    /// What a code reads as, when the font says; a simple font's code the
    /// map and names leave, code by code where `by_code`, else as lopdf
    /// reads it.
    fn text(&self, code: u16, by_code: bool) -> Option<String> {
        let mapped = self
            .cmap
            .as_ref()
            .and_then(|cmap| cmap.lookup(code))
            .filter(|text| !text.contains('\u{FFFD}'));
        if let Some(text) = mapped {
            return Some(text);
        }
        if self.two_byte {
            // A control character or a surrogate is no text.
            return matches!(self.unnamed, Unnamed::CodePoints).then(|| {
                char::from_u32(u32::from(code))
                    .filter(|character| !character.is_control() || matches!(character, '\t' | '\n'))
                    .map(String::from)
                    .unwrap_or_default()
            });
        }
        let byte = code as u8;
        if let Some(name) = self.names.get(&byte) {
            return glyph_name_to_string(name);
        }
        match &self.unnamed {
            Unnamed::Ascii => (0x20..=0x7E)
                .contains(&byte)
                .then(|| char::from(byte).to_string()),
            Unnamed::Simple(simple) => simple.read(byte, by_code || self.cmap.is_some()),
            _ => None,
        }
    }

    /// What a code reads as for a word, and its width, when the font gives
    /// its widths: as a string of that code alone reads.
    fn glyph(&mut self, code: u16) -> (Reading, Option<f64>) {
        if let Some(glyph) = self.glyphs.get(&code) {
            return *glyph;
        }
        let glyph = (
            Reading::of(self.text(code, false).as_deref()),
            self.widths.as_ref().map(|widths| widths.of(code)),
        );
        self.glyphs.insert(code, glyph);
        glyph
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

    /// Whether the font's codes are two bytes each.
    pub(crate) fn two_byte(&self, font: usize) -> bool {
        self.decoders[font].two_byte
    }

    /// What a code shown in `font` reads as for a word, and its width in
    /// em, when the font gives its widths.
    pub(crate) fn glyph(&mut self, font: usize, code: u16) -> (Reading, Option<f64>) {
        self.decoders[font].glyph(code)
    }

    /// A code's width in em, when `font` gives its widths, or names a
    /// standard font whose widths it takes (see `standard_widths`).
    pub(crate) fn width(&self, font: usize, code: u16) -> Option<f64> {
        let decoder = &self.decoders[font];
        match (&decoder.widths, &decoder.standard) {
            (Some(widths), _) => Some(widths.of(code)),
            (None, Some(standard)) => standard.get(usize::from(code)).copied(),
            (None, None) => None,
        }
    }

    /// What a string shown in `font` reads as, when every glyph of it can
    /// be read.
    pub(crate) fn text(&mut self, font: usize, bytes: &[u8]) -> Option<String> {
        self.read(font, bytes, None)
    }

    /// What a string shown in `font` reads as, with `unread` for each code
    /// that cannot be read; `None` where none can.
    pub(crate) fn text_or(&mut self, font: usize, bytes: &[u8], unread: char) -> Option<String> {
        self.read(font, bytes, Some(unread))
    }

    fn read(&mut self, font: usize, bytes: &[u8], unread: Option<char>) -> Option<String> {
        let decoder = &mut self.decoders[font];
        let width = if decoder.two_byte { 2 } else { 1 };
        if !bytes.len().is_multiple_of(width) {
            return None;
        }
        // A string pdf-inspector reads whole, not code by code.
        let by_code = match &decoder.unnamed {
            Unnamed::Simple(simple)
                if decoder.cmap.is_none()
                    && simple.base.is_none()
                    && !bytes.iter().any(|byte| decoder.names.contains_key(byte)) =>
            {
                if let Some(text) = crate::text_paints::read_as_unicode(bytes) {
                    return Some(text);
                }
                false
            }
            _ => true,
        };
        let mut text = String::with_capacity(bytes.len());
        // Whether any code was read, and how many read as nothing.
        let (mut read, mut nothing) = (false, 0);
        for code in bytes.chunks(width) {
            let code = match code {
                [byte] => u16::from(*byte),
                [high, low] => u16::from_be_bytes([*high, *low]),
                _ => return None,
            };
            let key = (code, by_code);
            if !decoder.read.contains_key(&key) {
                let reading = decoder.text(code, by_code);
                decoder.read.insert(key, reading);
            }
            match (decoder.read[&key].as_deref(), unread) {
                (Some(reading), _) => {
                    read = true;
                    nothing += usize::from(reading.is_empty());
                    text.push_str(reading);
                }
                (None, Some(unread)) => text.push(unread),
                (None, None) => return None,
            }
        }
        // A string most of whose codes are no code points pdf-inspector
        // reads otherwise.
        let codes = bytes.len() / width;
        if matches!(decoder.unnamed, Unnamed::CodePoints) && nothing > codes / 2 {
            return None;
        }
        (read || unread.is_none() || bytes.is_empty()).then_some(text)
    }
}

fn decoder(document: &Document, font: &Dictionary, steps: &mut usize) -> Option<Decoder> {
    let subtype = font.get(b"Subtype").ok()?.as_name().ok()?;
    // Whether its codes are two bytes, and whether they are the glyphs'
    // CIDs, which its widths are given by: a UCS-2 CMap's are not, and
    // its codes are read through its ToUnicode map alone.
    let (two_byte, by_cid) = match subtype {
        b"Type0" => match font.get(b"Encoding").ok()?.as_name().ok()? {
            b"Identity-H" => (true, true),
            b"Identity-V" => (true, false),
            name if crate::cjk_fonts::ucs2_cmap(name) => (true, false),
            _ => return None,
        },
        b"Type1" | b"TrueType" | b"MMType1" | b"Type3" => (false, false),
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
    let unnamed = if !two_byte {
        simple(document, font, cmap.is_some(), steps)
    } else if cmap.is_some() {
        Unnamed::Unknown
    } else if reads_code_points(document, font) {
        Unnamed::CodePoints
    } else {
        return None;
    };
    let names = if two_byte {
        HashMap::new()
    } else {
        differences(document, font, steps)
    };
    let widths = match (two_byte, by_cid) {
        (false, _) => simple_widths(document, font, steps),
        (true, true) => composite_widths(document, font, steps),
        (true, false) => None,
    };
    let standard = (widths.is_none() && !two_byte)
        .then(|| standard_widths(document, font, &names))
        .flatten();
    Some(Decoder {
        two_byte,
        cmap,
        names,
        unnamed,
        widths,
        standard,
        read: HashMap::new(),
        glyphs: HashMap::new(),
    })
}

/// An object, its reference followed.
fn resolved<'a>(document: &'a Document, object: &'a Object) -> Option<&'a Object> {
    match object {
        Object::Reference(id) => document.get_object(*id).ok(),
        object => Some(object),
    }
}

/// How pdf-inspector reads a simple font's codes that its ToUnicode map, if
/// `mapped`, and its differences leave (see `Simple`).
fn simple(document: &Document, font: &Dictionary, mapped: bool, steps: &mut usize) -> Unnamed {
    let base_font = font
        .get(b"BaseFont")
        .ok()
        .and_then(|name| name.as_name().ok());
    let encoding = font.get(b"Encoding").ok();
    // The encoding the font names outright, and the base encoding its
    // encoding dictionary names, references followed as pdf-inspector
    // follows them.
    let named = encoding
        .and_then(|encoding| resolved(document, encoding))
        .and_then(|encoding| encoding.as_name().ok());
    let base = match encoding {
        Some(Object::Reference(id)) => document.get_dictionary(*id).ok(),
        Some(Object::Dictionary(encoding)) => Some(encoding),
        _ => None,
    }
    .and_then(|encoding| encoding.get(b"BaseEncoding").ok())
    .and_then(|base| resolved(document, base))
    .and_then(|base| base.as_name().ok())
    .and_then(predefined);
    // Symbol and ZapfDingbats read through built-in encodings of their own,
    // unless the font names another outright.
    if base.is_none()
        && symbol_font(base_font).is_some_and(|own| named.is_none_or(|name| name == own))
    {
        return Unnamed::Unknown;
    }
    // pdf-inspector reads a font with a ToUnicode map, or without an
    // encoding, through its embedded program, where it has one; and a font
    // whose ToUnicode map the check does not read, through that map.
    let program = font
        .get(b"FontDescriptor")
        .ok()
        .and_then(|descriptor| resolved(document, descriptor))
        .and_then(|descriptor| descriptor.as_dict().ok())
        .is_some_and(|descriptor| {
            [&b"FontFile2"[..], b"FontFile3"]
                .iter()
                .any(|key| descriptor.get(key).and_then(Object::as_reference).is_ok())
        });
    let unread_map = !mapped
        && font
            .get(b"ToUnicode")
            .ok()
            .and_then(|map| resolved(document, map))
            .is_some_and(|map| map.as_stream().is_ok());
    let encoded = matches!(
        encoding,
        Some(Object::Name(_) | Object::Dictionary(_) | Object::Reference(_))
    );
    if unread_map || (program && (mapped || !encoded)) {
        return Unnamed::Ascii;
    }
    // A font with a map or a base encoding reads its strings code by code,
    // never by lopdf.
    let lopdf = if mapped || base.is_some() {
        Lopdf::Unknown
    } else {
        lopdf_reading(document, font, base_font, steps)
    };
    Unnamed::Simple(Simple {
        base,
        lopdf,
        cp1252: windows_1252_font(base_font),
    })
}

/// The encoding a symbol font among the standard 14 reads through when it
/// names none other, by the name that names it: `SymbolEncoding` for
/// Symbol, `ZapfDingbatsEncoding` for ZapfDingbats, as pdf-inspector tells
/// them by name.
fn symbol_font(base_font: Option<&[u8]>) -> Option<&'static [u8]> {
    let name = String::from_utf8_lossy(base_font?);
    let name = match name.split_once('+') {
        Some((prefix, rest))
            if prefix.len() == 6 && prefix.chars().all(|letter| letter.is_ascii_uppercase()) =>
        {
            rest
        }
        _ => &name,
    };
    match name.to_ascii_lowercase().as_str() {
        "zapfdingbats" | "dingbats" | "itczapfdingbats" | "zapfdingbatsitc" => {
            Some(b"ZapfDingbatsEncoding")
        }
        "symbol" | "symbolmt" | "symbolitc" => Some(b"SymbolEncoding"),
        _ => None,
    }
}

/// Whether pdf-inspector reads a simple font's bytes byte by byte as
/// Windows-1252 has them, not as Latin-1: not for a TeX font, nor one named
/// for mathematics, symbols, dingbats, or emoji.
fn windows_1252_font(base_font: Option<&[u8]>) -> bool {
    const TEX: [&str; 16] = [
        "cmr", "cmb", "cmmi", "cmsy", "cmex", "cmtt", "cmss", "cmti", "ecrm", "ecbx", "ecti",
        "tcrm", "tctt", "msam", "msbm", "ttdc",
    ];
    let Some(name) = base_font else {
        return true;
    };
    let name = String::from_utf8_lossy(name);
    let name = name
        .rsplit_once('+')
        .map_or(&*name, |(_, name)| name)
        .to_ascii_lowercase();
    !TEX.iter().any(|prefix| name.starts_with(prefix))
        && !["math", "symbol", "dingbat", "emoji"]
            .iter()
            .any(|word| name.contains(word))
}

/// How lopdf reads a simple font's encoding (see `Lopdf`): as pdf-inspector
/// reads a string by it, code by code.
fn lopdf_reading(
    document: &Document,
    font: &Dictionary,
    base_font: Option<&[u8]>,
    steps: &mut usize,
) -> Lopdf {
    let symbols = base_font.is_some_and(|name| {
        let name = name.to_ascii_lowercase();
        [&b"symbol"[..], b"wingdings", b"zapfdingbats"]
            .iter()
            .any(|word| name.windows(word.len()).any(|window| window == *word))
    });
    match font.get_font_encoding_with_limit(document, MAX_CMAP_BYTES) {
        Ok(lopdf::Encoding::UnicodeMapEncoding(_)) => Lopdf::Unknown,
        Ok(encoding) => {
            *steps += 256;
            let mut codes = Vec::with_capacity(256);
            for code in 0..=u8::MAX {
                match Document::decode_text(&encoding, &[code]) {
                    Ok(text) => codes.push(text),
                    Err(_) => return Lopdf::Bytes { symbols },
                }
            }
            Lopdf::Codes(codes.into_boxed_slice())
        }
        Err(lopdf::Error::Decompress(_)) => Lopdf::Unknown,
        Err(_) => Lopdf::Bytes { symbols },
    }
}

/// Whether pdf-inspector reads a composite font's codes as the code points
/// they are, as it does a font under `Identity-H` or `Identity-V`, named in
/// place, with no ToUnicode map and no other map to read: its descendant,
/// given by reference, embeds no TrueType or OpenType program and names no
/// collection whose map is read first (Korea1, which pdf-inspector holds,
/// and Japan1, GB1, and CNS1, whose fonts `cjk_fonts` reads), and the codes
/// its widths list have a median of 0x41 or more, as letters' code points
/// do.
fn reads_code_points(document: &Document, font: &Dictionary) -> bool {
    let identity = matches!(font.get(b"Encoding"),
        Ok(Object::Name(name)) if name == b"Identity-H" || name == b"Identity-V");
    if !identity || font.has(b"ToUnicode") {
        return false;
    }
    let descendant = match font.get(b"DescendantFonts") {
        Ok(Object::Array(fonts)) => fonts.first(),
        Ok(Object::Reference(id)) => document
            .get_object(*id)
            .ok()
            .and_then(|fonts| fonts.as_array().ok())
            .and_then(|fonts| fonts.first()),
        _ => None,
    };
    let Some(Object::Reference(id)) = descendant else {
        return false;
    };
    let Ok(descendant) = document.get_dictionary(*id) else {
        return false;
    };
    let dictionary = |object| resolved(document, object).and_then(|object| object.as_dict().ok());
    let program = descendant
        .get(b"FontDescriptor")
        .ok()
        .and_then(dictionary)
        .is_some_and(|descriptor| {
            [&b"FontFile2"[..], b"FontFile3"]
                .iter()
                .any(|key| descriptor.get(key).and_then(Object::as_reference).is_ok())
        });
    let collection = descendant
        .get(b"CIDSystemInfo")
        .ok()
        .and_then(dictionary)
        .and_then(|info| info.get(b"Ordering").ok())
        .is_some_and(|ordering| {
            matches!(ordering, Object::String(name, _)
                if [&b"Korea1"[..], b"Japan1", b"GB1", b"CNS1"].contains(&name.as_slice()))
        });
    !program && !collection && median_width_code(descendant).is_some_and(|median| median >= 0x41)
}

/// The median of the distinct codes a composite font's `/W` array gives
/// widths, read as pdf-inspector reads them: only an array written in
/// place, whole numbers, and up to `MAX_CID_WIDTHS` codes.
fn median_width_code(descendant: &Dictionary) -> Option<u16> {
    let Ok(Object::Array(entries)) = descendant.get(b"W") else {
        return None;
    };
    let mut seen: HashSet<u16> = HashSet::new();
    let mut index = 0;
    while index < entries.len() && seen.len() < MAX_CID_WIDTHS {
        let Ok(start) = entries[index].as_i64() else {
            index += 1;
            continue;
        };
        let start = start as u16;
        match entries.get(index + 1) {
            // `c [w1 w2 …]` gives the codes from c on, one a width.
            Some(Object::Array(widths)) => {
                for offset in 0..widths.len() {
                    if seen.len() >= MAX_CID_WIDTHS {
                        break;
                    }
                    seen.insert(start.wrapping_add(offset as u16));
                }
                index += 2;
            }
            // `c1 c2 w` gives the codes from c1 to c2.
            Some(_) if index + 2 < entries.len() => {
                if let Ok(end) = entries[index + 1].as_i64() {
                    for code in start..=end as u16 {
                        if seen.len() >= MAX_CID_WIDTHS {
                            break;
                        }
                        seen.insert(code);
                    }
                }
                index += 3;
            }
            Some(_) => index += 1,
            None => {
                seen.insert(start);
                index += 1;
            }
        }
    }
    let mut codes: Vec<u16> = seen.into_iter().collect();
    codes.sort_unstable();
    codes.get(codes.len() / 2).copied()
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

/// The width in em of each code of a simple font naming a standard font
/// (see `standard_fonts`) without widths of its own, as a viewer and
/// pdf-inspector take it: the width of the glyph for the character its
/// differences name, else the character the encoding it names, or its
/// encoding's base, gives it, else the character Windows-1252 reads it as,
/// its letters' together for a ligature named by them; half an em where
/// the font has no such glyph, as pdf-inspector takes one.
fn standard_widths(
    document: &Document,
    font: &Dictionary,
    names: &HashMap<u8, String>,
) -> Option<Box<[f64]>> {
    let subtype = font.get(b"Subtype").ok()?.as_name().ok()?;
    if !matches!(subtype, b"Type1" | b"TrueType" | b"MMType1") {
        return None;
    }
    let standard = crate::standard_fonts::widths(font.get(b"BaseFont").ok()?.as_name().ok()?)?;
    let base = match font
        .get(b"Encoding")
        .ok()
        .and_then(|encoding| resolved(document, encoding))
    {
        Some(Object::Name(name)) => predefined(name),
        Some(Object::Dictionary(encoding)) => encoding
            .get(b"BaseEncoding")
            .ok()
            .and_then(|base| resolved(document, base))
            .and_then(|base| base.as_name().ok())
            .and_then(predefined),
        _ => None,
    };
    let width = |text: &str| -> f64 {
        text.chars()
            .map(|character| standard.width(character).unwrap_or(0.5))
            .sum()
    };
    Some(
        (0..=u8::MAX)
            .map(|code| match names.get(&code) {
                Some(name) => glyph_name_to_string(name).map_or(0.5, |text| width(&text)),
                None => base
                    .and_then(|base| base[usize::from(code)])
                    .or_else(|| (code >= 0x20).then(|| crate::text_paints::windows_1252(code)))
                    .map_or(0.5, |character| width(&character.to_string())),
            })
            .collect(),
    )
}

/// A width, or a reference to one, whole as pdf-inspector reads it.
fn width(document: &Document, value: &Object) -> Option<f64> {
    let value = match value {
        Object::Reference(id) => document.get_object(*id).ok()?,
        value => value,
    };
    let width = match value {
        Object::Integer(width) => *width as f64,
        Object::Real(width) => f64::from(*width),
        _ => return None,
    };
    (width.is_finite() && width >= 0.0).then_some(width.trunc())
}

/// A simple font's widths, from its `/FirstChar` and `/Widths`, scaled by
/// its `/FontMatrix`, as pdf-inspector 1.25.0 reads them.
fn simple_widths(document: &Document, font: &Dictionary, steps: &mut usize) -> Option<Widths> {
    let first = code_bound(document, font.get(b"FirstChar").ok()?)?;
    let last = code_bound(document, font.get(b"LastChar").ok()?)?;
    let values = resolved_array(document, font.get(b"Widths").ok()?)?;
    let scale = match font
        .get(b"FontMatrix")
        .ok()
        .and_then(|matrix| resolved_array(document, matrix))
        .and_then(<[Object]>::first)
    {
        Some(Object::Real(scale)) => f64::from(*scale).abs(),
        Some(Object::Integer(scale)) => (*scale as f64).abs(),
        _ => 0.001,
    };
    let mut widths = vec![0.0; 256];
    for (index, value) in values.iter().enumerate() {
        *steps += 1;
        let code = usize::from(first) + index;
        if code > usize::from(last) || code >= widths.len() {
            break;
        }
        if let Some(width) = width(document, value) {
            widths[code] = width * scale;
        }
    }
    Some(Widths::Simple(widths))
}

/// A composite font's widths, from its descendant's `/W` and `/DW`, as
/// pdf-inspector 1.25.0 reads them.
fn composite_widths(document: &Document, font: &Dictionary, steps: &mut usize) -> Option<Widths> {
    let descendant = match resolved_array(document, font.get(b"DescendantFonts").ok()?)?.first()? {
        Object::Reference(id) => document.get_dictionary(*id).ok()?,
        Object::Dictionary(descendant) => descendant,
        _ => return None,
    };
    let default = match descendant.get(b"DW") {
        Ok(Object::Integer(width)) => *width as f64,
        Ok(Object::Real(width)) => f64::from(*width),
        _ => 1000.0,
    };
    let code = |value: &Object| match value {
        Object::Integer(code) => Some(*code as u16),
        Object::Real(code) => Some(*code as u16),
        _ => None,
    };
    let mut widths = HashMap::new();
    let entries = descendant
        .get(b"W")
        .ok()
        .and_then(|entries| resolved_array(document, entries))
        .unwrap_or_default();
    let mut index = 0;
    // `c [w1 w2 …]` gives the codes from c on a width each; `c1 c2 w`, the
    // codes from c1 to c2 one width.
    while index < entries.len() && widths.len() < MAX_CID_WIDTHS && *steps <= MAX_FONT_STEPS {
        *steps += 1;
        let Some(start) = code(&entries[index]) else {
            index += 1;
            continue;
        };
        let listed = entries
            .get(index + 1)
            .and_then(|next| resolved_array(document, next));
        if let Some(listed) = listed {
            for (offset, value) in listed.iter().enumerate().take(MAX_CID_WIDTHS) {
                *steps += 1;
                if let Some(width) = width(document, value) {
                    widths.insert(start.wrapping_add(offset as u16), width * 0.001);
                }
            }
            index += 2;
            continue;
        }
        let end = entries.get(index + 1).and_then(code);
        let value = entries
            .get(index + 2)
            .and_then(|value| width(document, value));
        let (Some(end), Some(value)) = (end, value) else {
            break;
        };
        for code in (start..=end).take(MAX_CID_WIDTHS) {
            *steps += 1;
            widths.insert(code, value * 0.001);
        }
        index += 3;
    }
    Some(Widths::Composite(widths, default * 0.001))
}

/// A glyph shown in a string.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Glyph {
    /// The font it is shown in (see [`GlyphFonts`]).
    pub(crate) font: usize,
    /// What it reads as.
    pub(crate) reading: Reading,
    /// Where its origin is, in device space.
    pub(crate) at: [f64; 2],
    /// The baseline's direction, one em long, in device space.
    pub(crate) em: [f64; 2],
    /// How far it moves the pen along the baseline, in em, when the font's
    /// widths say.
    pub(crate) advance: Option<f64>,
    /// Whether it starts its string.
    pub(crate) first: bool,
}

/// The word being shown.
struct Word {
    text: String,
    font: usize,
    /// Where its last glyph is, the baseline, and how far that glyph moves
    /// the pen.
    last: [f64; 2],
    em: [f64; 2],
    advance: Option<f64>,
    /// How many strings hold its glyphs.
    strings: u32,
    /// The widest step past a glyph's advance where two of its strings
    /// meet, in em: infinite where the advance is not known.
    gap: f64,
}

impl Word {
    fn new(glyph: &Glyph) -> Word {
        Word {
            text: String::new(),
            font: glyph.font,
            last: glyph.at,
            em: glyph.em,
            advance: glyph.advance,
            strings: 1,
            gap: f64::NEG_INFINITY,
        }
    }

    /// How far past the last glyph's advance, in em, a glyph at `next`
    /// starts, when it continues the word: on its baseline and ahead of the
    /// last glyph, no further past it than pdf-inspector joins two runs, or,
    /// where the advance is not known, than the widest glyph's advance.
    fn step(&self, next: [f64; 2]) -> Option<f64> {
        let length = self.em[0].hypot(self.em[1]);
        if length <= 0.0 || !length.is_finite() {
            return None;
        }
        let step = [next[0] - self.last[0], next[1] - self.last[1]];
        let along = (step[0] * self.em[0] + step[1] * self.em[1]) / (length * length);
        let across = (self.em[0] * step[1] - self.em[1] * step[0]) / (length * length);
        if along <= 0.0 || along.is_nan() || across.abs() > MAX_BASELINE_SHIFT_EM {
            return None;
        }
        match self.advance {
            Some(advance) => Some(along - advance).filter(|gap| *gap <= MAX_GLYPH_GAP_EM),
            None => (along <= MAX_GLYPH_STEP_EM).then_some(f64::INFINITY),
        }
    }
}

/// The words a page shows glyph by glyph, as the scan goes.
#[derive(Default)]
pub(crate) struct GlyphWords {
    current: Option<Word>,
    /// Fonts seen painting a space glyph right after a word's glyphs, the
    /// space at the start of its string or the word across strings.
    painting_spaces: HashSet<usize>,
    /// Words across two strings or more that meet a word gap apart, by text
    /// and font, with the widest gap.
    words: HashMap<(String, usize), f64>,
}

impl GlyphWords {
    /// A glyph shown in a string, in the order the string shows them.
    pub(crate) fn glyph(&mut self, glyph: Glyph) {
        let step = self
            .current
            .as_ref()
            .filter(|word| word.font == glyph.font)
            .and_then(|word| word.step(glyph.at));
        match glyph.reading {
            Reading::Space => {
                // A string of a word and its space, as most producers write
                // text, says nothing of how the words are shown.
                let painted = step.is_some()
                    && self
                        .current
                        .as_ref()
                        .is_some_and(|word| glyph.first || word.strings >= 2);
                if painted {
                    self.painting_spaces.insert(glyph.font);
                }
                self.end_word();
            }
            Reading::Word(letters, count) => {
                match (step, self.current.as_mut()) {
                    (Some(step), Some(word)) => {
                        if glyph.first {
                            word.strings += 1;
                            word.gap = word.gap.max(step);
                        }
                    }
                    _ => {
                        self.end_word();
                        self.current = Some(Word::new(&glyph));
                    }
                }
                let Some(word) = self.current.as_mut() else {
                    return;
                };
                word.text
                    .extend(letters[..usize::from(count)].iter().map(|&b| char::from(b)));
                word.last = glyph.at;
                word.advance = glyph.advance;
                if word.text.len() > MAX_WORD_BYTES {
                    self.current = None;
                }
            }
            Reading::Other => self.end_word(),
            Reading::Unread => self.current = None,
        }
    }

    /// Text shown at `at` in a font whose glyphs are not read: a word it
    /// goes on with is not read whole, and one it does not ends where it
    /// stands.
    pub(crate) fn interrupt(&mut self, at: [f64; 2]) {
        if self
            .current
            .as_ref()
            .is_some_and(|word| word.step(at).is_some())
        {
            self.current = None;
        } else {
            self.end_word();
        }
    }

    /// Text or a form whose place is not known, where the word being shown
    /// may go on: it is not read whole.
    pub(crate) fn lose(&mut self) {
        self.current = None;
    }

    /// The word being shown ends here, as at the end of a text object.
    pub(crate) fn end_word(&mut self) {
        let Some(word) = self.current.take() else {
            return;
        };
        // pdf-inspector reads a word in one string whole, and one whose
        // strings meet closer than a word gap.
        if word.strings < 2 || word.gap <= MIN_WORD_GAP_EM {
            return;
        }
        let key = (word.text, word.font);
        if let Some(gap) = self.words.get_mut(&key) {
            *gap = gap.max(word.gap);
        } else if self.words.len() < MAX_WORDS_PER_PAGE {
            self.words.insert(key, word.gap);
        }
    }

    /// The words shown, each with its font and widest gap, and the fonts
    /// seen painting their spaces.
    pub(crate) fn finish(mut self) -> (Vec<(String, usize, f64)>, HashSet<usize>) {
        self.end_word();
        let words = self
            .words
            .into_iter()
            .map(|((text, font), gap)| (text, font, gap))
            .collect();
        (words, self.painting_spaces)
    }
}

/// A word a page shows glyph by glyph in a font painting its spaces, with
/// the widest step past a glyph's advance where its strings meet, in em
/// (infinite where the font's widths are not known).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ShownWord {
    pub(crate) page: u32,
    pub(crate) text: String,
    pub(crate) gap: f64,
}

/// Words shown glyph by glyph kept across a document: those of fonts seen
/// painting their spaces so far, and, up to a bound for each, those of the
/// other fonts, kept until the font is seen painting its spaces, on any
/// page.
pub(crate) struct KeptWords {
    painting: HashSet<usize>,
    /// The widest gap of each word, by page and text.
    kept: HashMap<(u32, String), f64>,
    pending: HashMap<usize, Vec<(u32, String, f64)>>,
    pending_total: usize,
    max_kept: usize,
    max_pending: usize,
}

impl Default for KeptWords {
    fn default() -> Self {
        KeptWords::bounded(MAX_GLYPH_WORDS, MAX_PENDING_WORDS)
    }
}

impl KeptWords {
    /// Keeping `max_kept` words, and `max_pending` for each font not yet
    /// seen painting its spaces, as many in all.
    fn bounded(max_kept: usize, max_pending: usize) -> Self {
        KeptWords {
            painting: HashSet::new(),
            kept: HashMap::new(),
            pending: HashMap::new(),
            pending_total: 0,
            max_kept,
            max_pending,
        }
    }

    /// The words a page shows, with their fonts, and the fonts it shows
    /// painting their spaces.
    pub(crate) fn page(
        &mut self,
        page: u32,
        words: Vec<(String, usize, f64)>,
        painting: HashSet<usize>,
    ) {
        for font in painting {
            if self.painting.insert(font) {
                let pending = self.pending.remove(&font).unwrap_or_default();
                self.pending_total -= pending.len();
                for (page, text, gap) in pending {
                    self.keep(page, text, gap);
                }
            }
        }
        for (text, font, gap) in words {
            if self.painting.contains(&font) {
                self.keep(page, text, gap);
                continue;
            }
            let pending = self.pending.entry(font).or_default();
            if pending.len() < self.max_pending && self.pending_total < self.max_kept {
                pending.push((page, text, gap));
                self.pending_total += 1;
            }
        }
    }

    fn keep(&mut self, page: u32, text: String, gap: f64) {
        let room = self.kept.len() < self.max_kept;
        match self.kept.entry((page, text)) {
            Entry::Occupied(mut kept) => {
                let widest = kept.get().max(gap);
                kept.insert(widest);
            }
            Entry::Vacant(slot) if room => {
                slot.insert(gap);
            }
            Entry::Vacant(_) => {}
        }
    }

    /// The words kept: those of the fonts seen painting their spaces.
    pub(crate) fn finish(self) -> Vec<ShownWord> {
        let mut words: Vec<ShownWord> = self
            .kept
            .into_iter()
            .map(|((page, text), gap)| ShownWord { page, text, gap })
            .collect();
        words.sort_by(|one, other| (one.page, &one.text).cmp(&(other.page, &other.text)));
        words
    }
}

/// How often a text shows a word split by a space or a cell edge, and how
/// often split other than right after a hyphen.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Splits {
    all: u32,
    /// A space after a hyphen may be where a paragraph joins the lines of a
    /// compound the page wrapped ("self- employment"), which splits nothing.
    definite: u32,
}

/// Each split of one of `automaton`'s patterns in `text`: where the text
/// shows it split by a space or a cell edge, with nothing of a word
/// adjoining it; escapes and emphasis marks are read through. A line end
/// inside a word is where the page wrapped it, which splits nothing.
/// `split` is given the pattern, the line it stands on, and whether it is
/// split other than right after a hyphen.
fn each_split(text: &str, automaton: &AhoCorasick, mut split: impl FnMut(usize, usize, bool)) {
    let mut squeezed: Vec<u8> = Vec::with_capacity(text.len());
    let mut separated: Vec<bool> = Vec::with_capacity(text.len() + 1);
    let mut wrapped: Vec<bool> = Vec::with_capacity(text.len());
    // Where each line starts in the squeezed text.
    let mut lines: Vec<usize> = vec![0];
    let (mut gap, mut line_end) = (true, false);
    for character in text.chars() {
        if character == '\n' {
            lines.push(squeezed.len());
        }
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
        let mut gaps = (start + 1..end).filter(|&at| separated[at]).peekable();
        if gaps.peek().is_none() {
            continue;
        }
        let definite = gaps.any(|at| squeezed[at - 1] != b'-');
        let line = lines.partition_point(|&line| line <= start) - 1;
        split(found.pattern().as_usize(), line, definite);
    }
}

/// How often `text` shows each of `patterns` split (see [`each_split`]).
fn counts(text: &str, patterns: &[&str]) -> Vec<Splits> {
    let mut splits = vec![Splits::default(); patterns.len()];
    if let Ok(automaton) = AhoCorasick::new(patterns) {
        each_split(text, &automaton, |pattern, _, definite| {
            splits[pattern].all += 1;
            splits[pattern].definite += u32::from(definite);
        });
    }
    splits
}

/// The words among `words` that the Markdown shows split somewhere, with how
/// often: which pages split them, the pages' own text tells (see
/// [`split_pages`]).
pub(crate) fn misread(markdown: &str, words: &[ShownWord]) -> HashMap<String, Splits> {
    let patterns: Vec<&str> = words
        .iter()
        .map(|word| word.text.as_str())
        .collect::<HashSet<&str>>()
        .into_iter()
        .collect();
    if patterns.is_empty() {
        return HashMap::new();
    }
    patterns
        .iter()
        .zip(counts(markdown, &patterns))
        .filter(|(_, splits)| splits.all > 0)
        .map(|(text, splits)| ((*text).to_string(), splits))
        .collect()
}

/// The pages showing a misread word, whose own text tells whether they
/// split it: those whose words step widest past their glyphs' advances
/// first, then in order.
pub(crate) fn pages_to_read(misread: &HashMap<String, Splits>, words: &[ShownWord]) -> Vec<u32> {
    let mut widest: HashMap<u32, f64> = HashMap::new();
    for word in words.iter().filter(|word| misread.contains_key(&word.text)) {
        let gap = widest.entry(word.page).or_insert(f64::NEG_INFINITY);
        *gap = gap.max(word.gap);
    }
    let mut pages: Vec<(u32, f64)> = widest.into_iter().collect();
    pages.sort_by(|one, other| other.1.total_cmp(&one.1).then(one.0.cmp(&other.0)));
    pages.into_iter().map(|(page, _)| page).collect()
}

/// A line of a page's text as pdf-inspector compares lines repeated across
/// pages: its words, without the page number at either end.
fn repeat_key(line: &str) -> String {
    let words: Vec<&str> = line
        .split(|character: char| character.is_whitespace() || character == ITEM_EDGE)
        .filter(|word| !word.is_empty())
        .collect();
    words
        .join(" ")
        .trim_start_matches(char::is_numeric)
        .trim_start()
        .trim_end_matches(char::is_numeric)
        .trim_end()
        .to_string()
}

/// The pages whose own text, as pdf-inspector places it (`page_text`),
/// shows a misread word split, a page for each split the Markdown shows,
/// and, past the pages read, those showing a word whose splits the pages
/// read do not account for. A page's splits count in order, those in a
/// line an earlier page shows too after the others: pdf-inspector keeps a
/// running header on the first page showing it and strips the rest. A page
/// not read counts for one split, those whose words step widest past their
/// glyphs' advances first, and only for a split away from a hyphen.
pub(crate) fn split_pages(
    misread: &HashMap<String, Splits>,
    words: &[ShownWord],
    page_text: &HashMap<u32, String>,
) -> Vec<u32> {
    let mut patterns: Vec<&str> = misread.keys().map(String::as_str).collect();
    patterns.sort_unstable();
    let index: HashMap<&str, usize> = patterns
        .iter()
        .enumerate()
        .map(|(index, pattern)| (*pattern, index))
        .collect();
    let mut shown: HashMap<u32, HashSet<usize>> = HashMap::new();
    let mut unread: Vec<Vec<(u32, f64)>> = vec![Vec::new(); patterns.len()];
    for word in words {
        let Some(&pattern) = index.get(word.text.as_str()) else {
            continue;
        };
        if page_text.contains_key(&word.page) {
            shown.entry(word.page).or_default().insert(pattern);
        } else {
            unread[pattern].push((word.page, word.gap));
        }
    }
    let mut read: Vec<u32> = shown.keys().copied().collect();
    read.sort_unstable();
    // Each word's splits on the pages read, in order: in lines first shown
    // there, and in lines an earlier page shows too.
    let mut own: Vec<Vec<(u32, [Splits; 2])>> = vec![Vec::new(); patterns.len()];
    if let Ok(automaton) = AhoCorasick::new(&patterns) {
        let mut seen: HashSet<String> = HashSet::new();
        for &page in &read {
            let text = &page_text[&page];
            let keys: Vec<String> = text.lines().map(repeat_key).collect();
            let repeated: Vec<bool> = keys.iter().map(|key| seen.contains(key)).collect();
            let showing = &shown[&page];
            each_split(text, &automaton, |pattern, line, definite| {
                if !showing.contains(&pattern) {
                    return;
                }
                let pages = &mut own[pattern];
                if pages.last().is_none_or(|(last, _)| *last != page) {
                    pages.push((page, [Splits::default(); 2]));
                }
                if let Some((_, splits)) = pages.last_mut() {
                    let class = usize::from(repeated.get(line).copied().unwrap_or(false));
                    splits[class].all += 1;
                    splits[class].definite += u32::from(definite);
                }
            });
            seen.extend(keys);
        }
    }
    let mut named: HashSet<u32> = HashSet::new();
    for (pattern, text) in patterns.iter().enumerate() {
        let (mut all, mut definite) = (misread[*text].all, misread[*text].definite);
        for class in 0..2 {
            for (page, found) in &own[pattern] {
                let found = found[class];
                if found.all == 0 || all == 0 {
                    continue;
                }
                named.insert(*page);
                all -= found.all.min(all);
                definite -= found.definite.min(definite);
            }
        }
        let pages = &mut unread[pattern];
        pages.sort_by(|one, other| other.1.total_cmp(&one.1).then(one.0.cmp(&other.0)));
        for &(page, _) in pages.iter().take(all.min(definite) as usize) {
            named.insert(page);
        }
    }
    let mut named: Vec<u32> = named.into_iter().collect();
    named.sort_unstable();
    named
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Words shown on pages, each with its widest gap.
    fn words(shown: &[(u32, &str, f64)]) -> Vec<ShownWord> {
        shown
            .iter()
            .map(|(page, text, gap)| ShownWord {
                page: *page,
                text: (*text).to_string(),
                gap: *gap,
            })
            .collect()
    }

    /// The pages the Markdown names, where no page is read again.
    fn split_pages(markdown: &str, shown: &[ShownWord]) -> Vec<u32> {
        super::split_pages(&misread(markdown, shown), shown, &HashMap::new())
    }

    /// Page texts by page.
    fn texts(pages: &[(u32, &str)]) -> HashMap<u32, String> {
        pages
            .iter()
            .map(|(page, text)| (*page, (*text).to_string()))
            .collect()
    }

    #[test]
    fn words_the_markdown_splits_name_their_pages() {
        let shown = words(&[
            (1, "LIABILITIES", 0.1),
            (2, "EQUITY", 0.1),
            (3, "83,476.03", 0.1),
        ]);
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
        // without their text, a page for each split, those whose words
        // step widest past their glyphs first.
        let shown = words(&[
            (2, "Limitations", 0.1),
            (7, "Limitations", 0.1),
            (18, "Limitations", 0.3),
        ]);
        let found = misread("Limitations; (Limitations) \"L imitations\"", &shown);
        assert_eq!(pages_to_read(&found, &shown), vec![18, 2, 7]);
        assert_eq!(
            super::split_pages(&found, &shown, &HashMap::new()),
            vec![18]
        );
        let own = texts(&[
            (2, "Limitations"),
            (7, "(A) L imitations"),
            (18, "Limitations"),
        ]);
        assert_eq!(super::split_pages(&found, &shown, &own), vec![7]);
    }

    #[test]
    fn whole_words_and_other_text_are_not_split() {
        let shown = words(&[(1, "LIABILITIES", 0.1), (1, "OF", 0.1)]);
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
            &words(&[(1, "LIABI-LITIES", 0.1)])
        )
        .is_empty());
        // A split elsewhere in the Markdown names a page only when its own
        // text splits the word.
        let found = misread("LIABILITIES LIABILITIES LIAB ILITIES", &shown);
        let own = texts(&[(1, "LIABILITIES and LIABILITIES")]);
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
    fn a_split_after_a_hyphen_is_found_by_the_page_text() {
        // A paragraph joins a compound the page wrapped with a space after
        // its hyphen: the Markdown alone does not tell it from a split.
        let shown = words(&[(1, "2025-12-31", 0.1), (2, "self-employment", 0.1)]);
        let markdown = "Trade date 2025- 12- 31 and self- employment";
        assert!(split_pages(markdown, &shown).is_empty());
        let found = misread(markdown, &shown);
        let own = texts(&[
            (1, "Trade date 2025- 12- 31"),
            (2, "income from self-\nemployment"),
        ]);
        assert_eq!(super::split_pages(&found, &shown, &own), vec![1]);
    }

    #[test]
    fn a_page_is_named_for_each_split_the_markdown_shows() {
        // A running header each page's text splits, which the Markdown
        // shows once: pdf-inspector keeps it on the first page.
        let header = "LIAB ILITIES AND EQ UITY STATEMENT";
        let shown: Vec<ShownWord> = (1..=5)
            .flat_map(|page| words(&[(page, "LIABILITIES", 0.1), (page, "EQUITY", 0.1)]))
            .collect();
        let own: HashMap<u32, String> = (1..=5)
            .map(|page| (page, format!("{header}\nDeposit {page} reference")))
            .collect();
        let found = misread(&format!("**{header}**\n\nDeposits"), &shown);
        assert_eq!(super::split_pages(&found, &shown, &own), vec![1]);
        // A split of its own on a later page names that page too.
        let mut own = own;
        own.insert(3, format!("{header}\nTotal LIAB ILITIES 4,514"));
        let found = misread(&format!("**{header}**\n\nTotal LIAB ILITIES 4,514"), &shown);
        assert_eq!(super::split_pages(&found, &shown, &own), vec![1, 3]);
        // A header the Markdown shows on every page names every page.
        let found = misread(&format!("{header}\n\n").repeat(5), &shown);
        assert_eq!(
            super::split_pages(&found, &shown, &own),
            vec![1, 2, 3, 4, 5]
        );
    }

    #[test]
    fn splits_the_pages_read_leave_name_pages_past_them() {
        // Seventy pages show the word; the Markdown splits it once, on a
        // page past those read, where its glyphs step widest.
        let mut shown: Vec<ShownWord> = (1..=69)
            .map(|page| words(&[(page, "LIABILITIES", 0.05)]).remove(0))
            .collect();
        shown.extend(words(&[(70, "LIABILITIES", 0.12)]));
        let markdown = format!("{}LIAB ILITIES\n", "LIABILITIES\n".repeat(69));
        let found = misread(&markdown, &shown);
        assert_eq!(pages_to_read(&found, &shown)[0], 70);
        let own: HashMap<u32, String> = (1..=64).map(|page| (page, "LIABILITIES".into())).collect();
        assert_eq!(super::split_pages(&found, &shown, &own), vec![70]);
        // Splits the pages read account for leave none.
        let own: HashMap<u32, String> = (1..=64)
            .map(|page| {
                let text = if page == 10 {
                    "LIAB ILITIES"
                } else {
                    "LIABILITIES"
                };
                (page, text.to_string())
            })
            .collect();
        assert_eq!(super::split_pages(&found, &shown, &own), vec![10]);
    }

    /// Show strings, each at its x on the baseline at 100, one em 8 wide,
    /// in `font`; each glyph `width` em wide, when the font says.
    fn show(page: &mut GlyphWords, font: usize, strings: &[(f64, &str)], width: Option<f64>) {
        let em = [8.0, 0.0];
        for (x, text) in strings {
            let mut at = [*x, 100.0];
            for (index, character) in text.chars().enumerate() {
                page.glyph(Glyph {
                    font,
                    reading: Reading::of(Some(&character.to_string())),
                    at,
                    em,
                    advance: width,
                    first: index == 0,
                });
                at[0] += width.unwrap_or(0.0) * 8.0;
            }
        }
    }

    /// Strings of one glyph each, advancing `step` from one to the next.
    fn glyph_by_glyph(x: f64, text: &str, step: f64) -> Vec<(f64, String)> {
        text.chars()
            .enumerate()
            .map(|(index, character)| (x + step * index as f64, character.to_string()))
            .collect()
    }

    /// The words a page shows in fonts it sees painting their spaces, with
    /// their widest gaps.
    fn painted(page: GlyphWords) -> Vec<(String, f64)> {
        let (words, painting) = page.finish();
        let mut found: Vec<(String, f64)> = words
            .into_iter()
            .filter(|(_, font, _)| painting.contains(font))
            .map(|(text, _, gap)| (text, (gap * 1000.0).round() / 1000.0))
            .collect();
        found.sort_by(|one, other| one.0.cmp(&other.0));
        found
    }

    fn strings(shown: &[(f64, String)]) -> Vec<(f64, &str)> {
        shown.iter().map(|(x, text)| (*x, text.as_str())).collect()
    }

    #[test]
    fn glyphs_on_one_baseline_make_words_ended_by_painted_spaces() {
        // Each glyph half an em wide, hinted to 5 units: a tenth of an em
        // past its width.
        let mut page = GlyphWords::default();
        let line = glyph_by_glyph(10.0, "LIABILITIES ARE 1,120", 5.0);
        show(&mut page, 0, &strings(&line), Some(0.5));
        assert_eq!(
            painted(page),
            vec![
                ("1,120".to_string(), 0.125),
                ("ARE".to_string(), 0.125),
                ("LIABILITIES".to_string(), 0.125)
            ]
        );
        // A font that never paints a space gives no words, and a font
        // without widths steps up to one and a half em.
        let mut page = GlyphWords::default();
        show(
            &mut page,
            1,
            &strings(&glyph_by_glyph(10.0, "TOTAL", 6.0)),
            None,
        );
        assert!(painted(page).is_empty());
        let mut page = GlyphWords::default();
        show(
            &mut page,
            1,
            &strings(&glyph_by_glyph(10.0, "NET DUE", 6.0)),
            None,
        );
        assert_eq!(
            painted(page),
            vec![
                ("DUE".to_string(), f64::INFINITY),
                ("NET".to_string(), f64::INFINITY)
            ]
        );
        // A step off the baseline, back, or past the width further than
        // pdf-inspector joins two runs, ends a word.
        let word = Word::new(&Glyph {
            font: 0,
            reading: Reading::Other,
            at: [10.0, 100.0],
            em: [8.0, 0.0],
            advance: Some(0.5),
            first: true,
        });
        assert_eq!(word.step([14.5, 100.0]), Some(0.0625));
        assert!(word.step([14.5, 102.0]).is_none());
        assert!(word.step([9.0, 100.0]).is_none());
        assert!(word.step([18.5, 100.0]).is_none());
    }

    #[test]
    fn words_in_strings_of_several_glyphs_are_placed_by_their_widths() {
        // As Skia writes a line: a glyph whose hinted advance is its width
        // stays in the string, so a space opens the string of the glyph
        // after it and a word goes on in a string of two.
        let mut page = GlyphWords::default();
        show(
            &mut page,
            0,
            &[
                (0.0, "B"),
                (5.0, "on"),
                (13.0, "d"),
                (17.0, " I"),
                (26.0, "n"),
            ],
            Some(0.5),
        );
        assert_eq!(
            painted(page),
            vec![("Bond".to_string(), 0.125), ("In".to_string(), 0.125)]
        );
        // A word in one string, as most producers write text, is read
        // whole, and its space is no sign of text shown glyph by glyph.
        let mut page = GlyphWords::default();
        show(&mut page, 0, &[(0.0, "Bond "), (22.0, "fund")], Some(0.5));
        assert!(painted(page).is_empty());
        // A word whose strings meet closer than a word gap is read whole.
        let mut page = GlyphWords::default();
        show(
            &mut page,
            0,
            &[(0.0, "B"), (4.1, "ond"), (16.1, " ")],
            Some(0.5),
        );
        assert!(painted(page).is_empty());
    }

    #[test]
    fn a_word_ends_where_its_run_does() {
        // A label and its value, shown as runs of their own without a space
        // between: the end of the label's text object ends it.
        let label = glyph_by_glyph(0.0, "Due date:", 5.0);
        let mut page = GlyphWords::default();
        show(&mut page, 0, &strings(&label), Some(0.5));
        page.end_word();
        show(
            &mut page,
            0,
            &strings(&glyph_by_glyph(46.0, "09", 5.0)),
            Some(0.5),
        );
        page.end_word();
        let apart = vec![
            ("09".to_string(), 0.125),
            ("Due".to_string(), 0.125),
            ("date:".to_string(), 0.125),
        ];
        assert_eq!(painted(page), apart);
        // In one run, a gap wider than pdf-inspector joins ends it too.
        let mut page = GlyphWords::default();
        show(&mut page, 0, &strings(&label), Some(0.5));
        show(
            &mut page,
            0,
            &strings(&glyph_by_glyph(49.0, "09", 5.0)),
            Some(0.5),
        );
        assert_eq!(painted(page), apart);
        // A word text in another font goes on with is not read whole; one
        // a string past it does not reach ends where it stands, and one text
        // whose place is not known may go on in is not read.
        let mut page = GlyphWords::default();
        show(
            &mut page,
            0,
            &[(0.0, "A"), (5.0, "B"), (10.0, " ")],
            Some(0.5),
        );
        show(&mut page, 0, &[(12.0, "E"), (17.0, "F")], Some(0.5));
        page.interrupt([22.0, 100.0]);
        show(
            &mut page,
            0,
            &[(40.0, " "), (42.0, "O"), (47.0, "F")],
            Some(0.5),
        );
        page.interrupt([90.0, 100.0]);
        show(&mut page, 0, &[(100.0, "T"), (105.0, "O")], Some(0.5));
        page.lose();
        assert_eq!(
            painted(page),
            vec![("AB".to_string(), 0.125), ("OF".to_string(), 0.125)]
        );
    }

    #[test]
    fn a_ligature_stands_for_its_letters() {
        assert_eq!(Reading::of(Some(" ")), Reading::Space);
        assert_eq!(Reading::of(Some("(")), Reading::Other);
        assert_eq!(Reading::of(Some("\u{e9}")), Reading::Other);
        assert_eq!(Reading::of(Some("fi")), Reading::Word(*b"fi\0", 2));
        assert_eq!(Reading::of(Some("ffi")), Reading::Word(*b"ffi", 3));
        for text in [None, Some(""), Some("ffil"), Some("f)")] {
            assert_eq!(Reading::of(text), Reading::Unread);
        }
        let mut page = GlyphWords::default();
        let em = [8.0, 0.0];
        let glyphs = ["B", "e", "n", "e", "fi", "t", "s", " "];
        for (index, text) in glyphs.into_iter().enumerate() {
            page.glyph(Glyph {
                font: 0,
                reading: Reading::of(Some(text)),
                at: [5.0 * index as f64, 100.0],
                em,
                advance: Some(0.5),
                first: true,
            });
        }
        assert_eq!(painted(page), vec![("Benefits".to_string(), 0.125)]);
    }

    /// What `bytes` read as in `font`, in `document`: `None` where the font
    /// or a code cannot be read.
    fn read(document: &Document, font: &Dictionary, bytes: &[u8]) -> Option<String> {
        let mut fonts = GlyphFonts::default();
        let font = fonts.font(document, font)?;
        fonts.text(font, bytes)
    }

    #[test]
    fn simple_fonts_read_codes_past_ascii_as_pdf_inspector_reads_them() {
        use lopdf::dictionary;
        let mut document = Document::with_version("1.7");
        let program = document.add_object(lopdf::Stream::new(dictionary! {}, Vec::new()));
        let helvetica = |encoding: Option<Object>| {
            let mut font = dictionary! {
                "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
            };
            if let Some(encoding) = encoding {
                font.set("Encoding", encoding);
            }
            font
        };
        let text = |font: &Dictionary, bytes: &[u8]| read(&document, font, bytes);
        // By the encoding the font names, as lopdf reads it: Windows ANSI,
        // Mac Roman, and, for a font naming none, Standard, whose apostrophe
        // is curly.
        let win_ansi = helvetica(Some("WinAnsiEncoding".into()));
        assert_eq!(
            text(&win_ansi, b"remplac\xe9"),
            Some("remplac\u{e9}".into())
        );
        assert_eq!(
            text(&win_ansi, b"Don\x92t \x96 \x80"),
            Some("Don\u{2019}t \u{2013} \u{20ac}".into())
        );
        let mac_roman = helvetica(Some("MacRomanEncoding".into()));
        assert_eq!(text(&mac_roman, b"\x8e"), Some("\u{e9}".into()));
        assert_eq!(
            text(&helvetica(None), b"don't"),
            Some("don\u{2019}t".into())
        );
        // A string written in UTF-8 reads as UTF-8.
        assert_eq!(
            text(&win_ansi, "Jos\u{e9}".as_bytes()),
            Some("Jos\u{e9}".into())
        );
        // A base encoding reads the codes the differences leave; a control
        // code reads as nothing.
        let based = helvetica(Some(Object::Dictionary(dictionary! {
            "Type" => "Encoding", "BaseEncoding" => "MacRomanEncoding",
            "Differences" => vec![233.into(), "egrave".into()],
        })));
        assert_eq!(text(&based, b"\xe9\x8e\x01"), Some("\u{e8}\u{e9}".into()));
        // With none, a string one of whose codes the differences name reads
        // its other codes as Windows-1252 has them, and a string with none
        // reads as lopdf reads Standard, the base it takes.
        let named = helvetica(Some(Object::Dictionary(dictionary! {
            "Type" => "Encoding", "Differences" => vec![65.into(), "B".into()],
        })));
        assert_eq!(text(&named, b"A\xe9"), Some("B\u{e9}".into()));
        assert_eq!(text(&named, b"\xe9"), Some("\u{d8}".into()));
        // A font lopdf cannot read reads byte by byte: a TeX font as
        // Latin-1, any other as Windows-1252.
        let untyped = |name: &str| dictionary! { "Subtype" => "Type1", "BaseFont" => name };
        assert_eq!(text(&untyped("CMR10"), b"\x92"), Some("\u{92}".into()));
        assert_eq!(
            text(&untyped("Helvetica"), b"\x92"),
            Some("\u{2019}".into())
        );
        // Symbol reads through an encoding of its own, and a font without
        // an encoding through its embedded program: past ASCII, the check
        // cannot tell.
        let symbol = dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Symbol" };
        assert_eq!(text(&symbol, b"a"), None);
        let embedded = dictionary! {
            "Type" => "Font", "Subtype" => "TrueType", "BaseFont" => "ABCDEF+Sans",
            "FontDescriptor" => dictionary! { "Type" => "FontDescriptor", "FontFile2" => program },
        };
        assert_eq!(text(&embedded, b"AB"), Some("AB".into()));
        assert_eq!(text(&embedded, b"A\xe9"), None);
        // A word ends at a letter past ASCII, where it ended at the code.
        let mut fonts = GlyphFonts::default();
        let font = fonts.font(&document, &win_ansi).expect("a font");
        assert_eq!(fonts.glyph(font, 0xE9).0, Reading::Other);
        assert_eq!(fonts.glyph(font, 0x01).0, Reading::Unread);
    }

    #[test]
    fn composite_fonts_without_a_map_read_as_pdf_inspector_reads_them() {
        use lopdf::dictionary;
        let mut document = Document::with_version("1.7");
        let program = document.add_object(lopdf::Stream::new(dictionary! {}, Vec::new()));
        // A descendant in `ordering` whose widths list `widths`, embedding a
        // program when `embedded`.
        let mut descendant = |ordering: &str, widths: Vec<Object>, embedded: bool| {
            let mut descriptor = dictionary! { "Type" => "FontDescriptor" };
            if embedded {
                descriptor.set("FontFile2", program);
            }
            document.add_object(dictionary! {
                "Type" => "Font", "Subtype" => "CIDFontType2", "BaseFont" => "ArialMT",
                "CIDSystemInfo" => dictionary! {
                    "Registry" => Object::string_literal("Adobe"),
                    "Ordering" => Object::string_literal(ordering), "Supplement" => 0,
                },
                "FontDescriptor" => descriptor, "W" => widths,
            })
        };
        // Widths given for the code points of the space and the letters, as
        // a font whose codes are them has; and for glyph indexes from 3 on.
        let letters: Vec<Object> = vec![
            32.into(),
            vec![Object::from(278)].into(),
            65.into(),
            vec![Object::from(667); 26].into(),
            97.into(),
            vec![Object::from(556); 26].into(),
        ];
        let code_points = descendant("Identity", letters.clone(), false);
        let glyphs = descendant(
            "Identity",
            vec![3.into(), vec![Object::from(278); 40].into()],
            false,
        );
        let embedded = descendant("Identity", letters.clone(), true);
        let japanese = descendant("Japan1", letters, false);
        let map = document.add_object(lopdf::Stream::new(
            dictionary! {},
            b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap /CMapName /Custom def \
              1 begincodespacerange <0000> <FFFF> endcodespacerange 1 beginbfchar <0001> <0041> \
              endbfchar endcmap CMapName currentdict /CMap defineresource pop end end"
                .to_vec(),
        ));
        let font = |descendant: lopdf::ObjectId| {
            dictionary! {
                "Type" => "Font", "Subtype" => "Type0", "BaseFont" => "ArialMT",
                "Encoding" => "Identity-H", "DescendantFonts" => vec![descendant.into()],
            }
        };
        // Codes read as code points, where the widths say they are; a string
        // most of whose codes are no characters pdf-inspector reads otherwise.
        assert_eq!(
            read(&document, &font(code_points), b"\x00I\x00g\x00n"),
            Some("Ign".into())
        );
        assert_eq!(
            read(&document, &font(code_points), b"\x00\x01\x00\x02\x00A"),
            None
        );
        // Not where they are glyph indexes, or pdf-inspector reads a program
        // or a collection's map first.
        for descendant in [glyphs, embedded, japanese] {
            assert_eq!(read(&document, &font(descendant), b"\x00I"), None);
        }
        // A code a font's map does not read stands as the character given
        // for it, where another code of the string is read.
        let mut mapped = font(glyphs);
        mapped.set("ToUnicode", map);
        let mut fonts = GlyphFonts::default();
        let mapped = fonts.font(&document, &mapped).expect("a font");
        let string = b"\x00\x01\x00\x09\x00\x01";
        assert_eq!(fonts.text(mapped, string), None);
        assert_eq!(fonts.text_or(mapped, string, ' '), Some("A A".into()));
        assert_eq!(fonts.text_or(mapped, b"\x00\x09", ' '), None);
        // Nor a font under a predefined CMap, whatever its widths say, which
        // pdf-inspector reads by the CMap (see `cjk_fonts`).
        let mut ucs2 = font(code_points);
        ucs2.set("Encoding", "UniJIS-UCS2-H");
        assert_eq!(read(&document, &ucs2, b"\x00I"), None);
    }

    #[test]
    fn widths_are_read_as_pdf_inspector_reads_them() {
        let document = Document::new();
        let mut steps = 0;
        let thousandths = |widths: &Widths, codes: &[u16]| -> Vec<f64> {
            codes
                .iter()
                .map(|&code| (widths.of(code) * 1000.0).round())
                .collect()
        };
        // A simple font's widths, scaled by its font matrix; a code without
        // one is none wide.
        let mut font = Dictionary::new();
        font.set("FirstChar", Object::Integer(65));
        font.set("LastChar", Object::Integer(66));
        font.set("Widths", vec![Object::Integer(10), Object::Real(20.5)]);
        font.set(
            "FontMatrix",
            [0.01, 0.0, 0.0, 0.01, 0.0, 0.0].map(Object::Real).to_vec(),
        );
        let widths = simple_widths(&document, &font, &mut steps).expect("simple widths");
        assert_eq!(thousandths(&widths, &[65, 66, 67]), vec![100.0, 200.0, 0.0]);
        font.remove(b"Widths");
        assert!(simple_widths(&document, &font, &mut steps).is_none());
        // A composite font's, listed or by range, and its default for the
        // rest.
        let mut descendant = Dictionary::new();
        descendant.set("DW", Object::Integer(500));
        descendant.set(
            "W",
            vec![
                Object::Integer(3),
                Object::Array(vec![Object::Integer(250), Object::Integer(333)]),
                Object::Integer(36),
                Object::Integer(38),
                Object::Integer(722),
            ],
        );
        let mut font = Dictionary::new();
        font.set("DescendantFonts", vec![Object::Dictionary(descendant)]);
        let widths = composite_widths(&document, &font, &mut steps).expect("composite widths");
        assert_eq!(
            thousandths(&widths, &[3, 4, 36, 38, 39]),
            vec![250.0, 333.0, 722.0, 722.0, 500.0]
        );
    }

    #[test]
    fn fonts_not_seen_painting_spaces_keep_their_words_apart() {
        let word = |text: &str, font: usize| (text.to_string(), font, 0.1);
        let mut kept = KeptWords::bounded(4, 2);
        // A figures font never seen painting a space fills its own share.
        kept.page(
            1,
            vec![word("1,120", 2), word("1,015", 2), word("2,000", 2)],
            HashSet::new(),
        );
        // A font's words before its first painted space are kept for it.
        kept.page(2, vec![word("ASSETS", 1)], HashSet::new());
        kept.page(3, vec![word("LIABILITIES", 1)], HashSet::from([1]));
        kept.page(4, vec![word("EQUITY", 1), word("3,394", 2)], HashSet::new());
        let found: Vec<(u32, String)> = kept
            .finish()
            .into_iter()
            .map(|word| (word.page, word.text))
            .collect();
        assert_eq!(
            found,
            vec![
                (2, "ASSETS".to_string()),
                (3, "LIABILITIES".to_string()),
                (4, "EQUITY".to_string())
            ]
        );
    }
}
