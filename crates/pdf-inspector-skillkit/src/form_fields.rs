//! Form field values pdf-inspector 1.24.0 misreads or never reads.
//!
//! pdf-inspector writes each filled form field into the Markdown as its name
//! and value, "payee_name: José García", from the form's field tree. It
//! reads a field's name and text as UTF-8, while a PDF writes them in
//! PDFDocEncoding or UTF-16, so an accented letter reads as "�" and a UTF-16
//! value as "��\0J\0o\0s…" (upstream issue #504). And it reads a value only
//! from a field that is its own widget: a field whose widgets are its kids,
//! as every group of radio buttons is, and any field shown in more than one
//! place, keeps its value on itself, where pdf-inspector never looks, so the
//! value is not written at all.
//!
//! The walk follows pdf-inspector's through the field tree, within its
//! bounds, and gives each value it misreads or passes over, as it would
//! write it if it read it right, with the pages its widgets sit on. The
//! Markdown decides: a value it shows was not lost, as when the page itself
//! draws the value.

use std::collections::{HashMap, HashSet};

use lopdf::{Dictionary, Document, Object, ObjectId};

/// Field tree nodes pdf-inspector visits, and how deep it goes.
const MAX_FIELD_NODES: usize = 100_000;
const MAX_FIELD_DEPTH: usize = 100;
/// Values reported, at most.
const MAX_VALUES: usize = 4_096;

/// A field value pdf-inspector misreads or never writes, and the pages it
/// belongs to. A text or choice is the value read right; a button's, whose
/// value is a word the page's own labels may show, is the field as
/// pdf-inspector would write it, "filing_status: Married".
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct FormValue {
    pub(crate) text: String,
    pub(crate) pages: Vec<u32>,
}

/// A text string as pdf-inspector reads it, and as the PDF means it.
fn readings(object: &Object) -> Option<(String, String)> {
    let bytes = object.as_str().ok()?;
    let read = String::from_utf8_lossy(bytes).to_string();
    let meant = lopdf::decode_text_string(object)
        .map(|text| text.trim_start_matches('\u{FEFF}').to_string())
        .unwrap_or_else(|_| read.clone());
    Some((read, meant))
}

/// Whether pdf-inspector's reading of a text shows what the PDF does not
/// mean: a replacement mark or a control character.
fn garbled(read: &str, meant: &str) -> bool {
    read != meant
        && read
            .chars()
            .any(|character| character == '\u{FFFD}' || character.is_control())
}

/// How pdf-inspector writes a field: its name, a colon, and its value; or
/// its value alone when it has no name.
fn written(name: &str, value: &str) -> String {
    if name.is_empty() {
        value.to_string()
    } else {
        format!("{name}: {value}")
    }
}

struct Walk<'a> {
    document: &'a Document,
    /// Pages by object, and the pages whose annotations hold a widget.
    pages: HashMap<ObjectId, u32>,
    annotation_pages: HashMap<ObjectId, u32>,
    visited: HashSet<ObjectId>,
    examined: usize,
    values: Vec<FormValue>,
}

impl Walk<'_> {
    fn exhausted(&self) -> bool {
        self.visited.len() >= MAX_FIELD_NODES || self.examined >= MAX_FIELD_NODES
    }

    fn dictionary<'b>(&'b self, object: &'b Object) -> Option<&'b Dictionary> {
        match object {
            Object::Reference(id) => self.document.get_dictionary(*id).ok(),
            Object::Dictionary(dictionary) => Some(dictionary),
            _ => None,
        }
    }

    fn array<'b>(&'b self, object: &'b Object) -> Option<&'b [Object]> {
        match object {
            Object::Reference(id) => self
                .document
                .get_object(*id)
                .ok()
                .and_then(|object| object.as_array().ok())
                .map(Vec::as_slice),
            Object::Array(array) => Some(array),
            _ => None,
        }
    }

    /// The page a widget sits on, as pdf-inspector places it: its `/P`, the
    /// page whose annotations hold it, or the first.
    fn page(&self, id: ObjectId, dictionary: &Dictionary) -> u32 {
        dictionary
            .get(b"P")
            .ok()
            .and_then(|page| page.as_reference().ok())
            .and_then(|page| self.pages.get(&page).copied())
            .or_else(|| self.annotation_pages.get(&id).copied())
            .unwrap_or(1)
    }

    /// A value as pdf-inspector reads it and as the PDF means it, for a
    /// field of type `kind`; `None` where it writes nothing.
    fn value(&self, kind: &[u8], value: &Object) -> Option<(String, String)> {
        match kind {
            b"Tx" | b"Ch" => match value {
                Object::String(..) => readings(value).filter(|(read, _)| !read.is_empty()),
                Object::Array(parts) => {
                    let parts: Vec<(String, String)> = parts.iter().filter_map(readings).collect();
                    if parts.is_empty() {
                        return None;
                    }
                    let read: Vec<&str> = parts.iter().map(|(read, _)| read.as_str()).collect();
                    let meant: Vec<&str> = parts.iter().map(|(_, meant)| meant.as_str()).collect();
                    Some((read.join(", "), meant.join(", ")))
                }
                _ => None,
            },
            b"Btn" => {
                let name = value.as_name().ok()?;
                if name == b"Off" {
                    return None;
                }
                let name = String::from_utf8_lossy(name).to_string();
                let shown = if name == "Yes" || name == "1" {
                    "Yes".to_string()
                } else {
                    name
                };
                Some((shown.clone(), shown))
            }
            _ => None,
        }
    }

    /// Walk a field and its kids as pdf-inspector does, with the type and
    /// the name each inherits, as it reads them and as the PDF means them.
    fn field(
        &mut self,
        id: ObjectId,
        parent_kind: Option<&[u8]>,
        parent_names: (&str, &str),
        depth: usize,
    ) {
        if depth > MAX_FIELD_DEPTH || self.exhausted() || self.values.len() >= MAX_VALUES {
            return;
        }
        if !self.visited.insert(id) {
            return;
        }
        let Ok(dictionary) = self.document.get_dictionary(id) else {
            return;
        };
        let (local_read, local_meant) = dictionary
            .get(b"T")
            .ok()
            .and_then(readings)
            .unwrap_or_default();
        let join = |parent: &str, local: &str| {
            if parent.is_empty() {
                local.to_string()
            } else if local.is_empty() {
                parent.to_string()
            } else {
                format!("{parent}.{local}")
            }
        };
        let name_read = join(parent_names.0, &local_read);
        let name_meant = join(parent_names.1, &local_meant);
        let kind: Option<Vec<u8>> = dictionary
            .get(b"FT")
            .ok()
            .and_then(|kind| kind.as_name().ok())
            .or(parent_kind)
            .map(<[u8]>::to_vec);
        let own = dictionary.get(b"V").ok();

        if let Some(kids) = dictionary
            .get(b"Kids")
            .ok()
            .and_then(|kids| self.array(kids))
        {
            let kids: Vec<ObjectId> = kids
                .iter()
                .filter_map(|kid| kid.as_reference().ok())
                .collect();
            // A value on a field whose kids are its widgets is written by
            // none of them unless one holds a value of its own.
            if let (Some(kind), Some(own)) = (kind.as_deref(), own) {
                let widgets: Vec<(ObjectId, &Dictionary)> = kids
                    .iter()
                    .filter_map(|&kid| {
                        let kid_dictionary = self.document.get_dictionary(kid).ok()?;
                        (!kid_dictionary.has(b"T")).then_some((kid, kid_dictionary))
                    })
                    .collect();
                let written_by_kid = widgets.iter().any(|(_, kid)| kid.has(b"V"));
                if kind != b"Sig" && !widgets.is_empty() && !written_by_kid {
                    if let Some((_, meant)) = self.value(kind, own) {
                        let mut pages: Vec<u32> = widgets
                            .iter()
                            .map(|(kid, kid_dictionary)| self.page(*kid, kid_dictionary))
                            .collect();
                        pages.sort_unstable();
                        pages.dedup();
                        let text = if kind == b"Btn" {
                            written(&name_meant, &meant)
                        } else {
                            meant
                        };
                        self.values.push(FormValue { text, pages });
                    }
                }
            }
            for kid in kids {
                if self.exhausted() {
                    break;
                }
                self.examined += 1;
                self.field(kid, kind.as_deref(), (&name_read, &name_meant), depth + 1);
            }
            return;
        }

        let (Some(kind), Some(own)) = (kind.as_deref(), own) else {
            return;
        };
        if kind == b"Sig" {
            return;
        }
        let Some((read, meant)) = self.value(kind, own) else {
            return;
        };
        if garbled(&read, &meant) {
            let page = self.page(id, dictionary);
            self.values.push(FormValue {
                text: meant,
                pages: vec![page],
            });
        }
    }
}

/// The field values pdf-inspector misreads or passes over in the form of
/// `document`, if it has one.
pub(crate) fn misread(document: &Document) -> Vec<FormValue> {
    let pages: HashMap<ObjectId, u32> = document
        .get_pages()
        .into_iter()
        .map(|(number, id)| (id, number))
        .collect();
    let mut walk = Walk {
        document,
        pages,
        annotation_pages: HashMap::new(),
        visited: HashSet::new(),
        examined: 0,
        values: Vec::new(),
    };
    let fields: Vec<ObjectId> = {
        let Some(root) = document
            .trailer
            .get(b"Root")
            .ok()
            .and_then(|root| walk.dictionary(root))
        else {
            return Vec::new();
        };
        let Some(form) = root
            .get(b"AcroForm")
            .ok()
            .and_then(|form| walk.dictionary(form))
        else {
            return Vec::new();
        };
        let Some(fields) = form
            .get(b"Fields")
            .ok()
            .and_then(|fields| walk.array(fields))
        else {
            return Vec::new();
        };
        fields
            .iter()
            .filter_map(|field| field.as_reference().ok())
            .collect()
    };
    if fields.is_empty() {
        return Vec::new();
    }
    let mut annotation_pages = HashMap::new();
    for (&page, &number) in &walk.pages {
        let Some(annotations) = document
            .get_dictionary(page)
            .ok()
            .and_then(|page| page.get(b"Annots").ok())
            .and_then(|annotations| walk.array(annotations))
        else {
            continue;
        };
        for annotation in annotations {
            if let Ok(id) = annotation.as_reference() {
                annotation_pages.insert(id, number);
            }
        }
    }
    walk.annotation_pages = annotation_pages;
    for field in fields {
        if walk.exhausted() {
            break;
        }
        walk.examined += 1;
        walk.field(field, None, ("", ""), 0);
    }
    walk.values
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{dictionary, StringFormat};

    /// A one-page document whose form holds `fields`, each added as it is
    /// given, with the page's annotations listing `widgets`.
    fn form(
        build: impl FnOnce(&mut Document, ObjectId) -> (Vec<ObjectId>, Vec<ObjectId>),
    ) -> Document {
        let mut document = Document::with_version("1.7");
        let pages_id = document.new_object_id();
        let page_id = document.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        });
        let (fields, widgets) = build(&mut document, page_id);
        let annotations: Vec<Object> = widgets.iter().map(|&id| id.into()).collect();
        document
            .get_dictionary_mut(page_id)
            .expect("page")
            .set("Annots", annotations);
        document.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page_id.into()],
                "Count" => 1,
            }),
        );
        let fields: Vec<Object> = fields.iter().map(|&id| id.into()).collect();
        let catalog = document.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
            "AcroForm" => dictionary! { "Fields" => fields },
        });
        document.trailer.set("Root", catalog);
        document
    }

    fn utf16(text: &str) -> Object {
        let mut bytes = vec![0xFE, 0xFF];
        for unit in text.encode_utf16() {
            bytes.extend(unit.to_be_bytes());
        }
        Object::String(bytes, StringFormat::Hexadecimal)
    }

    #[test]
    fn values_read_as_utf8_are_found_garbled() {
        let document = form(|document, page| {
            let name = document.add_object(dictionary! {
                "FT" => "Tx", "T" => Object::string_literal("payee_name"),
                "V" => utf16("José García"), "P" => page,
            });
            let city = document.add_object(dictionary! {
                "FT" => "Tx", "T" => Object::string_literal("city"),
                "V" => Object::String(b"S\xE3o Paulo".to_vec(), StringFormat::Literal),
                "P" => page,
            });
            let amount = document.add_object(dictionary! {
                "FT" => "Tx", "T" => Object::string_literal("amount"),
                "V" => Object::string_literal("1,250.00"), "P" => page,
            });
            (vec![name, city, amount], vec![name, city, amount])
        });
        assert_eq!(
            misread(&document),
            vec![
                FormValue {
                    text: "José García".to_string(),
                    pages: vec![1]
                },
                FormValue {
                    text: "São Paulo".to_string(),
                    pages: vec![1]
                },
            ]
        );
    }

    #[test]
    fn values_kept_on_a_field_whose_kids_are_its_widgets_are_found() {
        let document = form(|document, page| {
            // A group of radio buttons: the choice is on the group.
            let yes = document.add_object(dictionary! { "Subtype" => "Widget", "P" => page });
            let no = document.add_object(dictionary! { "Subtype" => "Widget", "P" => page });
            let status = document.add_object(dictionary! {
                "FT" => "Btn", "T" => Object::string_literal("filing_status"),
                "V" => "Married", "Kids" => vec![yes.into(), no.into()],
            });
            // A text field shown twice, and one whose widget repeats its value.
            let first = document.add_object(dictionary! { "Subtype" => "Widget" });
            let second = document.add_object(dictionary! { "Subtype" => "Widget" });
            let name = document.add_object(dictionary! {
                "FT" => "Tx", "T" => Object::string_literal("name"),
                "V" => Object::string_literal("Jane Sample"),
                "Kids" => vec![first.into(), second.into()],
            });
            let echoed = document.add_object(dictionary! {
                "Subtype" => "Widget", "P" => page,
                "V" => Object::string_literal("100.00"),
            });
            let total = document.add_object(dictionary! {
                "FT" => "Tx", "T" => Object::string_literal("total"),
                "V" => Object::string_literal("100.00"), "Kids" => vec![echoed.into()],
            });
            // A choice left off is not a value.
            let off = document.add_object(dictionary! { "Subtype" => "Widget", "P" => page });
            let box_off = document.add_object(dictionary! {
                "FT" => "Btn", "T" => Object::string_literal("dependent"),
                "V" => "Off", "Kids" => vec![off.into()],
            });
            (
                vec![status, name, total, box_off],
                vec![yes, no, first, second, echoed, off],
            )
        });
        assert_eq!(
            misread(&document),
            vec![
                FormValue {
                    text: "filing_status: Married".to_string(),
                    pages: vec![1]
                },
                FormValue {
                    text: "Jane Sample".to_string(),
                    pages: vec![1]
                },
            ]
        );
    }

    #[test]
    fn a_document_without_a_form_has_no_values() {
        let mut document = Document::with_version("1.7");
        let catalog = document.add_object(dictionary! { "Type" => "Catalog" });
        document.trailer.set("Root", catalog);
        assert!(misread(&document).is_empty());
        assert!(garbled("S\u{FFFD}o", "São"));
        assert!(!garbled("José", "Jos\u{e9}"));
        assert_eq!(written("", "x"), "x");
    }
}
