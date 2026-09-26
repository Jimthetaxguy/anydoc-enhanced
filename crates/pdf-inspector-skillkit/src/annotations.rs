//! Annotation text pdf-inspector 1.25.0 never reads.
//!
//! Besides a page's own content, a PDF shows text in its annotations: a text
//! box typed onto the page (FreeText), as a reviewer adds "Adjusted basis
//! 12,500.00 per preparer", a line's caption, or a stamp or watermark drawn
//! in text, "RECEIVED APR 15 2025". pdf-inspector reads a page's content,
//! its links, and its form values, and no other annotation, so such text is
//! not in the Markdown. The check finds the annotations a reader shows on
//! their page with text of their own: text boxes, captioned lines, and
//! stamps and watermarks whose appearance draws text. It gives the text
//! each holds, the plain text of its rich text, from which a viewer draws
//! it, or its `/Contents`, or, for a stamp or watermark holding neither,
//! the text its appearance draws, with its page; the Markdown decides.
//! Notes shown only in a popup, markup that comments on the page's own
//! text, and annotations hidden or set off their page are not what the
//! page shows, and are not read.
//!
//! An appearance is read within bounds: each stream once, however many
//! annotations share it, to `MAX_APPEARANCE_BYTES` decoded, and a
//! document's streams to `MAX_APPEARANCE_TOTAL` in all, looking through the
//! forms it draws to `MAX_FORM_DEPTH` deep, for an operator that shows a
//! string in a render mode that paints: text in mode 3, which paints
//! nothing, or 7, which only clips, is not drawn. Past the bounds, a stamp
//! is taken to draw no text; a stream whose verdict the bounds cut short is
//! read again where it is reached another way, as a form nearer a stamp
//! than it was to the first. The text a stamp's appearance draws is read
//! within what the bounds leave, to the same depth.

use std::collections::{HashMap, HashSet};

use lopdf::{Dictionary, Document, Object, ObjectId, Stream};

/// Annotations of the kinds that can show text of their own read, at most,
/// across a document; those of other kinds, links above all, are passed
/// over uncounted.
const MAX_ANNOTATIONS: usize = 10_000;
/// Bytes an appearance stream may decode to for its text to be looked for.
const MAX_APPEARANCE_BYTES: usize = 1 << 20;
/// Bytes a document's appearance streams may decode to, in all.
const MAX_APPEARANCE_TOTAL: usize = 16 << 20;
/// Forms an appearance is read through, one drawn inside another.
const MAX_FORM_DEPTH: usize = 8;
/// Forms a stream draws that are looked through, at most.
const MAX_FORMS_DRAWN: usize = 64;
/// Levels of the page tree looked up for a page's inherited box.
const MAX_TREE_DEPTH: usize = 64;
/// Annotation flags that keep it from view: hidden, and not viewed.
const HIDDEN: i64 = 2;
const NO_VIEW: i64 = 32;

/// Text an annotation shows, and its page.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct AnnotationText {
    pub(crate) page: u32,
    pub(crate) text: String,
}

fn resolve<'a>(document: &'a Document, object: &'a Object) -> Option<&'a Object> {
    match object {
        Object::Reference(id) => document.get_object(*id).ok(),
        object => Some(object),
    }
}

fn dictionary<'a>(document: &'a Document, object: &'a Object) -> Option<&'a Dictionary> {
    resolve(document, object).and_then(|object| object.as_dict().ok())
}

/// A text string as the PDF means it.
fn text(document: &Document, object: &Object) -> Option<String> {
    let object = resolve(document, object)?;
    lopdf::decode_text_string(object)
        .ok()
        .map(|text| text.trim_start_matches('\u{FEFF}').to_string())
}

/// Text with its XML character references, and the entities XHTML text
/// uses most, read.
fn entities(text: &str) -> String {
    let mut read = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        read.push_str(&rest[..at]);
        rest = &rest[at..];
        // An entity is short: its name ends within a few bytes.
        let end = rest.bytes().take(12).position(|byte| byte == b';');
        let character = end.and_then(|end| match &rest[1..end] {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some('\u{A0}'),
            name => name
                .strip_prefix('#')
                .and_then(|number| match number.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok(),
                    None => number.parse().ok(),
                })
                .and_then(char::from_u32),
        });
        match (character, end) {
            (Some(character), Some(end)) => {
                read.push(character);
                rest = &rest[end + 1..];
            }
            _ => {
                read.push('&');
                rest = &rest[1..];
            }
        }
    }
    read.push_str(rest);
    read
}

/// The plain text of rich text: its XHTML with the tags left out.
pub(crate) fn plain(rich: &str) -> String {
    let mut plain = String::with_capacity(rich.len());
    let mut in_tag = false;
    for character in rich.chars() {
        match character {
            '<' => in_tag = true,
            '>' if in_tag => {
                in_tag = false;
                plain.push(' ');
            }
            _ if !in_tag => plain.push(character),
            _ => {}
        }
    }
    entities(&plain)
}

/// Whether a byte ends a token of a content stream.
fn delimits(byte: u8) -> bool {
    matches!(
        byte,
        b'\0'
            | b'\t'
            | b'\n'
            | b'\x0C'
            | b'\r'
            | b' '
            | b'('
            | b')'
            | b'<'
            | b'>'
            | b'['
            | b']'
            | b'{'
            | b'}'
            | b'/'
            | b'%'
    )
}

/// A name of a content stream, its `#` escapes read.
fn name(token: &[u8]) -> Vec<u8> {
    let mut name = Vec::with_capacity(token.len());
    let mut at = 0;
    while at < token.len() {
        let hex = token
            .get(at + 1..at + 3)
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match (token[at], hex) {
            (b'#', Some(byte)) => {
                name.push(byte);
                at += 3;
            }
            (byte, _) => {
                name.push(byte);
                at += 1;
            }
        }
    }
    name
}

/// The operators of a content stream.
const OPERATORS: [&[u8]; 73] = [
    b"b", b"B", b"b*", b"B*", b"BDC", b"BI", b"BMC", b"BT", b"BX", b"c", b"cm", b"CS", b"cs", b"d",
    b"d0", b"d1", b"Do", b"DP", b"EI", b"EMC", b"ET", b"EX", b"f", b"F", b"f*", b"G", b"g", b"gs",
    b"h", b"i", b"ID", b"j", b"J", b"K", b"k", b"l", b"m", b"M", b"MP", b"n", b"q", b"Q", b"re",
    b"RG", b"rg", b"ri", b"s", b"S", b"SC", b"sc", b"SCN", b"scn", b"sh", b"T*", b"Tc", b"Td",
    b"TD", b"Tf", b"Tj", b"TJ", b"TL", b"Tm", b"Tr", b"Ts", b"Tw", b"Tz", b"v", b"w", b"W", b"W*",
    b"y", b"'", b"\"",
];
/// Bytes after the white space ending an `EI` that must be text for
/// content to follow it, as pdf.js reads them; and bytes looked through
/// for the operator content begins with.
const AFTER_IMAGE_TEXT: usize = 10;
const AFTER_IMAGE: usize = 64;

/// Whether content, not an inline image's data, follows an `EI` and the
/// white space after it, `rest` being what follows the `EI`, as pdf.js
/// tells them apart: the end of the stream, or bytes that are text, but for
/// a NUL standing alone, for `AFTER_IMAGE_TEXT` bytes, and in which an
/// operator comes before any other word, the operands before it passed
/// over.
fn content_follows(rest: &[u8]) -> bool {
    let text = rest.get(1..).unwrap_or_default();
    let text = &text[..text.len().min(AFTER_IMAGE_TEXT)];
    let binary = text.iter().enumerate().any(|(index, &byte)| {
        let lone_nul = byte == 0 && text.get(index + 1) != Some(&0);
        !(byte.is_ascii_graphic() || byte.is_ascii_whitespace() || lone_nul)
    });
    if binary {
        return false;
    }
    let window = &rest[..rest.len().min(AFTER_IMAGE)];
    let mut at = 0;
    while at < window.len() {
        match window[at] {
            b'(' => {
                let mut depth = 0usize;
                while at < window.len() {
                    match window[at] {
                        b'\\' => at += 1,
                        b'(' => depth += 1,
                        b')' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    at += 1;
                }
                at += 1;
            }
            b'<' if window.get(at + 1) != Some(&b'<') => {
                while at < window.len() && window[at] != b'>' {
                    at += 1;
                }
                at += 1;
            }
            b'/' => {
                at += 1;
                while at < window.len() && !delimits(window[at]) {
                    at += 1;
                }
            }
            b'%' => return true,
            byte if delimits(byte) => at += 1,
            _ => {
                let start = at;
                while at < window.len() && !delimits(window[at]) {
                    at += 1;
                }
                let word = &window[start..at];
                if !matches!(word[0], b'0'..=b'9' | b'+' | b'-' | b'.') {
                    return OPERATORS.contains(&word) && (at < window.len() || at == rest.len());
                }
            }
        }
    }
    // Nothing but operands, or nothing at all, to the end of the stream.
    window.len() == rest.len()
}

/// Where an inline image's data ends, from `start`, past the white space
/// after `ID`, where its dictionary gives no length: at the first `EI` set
/// apart by white space that content follows (see `content_follows`), or
/// the end of the stream.
fn image_end(content: &[u8], start: usize) -> usize {
    let mut at = start.max(1);
    while at + 2 <= content.len() {
        if content[at - 1].is_ascii_whitespace()
            && content[at..].starts_with(b"EI")
            && content
                .get(at + 2)
                .is_none_or(|next| next.is_ascii_whitespace())
            && content_follows(&content[at + 2..])
        {
            return at + 2;
        }
        at += 1;
    }
    content.len()
}

/// Whether `content` shows a string with `Tj`, `TJ`, `'`, or `"` in a
/// render mode that paints, starting `invisible` or not as the mode it is
/// drawn in says; the names of the XObjects it draws with `Do` go in
/// `drawn`, up to `MAX_FORMS_DRAWN`, each with whether the mode it is drawn
/// in paints nothing. The content is read token by token, its strings,
/// comments, and inline images passed over whole: an image's data for the
/// length its dictionary gives, else to the `EI` that ends it (see
/// `image_end`).
fn shows_text(content: &[u8], invisible: bool, drawn: &mut Vec<(Vec<u8>, bool)>) -> bool {
    let mut at = 0;
    // The last name and number, and whether a string with text in it came,
    // since the last operator; the length the dictionary of an inline image
    // being read gives its data; and whether the render mode, which `q`
    // saves and `Q` restores, paints nothing.
    let mut operand: Option<&[u8]> = None;
    let mut number: Option<f64> = None;
    let mut string = false;
    let mut length: Option<usize> = None;
    let mut invisible = invisible;
    let mut saved: Vec<bool> = Vec::new();
    while at < content.len() {
        match content[at] {
            b'%' => {
                while at < content.len() && !matches!(content[at], b'\n' | b'\r') {
                    at += 1;
                }
            }
            b'(' => {
                let (start, mut depth) = (at, 0usize);
                while at < content.len() {
                    match content[at] {
                        b'\\' => at += 1,
                        b'(' => depth += 1,
                        b')' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    at += 1;
                }
                string |= at > start + 1;
                at += 1;
            }
            b'<' if content.get(at + 1) == Some(&b'<') => at += 2,
            b'<' => {
                let start = at;
                while at < content.len() && content[at] != b'>' {
                    at += 1;
                }
                string |= content[start..at].iter().any(u8::is_ascii_hexdigit);
                at += 1;
            }
            b'/' => {
                let start = at + 1;
                at = start;
                while at < content.len() && !delimits(content[at]) {
                    at += 1;
                }
                operand = Some(&content[start..at]);
            }
            byte if delimits(byte) => at += 1,
            _ => {
                let start = at;
                while at < content.len() && !delimits(content[at]) {
                    at += 1;
                }
                let token = &content[start..at];
                if matches!(token[0], b'0'..=b'9' | b'+' | b'-' | b'.') {
                    let value = std::str::from_utf8(token).ok();
                    if matches!(operand, Some(b"L" | b"Length")) {
                        length = value.and_then(|length| length.parse().ok());
                    }
                    number = value.and_then(|number| number.parse().ok());
                    continue;
                }
                match token {
                    b"Tj" | b"TJ" | b"'" | b"\"" if string && !invisible => return true,
                    // A viewer takes a mode it knows, cut to a whole number.
                    b"Tr" => {
                        if let Some(mode) = number
                            .map(|mode| mode as i64)
                            .filter(|mode| (0..=7).contains(mode))
                        {
                            invisible = matches!(mode, 3 | 7);
                        }
                    }
                    b"q" => saved.push(invisible),
                    b"Q" => invisible = saved.pop().unwrap_or(invisible),
                    b"Do" => {
                        if let Some(operand) = operand.filter(|_| drawn.len() < MAX_FORMS_DRAWN) {
                            drawn.push((name(operand), invisible));
                        }
                    }
                    b"BI" => length = None,
                    // An inline image's data runs from past the white space
                    // after `ID`.
                    b"ID" => {
                        let start = at + 1;
                        at = match length.take() {
                            Some(length) => start.saturating_add(length).min(content.len()),
                            None => image_end(content, start),
                        };
                    }
                    _ => {}
                }
                operand = None;
                number = None;
                string = false;
            }
        }
    }
    false
}

/// The appearance streams of a document, each read at most once in each
/// render mode it may be drawn in, within a budget for the whole document,
/// but where its verdict was cut short.
struct Appearances<'a> {
    document: &'a Document,
    /// Whether each stream read draws text, drawn in a mode that paints
    /// nothing or not, and `None` for one being read.
    drawn: HashMap<(ObjectId, bool), Option<bool>>,
    /// Decoded bytes left to read.
    budget: usize,
    /// How each font of the text read reads its codes.
    glyphs: crate::glyph_words::GlyphFonts,
}

/// Whether a stream draws text, and whether that was read whole: not cut
/// short by the depth read, the document's budget, or a form met again
/// while it is read.
#[derive(Clone, Copy)]
struct Verdict {
    drawn: bool,
    whole: bool,
}

impl Verdict {
    const DRAWN: Verdict = Verdict {
        drawn: true,
        whole: true,
    };
    const NONE: Verdict = Verdict {
        drawn: false,
        whole: true,
    };
    const CUT_SHORT: Verdict = Verdict {
        drawn: false,
        whole: false,
    };
}

impl<'a> Appearances<'a> {
    fn new(document: &'a Document) -> Self {
        Appearances {
            document,
            drawn: HashMap::new(),
            budget: MAX_APPEARANCE_TOTAL,
            glyphs: crate::glyph_words::GlyphFonts::default(),
        }
    }

    /// An annotation's normal appearance: a stream, or one of the streams
    /// it gives by appearance state, as the annotation's state names.
    fn normal(&self, annotation: &'a Dictionary) -> Option<&'a Object> {
        let document = self.document;
        let normal = annotation
            .get(b"AP")
            .ok()
            .and_then(|appearance| dictionary(document, appearance))
            .and_then(|appearance| appearance.get(b"N").ok())?;
        match resolve(document, normal) {
            Some(Object::Dictionary(states)) => annotation
                .get(b"AS")
                .ok()
                .and_then(|state| state.as_name().ok())
                .and_then(|state| states.get(state).ok()),
            _ => Some(normal),
        }
    }

    /// Whether an annotation's normal appearance draws text.
    fn draws_text(&mut self, annotation: &'a Dictionary) -> bool {
        self.normal(annotation)
            .is_some_and(|stream| self.object(stream, 0, false).drawn)
    }

    /// The text an annotation's normal appearance draws, as a viewer shows
    /// it: each string shown in a render mode that paints, as its font
    /// reads it, itself or through the forms it draws, a space between
    /// text objects; `None` where it draws none with a letter or digit.
    fn text(&mut self, annotation: &'a Dictionary) -> Option<String> {
        let stream = self.normal(annotation)?;
        let mut texts = Vec::new();
        self.read_text(stream, 0, false, &mut Vec::new(), &mut texts);
        let text = texts.join(" ");
        let text = text.trim();
        text.chars()
            .any(char::is_alphanumeric)
            .then(|| text.to_string())
    }

    /// Read into `texts` what a stream, given as it is referred to and
    /// drawn `invisible` or not (see `shows_text`), draws: each text
    /// object's strings, then those of each form it draws, to
    /// `MAX_FORM_DEPTH` deep, but for a form among `reading`, the forms
    /// being read, which draws itself.
    fn read_text(
        &mut self,
        object: &'a Object,
        depth: usize,
        invisible: bool,
        reading: &mut Vec<ObjectId>,
        texts: &mut Vec<String>,
    ) {
        let document = self.document;
        let (id, stream) = match object {
            Object::Reference(id) => match document.get_object(*id) {
                Ok(Object::Stream(stream)) if !reading.contains(id) => (Some(*id), stream),
                _ => return,
            },
            Object::Stream(stream) => (None, stream),
            _ => return,
        };
        let limit = self.budget.min(MAX_APPEARANCE_BYTES);
        let content = stream.get_plain_content_with_limit(limit);
        self.budget -= match &content {
            Ok(content) => content.len().min(limit),
            Err(_) => stream.content.len().min(limit),
        };
        let Some(operations) = content
            .ok()
            .and_then(|content| lopdf::content::Content::decode(&content).ok())
            .map(|content| content.operations)
        else {
            return;
        };
        let resources = |kind: &[u8]| {
            stream
                .dict
                .get(b"Resources")
                .ok()
                .and_then(|resources| dictionary(document, resources))
                .and_then(|resources| resources.get(kind).ok())
                .and_then(|named| dictionary(document, named))
        };
        let (fonts, xobjects) = (resources(b"Font"), resources(b"XObject"));
        // The font and whether the render mode paints nothing, which `q`
        // saves and `Q` restores, and the text of the text object open.
        let (mut font, mut invisible) = (None, invisible);
        let mut saved = Vec::new();
        let mut text = String::new();
        for operation in &operations {
            let operands = &operation.operands;
            match operation.operator.as_str() {
                "q" => saved.push((font, invisible)),
                "Q" => (font, invisible) = saved.pop().unwrap_or((font, invisible)),
                "Tf" => {
                    font = operands
                        .first()
                        .and_then(|name| name.as_name().ok())
                        .and_then(|name| fonts?.get(name).ok())
                        .and_then(|font| dictionary(document, font))
                        .and_then(|font| self.glyphs.font(document, font));
                }
                // A viewer takes a mode it knows, cut to a whole number.
                "Tr" => {
                    if let Some(mode) = operands
                        .last()
                        .and_then(|mode| mode.as_float().ok())
                        .map(|mode| mode as i64)
                        .filter(|mode| (0..=7).contains(mode))
                    {
                        invisible = matches!(mode, 3 | 7);
                    }
                }
                "Tj" | "'" | "\"" | "TJ" => {
                    let shown = if operation.operator == "TJ" {
                        operands.first()
                    } else {
                        operands.last()
                    };
                    let strings: Vec<&[u8]> = match shown {
                        Some(Object::Array(parts)) => {
                            parts.iter().filter_map(|part| part.as_str().ok()).collect()
                        }
                        Some(part) => part.as_str().ok().into_iter().collect(),
                        None => Vec::new(),
                    };
                    for bytes in strings {
                        let read = font
                            .filter(|_| !invisible)
                            .and_then(|font| self.glyphs.text_or(font, bytes, ' '));
                        text.extend(read);
                    }
                }
                "ET" if !text.trim().is_empty() => texts.push(std::mem::take(&mut text)),
                "Do" if depth < MAX_FORM_DEPTH => {
                    let Some(form) = operands
                        .first()
                        .and_then(|name| name.as_name().ok())
                        .and_then(|name| xobjects?.get(name).ok())
                        .filter(|form| {
                            resolve(document, form)
                                .and_then(|form| form.as_stream().ok())
                                .and_then(|form| form.dict.get(b"Subtype").ok())
                                .and_then(|subtype| subtype.as_name().ok())
                                == Some(b"Form")
                        })
                    else {
                        continue;
                    };
                    reading.extend(id);
                    self.read_text(form, depth + 1, invisible, reading, texts);
                    if id.is_some() {
                        reading.pop();
                    }
                }
                _ => {}
            }
        }
        if !text.trim().is_empty() {
            texts.push(text);
        }
    }

    /// Whether a stream, given as it is referred to and drawn `invisible`
    /// or not (see `shows_text`), draws text. A verdict read whole, or that
    /// it draws text, is kept; any other depends on the way the stream was
    /// reached, and is not.
    fn object(&mut self, object: &Object, depth: usize, invisible: bool) -> Verdict {
        match object {
            Object::Reference(id) => {
                let key = (*id, invisible);
                match self.drawn.get(&key) {
                    Some(Some(drawn)) => {
                        return Verdict {
                            drawn: *drawn,
                            whole: true,
                        }
                    }
                    // A form drawing itself, which ends the read; what the
                    // forms it passes through draw depends on it.
                    Some(None) => return Verdict::CUT_SHORT,
                    None => {}
                }
                self.drawn.insert(key, None);
                let document = self.document;
                let verdict = match document.get_object(*id) {
                    Ok(Object::Stream(stream)) => self.read(stream, depth, invisible),
                    _ => Verdict::NONE,
                };
                if verdict.drawn || verdict.whole {
                    self.drawn.insert(key, Some(verdict.drawn));
                } else {
                    self.drawn.remove(&key);
                }
                verdict
            }
            Object::Stream(stream) => self.read(stream, depth, invisible),
            _ => Verdict::NONE,
        }
    }

    /// Whether a stream draws text, itself or through the forms it draws,
    /// drawn `invisible` or not.
    fn read(&mut self, stream: &Stream, depth: usize, invisible: bool) -> Verdict {
        let limit = self.budget.min(MAX_APPEARANCE_BYTES);
        if limit == 0 {
            return Verdict::CUT_SHORT;
        }
        let content = match stream.get_plain_content_with_limit(limit) {
            Ok(content) => content,
            Err(error) => {
                // A stream that would decode past the limit spends it; one
                // that does not decode, its own bytes. One past its own
                // bound draws nothing however it is reached; past what is
                // left of the document's, it may yet.
                let (spent, whole) = match error {
                    lopdf::Error::Decompress(lopdf::DecompressError::MemoryLimitExceeded {
                        ..
                    }) => (limit, limit == MAX_APPEARANCE_BYTES),
                    _ => (stream.content.len().min(limit), true),
                };
                self.budget -= spent;
                return Verdict {
                    drawn: false,
                    whole,
                };
            }
        };
        self.budget -= content.len().min(limit);
        let mut drawn = Vec::new();
        if shows_text(&content, invisible, &mut drawn) {
            return Verdict::DRAWN;
        }
        if drawn.is_empty() {
            return Verdict::NONE;
        }
        if depth >= MAX_FORM_DEPTH {
            return Verdict::CUT_SHORT;
        }
        let document = self.document;
        let Some(xobjects) = stream
            .dict
            .get(b"Resources")
            .ok()
            .and_then(|resources| dictionary(document, resources))
            .and_then(|resources| resources.get(b"XObject").ok())
            .and_then(|xobjects| dictionary(document, xobjects))
        else {
            return Verdict::NONE;
        };
        let mut looked: HashSet<(Vec<u8>, bool)> = HashSet::new();
        let mut whole = true;
        for (name, invisible) in drawn {
            let form = xobjects.get(&name).ok().filter(|form| {
                resolve(document, form)
                    .and_then(|form| form.as_stream().ok())
                    .and_then(|form| form.dict.get(b"Subtype").ok())
                    .and_then(|subtype| subtype.as_name().ok())
                    == Some(b"Form")
            });
            let Some(form) = form.filter(|_| looked.insert((name, invisible))) else {
                continue;
            };
            let verdict = self.object(form, depth + 1, invisible);
            if verdict.drawn {
                return Verdict::DRAWN;
            }
            whole &= verdict.whole;
        }
        Verdict {
            drawn: false,
            whole,
        }
    }
}

/// A rectangle given as four numbers, its corners in order.
fn rectangle(document: &Document, object: &Object) -> Option<[f32; 4]> {
    let numbers: Vec<f32> = resolve(document, object)?
        .as_array()
        .ok()?
        .iter()
        .map(|number| resolve(document, number).and_then(|number| number.as_float().ok()))
        .collect::<Option<_>>()?;
    let [x1, y1, x2, y2] = numbers[..] else {
        return None;
    };
    Some([x1.min(x2), y1.min(y2), x1.max(x2), y1.max(y2)])
}

/// The box a page shows: its crop box within its media box, its own or
/// inherited.
fn page_box(document: &Document, page: ObjectId) -> Option<[f32; 4]> {
    let (mut crop, mut media) = (None, None);
    let mut node = document.get_dictionary(page).ok();
    for _ in 0..MAX_TREE_DEPTH {
        let Some(dictionary) = node else {
            break;
        };
        crop = crop.or_else(|| rectangle(document, dictionary.get(b"CropBox").ok()?));
        media = media.or_else(|| rectangle(document, dictionary.get(b"MediaBox").ok()?));
        node = dictionary
            .get(b"Parent")
            .ok()
            .and_then(|parent| parent.as_reference().ok())
            .and_then(|parent| document.get_dictionary(parent).ok());
    }
    match (crop, media) {
        (Some(crop), Some(media)) => Some([
            crop[0].max(media[0]),
            crop[1].max(media[1]),
            crop[2].min(media[2]),
            crop[3].min(media[3]),
        ]),
        (crop, media) => crop.or(media),
    }
}

/// The text the annotations of `document` show on its pages, among the
/// pages `only` names, if any.
pub(crate) fn unread(
    document: &Document,
    only: Option<&std::collections::HashSet<u32>>,
    layers: Option<&crate::optional_content::Layers>,
) -> Vec<AnnotationText> {
    let mut texts = Vec::new();
    let mut appearances = Appearances::new(document);
    let mut read = 0;
    for (number, page) in document.get_pages() {
        if only.is_some_and(|only| !only.contains(&number)) {
            continue;
        }
        let Some(annotations) = document
            .get_dictionary(page)
            .ok()
            .and_then(|page| page.get(b"Annots").ok())
            .and_then(|annotations| resolve(document, annotations))
            .and_then(|annotations| annotations.as_array().ok())
        else {
            continue;
        };
        let mut shown_box: Option<Option<[f32; 4]>> = None;
        for annotation in annotations {
            let Some(annotation) = dictionary(document, annotation) else {
                continue;
            };
            let subtype = annotation
                .get(b"Subtype")
                .ok()
                .and_then(|subtype| subtype.as_name().ok())
                .unwrap_or_default();
            if !matches!(subtype, b"FreeText" | b"Line" | b"Stamp" | b"Watermark") {
                continue;
            }
            read += 1;
            if read > MAX_ANNOTATIONS {
                return texts;
            }
            let flags = annotation
                .get(b"F")
                .ok()
                .and_then(|flags| resolve(document, flags))
                .and_then(|flags| flags.as_i64().ok())
                .unwrap_or(0);
            // Hidden by its flags, or by a layer a reader hides.
            if flags & (HIDDEN | NO_VIEW) != 0
                || layers.is_some_and(|layers| layers.hide(document, annotation))
            {
                continue;
            }
            let captioned = || {
                annotation
                    .get(b"Cap")
                    .ok()
                    .and_then(|caption| caption.as_bool().ok())
                    .unwrap_or(false)
            };
            let shown = match subtype {
                b"FreeText" => true,
                b"Line" => captioned(),
                b"Stamp" | b"Watermark" => appearances.draws_text(annotation),
                _ => false,
            };
            if !shown {
                continue;
            }
            // An annotation set wholly off its page is not shown.
            let on_page = shown_box
                .get_or_insert_with(|| page_box(document, page))
                .zip(
                    annotation
                        .get(b"Rect")
                        .ok()
                        .and_then(|rect| rectangle(document, rect)),
                )
                .is_none_or(|(page, rect)| {
                    rect[0] < page[2] && rect[2] > page[0] && rect[1] < page[3] && rect[3] > page[1]
                });
            if !on_page {
                continue;
            }
            let contents = annotation
                .get(b"RC")
                .ok()
                .and_then(|rich| text(document, rich))
                .map(|rich| plain(&rich))
                .filter(|rich| !rich.trim().is_empty())
                .or_else(|| {
                    annotation
                        .get(b"Contents")
                        .ok()
                        .and_then(|contents| text(document, contents))
                        .filter(|contents| !contents.trim().is_empty())
                });
            // A stamp or watermark with no text of its own shows the text
            // its appearance draws.
            let contents = contents.or_else(|| {
                matches!(subtype, b"Stamp" | b"Watermark")
                    .then(|| appearances.text(annotation))
                    .flatten()
            });
            if let Some(contents) = contents {
                texts.push(AnnotationText {
                    page: number,
                    text: contents,
                });
            }
        }
    }
    texts
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{dictionary, Stream};

    /// A one-page document whose page holds the annotations `build` makes.
    fn document(build: impl FnOnce(&mut Document) -> Vec<Dictionary>) -> Document {
        let mut document = Document::with_version("1.7");
        let pages_id = document.new_object_id();
        let annotations: Vec<Object> = build(&mut document)
            .into_iter()
            .map(|annotation| document.add_object(annotation).into())
            .collect();
        let page = document.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Annots" => annotations,
        });
        document.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1,
            }),
        );
        let catalog = document.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        document.trailer.set("Root", catalog);
        document
    }

    #[test]
    fn text_boxes_and_stamps_drawn_in_text_are_found() {
        let found = document(|document| {
            let drawn = document.add_object(Stream::new(
                dictionary! { "Subtype" => "Form" },
                b"BT /F1 12 Tf (RECEIVED APR 15 2025) Tj ET".to_vec(),
            ));
            let picture = document.add_object(Stream::new(
                dictionary! { "Subtype" => "Form" },
                b"q 100 0 0 40 0 0 cm /Im1 Do Q".to_vec(),
            ));
            vec![
                dictionary! {
                    "Subtype" => "FreeText",
                    "Contents" => Object::string_literal("Adjusted basis 12,500.00 per preparer"),
                },
                dictionary! {
                    "Subtype" => "FreeText",
                    "RC" => Object::string_literal("<body><p>Basis <b>12,500.00</b></p></body>"),
                },
                dictionary! {
                    "Subtype" => "Stamp",
                    "Contents" => Object::string_literal("RECEIVED APR 15 2025"),
                    "AP" => dictionary! { "N" => drawn },
                },
                // A stamp drawn as a picture, a hidden box, and a note shown
                // only in its popup show no text of their own on the page.
                dictionary! {
                    "Subtype" => "Stamp",
                    "Contents" => Object::string_literal("Approved"),
                    "AP" => dictionary! { "N" => picture },
                },
                dictionary! {
                    "Subtype" => "FreeText", "F" => 2,
                    "Contents" => Object::string_literal("hidden"),
                },
                dictionary! {
                    "Subtype" => "Text",
                    "Contents" => Object::string_literal("Client confirmed by phone"),
                },
            ]
        });
        let texts: Vec<String> = unread(&found, None, None)
            .into_iter()
            .map(|annotation| annotation.text)
            .collect();
        assert_eq!(
            texts,
            [
                "Adjusted basis 12,500.00 per preparer",
                "  Basis  12,500.00   ",
                "RECEIVED APR 15 2025"
            ]
        );
        assert!(unread(&found, Some(&[2].into_iter().collect()), None).is_empty());
    }

    #[test]
    fn stamps_drawing_text_through_forms_captions_and_rich_text_are_found() {
        let found = document(|document| {
            // Acrobat draws a stamp's text in a form its appearance draws.
            let inner = document.add_object(Stream::new(
                dictionary! { "Subtype" => "Form" },
                b"BT /F1 12 Tf (PAID) Tj ET".to_vec(),
            ));
            let outer = document.add_object(Stream::new(
                dictionary! {
                    "Subtype" => "Form",
                    "Resources" => dictionary! { "XObject" => dictionary! { "FRM 1" => inner } },
                },
                b"q /FRM#201 Do Q".to_vec(),
            ));
            // A form that draws itself shows nothing, and the read ends.
            let looping = document.new_object_id();
            document.objects.insert(
                looping,
                Object::Stream(Stream::new(
                    dictionary! {
                        "Subtype" => "Form",
                        "Resources" => dictionary! { "XObject" => dictionary! { "Me" => looping } },
                    },
                    b"/Me Do".to_vec(),
                )),
            );
            vec![
                dictionary! {
                    "Subtype" => "Stamp",
                    "Contents" => Object::string_literal("PAID 2025-04-15"),
                    "AP" => dictionary! { "N" => outer },
                },
                dictionary! {
                    "Subtype" => "Stamp",
                    "Contents" => Object::string_literal("Looping"),
                    "AP" => dictionary! { "N" => looping },
                },
                // A viewer draws a text box from its rich text.
                dictionary! {
                    "Subtype" => "FreeText",
                    "Contents" => Object::string_literal("Client's basis"),
                    "RC" => Object::string_literal("<p>Client&#8217;s basis&#xA0;12,500.00 &amp; costs</p>"),
                },
                dictionary! {
                    "Subtype" => "Line", "Cap" => true,
                    "Contents" => Object::string_literal("Setback 25 ft"),
                },
                dictionary! {
                    "Subtype" => "Line",
                    "Contents" => Object::string_literal("Uncaptioned"),
                },
                // Set off the page, a text box is not shown.
                dictionary! {
                    "Subtype" => "FreeText",
                    "Rect" => vec![700.into(), 100.into(), 800.into(), 140.into()],
                    "Contents" => Object::string_literal("Off the page"),
                },
                dictionary! {
                    "Subtype" => "FreeText",
                    "Rect" => vec![500.into(), 100.into(), 700.into(), 140.into()],
                    "Contents" => Object::string_literal("Over the edge"),
                },
            ]
        });
        let texts: Vec<String> = unread(&found, None, None)
            .into_iter()
            .map(|annotation| annotation.text)
            .collect();
        assert_eq!(
            texts,
            [
                "PAID 2025-04-15",
                " Client\u{2019}s basis\u{A0}12,500.00 & costs ",
                "Setback 25 ft",
                "Over the edge"
            ]
        );
    }

    #[test]
    fn stamps_with_no_text_of_their_own_are_found_as_their_appearance_draws_it() {
        let found = document(|document| {
            let font = document.add_object(dictionary! {
                "Type" => "Font", "Subtype" => "Type1",
                "BaseFont" => "Helvetica", "Encoding" => "WinAnsiEncoding",
            });
            let form = |document: &mut Document, content: &[u8], resources: Dictionary| {
                document.add_object(Stream::new(
                    dictionary! { "Subtype" => "Form", "Resources" => resources },
                    content.to_vec(),
                ))
            };
            let fonts = || dictionary! { "Font" => dictionary! { "Helv" => font } };
            let received = form(
                document,
                b"2 w 2 2 216 36 re S BT /Helv 16 Tf 10 14 Td [(RECEIVED APR ) -20 (15 2025)] TJ ET",
                fonts(),
            );
            // Acrobat draws a stamp's text in a form its appearance draws;
            // text in a mode that paints nothing is not drawn.
            let inner = form(
                document,
                b"BT /Helv 12 Tf (PAID) Tj ET BT 3 Tr /Helv 12 Tf (DRAFT) Tj ET",
                fonts(),
            );
            let outer = form(
                document,
                b"q /FRM Do Q",
                dictionary! { "XObject" => dictionary! { "FRM" => inner } },
            );
            // Text in a font the appearance does not give reads as nothing.
            let unread = form(document, b"BT /F9 12 Tf (VOID) Tj ET", Dictionary::new());
            let stamp = |subtype: &str, appearance: ObjectId| {
                dictionary! {
                    "Subtype" => Object::Name(subtype.as_bytes().to_vec()),
                    "AP" => dictionary! { "N" => appearance },
                }
            };
            vec![
                stamp("Stamp", received),
                stamp("Watermark", outer),
                stamp("Stamp", unread),
                // A text box shows only the text it holds.
                stamp("FreeText", received),
            ]
        });
        let texts: Vec<String> = unread(&found, None, None)
            .into_iter()
            .map(|annotation| annotation.text)
            .collect();
        assert_eq!(texts, ["RECEIVED APR 15 2025", "PAID"]);
    }

    #[test]
    fn verdicts_cut_short_by_the_depth_read_are_not_kept() {
        let found = document(|document| {
            let text = document.add_object(Stream::new(
                dictionary! { "Subtype" => "Form" },
                b"BT /F1 12 Tf (VOID) Tj ET".to_vec(),
            ));
            // Forms each drawing the one below, one more than are read.
            let mut chain = vec![text];
            for _ in 0..=MAX_FORM_DEPTH {
                let below = *chain.last().expect("a form");
                chain.push(document.add_object(Stream::new(
                    dictionary! {
                        "Subtype" => "Form",
                        "Resources" => dictionary! { "XObject" => dictionary! { "Fx" => below } },
                    },
                    b"/Fx Do".to_vec(),
                )));
            }
            let stamp = |contents: &str, appearance: ObjectId| {
                dictionary! {
                    "Subtype" => "Stamp", "Contents" => Object::string_literal(contents),
                    "AP" => dictionary! { "N" => appearance },
                }
            };
            // A stamp drawing its text past the depth read, taken to draw
            // none, and then one drawing it through one form, which does.
            vec![
                stamp("Void stamp deep", *chain.last().expect("a form")),
                stamp("Void stamp near", chain[1]),
            ]
        });
        let texts: Vec<String> = unread(&found, None, None)
            .into_iter()
            .map(|annotation| annotation.text)
            .collect();
        assert_eq!(texts, ["Void stamp near"]);
    }

    #[test]
    fn stamps_whose_text_paints_nothing_are_passed_over() {
        let found = document(|document| {
            let text = document.add_object(Stream::new(
                dictionary! { "Subtype" => "Form" },
                b"BT /F1 12 Tf (VOID) Tj ET".to_vec(),
            ));
            let drawing = |document: &mut Document, content: &[u8]| {
                document.add_object(Stream::new(
                    dictionary! {
                        "Subtype" => "Form",
                        "Resources" => dictionary! { "XObject" => dictionary! { "Fx" => text } },
                    },
                    content.to_vec(),
                ))
            };
            // The same form drawn in a mode that paints nothing, under a
            // box, and in one restored to paint.
            let unseen = drawing(document, b"0 0 200 50 re S 3 Tr /Fx Do");
            let seen = drawing(document, b"q 3 Tr Q /Fx Do");
            let stamp = |contents: &str, appearance: ObjectId| {
                dictionary! {
                    "Subtype" => "Stamp", "Contents" => Object::string_literal(contents),
                    "AP" => dictionary! { "N" => appearance },
                }
            };
            vec![
                stamp("Boxed stamp reviewed", unseen),
                stamp("Void stamp reviewed", seen),
            ]
        });
        let texts: Vec<String> = unread(&found, None, None)
            .into_iter()
            .map(|annotation| annotation.text)
            .collect();
        assert_eq!(texts, ["Void stamp reviewed"]);
    }

    #[test]
    fn links_do_not_count_against_the_annotations_read() {
        // A page of links, as a table of contents has, before a text box.
        let found = document(|_| {
            let mut annotations: Vec<Dictionary> = (0..MAX_ANNOTATIONS)
                .map(|_| dictionary! { "Subtype" => "Link" })
                .collect();
            annotations.push(dictionary! {
                "Subtype" => "FreeText",
                "Contents" => Object::string_literal("Adjusted basis 12,500.00 per preparer"),
            });
            annotations
        });
        let texts: Vec<String> = unread(&found, None, None)
            .into_iter()
            .map(|annotation| annotation.text)
            .collect();
        assert_eq!(texts, ["Adjusted basis 12,500.00 per preparer"]);
    }

    #[test]
    fn only_operators_that_show_a_string_count() {
        let shows = |content: &[u8]| shows_text(content, false, &mut Vec::new());
        assert!(shows(b"BT (x) Tj ET"));
        assert!(shows(b"BT [(A) -120 (B)] TJ ET"));
        assert!(shows(b"BT 1 2 (y) \" ET"));
        assert!(shows(b"BT <41> Tj ET"));
        // Operators named in strings, comments, and an inline image's data,
        // and strings with nothing in them, show nothing.
        assert!(!shows(b"BT () Tj <> Tj ET"));
        assert!(!shows(b"(Tj) pop % (x) Tj\n"));
        assert!(!shows(b"BI /W 1 /H 1 ID \x00(x) Tj\xFF EI Q"));
        // An `EI` in an image's data that data, not content, follows, and
        // data as long as the image's dictionary says, which looks like
        // content, do not end it; content after the image is read.
        assert!(!shows(
            b"BI /W 12 /H 1 /BPC 8 /CS /G ID \x00 EI x(ZZ) Tj\x00\x00\x00\n EI Q"
        ));
        assert!(!shows(b"BI /W 9 /H 1 /BPC 8 /CS /G /L 9 ID EI (x) Tj EI Q"));
        assert!(shows(b"BI /W 1 /H 1 /BPC 8 /CS /G ID \x00 EI BT (x) Tj ET"));
        assert!(shows(b"BI /W 1 /H 1 /L 1 ID \x00 EI BT (x) Tj ET"));
        // Binary bytes just after an `EI` are the image's data; a string's
        // bytes past ASCII further on are content's.
        assert!(!shows(b"BI /W 9 /H 1 ID \x00 EI Q\x00\x00(x) Tj EI Q"));
        assert!(shows(
            b"BI /W 1 /H 1 ID \x00 EI q BT /F1 12 Tf (Re\xe7u) Tj ET Q"
        ));
        assert!(!shows(b"(unclosed Tj"));
        // Text in a mode that paints nothing is not drawn; the mode goes on
        // across text objects, and `Q` restores the one `q` saved.
        assert!(!shows(
            b"0 0 200 50 re S BT 3 Tr /F1 12 Tf (hidden words) Tj ET"
        ));
        assert!(!shows(b"BT 7 Tr (clip) Tj ET BT (clip) Tj ET"));
        assert!(shows(b"BT 3 Tr (a) Tj 0 Tr (b) Tj ET"));
        assert!(shows(b"q 3 Tr Q BT (a) Tj ET"));
        // A mode no viewer knows leaves the mode as it was.
        assert!(!shows(b"BT 3 Tr 9 Tr (a) Tj ET"));
        assert!(shows_text(b"BT 0 Tr (a) Tj ET", true, &mut Vec::new()));
        let mut drawn = Vec::new();
        assert!(!shows_text(
            b"q /Im1 Do 3 Tr /Fm#231 Do Q",
            false,
            &mut drawn
        ));
        assert_eq!(drawn, [(b"Im1".to_vec(), false), (b"Fm#1".to_vec(), true)]);
        assert_eq!(
            entities("a &unknown; &#65; &#x42; & b"),
            "a &unknown; A B & b"
        );
    }
}
