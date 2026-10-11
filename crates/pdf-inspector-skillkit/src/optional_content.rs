//! Layers a reader hides, whose text pdf-inspector 1.25.0 reads anyway.
//!
//! A PDF can set content in optional layers and hide some of them by
//! default: a superseded figure kept beside the current one, a draft stamp,
//! text meant only for print. A reader shows the page as the document's
//! default configuration sets its layers; pdf-inspector reads no layer
//! settings, so it reads every layer as shown, and the Markdown holds text
//! the page does not show, "Ending balance 1,000.00 superseded" beside the
//! "Ending balance 2,000.00" a reader sees. The layers are read from the
//! default configuration: its base state, the layers it turns on or off,
//! and the states a viewer sets them to from their usage on opening the
//! document; a layer is hidden only where the common readers all hide it
//! (see `Layers::hidden`). A membership dictionary shows its content as its
//! policy or visibility expression says, and a layer meant for design only
//! has no effect on viewing. Content is hidden where a marked-content span,
//! a form, or an annotation names a layer so hidden.
//!
//! Each verdict is reached once: a layer's, and that of a membership
//! dictionary or an expression the document holds, are kept, so a page
//! naming one dictionary of a thousand layers in each of a hundred thousand
//! spans reads the thousand layers once. The layers and terms read to reach
//! verdicts not yet kept are bounded per document; past the bound, content
//! whose verdict is not kept shows, so the check reports less, never text a
//! reader sees.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};

use lopdf::{Dictionary, Document, Object, ObjectId};

/// How deep a visibility expression is read; deeper, its content shows.
const MAX_EXPRESSION_DEPTH: usize = 32;
/// Layers and expression terms read per document to reach verdicts not yet
/// kept (see the module's documentation).
const MAX_TERMS: usize = 1_000_000;

/// The categories of a layer's usage dictionary a viewer sets its state
/// from on opening the document (ISO 32000-1, 8.11.4.4), each with the entry
/// giving the state it recommends; the rest recommend a state the reader
/// decides, by its magnification, its user, or its language.
const CATEGORIES: [(&[u8], Option<&[u8]>); 6] = [
    (b"View", Some(b"ViewState")),
    (b"Print", Some(b"PrintState")),
    (b"Export", Some(b"ExportState")),
    (b"Zoom", None),
    (b"User", None),
    (b"Language", None),
];

/// What a verdict is kept by: a layer, by its object id; a membership
/// dictionary or an expression the document refers to, by its object id;
/// or a dictionary the document holds in place, by its address, which
/// stays put while the document is read, as `GlyphFonts` keeps fonts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Key {
    Layer(ObjectId),
    Object(ObjectId),
    Address(usize),
}

/// The states a configuration of the document's layers sets them to.
#[derive(Debug, Default)]
struct Configuration {
    /// Whether layers are off unless turned on.
    base_off: bool,
    on: HashSet<ObjectId>,
    off: HashSet<ObjectId>,
}

impl Configuration {
    /// The configuration `dictionary` holds.
    fn of(document: &Document, dictionary: &Dictionary) -> Self {
        Configuration {
            base_off: dictionary
                .get(b"BaseState")
                .ok()
                .and_then(|state| state.as_name().ok())
                .is_some_and(|state| state == b"OFF"),
            on: references(document, dictionary.get(b"ON").ok()),
            off: references(document, dictionary.get(b"OFF").ok()),
        }
    }

    /// Whether it turns the layer `id` on.
    fn sets_on(&self, id: ObjectId) -> bool {
        if self.base_off {
            self.on.contains(&id)
        } else {
            !self.off.contains(&id)
        }
    }
}

/// The document's layers, as its default configuration sets them.
#[derive(Debug)]
pub(crate) struct Layers {
    /// The default configuration.
    default: Configuration,
    /// The configuration PDFium sets layers by, where it is not the default
    /// one: the first of the alternate ones meant for viewing.
    viewing: Option<Configuration>,
    /// The layers the document lists; PDFium shows any other.
    listed: HashSet<ObjectId>,
    /// The categories a viewer sets each layer's state from on opening the
    /// document (see `automatic`); `None` where they were too many to read.
    automatic: Option<HashMap<ObjectId, u8>>,
    /// Whether the content each layer, membership dictionary, and
    /// expression judged so far marks shows.
    shows: RefCell<HashMap<Key, bool>>,
    /// Layers and terms left to read (see `MAX_TERMS`).
    terms: Cell<usize>,
}

fn resolve<'a>(document: &'a Document, object: &'a Object) -> Option<&'a Object> {
    match object {
        Object::Reference(id) => document.get_object(*id).ok(),
        object => Some(object),
    }
}

/// The entries of `object`, where it is an array or refers to one; none
/// where it is not.
fn array<'a>(document: &'a Document, object: Option<&'a Object>) -> &'a [Object] {
    object
        .and_then(|object| resolve(document, object))
        .and_then(|object| object.as_array().ok())
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn references(document: &Document, object: Option<&Object>) -> HashSet<ObjectId> {
    array(document, object)
        .iter()
        .filter_map(|entry| entry.as_reference().ok())
        .collect()
}

impl Layers {
    /// The layers of `document`, `None` when it has none.
    pub(crate) fn new(document: &Document) -> Option<Self> {
        let root = document
            .trailer
            .get(b"Root")
            .ok()
            .and_then(|root| resolve(document, root))
            .and_then(|root| root.as_dict().ok())?;
        let properties = root
            .get(b"OCProperties")
            .ok()
            .and_then(|properties| resolve(document, properties))
            .and_then(|properties| properties.as_dict().ok())?;
        let default = properties
            .get(b"D")
            .ok()
            .and_then(|default| resolve(document, default))
            .and_then(|default| default.as_dict().ok());
        let viewing = array(document, properties.get(b"Configs").ok())
            .iter()
            .filter_map(|configuration| {
                resolve(document, configuration)
                    .and_then(|configuration| configuration.as_dict().ok())
            })
            .find(|configuration| for_viewing(document, configuration))
            .map(|configuration| Configuration::of(document, configuration));
        let listed = references(document, properties.get(b"OCGs").ok());
        let (default, automatic) = match default {
            Some(default) => (
                Configuration::of(document, default),
                automatic(document, default.get(b"AS").ok()),
            ),
            None => (Configuration::default(), Some(HashMap::new())),
        };
        Some(Layers {
            default,
            viewing,
            listed,
            automatic,
            shows: RefCell::new(HashMap::new()),
            terms: Cell::new(MAX_TERMS),
        })
    }

    /// Take a layer or a term to read from the document's bound; `None`
    /// once it is spent.
    fn term(&self) -> Option<()> {
        let left = self.terms.get().checked_sub(1)?;
        self.terms.set(left);
        Some(())
    }

    /// The verdict kept for `key`, if any.
    fn kept(&self, key: Key) -> Option<bool> {
        self.shows.borrow().get(&key).copied()
    }

    /// Whether the layer `id` shows, judged once: a layer for design only
    /// always does, and any other unless readers hide it (see `hidden`).
    /// `None` once the bound is spent.
    fn layer_shows(&self, document: &Document, id: ObjectId) -> Option<bool> {
        self.term()?;
        if let Some(shows) = self.kept(Key::Layer(id)) {
            return Some(shows);
        }
        let layer = document.get_dictionary(id).ok();
        let design_only = layer
            .and_then(|layer| layer.get(b"Intent").ok())
            .is_some_and(|intent| match intent {
                Object::Name(name) => name != b"View" && name != b"All",
                Object::Array(names) => !names.iter().any(|name| {
                    name.as_name()
                        .is_ok_and(|name| name == b"View" || name == b"All")
                }),
                _ => false,
            });
        let shows = design_only || !self.hidden(document, id, layer);
        self.shows.borrow_mut().insert(Key::Layer(id), shows);
        Some(shows)
    }

    /// Whether readers hide the layer `id`, whose dictionary is `layer`, on
    /// opening the document. The default configuration turns it on or off;
    /// a viewer following ISO 32000-1 (8.11.4.4) then sets it as its usage
    /// recommends in the categories the configuration's usage application
    /// dictionaries for viewing name for it (see `opened`). PDFium and
    /// pdf.js read no usage application dictionaries: PDFium sets a layer
    /// as its usage recommends for viewing, where it does, whatever the
    /// configuration says, and else shows a layer the document does not
    /// list, and sets any other by the first alternate configuration meant
    /// for viewing, where there is one, in place of the default one; pdf.js
    /// hides a layer that either the default configuration or its usage
    /// turns off. Readers differ where the usage recommends a state for
    /// viewing and nothing applies it, or recommends showing a layer turned
    /// off, and where PDFium's configuration is not the default one; the
    /// layer is taken to be hidden only where all of them hide it.
    fn hidden(&self, document: &Document, id: ObjectId, layer: Option<&Dictionary>) -> bool {
        let set_on = self.default.sets_on(id);
        let usage = layer
            .and_then(|layer| layer.get(b"Usage").ok())
            .and_then(|usage| resolve(document, usage))
            .and_then(|usage| usage.as_dict().ok());
        let opened = match &self.automatic {
            Some(automatic) => match automatic.get(&id) {
                Some(&named) => opened(document, usage, named, set_on),
                None => Some(set_on),
            },
            None => None,
        };
        let shown_by_pdfium =
            recommended(document, usage, b"View", b"ViewState").unwrap_or_else(|| {
                !self.listed.contains(&id)
                    || self.viewing.as_ref().unwrap_or(&self.default).sets_on(id)
            });
        opened == Some(false) && !shown_by_pdfium
    }

    /// Whether a visibility expression shows its content, read `depth`
    /// deep. A term past `MAX_EXPRESSION_DEPTH` is taken to show, and
    /// clears `whole`: the verdict then depends on where the expression was
    /// reached from, and is not kept. `None` once the bound is spent.
    fn expression_shows(
        &self,
        document: &Document,
        expression: &Object,
        depth: usize,
        whole: &mut bool,
    ) -> Option<bool> {
        if depth > MAX_EXPRESSION_DEPTH {
            *whole = false;
            return Some(true);
        }
        self.term()?;
        match expression {
            Object::Reference(id) => match document.get_object(*id) {
                Ok(Object::Array(terms)) => {
                    let key = Key::Object(*id);
                    if let Some(shows) = self.kept(key) {
                        return Some(shows);
                    }
                    let mut own = true;
                    let shows = self.terms_show(document, terms, depth + 1, &mut own)?;
                    if own {
                        self.shows.borrow_mut().insert(key, shows);
                    } else {
                        *whole = false;
                    }
                    Some(shows)
                }
                Ok(_) => self.layer_shows(document, *id),
                Err(_) => Some(true),
            },
            Object::Array(terms) => self.terms_show(document, terms, depth, whole),
            _ => Some(true),
        }
    }

    /// Whether an expression given as its terms, an operator and then its
    /// operands, shows its content (see `expression_shows`).
    fn terms_show(
        &self,
        document: &Document,
        terms: &[Object],
        depth: usize,
        whole: &mut bool,
    ) -> Option<bool> {
        if depth > MAX_EXPRESSION_DEPTH {
            *whole = false;
            return Some(true);
        }
        let Some((operator, operands)) = terms.split_first() else {
            return Some(true);
        };
        let Ok(operator) = operator.as_name() else {
            return Some(true);
        };
        let mut operands = operands.iter();
        // The next operand's verdict, if there is one; `None` once the bound
        // is spent.
        let mut next = |operands: &mut std::slice::Iter<'_, Object>| match operands.next() {
            Some(term) => self
                .expression_shows(document, term, depth + 1, whole)
                .map(Some),
            None => Some(None),
        };
        match operator {
            b"And" => {
                while let Some(shows) = next(&mut operands)? {
                    if !shows {
                        return Some(false);
                    }
                }
                Some(true)
            }
            b"Or" => {
                while let Some(shows) = next(&mut operands)? {
                    if shows {
                        return Some(true);
                    }
                }
                Some(false)
            }
            b"Not" => Some(!next(&mut operands)?.unwrap_or(false)),
            _ => Some(true),
        }
    }

    /// Whether a form or an annotation whose dictionary is `dictionary`, one
    /// the document holds, is hidden by the layer its `/OC` names.
    pub(crate) fn hide(&self, document: &Document, dictionary: &Dictionary) -> bool {
        dictionary
            .get(b"OC")
            .is_ok_and(|properties| self.hides(document, properties))
    }

    /// Whether optional content marked with `properties`, a layer or a
    /// membership dictionary the document holds, is hidden; the verdict is
    /// kept for the next span naming it. Past the bound, content shows.
    pub(crate) fn hides(&self, document: &Document, properties: &Object) -> bool {
        let key = match properties {
            Object::Reference(id) => Key::Object(*id),
            Object::Dictionary(dictionary) => Key::Address(std::ptr::from_ref(dictionary) as usize),
            _ => return false,
        };
        if let Some(shows) = self.kept(key) {
            return !shows;
        }
        let (id, dictionary) = match properties {
            Object::Reference(id) => match document.get_dictionary(*id) {
                Ok(dictionary) => (Some(*id), dictionary),
                Err(_) => return false,
            },
            Object::Dictionary(dictionary) => (None, dictionary),
            _ => return false,
        };
        let mut whole = true;
        match self.membership_shows(document, id, dictionary, &mut whole) {
            Some(shows) => {
                if whole {
                    self.shows.borrow_mut().insert(key, shows);
                }
                !shows
            }
            None => false,
        }
    }

    /// Whether optional content marked with a membership dictionary written
    /// in the content itself is hidden. Its verdict is not kept: the content
    /// is let go once read, and another dictionary may take its address.
    /// Past the bound, content shows.
    pub(crate) fn hides_written(&self, document: &Document, dictionary: &Dictionary) -> bool {
        self.membership_shows(document, None, dictionary, &mut true)
            .is_some_and(|shows| !shows)
    }

    /// Whether the content a layer or a membership dictionary marks shows:
    /// a layer, where the document refers to it as `id`, as it is set; a
    /// membership dictionary as its expression, or else its policy over its
    /// layers, says. `None` once the bound is spent.
    fn membership_shows(
        &self,
        document: &Document,
        id: Option<ObjectId>,
        dictionary: &Dictionary,
        whole: &mut bool,
    ) -> Option<bool> {
        let membership = dictionary
            .get(b"Type")
            .ok()
            .and_then(|kind| kind.as_name().ok())
            .is_some_and(|kind| kind == b"OCMD");
        if !membership {
            return match id {
                Some(id) => self.layer_shows(document, id),
                None => Some(true),
            };
        }
        if let Ok(expression) = dictionary.get(b"VE") {
            return self.expression_shows(document, expression, 0, whole);
        }
        let single;
        let entries: &[Object] = match dictionary.get(b"OCGs") {
            Ok(Object::Reference(id)) => match document.get_object(*id) {
                Ok(Object::Array(array)) => array,
                _ => {
                    single = [Object::Reference(*id)];
                    &single
                }
            },
            Ok(Object::Array(array)) => array,
            _ => &[],
        };
        let mut layers = entries
            .iter()
            .filter_map(|entry| entry.as_reference().ok())
            .peekable();
        if layers.peek().is_none() {
            return Some(true);
        }
        let policy = dictionary
            .get(b"P")
            .ok()
            .and_then(|policy| policy.as_name().ok())
            .unwrap_or(b"AnyOn");
        // A policy asks whether all of the layers, or any, are on, or off;
        // the layers are read until one decides it.
        let (all, on) = match policy {
            b"AllOn" => (true, true),
            b"AnyOff" => (false, false),
            b"AllOff" => (true, false),
            _ => (false, true),
        };
        for layer in layers {
            let asked = self.layer_shows(document, layer)? == on;
            if asked != all {
                return Some(asked);
            }
        }
        Some(all)
    }
}

/// The categories, as bits in the order of `CATEGORIES`, a viewer sets each
/// layer's state from on opening the document, as `applications`, the
/// default configuration's usage application dictionaries, name them: those
/// for the View event naming the layer. `None` where they name more layers
/// and categories than `MAX_TERMS`; a viewer's states are then not known.
fn automatic(document: &Document, applications: Option<&Object>) -> Option<HashMap<ObjectId, u8>> {
    let mut automatic = HashMap::new();
    let mut left = MAX_TERMS;
    for application in array(document, applications) {
        let Some(application) =
            resolve(document, application).and_then(|application| application.as_dict().ok())
        else {
            continue;
        };
        let viewing = application
            .get(b"Event")
            .ok()
            .and_then(|event| event.as_name().ok())
            .is_some_and(|event| event == b"View");
        if !viewing {
            continue;
        }
        let mut named = 0;
        for category in array(document, application.get(b"Category").ok()) {
            left = left.checked_sub(1)?;
            let bit = category
                .as_name()
                .ok()
                .and_then(|category| CATEGORIES.iter().position(|(name, _)| *name == category));
            named |= bit.map_or(0, |bit| 1 << bit);
        }
        if named == 0 {
            continue;
        }
        for layer in array(document, application.get(b"OCGs").ok()) {
            left = left.checked_sub(1)?;
            if let Ok(layer) = layer.as_reference() {
                *automatic.entry(layer).or_insert(0) |= named;
            }
        }
    }
    Some(automatic)
}

/// The state a viewer following ISO 32000-1 sets a layer to on opening the
/// document, where the default configuration turned it on as `set_on` says,
/// its usage dictionary is `usage`, and usage application dictionaries for
/// viewing name the `named` categories for it: off where the usage
/// recommends off in one of them; else not known where it recommends a
/// state the reader decides; else on where it recommends on; else as the
/// configuration set it.
fn opened(
    document: &Document,
    usage: Option<&Dictionary>,
    named: u8,
    set_on: bool,
) -> Option<bool> {
    let mut on = false;
    let mut undecided = false;
    for (bit, (category, entry)) in CATEGORIES.iter().enumerate() {
        if named & (1 << bit) == 0 {
            continue;
        }
        match entry {
            Some(entry) => match recommended(document, usage, category, entry) {
                Some(false) => return Some(false),
                Some(true) => on = true,
                None => {}
            },
            None => undecided |= usage.is_some_and(|usage| usage.has(category)),
        }
    }
    if undecided {
        None
    } else {
        Some(on || set_on)
    }
}

/// Whether PDFium takes an alternate configuration to be meant for viewing:
/// its `/Intent` is `View` or `All`, alone or among others, as a name or a
/// string; a configuration with no intent is not.
fn for_viewing(document: &Document, configuration: &Dictionary) -> bool {
    let viewing = |intent: &Object| match resolve(document, intent) {
        Some(Object::Name(intent) | Object::String(intent, _)) => {
            intent == b"View" || intent == b"All"
        }
        _ => false,
    };
    match configuration
        .get(b"Intent")
        .ok()
        .and_then(|intent| resolve(document, intent))
    {
        Some(Object::Array(intents)) => intents.iter().any(viewing),
        Some(intent) => viewing(intent),
        None => false,
    }
}

/// The state `usage`, a layer's usage dictionary, recommends in `category`
/// by its `entry`: on unless the entry is `/OFF`; `None` where it has none.
fn recommended(
    document: &Document,
    usage: Option<&Dictionary>,
    category: &[u8],
    entry: &[u8],
) -> Option<bool> {
    let state = usage?
        .get(category)
        .ok()
        .and_then(|category| resolve(document, category))?
        .as_dict()
        .ok()?
        .get(entry)
        .ok()?;
    Some(!matches!(resolve(document, state), Some(Object::Name(name)) if name == b"OFF"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::dictionary;

    #[test]
    fn layers_hide_as_the_default_configuration_sets_them() {
        let mut document = Document::with_version("1.7");
        let old = document.add_object(
            dictionary! { "Type" => "OCG", "Name" => Object::string_literal("Superseded") },
        );
        let new = document.add_object(
            dictionary! { "Type" => "OCG", "Name" => Object::string_literal("Current") },
        );
        let design = document.add_object(dictionary! {
            "Type" => "OCG", "Name" => Object::string_literal("Guides"), "Intent" => "Design",
        });
        let either = document.add_object(dictionary! {
            "Type" => "OCMD", "OCGs" => vec![old.into(), new.into()], "P" => "AllOn",
        });
        let not_old = document.add_object(dictionary! {
            "Type" => "OCMD", "VE" => vec!["Not".into(), old.into()],
        });
        let catalog = document.add_object(dictionary! {
            "Type" => "Catalog",
            "OCProperties" => dictionary! {
                "OCGs" => vec![old.into(), new.into(), design.into()],
                "D" => dictionary! { "OFF" => vec![old.into(), design.into()] },
            },
        });
        document.trailer.set("Root", catalog);
        let layers = Layers::new(&document).expect("layers");
        assert!(layers.hides(&document, &old.into()));
        assert!(!layers.hides(&document, &new.into()));
        // A layer for design only has no effect on viewing.
        assert!(!layers.hides(&document, &design.into()));
        assert!(layers.hides(&document, &either.into()));
        assert!(!layers.hides(&document, &not_old.into()));
        // Off unless turned on.
        let mut dark = Document::with_version("1.7");
        let shown = dark.add_object(dictionary! { "Type" => "OCG" });
        let unlisted = dark.add_object(dictionary! { "Type" => "OCG" });
        let catalog = dark.add_object(dictionary! {
            "Type" => "Catalog",
            "OCProperties" => dictionary! {
                "OCGs" => vec![shown.into(), unlisted.into()],
                "D" => dictionary! { "BaseState" => "OFF", "ON" => vec![shown.into()] },
            },
        });
        dark.trailer.set("Root", catalog);
        let layers = Layers::new(&dark).expect("layers");
        assert!(!layers.hides(&dark, &shown.into()));
        assert!(layers.hides(&dark, &unlisted.into()));
        let mut plain = Document::with_version("1.7");
        let catalog = plain.add_object(dictionary! { "Type" => "Catalog" });
        plain.trailer.set("Root", catalog);
        assert!(Layers::new(&plain).is_none());
    }

    #[test]
    fn layers_pdfium_shows_are_not_hidden() {
        // A layer the default configuration turns off, the document listing
        // `listed` layers, with `configurations` besides the default one.
        let layer_in = |listed: bool, configurations: Vec<Object>| {
            let mut document = Document::with_version("1.7");
            let layer = document.add_object(dictionary! { "Type" => "OCG" });
            let other = document.add_object(dictionary! { "Type" => "OCG" });
            let mut ocgs: Vec<Object> = vec![other.into()];
            if listed {
                ocgs.push(layer.into());
            }
            let mut properties = dictionary! {
                "OCGs" => ocgs,
                "D" => dictionary! { "OFF" => vec![layer.into()] },
            };
            if !configurations.is_empty() {
                properties.set("Configs", configurations);
            }
            let catalog = document.add_object(dictionary! {
                "Type" => "Catalog", "OCProperties" => properties,
            });
            document.trailer.set("Root", catalog);
            let layers = Layers::new(&document).expect("layers");
            layers.hides(&document, &layer.into())
        };
        assert!(layer_in(true, Vec::new()));
        // PDFium shows a layer the document does not list.
        assert!(!layer_in(false, Vec::new()));
        // It sets layers by the first alternate configuration meant for
        // viewing, which here turns the layer on; not by one with no intent,
        // or one meant for design, which it passes over.
        let meant = |intent: Object| dictionary! { "Name" => Object::string_literal("Screen"), "Intent" => intent };
        assert!(!layer_in(true, vec![meant("View".into()).into()]));
        assert!(!layer_in(
            true,
            vec![
                meant("Design".into()).into(),
                meant(vec!["Design".into(), "All".into()].into()).into(),
            ]
        ));
        assert!(!layer_in(
            true,
            vec![meant(Object::string_literal("View")).into()]
        ));
        assert!(layer_in(
            true,
            vec![dictionary! { "Name" => Object::string_literal("Other") }.into()]
        ));
        assert!(layer_in(true, vec![meant("Design".into()).into()]));
    }

    /// A document whose default configuration turns `off` off, among
    /// `layers` layers, and the layers.
    fn with_layers(layers: usize, off: &[usize]) -> (Document, Vec<ObjectId>) {
        let mut document = Document::with_version("1.7");
        let ids: Vec<ObjectId> = (0..layers)
            .map(|_| document.add_object(dictionary! { "Type" => "OCG" }))
            .collect();
        let off: Vec<Object> = off.iter().map(|&index| ids[index].into()).collect();
        let catalog = document.add_object(dictionary! {
            "Type" => "Catalog",
            "OCProperties" => dictionary! {
                "OCGs" => ids.iter().map(|&id| id.into()).collect::<Vec<Object>>(),
                "D" => dictionary! { "OFF" => off },
            },
        });
        document.trailer.set("Root", catalog);
        (document, ids)
    }

    #[test]
    fn verdicts_are_kept_and_reading_them_is_bounded() {
        let (mut document, ids) = with_layers(1000, &[999]);
        let all: Vec<Object> = ids.iter().map(|&id| id.into()).collect();
        let all_on = document.add_object(dictionary! {
            "Type" => "OCMD", "OCGs" => all.clone(), "P" => "AllOn",
        });
        let any_off = document.add_object(dictionary! {
            "Type" => "OCMD", "OCGs" => all, "P" => "AnyOff",
        });
        // A membership dictionary held in place, as resources hold it.
        let held = Object::Dictionary(dictionary! {
            "Type" => "OCMD", "OCGs" => vec![ids[999].into()],
        });
        let layers = Layers::new(&document).expect("layers");
        assert!(layers.hides(&document, &all_on.into()));
        assert!(layers.hides(&document, &held));
        let spent = MAX_TERMS - layers.terms.get();
        assert!(spent > 1000);
        // Named again, a dictionary's verdict costs nothing to read.
        for _ in 0..100_000 {
            assert!(layers.hides(&document, &all_on.into()));
            assert!(layers.hides(&document, &held));
        }
        assert_eq!(MAX_TERMS - layers.terms.get(), spent);
        // Past the bound, content whose verdict is not kept shows; a kept
        // verdict stands.
        layers.terms.set(0);
        assert!(!layers.hides(&document, &any_off.into()));
        assert!(layers.hides(&document, &all_on.into()));
    }

    /// `levels` expressions over `inner`, each an `operator` naming the
    /// one below `times` times, the outermost last.
    fn wrapped(
        document: &mut Document,
        inner: ObjectId,
        levels: usize,
        operator: &str,
        times: usize,
    ) -> Vec<ObjectId> {
        let mut wrappers = vec![inner];
        for _ in 0..levels {
            let below = *wrappers.last().expect("an expression");
            let mut terms: Vec<Object> = vec![operator.into()];
            terms.extend(std::iter::repeat_n(Object::from(below), times));
            wrappers.push(document.add_object(terms));
        }
        wrappers
    }

    #[test]
    fn expressions_are_read_once_each_and_whole() {
        let (mut document, ids) = with_layers(2, &[1]);
        // Fifteen expressions, each naming the one below twice, as deep as
        // an expression is read: term by term, the innermost would be read
        // 32,768 times.
        let inner = document.add_object(vec!["Or".into(), ids[0].into(), ids[1].into()]);
        let shared = wrapped(&mut document, inner, 15, "And", 2);
        let shared = document.add_object(dictionary! {
            "Type" => "OCMD", "VE" => *shared.last().expect("an expression"),
        });
        let layers = Layers::new(&document).expect("layers");
        assert!(!layers.hides(&document, &shared.into()));
        assert!(MAX_TERMS - layers.terms.get() < 1_000);
        // An expression reached past the depth read is taken to show, and a
        // verdict reached so, which depends on the way there, is not kept:
        // the expression eight levels up reads its layer from near.
        let (mut document, ids) = with_layers(1, &[0]);
        let hidden = document.add_object(vec!["And".into(), ids[0].into()]);
        let levels = wrapped(&mut document, hidden, 16, "And", 1);
        let far = document.add_object(dictionary! { "Type" => "OCMD", "VE" => levels[16] });
        let near = document.add_object(dictionary! { "Type" => "OCMD", "VE" => levels[8] });
        let layers = Layers::new(&document).expect("layers");
        assert!(!layers.hides(&document, &far.into()));
        assert!(layers.hides(&document, &near.into()));
        // Written in the content, a dictionary is read each time.
        let written = dictionary! { "Type" => "OCMD", "VE" => hidden };
        assert!(layers.hides_written(&document, &written));
        assert!(layers.hides_written(&document, &written));
    }

    #[test]
    fn layers_hide_as_readers_set_them_on_opening() {
        let mut document = Document::with_version("1.7");
        let viewed = |state: &str| dictionary! { "View" => dictionary! { "ViewState" => state } };
        let zoomed = dictionary! { "Zoom" => dictionary! { "min" => 2 } };
        let mut zoomed_off = zoomed.clone();
        zoomed_off.extend(&viewed("OFF"));
        let usages = [
            Some(viewed("OFF")),
            Some(viewed("OFF")),
            Some(viewed("ON")),
            Some(viewed("ON")),
            Some(zoomed),
            Some(zoomed_off),
            Some(viewed("OFF")),
            None,
        ];
        let ids: Vec<ObjectId> = usages
            .into_iter()
            .map(|usage| {
                let mut layer = dictionary! { "Type" => "OCG" };
                if let Some(usage) = usage {
                    layer.set("Usage", usage);
                }
                document.add_object(layer)
            })
            .collect();
        let [viewed_off, unapplied_off, viewed_on, unapplied_on, zoomed, zoomed_off, printed, off] =
            ids[..]
        else {
            unreachable!("eight layers");
        };
        let application = |event: &str, category: &str, layers: &[ObjectId]| -> Object {
            dictionary! {
                "Event" => event,
                "Category" => vec![category.into()],
                "OCGs" => layers.iter().map(|&id| id.into()).collect::<Vec<Object>>(),
            }
            .into()
        };
        let catalog = document.add_object(dictionary! {
            "Type" => "Catalog",
            "OCProperties" => dictionary! {
                "OCGs" => ids.iter().map(|&id| id.into()).collect::<Vec<Object>>(),
                "D" => dictionary! {
                    "OFF" => vec![viewed_on.into(), unapplied_on.into(), zoomed.into(), off.into()],
                    "AS" => vec![
                        application("View", "View", &[viewed_off, viewed_on]),
                        application("View", "Zoom", &[zoomed, zoomed_off]),
                        application("View", "View", &[zoomed_off]),
                        application("Print", "View", &[printed]),
                    ],
                },
            },
        });
        document.trailer.set("Root", catalog);
        let layers = Layers::new(&document).expect("layers");
        let hides = |id: ObjectId| layers.hides(&document, &id.into());
        // On opening, a viewer sets a layer as its usage recommends for
        // viewing where the configuration applies that: hidden where it
        // recommends hiding, as PDFium and pdf.js hide it too; shown where
        // it recommends showing a layer turned off, as PDFium shows it too.
        assert!(hides(viewed_off));
        assert!(!hides(viewed_on));
        // Where nothing applies its usage for viewing, a viewer sets the
        // layer as the configuration does: one turned on shows, though
        // PDFium and pdf.js hide it by its usage; one turned off is hidden,
        // unless PDFium shows it by its usage.
        assert!(!hides(unapplied_off));
        assert!(hides(off));
        assert!(!hides(unapplied_on));
        // A recommendation applied on printing is not one for viewing.
        assert!(!hides(printed));
        // The magnification a layer wants may be the reader's; where another
        // category recommends hiding the layer, it is hidden all the same.
        assert!(!hides(zoomed));
        assert!(hides(zoomed_off));
        // Named past the bound, the states a viewer sets are not known, and
        // a layer turned off shows.
        for (applications, hides) in [(1, true), (1_001, false)] {
            let mut crowded = Document::with_version("1.7");
            let ids: Vec<ObjectId> = (0..1_000)
                .map(|_| crowded.add_object(dictionary! { "Type" => "OCG" }))
                .collect();
            let named =
                crowded.add_object(ids.iter().map(|&id| id.into()).collect::<Vec<Object>>());
            let application = crowded.add_object(dictionary! {
                "Event" => "View", "Category" => vec!["View".into()], "OCGs" => named,
            });
            let catalog = crowded.add_object(dictionary! {
                "Type" => "Catalog",
                "OCProperties" => dictionary! {
                    "OCGs" => named,
                    "D" => dictionary! {
                        "OFF" => vec![ids[0].into()],
                        "AS" => vec![Object::from(application); applications],
                    },
                },
            });
            crowded.trailer.set("Root", catalog);
            let layers = Layers::new(&crowded).expect("layers");
            assert_eq!(layers.hides(&crowded, &ids[0].into()), hides);
        }
    }
}
