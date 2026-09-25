//! Annotation text pdf-inspector 1.24.0 never reads.
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
//! it, or its `/Contents`, with its page; the Markdown decides. Notes shown
//! only in a popup, markup that comments on the page's own text, and
//! annotations hidden or set off their page are not what the page shows,
//! and are not read.
//!
//! An appearance is read within bounds: each stream once, however many
//! annotations share it, to `MAX_APPEARANCE_BYTES` decoded, and a
//! document's streams to `MAX_APPEARANCE_TOTAL` in all, looking through the
//! forms it draws to `MAX_FORM_DEPTH` deep, for an operator that shows a
//! string. Past the bounds, a stamp is taken to draw no text.

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

/// Whether `content` shows a string with `Tj`, `TJ`, `'`, or `"`; the names
/// of the XObjects it draws with `Do` go in `drawn`, up to
/// `MAX_FORMS_DRAWN`. The content is read token by token, its strings,
/// comments, and inline images passed over whole.
fn shows_text(content: &[u8], drawn: &mut Vec<Vec<u8>>) -> bool {
    let mut at = 0;
    // The last name, and whether a string with text in it came, since the
    // last operator.
    let mut operand: Option<&[u8]> = None;
    let mut string = false;
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
                    continue;
                }
                match token {
                    b"Tj" | b"TJ" | b"'" | b"\"" if string => return true,
                    b"Do" => {
                        if let Some(operand) = operand.filter(|_| drawn.len() < MAX_FORMS_DRAWN) {
                            drawn.push(name(operand));
                        }
                    }
                    // An inline image's data runs from after `ID` to an
                    // `EI` set apart by white space.
                    b"ID" => {
                        at += 1;
                        while at < content.len()
                            && !(content[at - 1].is_ascii_whitespace()
                                && content[at..].starts_with(b"EI")
                                && content.get(at + 2).is_none_or(|next| delimits(*next)))
                        {
                            at += 1;
                        }
                        at += 2;
                    }
                    _ => {}
                }
                operand = None;
                string = false;
            }
        }
    }
    false
}

/// The appearance streams of a document, each read at most once, within a
/// budget for the whole document.
struct Appearances<'a> {
    document: &'a Document,
    /// Whether each stream read draws text.
    drawn: HashMap<ObjectId, bool>,
    /// Decoded bytes left to read.
    budget: usize,
}

impl<'a> Appearances<'a> {
    fn new(document: &'a Document) -> Self {
        Appearances {
            document,
            drawn: HashMap::new(),
            budget: MAX_APPEARANCE_TOTAL,
        }
    }

    /// Whether an annotation's normal appearance draws text.
    fn draws_text(&mut self, annotation: &Dictionary) -> bool {
        let document = self.document;
        let Some(normal) = annotation
            .get(b"AP")
            .ok()
            .and_then(|appearance| dictionary(document, appearance))
            .and_then(|appearance| appearance.get(b"N").ok())
        else {
            return false;
        };
        // The normal appearance is a stream, or streams by appearance state.
        let stream = match resolve(document, normal) {
            Some(Object::Dictionary(states)) => annotation
                .get(b"AS")
                .ok()
                .and_then(|state| state.as_name().ok())
                .and_then(|state| states.get(state).ok()),
            _ => Some(normal),
        };
        stream.is_some_and(|stream| self.object(stream, 0))
    }

    /// Whether a stream, given as it is referred to, draws text.
    fn object(&mut self, object: &Object, depth: usize) -> bool {
        match object {
            Object::Reference(id) => {
                if let Some(&drawn) = self.drawn.get(id) {
                    return drawn;
                }
                // Taken to draw nothing while it is read, so that a form
                // drawing itself ends.
                self.drawn.insert(*id, false);
                let document = self.document;
                let drawn = match document.get_object(*id) {
                    Ok(Object::Stream(stream)) => self.read(stream, depth),
                    _ => false,
                };
                self.drawn.insert(*id, drawn);
                drawn
            }
            Object::Stream(stream) => self.read(stream, depth),
            _ => false,
        }
    }

    /// Whether a stream draws text, itself or through the forms it draws.
    fn read(&mut self, stream: &Stream, depth: usize) -> bool {
        let limit = self.budget.min(MAX_APPEARANCE_BYTES);
        if limit == 0 {
            return false;
        }
        let content = match stream.get_plain_content_with_limit(limit) {
            Ok(content) => content,
            Err(error) => {
                // A stream that would decode past the limit spends it; one
                // that does not decode, its own bytes.
                self.budget -= match error {
                    lopdf::Error::Decompress(lopdf::DecompressError::MemoryLimitExceeded {
                        ..
                    }) => limit,
                    _ => stream.content.len().min(limit),
                };
                return false;
            }
        };
        self.budget -= content.len().min(limit);
        let mut drawn = Vec::new();
        if shows_text(&content, &mut drawn) {
            return true;
        }
        if depth >= MAX_FORM_DEPTH || drawn.is_empty() {
            return false;
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
            return false;
        };
        let mut looked: HashSet<Vec<u8>> = HashSet::new();
        drawn.into_iter().any(|name| {
            let form = xobjects.get(&name).ok().filter(|form| {
                resolve(document, form)
                    .and_then(|form| form.as_stream().ok())
                    .and_then(|form| form.dict.get(b"Subtype").ok())
                    .and_then(|subtype| subtype.as_name().ok())
                    == Some(b"Form")
            });
            looked.insert(name) && form.is_some_and(|form| self.object(form, depth + 1))
        })
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
        let shows = |content: &[u8]| shows_text(content, &mut Vec::new());
        assert!(shows(b"BT (x) Tj ET"));
        assert!(shows(b"BT [(A) -120 (B)] TJ ET"));
        assert!(shows(b"BT 1 2 (y) \" ET"));
        assert!(shows(b"BT <41> Tj ET"));
        // Operators named in strings, comments, and an inline image's data,
        // and strings with nothing in them, show nothing.
        assert!(!shows(b"BT () Tj <> Tj ET"));
        assert!(!shows(b"(Tj) pop % (x) Tj\n"));
        assert!(!shows(b"BI /W 1 /H 1 ID \x00(x) Tj\xFF EI Q"));
        assert!(!shows(b"(unclosed Tj"));
        let mut drawn = Vec::new();
        assert!(!shows_text(b"q /Im1 Do /Fm#231 Do Q", &mut drawn));
        assert_eq!(drawn, [b"Im1".to_vec(), b"Fm#1".to_vec()]);
        assert_eq!(
            entities("a &unknown; &#65; &#x42; & b"),
            "a &unknown; A B & b"
        );
    }
}
