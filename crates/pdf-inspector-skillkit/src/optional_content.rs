//! Layers a reader hides, whose text pdf-inspector 1.24.0 reads anyway.
//!
//! A PDF can set content in optional layers and hide some of them by
//! default: a superseded figure kept beside the current one, a draft stamp,
//! text meant only for print. A reader shows the page as the document's
//! default configuration sets its layers; pdf-inspector reads no layer
//! settings, so it reads every layer as shown, and the Markdown holds text
//! the page does not show, "Ending balance 1,000.00 superseded" beside the
//! "Ending balance 2,000.00" a reader sees. The layers are read from the
//! default configuration: its base state, and the layers it turns on or
//! off; a membership dictionary shows its content as its policy or
//! visibility expression says, and a layer meant for design only has no
//! effect on viewing. Content is hidden where a marked-content span, a
//! form, or an annotation names a layer so hidden.

use std::collections::HashSet;

use lopdf::{Dictionary, Document, Object, ObjectId};

/// How deep a visibility expression is read; deeper, its content shows.
const MAX_EXPRESSION_DEPTH: usize = 32;

/// The document's layers, as its default configuration sets them.
#[derive(Debug, Default)]
pub(crate) struct Layers {
    /// Whether layers are off unless turned on.
    base_off: bool,
    on: HashSet<ObjectId>,
    off: HashSet<ObjectId>,
}

fn resolve<'a>(document: &'a Document, object: &'a Object) -> Option<&'a Object> {
    match object {
        Object::Reference(id) => document.get_object(*id).ok(),
        object => Some(object),
    }
}

fn references(document: &Document, object: Option<&Object>) -> HashSet<ObjectId> {
    object
        .and_then(|object| resolve(document, object))
        .and_then(|object| object.as_array().ok())
        .map(|array| {
            array
                .iter()
                .filter_map(|entry| entry.as_reference().ok())
                .collect()
        })
        .unwrap_or_default()
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
        let Some(default) = default else {
            return Some(Layers::default());
        };
        Some(Layers {
            base_off: default
                .get(b"BaseState")
                .ok()
                .and_then(|state| state.as_name().ok())
                .is_some_and(|state| state == b"OFF"),
            on: references(document, default.get(b"ON").ok()),
            off: references(document, default.get(b"OFF").ok()),
        })
    }

    /// Whether the layer `id` shows: a layer for design only always does.
    fn layer_shows(&self, document: &Document, id: ObjectId) -> bool {
        let design_only = document
            .get_dictionary(id)
            .ok()
            .and_then(|layer| layer.get(b"Intent").ok())
            .is_some_and(|intent| match intent {
                Object::Name(name) => name != b"View" && name != b"All",
                Object::Array(names) => !names.iter().any(|name| {
                    name.as_name()
                        .is_ok_and(|name| name == b"View" || name == b"All")
                }),
                _ => false,
            });
        if design_only {
            return true;
        }
        if self.base_off {
            self.on.contains(&id)
        } else {
            !self.off.contains(&id)
        }
    }

    /// Whether a visibility expression shows its content.
    fn expression_shows(&self, document: &Document, expression: &Object, depth: usize) -> bool {
        if depth > MAX_EXPRESSION_DEPTH {
            return true;
        }
        match expression {
            Object::Reference(id) => match document.get_object(*id) {
                Ok(Object::Array(array)) => {
                    self.expression_shows(document, &Object::Array(array.clone()), depth + 1)
                }
                Ok(_) => self.layer_shows(document, *id),
                Err(_) => true,
            },
            Object::Array(terms) => {
                let Some(operator) = terms.first().and_then(|operator| operator.as_name().ok())
                else {
                    return true;
                };
                let mut operands = terms[1..]
                    .iter()
                    .map(|term| self.expression_shows(document, term, depth + 1));
                match operator {
                    b"And" => operands.all(|shows| shows),
                    b"Or" => operands.any(|shows| shows),
                    b"Not" => !operands.next().unwrap_or(false),
                    _ => true,
                }
            }
            _ => true,
        }
    }

    /// Whether a form or an annotation whose dictionary is `dictionary` is
    /// hidden by the layer its `/OC` names.
    pub(crate) fn hide(&self, document: &Document, dictionary: &Dictionary) -> bool {
        dictionary
            .get(b"OC")
            .is_ok_and(|properties| self.hides(document, properties))
    }

    /// Whether optional content marked with `properties`, a layer or a
    /// membership dictionary, is hidden.
    pub(crate) fn hides(&self, document: &Document, properties: &Object) -> bool {
        let (id, dictionary): (Option<ObjectId>, Option<&Dictionary>) = match properties {
            Object::Reference(id) => (Some(*id), document.get_dictionary(*id).ok()),
            Object::Dictionary(dictionary) => (None, Some(dictionary)),
            _ => (None, None),
        };
        let Some(dictionary) = dictionary else {
            return false;
        };
        let membership = dictionary
            .get(b"Type")
            .ok()
            .and_then(|kind| kind.as_name().ok())
            .is_some_and(|kind| kind == b"OCMD");
        if !membership {
            return id.is_some_and(|id| !self.layer_shows(document, id));
        }
        if let Ok(expression) = dictionary.get(b"VE") {
            return !self.expression_shows(document, expression, 0);
        }
        let layers: Vec<bool> = match dictionary.get(b"OCGs") {
            Ok(Object::Reference(id)) => match document.get_object(*id) {
                Ok(Object::Array(array)) => array
                    .iter()
                    .filter_map(|entry| entry.as_reference().ok())
                    .map(|id| self.layer_shows(document, id))
                    .collect(),
                _ => vec![self.layer_shows(document, *id)],
            },
            Ok(Object::Array(array)) => array
                .iter()
                .filter_map(|entry| entry.as_reference().ok())
                .map(|id| self.layer_shows(document, id))
                .collect(),
            _ => Vec::new(),
        };
        if layers.is_empty() {
            return false;
        }
        let policy = dictionary
            .get(b"P")
            .ok()
            .and_then(|policy| policy.as_name().ok())
            .unwrap_or(b"AnyOn");
        let shows = match policy {
            b"AllOn" => layers.iter().all(|shows| *shows),
            b"AnyOff" => layers.iter().any(|shows| !shows),
            b"AllOff" => layers.iter().all(|shows| !shows),
            _ => layers.iter().any(|shows| *shows),
        };
        !shows
    }
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
}
