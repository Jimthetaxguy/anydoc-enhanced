//! Composite fonts whose text pdf-inspector 1.24.0 reads without their
//! character collection's map (upstream issue #573).
//!
//! A font keyed by CID in one of Adobe's character collections, shown under
//! `Identity-H` or `Identity-V` with no `/ToUnicode` map and no embedded
//! program to read a map from, is read through its collection's map.
//! pdf-inspector bundles those maps, but cannot parse the Japanese and
//! Chinese ones (the Korean one it keeps as a table of its own), and then
//! reads the codes as the bytes they are made of. A string with a byte past
//! 0x7F reads as U+FFFD, which marks the page garbled; any other reads as
//! other letters, "Total" as "5PUBM", with its digits and punctuation
//! dropped as control codes, and nothing marks it.

use lopdf::{Dictionary, Document, Object};

/// The collections whose maps pdf-inspector 1.24.0 cannot parse.
const UNREAD_COLLECTIONS: [&[u8]; 3] = [b"Japan1", b"GB1", b"CNS1"];

/// The CIDs every Adobe collection gives the printable ASCII characters:
/// CID 1 is the space, and each CID after it the next character.
const ASCII_CIDS: std::ops::RangeInclusive<u16> = 1..=95;

fn resolved<'a>(document: &'a Document, object: &'a Object) -> Option<&'a Object> {
    match object {
        Object::Reference(id) => document.get_object(*id).ok(),
        other => Some(other),
    }
}

fn dictionary<'a>(document: &'a Document, object: &'a Object) -> Option<&'a Dictionary> {
    resolved(document, object)?.as_dict().ok()
}

/// Whether pdf-inspector 1.24.0 reads `font`'s codes without its
/// collection's map: a composite font under `Identity-H` or `Identity-V`
/// with no `/ToUnicode` map, whose descendant is in a collection whose map
/// it cannot parse, and which embeds no TrueType or OpenType program, whose
/// own map pdf-inspector would read instead.
pub(crate) fn unmapped(document: &Document, font: &Dictionary) -> bool {
    let named = |key: &[u8], names: &[&[u8]]| {
        font.get(key)
            .and_then(Object::as_name)
            .is_ok_and(|name| names.contains(&name))
    };
    if !named(b"Subtype", &[b"Type0"])
        || font.has(b"ToUnicode")
        || !named(b"Encoding", &[b"Identity-H", b"Identity-V"])
    {
        return false;
    }
    let Some(descendant) = font
        .get(b"DescendantFonts")
        .ok()
        .and_then(|fonts| resolved(document, fonts))
        .and_then(|fonts| fonts.as_array().ok())
        .and_then(|fonts| fonts.first())
        .and_then(|first| dictionary(document, first))
    else {
        return false;
    };
    let unread = descendant
        .get(b"CIDSystemInfo")
        .ok()
        .and_then(|info| dictionary(document, info))
        .and_then(|info| info.get(b"Ordering").ok())
        .is_some_and(|ordering| match ordering {
            Object::String(ordering, _) => UNREAD_COLLECTIONS.contains(&ordering.as_slice()),
            _ => false,
        });
    let program = descendant
        .get(b"FontDescriptor")
        .ok()
        .and_then(|descriptor| dictionary(document, descriptor))
        .is_some_and(|descriptor| {
            descriptor.has(b"FontFile2")
                || descriptor
                    .get(b"FontFile3")
                    .ok()
                    .and_then(|file| resolved(document, file))
                    .and_then(|file| file.as_stream().ok())
                    .is_some_and(|file| {
                        file.dict
                            .get(b"Subtype")
                            .and_then(Object::as_name)
                            .is_ok_and(|subtype| subtype == b"OpenType")
                    })
        });
    unread && !program
}

/// What a string shown in such a font says where pdf-inspector reads it
/// with no sign, none of its bytes past 0x7F: its codes the collection
/// gives letters, digits, or the marks amounts and dates are written with,
/// and a space for any other code. None for a string with a byte past 0x7F
/// or an odd byte.
pub(crate) fn silent_reading(bytes: &[u8]) -> Option<String> {
    if !bytes.len().is_multiple_of(2) || bytes.iter().any(|&byte| byte > 0x7F) {
        return None;
    }
    Some(
        bytes
            .chunks_exact(2)
            .map(|code| {
                let cid = u16::from_be_bytes([code[0], code[1]]);
                ASCII_CIDS
                    .contains(&cid)
                    .then(|| char::from(cid as u8 + 0x1F))
                    .filter(|character| {
                        character.is_ascii_alphanumeric() || ",.-/:()".contains(*character)
                    })
                    .unwrap_or(' ')
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{dictionary, Stream};

    fn document_with(font: impl FnOnce(&mut Document) -> Dictionary) -> (Document, Dictionary) {
        let mut document = Document::with_version("1.7");
        let font = font(&mut document);
        (document, font)
    }

    /// A descendant font in `ordering`, embedding `program` under its key,
    /// if any.
    fn cid_font(
        document: &mut Document,
        ordering: &str,
        program: Option<(&str, Dictionary)>,
    ) -> Object {
        let mut descriptor = dictionary! {
            "Type" => "FontDescriptor",
            "FontName" => "KozMinPr6N-Regular",
        };
        if let Some((key, dictionary)) = program {
            let program = document.add_object(Stream::new(dictionary, Vec::new()));
            descriptor.set(key, program);
        }
        let descriptor = document.add_object(descriptor);
        let descendant = document.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "CIDFontType0",
            "BaseFont" => "KozMinPr6N-Regular",
            "CIDSystemInfo" => dictionary! {
                "Registry" => Object::string_literal("Adobe"),
                "Ordering" => Object::string_literal(ordering),
                "Supplement" => 6,
            },
            "FontDescriptor" => descriptor,
        });
        Object::Array(vec![Object::Reference(descendant)])
    }

    fn type0(descendants: Object, encoding: &str) -> Dictionary {
        dictionary! {
            "Type" => "Font",
            "Subtype" => "Type0",
            "BaseFont" => "KozMinPr6N-Regular",
            "Encoding" => Object::Name(encoding.as_bytes().to_vec()),
            "DescendantFonts" => descendants,
        }
    }

    #[test]
    fn fonts_of_the_japanese_and_chinese_collections_without_a_map_are_unmapped() {
        for ordering in ["Japan1", "GB1", "CNS1"] {
            for encoding in ["Identity-H", "Identity-V"] {
                let (document, font) =
                    document_with(|document| type0(cid_font(document, ordering, None), encoding));
                assert!(unmapped(&document, &font), "{ordering} {encoding}");
            }
        }
        // A bare CFF program has no map of its own.
        let (document, font) = document_with(|document| {
            let program = ("FontFile3", dictionary! { "Subtype" => "CIDFontType0C" });
            type0(cid_font(document, "Japan1", Some(program)), "Identity-H")
        });
        assert!(unmapped(&document, &font));
    }

    #[test]
    fn fonts_read_through_a_map_are_not_unmapped() {
        // Korean, which pdf-inspector keeps a table for.
        let (document, font) =
            document_with(|document| type0(cid_font(document, "Korea1", None), "Identity-H"));
        assert!(!unmapped(&document, &font));
        // A ToUnicode map.
        let (document, mut font) =
            document_with(|document| type0(cid_font(document, "Japan1", None), "Identity-H"));
        font.set("ToUnicode", Object::Reference((99, 0)));
        assert!(!unmapped(&document, &font));
        // A predefined CMap whose codes are Unicode.
        let (document, font) =
            document_with(|document| type0(cid_font(document, "Japan1", None), "UniJIS-UCS2-H"));
        assert!(!unmapped(&document, &font));
        // An embedded TrueType or OpenType program, read for its own map.
        let (document, font) = document_with(|document| {
            let program = ("FontFile2", dictionary! {});
            type0(cid_font(document, "GB1", Some(program)), "Identity-H")
        });
        assert!(!unmapped(&document, &font));
        let (document, font) = document_with(|document| {
            let program = ("FontFile3", dictionary! { "Subtype" => "OpenType" });
            type0(cid_font(document, "CNS1", Some(program)), "Identity-H")
        });
        assert!(!unmapped(&document, &font));
        // A simple font.
        let (document, font) = document_with(|_| {
            dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica" }
        });
        assert!(!unmapped(&document, &font));
    }

    #[test]
    fn strings_read_with_no_sign_say_their_ascii_codes() {
        // "Total 52,000.00" in CIDs 1-95.
        let cids: Vec<u8> = "Total 52,000.00"
            .bytes()
            .flat_map(|character| u16::from(character - 0x1F).to_be_bytes())
            .collect();
        assert_eq!(silent_reading(&cids).as_deref(), Some("Total 52,000.00"));
        // HIRAGANA LETTER A in Adobe-Japan1, beside a dollar sign whose glyph
        // differs among the collections.
        assert_eq!(
            silent_reading(&[0x03, 0x4B, 0x00, 0x05, 0x00, 0x16]).as_deref(),
            Some("  5")
        );
        // A byte past 0x7F reads as U+FFFD, which pdf-inspector marks.
        assert_eq!(silent_reading(&[0x04, 0x9F, 0x00, 0x16]), None);
        assert_eq!(silent_reading(&[0x00]), None);
    }
}
