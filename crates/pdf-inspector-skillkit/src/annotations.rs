//! Annotation text pdf-inspector 1.24.0 never reads.
//!
//! Besides a page's own content, a PDF shows text in its annotations: a text
//! box typed onto the page (FreeText), as a reviewer adds "Adjusted basis
//! 12,500.00 per preparer", or a stamp or watermark drawn in text, "RECEIVED
//! APR 15 2025". pdf-inspector reads a page's content, its links, and its
//! form values, and no other annotation, so such text is not in the
//! Markdown. The check finds the annotations a reader shows with text of
//! their own, not hidden: text boxes, and stamps and watermarks whose
//! appearance draws text. It gives the text each holds, its `/Contents` or
//! the plain text of its rich text, with its page; the Markdown decides.
//! Notes shown only in a popup, and markup that comments on the page's own
//! text, are not what the page shows, and are not read.

use lopdf::{content::Content, Dictionary, Document, Object};

/// Annotations read, at most, across a document.
const MAX_ANNOTATIONS: usize = 10_000;
/// Bytes of an appearance stream decoded to find text in it.
const MAX_APPEARANCE_BYTES: usize = 1 << 20;
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

/// A text string as the PDF means it.
fn text(document: &Document, object: &Object) -> Option<String> {
    let object = resolve(document, object)?;
    lopdf::decode_text_string(object)
        .ok()
        .map(|text| text.trim_start_matches('\u{FEFF}').to_string())
}

/// The plain text of rich text: its XHTML with the tags left out.
fn plain(rich: &str) -> String {
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
    plain
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

/// Whether an annotation's normal appearance draws text: shows strings with
/// `Tj`, `TJ`, `'`, or `"`.
fn draws_text(document: &Document, annotation: &Dictionary) -> bool {
    let Some(appearance) = annotation
        .get(b"AP")
        .ok()
        .and_then(|appearance| resolve(document, appearance))
        .and_then(|appearance| appearance.as_dict().ok())
        .and_then(|appearance| appearance.get(b"N").ok())
        .and_then(|normal| resolve(document, normal))
    else {
        return false;
    };
    // The normal appearance is a stream, or streams by appearance state.
    let stream = match appearance {
        Object::Stream(stream) => Some(stream),
        Object::Dictionary(states) => annotation
            .get(b"AS")
            .ok()
            .and_then(|state| state.as_name().ok())
            .and_then(|state| states.get(state).ok())
            .and_then(|state| resolve(document, state))
            .and_then(|state| state.as_stream().ok()),
        _ => None,
    };
    let Some(stream) = stream else {
        return false;
    };
    if stream.content.len() > MAX_APPEARANCE_BYTES {
        return false;
    }
    let content = stream
        .decompressed_content()
        .unwrap_or_else(|_| stream.content.clone());
    if content.len() > MAX_APPEARANCE_BYTES {
        return false;
    }
    Content::decode(&content).is_ok_and(|content| {
        content
            .operations
            .iter()
            .any(|operation| matches!(operation.operator.as_str(), "Tj" | "TJ" | "'" | "\""))
    })
}

/// The text the annotations of `document` show on its pages, among the
/// pages `only` names, if any.
pub(crate) fn unread(
    document: &Document,
    only: Option<&std::collections::HashSet<u32>>,
) -> Vec<AnnotationText> {
    let mut texts = Vec::new();
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
        for annotation in annotations {
            read += 1;
            if read > MAX_ANNOTATIONS {
                return texts;
            }
            let Some(annotation) =
                resolve(document, annotation).and_then(|annotation| annotation.as_dict().ok())
            else {
                continue;
            };
            let flags = annotation
                .get(b"F")
                .ok()
                .and_then(|flags| resolve(document, flags))
                .and_then(|flags| flags.as_i64().ok())
                .unwrap_or(0);
            if flags & (HIDDEN | NO_VIEW) != 0 {
                continue;
            }
            let subtype = annotation
                .get(b"Subtype")
                .ok()
                .and_then(|subtype| subtype.as_name().ok())
                .unwrap_or_default();
            let shown = match subtype {
                b"FreeText" => true,
                b"Stamp" | b"Watermark" => draws_text(document, annotation),
                _ => false,
            };
            if !shown {
                continue;
            }
            let contents = annotation
                .get(b"Contents")
                .ok()
                .and_then(|contents| text(document, contents))
                .filter(|contents| !contents.trim().is_empty())
                .or_else(|| {
                    annotation
                        .get(b"RC")
                        .ok()
                        .and_then(|rich| text(document, rich))
                        .map(|rich| plain(&rich))
                        .filter(|rich| !rich.trim().is_empty())
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
        let texts: Vec<String> = unread(&found, None)
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
        assert!(unread(&found, Some(&[2].into_iter().collect())).is_empty());
    }
}
