//! Form field values pdf-inspector 1.24.0 misreads or never reads.
//!
//! pdf-inspector writes each filled form field into the Markdown as its name
//! and value, "payee_name: José García", from the form's field tree. It
//! reads a field's name and text as UTF-8, while a PDF writes them in
//! PDFDocEncoding or UTF-16, so an accented letter reads as "�" and a UTF-16
//! value as "��\0J\0o\0s…" (upstream issue #504). And it reads a value only
//! as a string, strings, or a name given on the field's own widget: a value
//! kept on a field whose widgets are its kids, as every group of radio
//! buttons keeps its choice, inherited from a field above, given by
//! reference, as a text stream, or only as rich text, is not written at
//! all, though a viewer shows it.
//!
//! The walk follows pdf-inspector's through the field tree, within its
//! bounds, which count every entry of the tree's arrays, references or
//! not, and gives each value it misreads or passes over, as it would
//! write it if it read it right, with the pages a viewer shows it on: those
//! whose annotations hold a widget of the field not hidden, by its flags or
//! by a layer a reader hides. The Markdown decides: a value it shows was
//! not lost, as when the page itself draws the value. A value pdf-inspector
//! writes from a widget in a hidden layer is one no reader sees, and is
//! given as it writes it, with the hidden-layer text of its page. Where
//! pdf-inspector's bounds end its walk, the walk goes on as far again, and
//! gives each value a viewer shows past them, which pdf-inspector never
//! writes.
//!
//! A dynamic XFA form, whose catalog marks it as needing rendering and whose
//! form holds XFA, keeps its content in XFA, which a viewer lays out; its
//! pages hold only the notice a viewer without XFA shows, "Please wait...".
//! pdf-inspector reads no XFA, so such a form converts to that notice alone.
//! Nor does it read the files a PDF embeds: a portfolio, which bundles
//! documents such as a year's tax forms behind a cover page, converts to its
//! cover.

use std::collections::{HashMap, HashSet};

use lopdf::{Dictionary, Document, Object, ObjectId, StringFormat};

use crate::optional_content::Layers;

/// Field tree nodes pdf-inspector visits, and entries of the tree's arrays
/// it examines, and how deep it goes.
const MAX_FIELD_NODES: usize = 100_000;
const MAX_FIELD_DEPTH: usize = 100;
/// Bytes of a text stream read as a field's value.
const MAX_VALUE_BYTES: usize = 64 << 10;
/// References followed to reach an object.
const MAX_REFERENCE_HOPS: usize = 8;
/// Annotation flags that keep a widget from view: hidden, and not viewed.
const HIDDEN: i64 = 2;
const NO_VIEW: i64 = 32;

/// A field value pdf-inspector misreads or never writes, and the pages it
/// belongs to. A text or choice is the value read right; a button's, whose
/// value is a word the page's own labels may show, is the field as
/// pdf-inspector would write it, "filing_status: Married", as is a value
/// whose field's name pdf-inspector garbles.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct FormValue {
    pub(crate) text: String,
    pub(crate) pages: Vec<u32>,
}

/// The values of a form the Markdown may hold otherwise than a viewer
/// shows them.
#[derive(Debug, Default)]
pub(crate) struct Values {
    /// Values pdf-inspector misreads or passes over, as a viewer shows them.
    pub(crate) misread: Vec<FormValue>,
    /// Values pdf-inspector writes from a widget in a layer a reader hides,
    /// as it writes them, on the page it writes each for.
    pub(crate) hidden: Vec<FormValue>,
}

/// An object, its references followed.
fn resolved<'a>(document: &'a Document, mut object: &'a Object) -> Option<&'a Object> {
    for _ in 0..MAX_REFERENCE_HOPS {
        match object {
            Object::Reference(id) => object = document.get_object(*id).ok()?,
            object => return Some(object),
        }
    }
    None
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
/// mean: a replacement mark, where its bytes are not UTF-8.
fn garbled(read: &str, meant: &str) -> bool {
    read != meant && read.contains('\u{FFFD}')
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

/// How pdf-inspector writes a button's choice.
fn button(name: &[u8]) -> String {
    let name = String::from_utf8_lossy(name).to_string();
    if name == "Yes" || name == "1" {
        "Yes".to_string()
    } else {
        name
    }
}

struct Walk<'a> {
    document: &'a Document,
    /// The document's layers, if it has any.
    layers: Option<&'a Layers>,
    /// Pages by object, and the pages whose annotations hold a widget.
    pages: HashMap<ObjectId, u32>,
    annotation_pages: HashMap<ObjectId, u32>,
    visited: HashSet<ObjectId>,
    examined: usize,
    /// Whether pdf-inspector's walk has ended at its bounds: past them, it
    /// writes no value.
    past: bool,
    values: Values,
}

impl<'a> Walk<'a> {
    /// Whether the walk goes no further: it goes on past pdf-inspector's
    /// bounds, where `past` is set, as far again.
    fn stopped(&mut self) -> bool {
        let spent = |bound: usize| self.visited.len() >= bound || self.examined >= bound;
        let (ended, stopped) = (spent(MAX_FIELD_NODES), spent(2 * MAX_FIELD_NODES));
        self.past |= ended;
        stopped
    }

    fn dictionary(&self, object: &'a Object) -> Option<&'a Dictionary> {
        match object {
            Object::Reference(id) => self.document.get_dictionary(*id).ok(),
            Object::Dictionary(dictionary) => Some(dictionary),
            _ => None,
        }
    }

    fn array(&self, object: &'a Object) -> Option<&'a [Object]> {
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

    /// Whether a widget is in a layer a reader hides.
    fn layered(&self, widget: &Dictionary) -> bool {
        self.layers
            .is_some_and(|layers| layers.hide(self.document, widget))
    }

    /// The page a viewer shows a widget on: the page whose annotations hold
    /// it, or its `/P`; `None` where it is hidden, by its flags or by a
    /// layer, or on no page.
    fn page(&self, id: ObjectId, widget: &Dictionary) -> Option<u32> {
        let flags = widget
            .get(b"F")
            .ok()
            .and_then(|flags| resolved(self.document, flags))
            .and_then(|flags| flags.as_i64().ok())
            .unwrap_or(0);
        if flags & (HIDDEN | NO_VIEW) != 0 || self.layered(widget) {
            return None;
        }
        self.annotation_pages.get(&id).copied().or_else(|| {
            widget
                .get(b"P")
                .ok()
                .and_then(|page| page.as_reference().ok())
                .and_then(|page| self.pages.get(&page).copied())
        })
    }

    /// The page pdf-inspector writes a widget's value for: its `/P`, else
    /// the page whose annotations hold it, else the first.
    fn written_page(&self, id: ObjectId, widget: &Dictionary) -> u32 {
        widget
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
                let shown = button(name);
                Some((shown.clone(), shown))
            }
            _ => None,
        }
    }

    /// A value as a viewer shows it, for a field of type `kind`: given by
    /// reference or not, as text, a text stream, or a choice; `None` where
    /// it shows none.
    fn shown(&self, kind: &[u8], value: &'a Object) -> Option<String> {
        let document = self.document;
        let text = |object: &'a Object| -> Option<String> {
            match resolved(document, object)? {
                Object::Stream(stream) => {
                    let bytes = stream.get_plain_content_with_limit(MAX_VALUE_BYTES).ok()?;
                    readings(&Object::String(bytes, StringFormat::Literal)).map(|(_, meant)| meant)
                }
                object => readings(object).map(|(_, meant)| meant),
            }
        };
        let shown = match (kind, resolved(document, value)?) {
            (b"Tx" | b"Ch", Object::Array(parts)) => {
                let parts: Vec<String> = parts.iter().filter_map(text).collect();
                (!parts.is_empty()).then(|| parts.join(", "))
            }
            (b"Tx" | b"Ch", value) => text(value),
            (b"Btn", value) => value
                .as_name()
                .ok()
                .filter(|name| *name != b"Off")
                .map(button),
            _ => None,
        }?;
        (!shown.trim().is_empty()).then_some(shown)
    }

    /// The plain text of a text field's rich value, which a viewer shows
    /// where it has no value as plain text.
    fn rich(&self, kind: &[u8], field: &'a Dictionary) -> Option<String> {
        if kind != b"Tx" {
            return None;
        }
        let rich = self.shown(kind, field.get(b"RV").ok()?)?;
        let plain = crate::annotations::plain(&rich);
        (!plain.trim().is_empty()).then_some(plain)
    }

    /// Note a value pdf-inspector leaves out, on `pages`: the value shown,
    /// `value` or else the field's rich value, as a field of type `kind`
    /// named `name` holds it.
    fn left_out(
        &mut self,
        kind: &[u8],
        name: &str,
        value: Option<&'a Object>,
        field: &'a Dictionary,
        pages: Vec<u32>,
    ) {
        if pages.is_empty() {
            return;
        }
        let Some(shown) = value
            .and_then(|value| self.shown(kind, value))
            .or_else(|| self.rich(kind, field))
        else {
            return;
        };
        let text = if kind == b"Btn" {
            written(name, &shown)
        } else {
            shown
        };
        self.values.misread.push(FormValue { text, pages });
    }

    /// Walk a field and its kids as pdf-inspector does, with the type, the
    /// name, and the value each inherits, the name as it reads it and as the
    /// PDF means it.
    fn field(
        &mut self,
        id: ObjectId,
        parent_kind: Option<&'a [u8]>,
        parent_names: (&str, &str),
        inherited: Option<&'a Object>,
        depth: usize,
    ) {
        if depth > MAX_FIELD_DEPTH || self.stopped() {
            return;
        }
        if !self.visited.insert(id) {
            return;
        }
        let document = self.document;
        let Ok(dictionary) = document.get_dictionary(id) else {
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
        let kind: Option<&'a [u8]> = dictionary
            .get(b"FT")
            .ok()
            .and_then(|kind| kind.as_name().ok())
            .or(parent_kind);
        let own = dictionary.get(b"V").ok();
        let value = own.or(inherited);

        if let Some(entries) = dictionary
            .get(b"Kids")
            .ok()
            .and_then(|kids| self.array(kids))
        {
            let kids: Vec<ObjectId> = entries
                .iter()
                .filter_map(|kid| kid.as_reference().ok())
                .collect();
            // A field whose kids are its widgets shows its value in each;
            // pdf-inspector writes only a value a widget holds of its own. A
            // field with no kids at all is its own widget, and writes none.
            if let Some(kind) = kind.filter(|kind| *kind != b"Sig") {
                let widgets: Vec<(ObjectId, &'a Dictionary)> = if kids.is_empty() {
                    vec![(id, dictionary)]
                } else {
                    kids.iter()
                        .filter_map(|&kid| {
                            let widget = document.get_dictionary(kid).ok()?;
                            (!widget.has(b"T") && !widget.has(b"Kids")).then_some((kid, widget))
                        })
                        .collect()
                };
                let written_by_widget = !self.past
                    && !kids.is_empty()
                    && widgets.iter().any(|(_, widget)| {
                        widget
                            .get(b"V")
                            .ok()
                            .and_then(|value| self.value(kind, value))
                            .is_some()
                    });
                if !written_by_widget {
                    let mut pages: Vec<u32> = widgets
                        .iter()
                        .filter_map(|(widget, dictionary)| self.page(*widget, dictionary))
                        .collect();
                    pages.sort_unstable();
                    pages.dedup();
                    self.left_out(kind, &name_meant, value, dictionary, pages);
                }
            }
            // pdf-inspector counts every entry against its bounds.
            for kid in entries {
                if self.stopped() {
                    break;
                }
                self.examined += 1;
                if let Ok(kid) = kid.as_reference() {
                    self.field(kid, kind, (&name_read, &name_meant), value, depth + 1);
                }
            }
            return;
        }

        let Some(kind) = kind.filter(|kind| *kind != b"Sig") else {
            return;
        };
        // A widget in a layer a reader hides shows no value, and
        // pdf-inspector writes the value it holds all the same.
        if self.layered(dictionary) {
            if let Some((read, _)) = own
                .filter(|_| !self.past)
                .and_then(|own| self.value(kind, own))
            {
                let page = self.written_page(id, dictionary);
                self.values.hidden.push(FormValue {
                    text: written(&name_read, &read),
                    pages: vec![page],
                });
            }
            return;
        }
        let Some(page) = self.page(id, dictionary) else {
            return;
        };
        // Past pdf-inspector's bounds, a field that is its own widget, whose
        // value pdf-inspector would have written, is not in the Markdown.
        if self.past {
            if dictionary.has(b"T") || depth == 0 {
                self.left_out(kind, &name_meant, value, dictionary, vec![page]);
            }
            return;
        }
        match own.and_then(|own| self.value(kind, own)) {
            Some((read, meant)) => {
                let name_garbled = garbled(&name_read, &name_meant);
                if name_garbled || garbled(&read, &meant) {
                    let text = if kind == b"Btn" || name_garbled {
                        written(&name_meant, &meant)
                    } else {
                        meant
                    };
                    self.values.misread.push(FormValue {
                        text,
                        pages: vec![page],
                    });
                }
            }
            // A field that is its own widget shows the value it holds or
            // inherits; a widget of a field above shows that field's, which
            // the field notes.
            None if dictionary.has(b"T") || depth == 0 => {
                self.left_out(kind, &name_meant, value, dictionary, vec![page]);
            }
            None => {}
        }
    }
}

/// The stream holding the file a file specification embeds, unless the
/// document says the file restates what its pages show: an alternative
/// form of its content, or the XML data behind it, as an e-invoice
/// (ZUGFeRD, Factur-X) carries its invoice.
fn embedded_stream(document: &Document, specification: &Object) -> Option<ObjectId> {
    let specification = resolved(document, specification)?.as_dict().ok()?;
    let files = resolved(document, specification.get(b"EF").ok()?)?
        .as_dict()
        .ok()?;
    let stream = [&b"UF"[..], b"F", b"DOS", b"Mac", b"Unix"]
        .iter()
        .find_map(|key| files.get(key).ok()?.as_reference().ok())?;
    let content = document.get_object(stream).ok()?.as_stream().ok()?;
    let xml = || {
        let named_xml = [&b"UF"[..], b"F"].iter().any(|key| {
            specification
                .get(key)
                .ok()
                .and_then(|name| resolved(document, name))
                .and_then(|name| lopdf::decode_text_string(name).ok())
                .is_some_and(|name| name.to_ascii_lowercase().ends_with(".xml"))
        });
        let typed_xml = content
            .dict
            .get(b"Subtype")
            .ok()
            .and_then(|subtype| subtype.as_name().ok())
            .is_some_and(|subtype| matches!(subtype, b"text/xml" | b"application/xml"));
        named_xml || typed_xml
    };
    let relationship = specification
        .get(b"AFRelationship")
        .ok()
        .and_then(|relationship| resolved(document, relationship))
        .and_then(|relationship| relationship.as_name().ok());
    match relationship {
        Some(b"Alternative") => None,
        Some(b"Data") if xml() => None,
        _ => Some(stream),
    }
}

/// Files embedded in a document, each counted once: in its catalog's name
/// tree, and in file attachment annotations, within `MAX_FIELD_NODES` steps;
/// and whether the catalog makes it a portfolio (a collection), whose pages
/// hold only a cover while its documents are the files.
pub(crate) fn embedded_files(document: &Document) -> (usize, bool) {
    let Some(root) = document
        .trailer
        .get(b"Root")
        .ok()
        .and_then(|root| resolved(document, root))
        .and_then(|root| root.as_dict().ok())
    else {
        return (0, false);
    };
    let portfolio = root.has(b"Collection");
    let mut files: HashSet<ObjectId> = HashSet::new();
    let mut steps = 0;
    // The name tree: `/Names` pairs in its leaves, `/Kids` above them, each
    // node read once.
    let mut nodes: HashSet<ObjectId> = HashSet::new();
    let mut stack: Vec<(&Object, usize)> = root
        .get(b"Names")
        .ok()
        .and_then(|names| resolved(document, names))
        .and_then(|names| names.as_dict().ok())
        .and_then(|names| names.get(b"EmbeddedFiles").ok())
        .map(|tree| vec![(tree, 0)])
        .unwrap_or_default();
    while let Some((node, depth)) = stack.pop() {
        steps += 1;
        if steps > MAX_FIELD_NODES {
            break;
        }
        if depth > MAX_FIELD_DEPTH {
            continue;
        }
        if let Object::Reference(id) = node {
            if !nodes.insert(*id) {
                continue;
            }
        }
        let Some(node) = resolved(document, node).and_then(|node| node.as_dict().ok()) else {
            continue;
        };
        if let Some(names) = node
            .get(b"Names")
            .ok()
            .and_then(|names| resolved(document, names))
            .and_then(|names| names.as_array().ok())
        {
            for value in names.iter().skip(1).step_by(2) {
                steps += 1;
                if steps > MAX_FIELD_NODES {
                    break;
                }
                files.extend(embedded_stream(document, value));
            }
        }
        if let Some(kids) = node
            .get(b"Kids")
            .ok()
            .and_then(|kids| resolved(document, kids))
            .and_then(|kids| kids.as_array().ok())
        {
            for kid in kids {
                steps += 1;
                if steps > MAX_FIELD_NODES {
                    break;
                }
                stack.push((kid, depth + 1));
            }
        }
    }
    'pages: for (_, page) in document.get_pages() {
        let Some(annotations) = document
            .get_dictionary(page)
            .ok()
            .and_then(|page| page.get(b"Annots").ok())
            .and_then(|annotations| resolved(document, annotations))
            .and_then(|annotations| annotations.as_array().ok())
        else {
            continue;
        };
        for annotation in annotations {
            steps += 1;
            if steps > MAX_FIELD_NODES {
                break 'pages;
            }
            let Some(annotation) =
                resolved(document, annotation).and_then(|annotation| annotation.as_dict().ok())
            else {
                continue;
            };
            let attached = annotation
                .get(b"Subtype")
                .ok()
                .and_then(|subtype| subtype.as_name().ok())
                == Some(b"FileAttachment");
            if attached {
                if let Ok(specification) = annotation.get(b"FS") {
                    files.extend(embedded_stream(document, specification));
                }
            }
        }
    }
    (files.len(), portfolio)
}

/// Whether `document` is a dynamic XFA form: its catalog says it needs
/// rendering, and its form holds XFA.
pub(crate) fn dynamic_xfa(document: &Document) -> bool {
    let Some(root) = document
        .trailer
        .get(b"Root")
        .ok()
        .and_then(|root| resolved(document, root))
        .and_then(|root| root.as_dict().ok())
    else {
        return false;
    };
    let needs_rendering = root
        .get(b"NeedsRendering")
        .ok()
        .and_then(|needs| resolved(document, needs))
        .and_then(|needs| needs.as_bool().ok())
        .unwrap_or(false);
    let holds_xfa = root
        .get(b"AcroForm")
        .ok()
        .and_then(|form| resolved(document, form))
        .and_then(|form| form.as_dict().ok())
        .and_then(|form| form.get(b"XFA").ok())
        .and_then(|xfa| resolved(document, xfa))
        .is_some_and(|xfa| {
            matches!(xfa, Object::Array(parts) if !parts.is_empty()) || xfa.as_stream().is_ok()
        });
    needs_rendering && holds_xfa
}

/// The field values of the form of `document`, if it has one, that the
/// Markdown may hold otherwise than a viewer shows them, with the layers
/// the document sets (see `Values`).
pub(crate) fn values(document: &Document, layers: Option<&Layers>) -> Values {
    let pages: HashMap<ObjectId, u32> = document
        .get_pages()
        .into_iter()
        .map(|(number, id)| (id, number))
        .collect();
    let mut walk = Walk {
        document,
        layers,
        pages,
        annotation_pages: HashMap::new(),
        visited: HashSet::new(),
        examined: 0,
        past: false,
        values: Values::default(),
    };
    let fields: &[Object] = {
        let Some(root) = document
            .trailer
            .get(b"Root")
            .ok()
            .and_then(|root| walk.dictionary(root))
        else {
            return Values::default();
        };
        let Some(form) = root
            .get(b"AcroForm")
            .ok()
            .and_then(|form| walk.dictionary(form))
        else {
            return Values::default();
        };
        let Some(fields) = form
            .get(b"Fields")
            .ok()
            .and_then(|fields| walk.array(fields))
        else {
            return Values::default();
        };
        fields
    };
    if fields.is_empty() {
        return Values::default();
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
    // pdf-inspector counts every entry against its bounds.
    for field in fields {
        if walk.stopped() {
            break;
        }
        walk.examined += 1;
        if let Ok(field) = field.as_reference() {
            walk.field(field, None, ("", ""), None, 0);
        }
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
            values(&document, None).misread,
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
            values(&document, None).misread,
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
    fn values_pdf_inspector_cannot_reach_are_found() {
        let document = form(|document, page| {
            // A group whose widgets each hold "Off" of their own.
            let yes = document.add_object(dictionary! { "Subtype" => "Widget", "V" => "Off" });
            let no = document.add_object(dictionary! { "Subtype" => "Widget", "V" => "Off" });
            let status = document.add_object(dictionary! {
                "FT" => "Btn", "T" => Object::string_literal("filing_status"),
                "V" => "Married", "Kids" => vec![yes.into(), no.into()],
            });
            // Values given by reference, as a text stream, or as rich text.
            let city_value = document.add_object(Object::string_literal("Springfield"));
            let city = document.add_object(dictionary! {
                "FT" => "Tx", "T" => Object::string_literal("city"), "V" => city_value,
            });
            let memo_value =
                document.add_object(lopdf::Stream::new(dictionary! {}, b"Paid in full".to_vec()));
            let memo = document.add_object(dictionary! {
                "FT" => "Tx", "T" => Object::string_literal("memo"), "V" => memo_value,
            });
            let note = document.add_object(dictionary! {
                "FT" => "Tx", "T" => Object::string_literal("note"),
                "RV" => Object::string_literal("<body><p>Basis 12,500.00</p></body>"),
            });
            // A value on a field above, which named fields below inherit.
            let spouse =
                document.add_object(dictionary! { "T" => Object::string_literal("spouse") });
            let joint = document.add_object(dictionary! {
                "FT" => "Tx", "T" => Object::string_literal("joint"),
                "V" => Object::string_literal("Alex Sample"), "Kids" => vec![spouse.into()],
            });
            // A field listing no kids is its own widget.
            let empty = document.add_object(dictionary! {
                "FT" => "Tx", "T" => Object::string_literal("ssn_last4"),
                "V" => Object::string_literal("6789"), "Kids" => Vec::<Object>::new(),
            });
            // A name pdf-inspector garbles, with its value read right.
            let named = document.add_object(dictionary! {
                "FT" => "Tx", "T" => utf16("Número"), "V" => Object::string_literal("42"),
            });
            // Hidden, on no page, or holding a NUL pdf-inspector keeps: not
            // lost.
            let hidden_widget =
                document.add_object(dictionary! { "Subtype" => "Widget", "F" => 2 });
            let hidden = document.add_object(dictionary! {
                "FT" => "Tx", "T" => Object::string_literal("hidden"),
                "V" => Object::string_literal("Secret"), "Kids" => vec![hidden_widget.into()],
            });
            let nowhere = document.add_object(dictionary! {
                "FT" => "Tx", "T" => Object::string_literal("nowhere"),
                "V" => Object::String(b"S\xE3o".to_vec(), StringFormat::Literal),
            });
            let nul = document.add_object(dictionary! {
                "FT" => "Tx", "T" => Object::string_literal("nul"),
                "V" => Object::String(b"Jane\0Sample".to_vec(), StringFormat::Literal),
                "P" => page,
            });
            (
                vec![
                    status, city, memo, note, joint, empty, named, hidden, nowhere, nul,
                ],
                vec![
                    yes,
                    no,
                    city,
                    memo,
                    note,
                    spouse,
                    empty,
                    named,
                    hidden_widget,
                    nul,
                ],
            )
        });
        let texts: Vec<(String, Vec<u32>)> = values(&document, None)
            .misread
            .into_iter()
            .map(|value| (value.text, value.pages))
            .collect();
        let expected = [
            "filing_status: Married",
            "Springfield",
            "Paid in full",
            "  Basis 12,500.00  ",
            "Alex Sample",
            "6789",
            "Número: 42",
        ];
        assert_eq!(
            texts,
            expected
                .iter()
                .map(|text| (text.to_string(), vec![1]))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn values_in_a_layer_a_reader_hides_are_found_as_written() {
        let mut layer = None;
        let mut document = form(|document, page| {
            let superseded = document.add_object(dictionary! {
                "Type" => "OCG", "Name" => Object::string_literal("Superseded"),
            });
            layer = Some(superseded);
            // A value in the layer, which pdf-inspector writes though no
            // reader sees it, on the page its widget names.
            let old = document.add_object(dictionary! {
                "FT" => "Tx", "T" => Object::string_literal("old_balance"),
                "V" => Object::string_literal("1,000.00 superseded"), "P" => page,
                "OC" => superseded,
            });
            // A group whose one widget is in the layer shows no value.
            let widget = document.add_object(dictionary! {
                "Subtype" => "Widget", "P" => page, "OC" => superseded,
            });
            let group = document.add_object(dictionary! {
                "FT" => "Btn", "T" => Object::string_literal("filing_status"),
                "V" => "Married", "Kids" => vec![widget.into()],
            });
            (vec![old, group], vec![old, widget])
        });
        // With no layers, the group's value is shown and passed over.
        let shown = values(&document, None);
        assert!(shown.hidden.is_empty());
        assert_eq!(
            shown.misread,
            vec![FormValue {
                text: "filing_status: Married".to_string(),
                pages: vec![1]
            }]
        );
        let superseded = layer.expect("a layer");
        let catalog = document
            .trailer
            .get(b"Root")
            .and_then(Object::as_reference)
            .expect("a catalog");
        document
            .get_dictionary_mut(catalog)
            .expect("a catalog")
            .set(
                "OCProperties",
                dictionary! {
                    "OCGs" => vec![superseded.into()],
                    "D" => dictionary! { "OFF" => vec![superseded.into()] },
                },
            );
        let layers = Layers::new(&document).expect("layers");
        let hidden = values(&document, Some(&layers));
        assert!(hidden.misread.is_empty(), "{:?}", hidden.misread);
        assert_eq!(
            hidden.hidden,
            vec![FormValue {
                text: "old_balance: 1,000.00 superseded".to_string(),
                pages: vec![1]
            }]
        );
    }

    #[test]
    fn values_past_the_bounds_of_pdf_inspectors_walk_are_found() {
        // pdf-inspector counts every entry of the form's fields against its
        // bounds, references or not, the field's own among them, and a
        // field listed past them is never written, though a viewer shows it.
        for (entries, lost) in [(MAX_FIELD_NODES - 2, false), (MAX_FIELD_NODES - 1, true)] {
            let mut document = form(|document, page| {
                let payee = document.add_object(dictionary! {
                    "FT" => "Tx", "T" => Object::string_literal("payee"),
                    "V" => Object::string_literal("Example Payee LLC"), "P" => page,
                });
                (vec![payee], vec![payee])
            });
            let catalog = document
                .trailer
                .get(b"Root")
                .and_then(Object::as_reference)
                .expect("a catalog");
            let fields = document
                .get_dictionary_mut(catalog)
                .and_then(|catalog| catalog.get_mut(b"AcroForm"))
                .and_then(Object::as_dict_mut)
                .and_then(|form| form.get_mut(b"Fields"))
                .and_then(Object::as_array_mut)
                .expect("fields");
            fields.splice(0..0, std::iter::repeat_n(Object::Integer(0), entries));
            let expected = lost.then(|| FormValue {
                text: "Example Payee LLC".to_string(),
                pages: vec![1],
            });
            assert_eq!(
                values(&document, None).misread,
                expected.into_iter().collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn a_document_without_a_form_has_no_values() {
        let mut document = Document::with_version("1.7");
        let catalog = document.add_object(dictionary! { "Type" => "Catalog" });
        document.trailer.set("Root", catalog);
        let found = values(&document, None);
        assert!(found.misread.is_empty() && found.hidden.is_empty());
        assert!(!dynamic_xfa(&document));
        assert_eq!(embedded_files(&document), (0, false));
        // Needing rendering makes a dynamic XFA form only with XFA to render.
        for (xfa, dynamic) in [(true, true), (false, false)] {
            let mut form = Document::with_version("1.7");
            let template = form.add_object(lopdf::Stream::new(dictionary! {}, b"<xdp/>".to_vec()));
            let acroform = if xfa {
                dictionary! { "Fields" => Vec::<Object>::new(), "XFA" => vec![Object::string_literal("template"), template.into()] }
            } else {
                dictionary! { "Fields" => Vec::<Object>::new() }
            };
            let catalog = form.add_object(dictionary! {
                "Type" => "Catalog", "NeedsRendering" => true, "AcroForm" => acroform,
            });
            form.trailer.set("Root", catalog);
            assert_eq!(dynamic_xfa(&form), dynamic);
        }
    }

    #[test]
    fn embedded_files_are_counted_once_each() {
        let mut portfolio = Document::with_version("1.7");
        let specification = |document: &mut Document, name: &str, relationship: Option<&str>| {
            let stream =
                document.add_object(lopdf::Stream::new(dictionary! {}, b"%PDF-1.7".to_vec()));
            let mut specification = dictionary! {
                "Type" => "Filespec", "F" => Object::string_literal(name),
                "EF" => dictionary! { "F" => stream },
            };
            if let Some(relationship) = relationship {
                specification.set(
                    "AFRelationship",
                    Object::Name(relationship.as_bytes().to_vec()),
                );
            }
            document.add_object(specification)
        };
        let dividends = specification(&mut portfolio, "1099-DIV.pdf", None);
        let interest = specification(&mut portfolio, "1099-INT.pdf", Some("Unspecified"));
        // An e-invoice's XML restates the invoice its pages show.
        let invoice = specification(&mut portfolio, "factur-x.xml", Some("Alternative"));
        let data = specification(&mut portfolio, "zugferd-invoice.xml", Some("Data"));
        let dangling: Object = (9_999, 0).into();
        let leaf = portfolio.add_object(dictionary! {
            "Names" => vec![
                Object::string_literal("1099-DIV.pdf"), dividends.into(),
                Object::string_literal("again.pdf"), dividends.into(),
                Object::string_literal("1099-INT.pdf"), interest.into(),
                Object::string_literal("factur-x.xml"), invoice.into(),
                Object::string_literal("zugferd-invoice.xml"), data.into(),
                Object::string_literal("null"), Object::Null,
                Object::string_literal("dangling"), dangling,
                Object::string_literal("unembedded"), dictionary! { "Type" => "Filespec" }.into(),
            ],
        });
        // A tree that lists its leaf twice, and a node that lists itself.
        let looping = portfolio.new_object_id();
        portfolio.objects.insert(
            looping,
            Object::Dictionary(dictionary! { "Kids" => vec![looping.into(), leaf.into()] }),
        );
        let catalog = portfolio.add_object(dictionary! {
            "Type" => "Catalog",
            "Collection" => dictionary! { "Type" => "Collection" },
            "Names" => dictionary! {
                "EmbeddedFiles" => dictionary! { "Kids" => vec![leaf.into(), leaf.into(), looping.into()] },
            },
        });
        portfolio.trailer.set("Root", catalog);
        assert_eq!(embedded_files(&portfolio), (2, true));
        assert!(garbled("S\u{FFFD}o", "São"));
        assert!(!garbled("José", "Jos\u{e9}"));
        assert!(!garbled("Jane\0Sample", "JaneSample"));
        assert_eq!(written("", "x"), "x");
    }
}
