//! Composite fonts of Adobe's Japanese, Chinese, and Korean collections,
//! and of embedded programs, whose text pdf-inspector 1.25.0 reads with no
//! map (upstream issue #573).
//!
//! A composite font with no `/ToUnicode` map, under `Identity-H` or
//! `Identity-V`, is read by the map of its embedded TrueType or OpenType
//! program, else by its collection's map. pdf-inspector bundles those maps
//! but cannot parse the Japanese and Chinese ones (it keeps the Korean one
//! as a table of its own), and never looks for one where the font names a
//! `/ToUnicode` that is no map, gives its encoding other than as a name in
//! place, or sets its descendant in place with no program, nor for a font
//! it does not collect from a page's resources or a form's given in place,
//! whose program it never reads. With no map, it reads a string with a
//! byte past 0x7F as U+FFFD, which marks the page garbled, and any other as
//! its bytes: "Total" as "5PUBM", with its digits and punctuation dropped
//! as control codes, and nothing marks it; so it reads a font of another
//! ordering, such as Identity, whose codes are its program's glyphs, where
//! it does not collect it. Where the font's widths are given mostly past
//! code 0x41, it takes the codes for Unicode, as Chromium's fonts' are,
//! and reads every one as the character of its value: "一壱溢" as "ҰұҲ".
//! A font under any other predefined CMap with no map is read byte by byte
//! too, as lopdf names the CMap but cannot decode it: kanji under
//! `UniJIS-UCS2-H` or `UniJIS-UTF16-H` as the bytes of their UTF-16 codes
//! ("住民税" as "OOlz"), and under `H` or `GB-H` as the bytes of their
//! two-byte codes; but Chinese under `UniGB-UCS2-H` and `UniGB-UTF16-H`,
//! which lopdf reads as UTF-16, and ASCII under a CMap whose single bytes
//! are ASCII, as the RKSJ and EUC ones', read as they say.

use std::collections::{HashMap, HashSet};

use lopdf::{Dictionary, Document, Encoding, Object, ObjectId, Stream};
use pdf_inspector::tounicode::{build_cmap_from_truetype, ToUnicodeCMap};

/// Adobe's collections for Japanese, Simplified and Traditional Chinese,
/// and Korean.
const COLLECTIONS: [&[u8]; 4] = [b"Japan1", b"GB1", b"CNS1", b"Korea1"];
/// The one pdf-inspector keeps a table of its own for.
const TABLED_COLLECTION: &[u8] = b"Korea1";

/// The CIDs every Adobe collection gives the printable ASCII characters:
/// CID 1 is the space, and each CID after it the next character.
const ASCII_CIDS: std::ops::RangeInclusive<u16> = 1..=95;

/// Bytes of maps and font programs read per document to tell whether
/// pdf-inspector finds a map; past them, a font is not judged (see
/// `Unmapped::judged`), but where its map or program is small.
const MAX_READ_BYTES: usize = 256 << 20;
/// Bytes one map or program may decode to; past them, its font is not
/// judged.
const MAX_STREAM_BYTES: usize = 64 << 20;
/// Bytes a map or program decodes to at most to be read past
/// `MAX_READ_BYTES`, up to `MAX_SMALL_READ_BYTES` more in all, as a
/// subset's program is, however large the programs read before it.
const MAX_SMALL_STREAM_BYTES: usize = 1 << 20;
const MAX_SMALL_READ_BYTES: usize = 64 << 20;
/// Distinct CIDs of a font's widths pdf-inspector reads, at most.
const MAX_WIDTH_CIDS: usize = 65_536;

/// A font pdf-inspector 1.25.0 reads with no map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Unmapped {
    /// Whether it takes the codes for Unicode, and reads each one as the
    /// character of its value; else a string with a byte past 0x7F reads
    /// as U+FFFD, and any other as its bytes.
    pub(crate) passthrough: bool,
    /// What the codes are, as far as they tell what a string says.
    pub(crate) codes: Codes,
    /// Whether it reads a byte by the standard encoding, which lopdf gives
    /// a font under an Identity CMap it has no map for, or an encoding it
    /// cannot read; else as the byte it is, as for a predefined CMap lopdf
    /// names but cannot read.
    pub(crate) standard: bool,
    /// Whether its text is judged by what it says: where it is told to
    /// have no map, or where its codes are Unicode, whose reading the
    /// Markdown shows if pdf-inspector finds a map after all. Else it may
    /// have one, as its map or program was past the bytes read
    /// (`MAX_STREAM_BYTES`, `MAX_READ_BYTES`), and its text is looked for
    /// only as pdf-inspector reads it with none, which is never what a
    /// collection's CIDs say.
    pub(crate) judged: bool,
}

/// What a font's codes are, as far as they tell what a string says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Codes {
    /// The CIDs of its collection, two bytes each.
    Cids,
    /// Unicode, as a predefined `Uni*-UCS2` or `Uni*-UTF16` CMap writes
    /// it: UTF-16, two bytes a character, or four.
    Utf16,
    /// Unicode as a `Uni*-UTF32` CMap writes it, four bytes a character.
    Utf32,
    /// Another predefined CMap's, which the check does not read: two bytes
    /// each from 0x21 to 0x7E, as `H` or `GB-H` has them, or single bytes
    /// for kana or symbols.
    Other,
    /// The glyphs of an embedded program, as the CIDs of another ordering
    /// than Adobe's four, such as Identity, are, which its own map, and no
    /// collection's, says the characters of.
    Glyphs,
}

/// How pdf-inspector reads a font under a CMap it names, in place or by
/// reference, when it finds no map for the font, by the CMap's name.
enum Named {
    /// `Identity-H` or `Identity-V`: codes are CIDs.
    Identity,
    /// As the text says, or marked: lopdf reads `UniGB-UCS2-H` and
    /// `UniGB-UTF16-H` as UTF-16, and a CMap whose single bytes 0x00 to
    /// 0x7F are ASCII (the RKSJ, EUC, Big Five, GBK, UHC, Johab, and UTF-8
    /// ones, `Hankaku`, and `Roman`) reads an ASCII string as it is, while
    /// pdf-inspector marks any string with a byte past 0x7F.
    Read,
    /// Byte by byte, whatever its codes are.
    Bytes(Codes),
    /// A name no predefined CMap has, which a viewer cannot read either.
    Unknown,
}

/// How pdf-inspector reads a font under the CMap `name` with no map for it
/// (see `Named`). lopdf gives any other predefined CMap an encoding it
/// cannot decode, so pdf-inspector reads its strings byte by byte.
fn named_cmap(name: &[u8]) -> Named {
    /// The predefined CMaps whose codes are two bytes each from 0x21 to
    /// 0x7E, and those whose single bytes name kana or symbols.
    const OTHER: [&[u8]; 23] = [
        b"H",
        b"V",
        b"78-H",
        b"78-V",
        b"Add-H",
        b"Add-V",
        b"Ext-H",
        b"Ext-V",
        b"NWP-H",
        b"NWP-V",
        b"GB-H",
        b"GB-V",
        b"GBT-H",
        b"GBT-V",
        b"CNS1-H",
        b"CNS1-V",
        b"CNS2-H",
        b"CNS2-V",
        b"KSC-H",
        b"KSC-V",
        b"Hiragana",
        b"Katakana",
        b"WP-Symbol",
    ];
    let has = |part: &[u8]| name.windows(part.len()).any(|window| window == part);
    let unicode = name.starts_with(b"Uni") && (name.ends_with(b"-H") || name.ends_with(b"-V"));
    match name {
        b"Identity-H" | b"Identity-V" => Named::Identity,
        b"UniGB-UCS2-H" | b"UniGB-UTF16-H" => Named::Read,
        _ if utf16_cmap(name) => Named::Bytes(Codes::Utf16),
        _ if unicode && has(b"-UTF32-") => Named::Bytes(Codes::Utf32),
        _ if unicode && has(b"-UTF8-") => Named::Read,
        _ if OTHER.contains(&name) => Named::Bytes(Codes::Other),
        _ if ["RKSJ", "EUC", "B5", "GBK", "UHC", "Johab"]
            .iter()
            .any(|family| has(family.as_bytes()))
            || matches!(name, b"Hankaku" | b"Roman") =>
        {
            Named::Read
        }
        _ => Named::Unknown,
    }
}

/// Fonts judged so far, by the address of their dictionary; the programs
/// read to judge them, by object, whether each gives a map, where it was
/// read; the bytes read; and the keys pdf-inspector files maps under (see
/// `collected_keys`), once read.
#[derive(Default)]
pub(crate) struct CjkFonts {
    known: HashMap<usize, Option<Unmapped>>,
    programs: HashMap<ObjectId, Option<bool>>,
    read: usize,
    collected: Option<HashSet<u32>>,
}

impl CjkFonts {
    /// How pdf-inspector reads `font`, when it is a font of Adobe's
    /// Japanese, Chinese, or Korean collections, or of an embedded
    /// program's glyphs, it finds no map for.
    pub(crate) fn font(&mut self, document: &Document, font: &Dictionary) -> Option<Unmapped> {
        let key = std::ptr::from_ref(font) as usize;
        if let Some(known) = self.known.get(&key) {
            return *known;
        }
        let found = unmapped(document, font, self);
        self.known.insert(key, found);
        found
    }

    /// Whether the program `file` gives pdf-inspector a map (see
    /// `program_maps`), read once however many fonts embed it.
    fn program_maps(&mut self, document: &Document, file: ObjectId) -> Option<bool> {
        if let Some(maps) = self.programs.get(&file) {
            return *maps;
        }
        let maps = program_maps(document, file, &mut self.read);
        self.programs.insert(file, maps);
        maps
    }

    /// Whether pdf-inspector files a map under `key` (see `collected_keys`).
    fn collected(&mut self, document: &Document, key: u32) -> bool {
        self.collected
            .get_or_insert_with(|| collected_keys(document))
            .contains(&key)
    }
}

/// The keys pdf-inspector files maps under, as `FontCMaps::from_doc`
/// collects fonts: those of each page's resources, its own and those it
/// inherits, the first of each name; and those of the forms these
/// resources name, and the forms theirs name, where a form gives its
/// `/Resources` in place. A font's map, or the fallback it builds from the
/// font's program, is filed under the object number of its `/ToUnicode`
/// given by reference; a font with no `/ToUnicode` has its program's map
/// filed as `collection_key` says. A font it does not collect finds a map
/// only where one it does is filed under the same key.
pub(crate) fn collected_keys(document: &Document) -> HashSet<u32> {
    fn in_place_or_by_reference<'a>(
        document: &'a Document,
        object: &'a Object,
    ) -> Option<&'a Dictionary> {
        match object {
            Object::Reference(id) => document.get_dictionary(*id).ok(),
            Object::Dictionary(dictionary) => Some(dictionary),
            _ => None,
        }
    }
    let mut keys = HashSet::new();
    let mut visited = HashSet::new();
    for page in document.get_pages().into_values() {
        if let Ok(fonts) = document.get_page_fonts(page) {
            keys.extend(
                fonts
                    .values()
                    .filter_map(|font| collection_key(document, font)),
            );
        }
        let Ok((own, inherited)) = document.get_page_resources(page) else {
            continue;
        };
        let mut pending: Vec<&Dictionary> = own
            .into_iter()
            .chain(
                inherited
                    .into_iter()
                    .filter_map(|id| document.get_dictionary(id).ok()),
            )
            .collect();
        while let Some(resources) = pending.pop() {
            let xobjects = match resources.get(b"XObject") {
                Ok(Object::Reference(id)) => {
                    document.get_object(*id).and_then(Object::as_dict).ok()
                }
                Ok(Object::Dictionary(xobjects)) => Some(xobjects),
                _ => None,
            };
            for (_, xobject) in xobjects.into_iter().flat_map(Dictionary::iter) {
                let Object::Reference(id) = xobject else {
                    continue;
                };
                if !visited.insert(*id) {
                    continue;
                }
                let Ok(form) = document.get_object(*id).and_then(Object::as_stream) else {
                    continue;
                };
                if !form
                    .dict
                    .get(b"Subtype")
                    .and_then(Object::as_name)
                    .is_ok_and(|subtype| subtype == b"Form")
                {
                    continue;
                }
                let Ok(resources) = form.dict.get(b"Resources").and_then(Object::as_dict) else {
                    continue;
                };
                let fonts = match resources.get(b"Font") {
                    Ok(Object::Reference(id)) => {
                        document.get_object(*id).and_then(Object::as_dict).ok()
                    }
                    Ok(Object::Dictionary(fonts)) => Some(fonts),
                    _ => None,
                };
                keys.extend(
                    fonts
                        .into_iter()
                        .flat_map(Dictionary::iter)
                        .filter_map(|(_, font)| in_place_or_by_reference(document, font))
                        .filter_map(|font| collection_key(document, font)),
                );
                pending.push(resources);
            }
        }
    }
    keys
}

/// The first of a font's descendants, and whether it is given by
/// reference.
fn first_descendant<'a>(document: &'a Document, font: &'a Dictionary) -> Option<&'a Object> {
    let descendants = match font.get(b"DescendantFonts").ok()? {
        Object::Array(descendants) => descendants,
        Object::Reference(id) => document.get_object(*id).ok()?.as_array().ok()?,
        _ => return None,
    };
    descendants.first()
}

/// Whether a font's encoding is an Identity CMap named in place.
fn named_identity(font: &Dictionary) -> bool {
    font.get(b"Encoding")
        .and_then(Object::as_name)
        .is_ok_and(|name| matches!(name, b"Identity-H" | b"Identity-V"))
}

/// The key pdf-inspector files a map of `font` under as it collects it:
/// the object number of its `/ToUnicode` given by reference; with no
/// `/ToUnicode`, under an Identity CMap named in place, its descendant's
/// program given by reference, else the descendant, where it is given by
/// reference.
fn collection_key(document: &Document, font: &Dictionary) -> Option<u32> {
    match font.get(b"ToUnicode") {
        Ok(Object::Reference(id)) => return Some(id.0),
        Ok(_) => return None,
        Err(_) => {}
    }
    if !named_identity(font) {
        return None;
    }
    let first = first_descendant(document, font)?;
    let descendant = dictionary(document, first)?;
    let file = descendant
        .get(b"FontDescriptor")
        .ok()
        .and_then(|descriptor| dictionary(document, descriptor))
        .and_then(program_of);
    file.map(|file| file.0)
        .or_else(|| first.as_reference().ok().map(|id| id.0))
        .filter(|key| *key != 0)
}

/// The key pdf-inspector looks the map of `font` up under as it reads the
/// font's text: as it files it (see `collection_key`), but only for a
/// descendant with a font descriptor.
fn lookup_key(document: &Document, font: &Dictionary) -> Option<u32> {
    if !named_identity(font) {
        return None;
    }
    let first = first_descendant(document, font)?;
    let descendant = dictionary(document, first)?;
    let descriptor = dictionary(document, descendant.get(b"FontDescriptor").ok()?)?;
    program_of(descriptor)
        .map(|file| file.0)
        .or_else(|| first.as_reference().ok().map(|id| id.0))
}

fn resolved<'a>(document: &'a Document, object: &'a Object) -> Option<&'a Object> {
    match object {
        Object::Reference(id) => document.get_object(*id).ok(),
        other => Some(other),
    }
}

fn dictionary<'a>(document: &'a Document, object: &'a Object) -> Option<&'a Dictionary> {
    resolved(document, object)?.as_dict().ok()
}

/// A stream's content as pdf-inspector reads it, decoded where it can be,
/// counted against `read`; None where it decodes past `MAX_STREAM_BYTES`,
/// or, past `MAX_READ_BYTES` read in all, past `MAX_SMALL_STREAM_BYTES`,
/// and past the small streams' bytes, at all.
fn content(stream: &Stream, read: &mut usize) -> Option<Vec<u8>> {
    let limit = if *read < MAX_READ_BYTES {
        MAX_STREAM_BYTES
    } else if *read < MAX_READ_BYTES + MAX_SMALL_READ_BYTES {
        MAX_SMALL_STREAM_BYTES
    } else {
        return None;
    };
    let data = match stream.decompressed_content_with_limit(limit) {
        Ok(data) if !data.is_empty() => data,
        Err(lopdf::Error::Decompress(lopdf::DecompressError::MemoryLimitExceeded { .. })) => {
            return None;
        }
        _ => stream.content.clone(),
    };
    *read += data.len();
    Some(data)
}

/// Whether a map holds any entry.
fn holds(cmap: &ToUnicodeCMap) -> bool {
    !cmap.char_map.is_empty() || !cmap.ranges.is_empty()
}

/// The program pdf-inspector reads `font` by the map of, where the font
/// has no `/ToUnicode`, is under an Identity CMap named in place, and its
/// descendant has a font descriptor naming a program by reference: the
/// key it files and looks up that map under (see `lookup_key`), where the
/// font is collected (see `collected_keys`).
pub(crate) fn program_key(document: &Document, font: &Dictionary) -> Option<ObjectId> {
    if font.has(b"ToUnicode") || !named_identity(font) {
        return None;
    }
    program(
        document,
        dictionary(document, first_descendant(document, font)?)?,
    )
}

/// The map pdf-inspector builds from the program `file`, counted against
/// `read` as maps are (see `content`); None where it gives none, as a
/// TrueType or OpenType program with no usable `cmap` table does, or was
/// not read.
pub(crate) fn program_map(
    document: &Document,
    file: ObjectId,
    read: &mut usize,
) -> Option<ToUnicodeCMap> {
    let stream = document.get_object(file).ok()?.as_stream().ok()?;
    build_cmap_from_truetype(&content(stream, read)?).filter(holds)
}

/// Whether the program `file` gives pdf-inspector a map, as a TrueType or
/// OpenType program with a `cmap` table does; None where it was not read.
fn program_maps(document: &Document, file: ObjectId, read: &mut usize) -> Option<bool> {
    let Some(stream) = document
        .get_object(file)
        .ok()
        .and_then(|file| file.as_stream().ok())
    else {
        return Some(false);
    };
    let data = content(stream, read)?;
    Some(build_cmap_from_truetype(&data).is_some_and(|cmap| holds(&cmap)))
}

/// The program a descendant font embeds, as pdf-inspector finds it: the
/// first of `/FontFile2` and `/FontFile3` given by reference.
fn program(document: &Document, descendant: &Dictionary) -> Option<ObjectId> {
    program_of(dictionary(
        document,
        descendant.get(b"FontDescriptor").ok()?,
    )?)
}

/// The program a font descriptor names (see `program`).
fn program_of(descriptor: &Dictionary) -> Option<ObjectId> {
    [&b"FontFile2"[..], b"FontFile3"]
        .into_iter()
        .find_map(|key| {
            descriptor
                .get(key)
                .ok()
                .and_then(|file| file.as_reference().ok())
        })
}

/// Whether the widths a descendant font gives, in place, are for codes
/// whose median is 0x41 or past it, which pdf-inspector takes for Unicode.
fn widths_look_like_unicode(descendant: &Dictionary) -> bool {
    let Ok(Object::Array(entries)) = descendant.get(b"W") else {
        return false;
    };
    let mut seen = HashSet::new();
    let mut index = 0;
    while index < entries.len() && seen.len() < MAX_WIDTH_CIDS {
        let Ok(first) = entries[index].as_i64() else {
            index += 1;
            continue;
        };
        let start = first as u16;
        match entries.get(index + 1) {
            Some(Object::Array(widths)) => {
                for offset in 0..widths.len() {
                    if seen.len() >= MAX_WIDTH_CIDS {
                        break;
                    }
                    seen.insert(start.wrapping_add(offset as u16));
                }
                index += 2;
            }
            Some(last) if index + 2 < entries.len() => {
                if let Ok(last) = last.as_i64() {
                    let last = last as u16;
                    if start <= last {
                        for cid in start..=last {
                            if seen.len() >= MAX_WIDTH_CIDS {
                                break;
                            }
                            seen.insert(cid);
                        }
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
    let mut cids: Vec<u16> = seen.into_iter().collect();
    cids.sort_unstable();
    cids.get(cids.len() / 2)
        .is_some_and(|median| *median >= 0x41)
}

/// Whether `encoding` names one of Adobe's predefined CMaps whose codes are
/// Unicode as UTF-16, two bytes each but for a character past the Basic
/// Multilingual Plane: UCS-2, as `UniJIS-UCS2-H` or `UniGB-UCS2-V` has
/// them, or UTF-16, as `UniJIS-UTF16-H` has them.
pub(crate) fn utf16_cmap(encoding: &[u8]) -> bool {
    let has = |part: &[u8]| encoding.windows(part.len()).any(|window| window == part);
    encoding.starts_with(b"Uni")
        && (has(b"-UCS2-") || has(b"-UTF16-"))
        && (encoding.ends_with(b"-H") || encoding.ends_with(b"-V"))
}

/// How pdf-inspector 1.25.0 reads `font`, when it is a composite font of
/// Adobe's Japanese, Chinese, or Korean collections that it finds no map
/// for, as it looks for one: where it collects the font (see
/// `collected_keys`), a `/ToUnicode` stream it parses, and one it cannot
/// parse, under an Identity CMap, the descendant's program or the Korean
/// table; where it does not, the `/ToUnicode` stream lopdf parses; with no
/// `/ToUnicode` at all, under an Identity CMap named in place, the
/// program or the table, and last its widths, taken for Unicode, looked up
/// by the program or by a descendant given by reference, where the
/// descendant has a font descriptor and a font it collects is filed under
/// that key. A font of another ordering, embedding its program, is told
/// only where it does not collect the font.
fn unmapped(document: &Document, font: &Dictionary, fonts: &mut CjkFonts) -> Option<Unmapped> {
    fn name(object: &Object) -> Option<&[u8]> {
        object.as_name().ok()
    }
    if font.get(b"Subtype").ok().and_then(name) != Some(b"Type0".as_slice()) {
        return None;
    }
    let first = first_descendant(document, font)?;
    let descendant = dictionary(document, first)?;
    let ordering = descendant
        .get(b"CIDSystemInfo")
        .ok()
        .and_then(|info| dictionary(document, info))
        .and_then(|info| info.get(b"Ordering").ok())?;
    // pdf-inspector reads its collection from an ordering given in place.
    let (ordering, in_place) = match ordering {
        Object::String(ordering, _) => (ordering.as_slice(), true),
        Object::Reference(id) => match document.get_object(*id) {
            Ok(Object::String(ordering, _)) => (ordering.as_slice(), false),
            _ => return None,
        },
        _ => return None,
    };
    // A font of another ordering, such as Identity, whose codes are its
    // program's glyphs, is judged only where pdf-inspector reads it with no
    // map as it does not collect it, while a viewer shows the glyphs.
    let adobe = COLLECTIONS.contains(&ordering);
    if !adobe && program(document, descendant).is_none() {
        return None;
    }
    let tabled = in_place && ordering == TABLED_COLLECTION;
    let encoding = font.get(b"Encoding").ok();
    // A CMap named in place or by reference, which lopdf follows; an
    // encoding given as a stream of its own lopdf cannot read, and gives
    // the standard encoding, as it does an Identity CMap it has no map for.
    let (codes, standard) = match encoding
        .and_then(|encoding| resolved(document, encoding))
        .and_then(name)
        .map(named_cmap)
    {
        Some(Named::Identity) | None if adobe => (Codes::Cids, true),
        // Glyphs whose widths are given as a font's whose codes are code
        // points have are those characters.
        Some(Named::Identity) | None if widths_look_like_unicode(descendant) => {
            (Codes::Utf16, true)
        }
        Some(Named::Identity) | None => (Codes::Glyphs, true),
        Some(Named::Bytes(codes)) if adobe => (codes, false),
        Some(_) => return None,
    };
    // lopdf gives an encoding only to a dictionary typed as a font.
    let standard = standard && font.has_type(b"Font");
    // Text whose codes are Unicode says what it is, so its reading with no
    // map, the same where it says ASCII, is never taken to show the font
    // has none.
    let unicode = matches!(codes, Codes::Utf16 | Codes::Utf32);
    let unmapped = |passthrough: bool, judged: bool| {
        Some(Unmapped {
            passthrough,
            codes,
            standard,
            judged: judged || unicode,
        })
    };
    let bytes = unmapped(false, true);
    let identity = |encoding: Option<&Object>| {
        matches!(encoding.and_then(name), Some(b"Identity-H" | b"Identity-V"))
    };
    match font.get(b"ToUnicode") {
        Ok(Object::Reference(id)) => {
            // A font it does not collect it reads by the encoding lopdf
            // gives it: the map where lopdf's strict grammar parses it, else
            // the standard encoding. No program or table stands in, as it
            // builds those as it files a map, and never reads the program.
            if !fonts.collected(document, id.0) {
                return match font.get_font_encoding_with_limit(document, MAX_STREAM_BYTES) {
                    Ok(Encoding::UnicodeMapEncoding(_)) => None,
                    Err(lopdf::Error::Decompress(
                        lopdf::DecompressError::MemoryLimitExceeded { .. },
                    )) => unmapped(false, false),
                    _ => bytes,
                };
            }
            if !adobe {
                return None;
            }
            let Some(stream) = document
                .get_object(*id)
                .ok()
                .and_then(|map| map.as_stream().ok())
            else {
                return bytes;
            };
            // A map it cannot parse, or that is empty, leaves it the
            // program's or the table, under an Identity CMap named in place
            // or by reference.
            let fallback = identity(encoding.and_then(|encoding| resolved(document, encoding)));
            let Some(map) = content(stream, &mut fonts.read) else {
                // A map past the bytes read may parse; else, the Korean
                // table reads the font.
                return if tabled && fallback {
                    None
                } else {
                    unmapped(false, false)
                };
            };
            if ToUnicodeCMap::parse(&map).is_some_and(|cmap| holds(&cmap)) {
                return None;
            }
            if !fallback {
                return bytes;
            }
            if let Some(file) = program(document, descendant) {
                match fonts.program_maps(document, file) {
                    Some(true) => return None,
                    Some(false) => {}
                    None if tabled => return None,
                    None => return unmapped(false, false),
                }
            }
            return if tabled { None } else { bytes };
        }
        // A `/ToUnicode` that is no stream gives no map, and keeps it from
        // looking for one.
        Ok(_) => return bytes.filter(|_| adobe),
        Err(_) => {}
    }
    // Another predefined CMap, whose codes it reads byte by byte, or an
    // encoding given by reference or as a stream of its own.
    if !identity(encoding) {
        return bytes.filter(|_| adobe);
    }
    // No map is looked up for a descendant with no font descriptor, nor
    // found for a font no page or form it collects fonts from names.
    let Some(key) = lookup_key(document, font) else {
        return bytes.filter(|_| adobe);
    };
    if !fonts.collected(document, key) {
        return bytes;
    }
    if !adobe {
        return None;
    }
    let passthrough = widths_look_like_unicode(descendant);
    if let Some(file) = program(document, descendant) {
        match fonts.program_maps(document, file) {
            Some(true) => return None,
            Some(false) => {}
            None if tabled => return None,
            None => return unmapped(passthrough, false),
        }
    }
    if tabled {
        return None;
    }
    unmapped(passthrough, true)
}

/// The codes of a string, two bytes each; None for an odd byte.
fn codes(bytes: &[u8]) -> Option<Vec<u16>> {
    bytes.len().is_multiple_of(2).then(|| {
        bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&code| u16::from_be_bytes(code))
            .collect()
    })
}

/// The characters of `codes` taken for UTF-16, but for control codes.
fn as_utf16(codes: &[u16]) -> String {
    char::decode_utf16(codes.iter().copied())
        .filter_map(Result::ok)
        .filter(|character| !character.is_control())
        .collect()
}

/// How pdf-inspector scores a reading of a string, to keep the UTF-16 one.
fn score(text: &str) -> i32 {
    const COMMON_WORDS: [&str; 22] = [
        "the", "and", "of", "to", "in", "a", "is", "that", "for", "with", "on", "as", "by", "from",
        "this", "be", "are", "at", "or", "not", "it", "our",
    ];
    let (mut letters, mut spaces, mut digits, mut other, mut words) = (0, 0, 0, 0, 0);
    let mut word = String::new();
    let end_word = |word: &mut String, words: &mut i32| {
        if !word.is_empty() && COMMON_WORDS.contains(&word.as_str()) {
            *words += 1;
        }
        word.clear();
    };
    for character in text.chars() {
        if character.is_ascii_alphabetic() {
            letters += 1;
            word.push(character.to_ascii_lowercase());
            continue;
        }
        end_word(&mut word, &mut words);
        if character == ' ' {
            spaces += 1;
        } else if character.is_ascii_digit() {
            digits += 1;
        } else if character.is_control() || character == '\u{FFFD}' {
            other += 3;
        } else if matches!(character, '\u{4E00}'..='\u{9FFF}' | '\u{3040}'..='\u{30FF}' | '\u{3400}'..='\u{4DBF}' | '\u{F900}'..='\u{FAFF}')
        {
            letters += 1;
        } else {
            other += 1;
        }
    }
    end_word(&mut word, &mut words);
    let mut score = words * 10 + letters + spaces * 2 + digits - other * 2;
    if letters > 15 && words == 0 {
        score -= 15;
    }
    score
}

/// What pdf-inspector reads a string shown in `font` as, but for the
/// control codes it drops, where it reads it with no sign: none for a string
/// with an odd byte, or, in a font whose codes it does not take for
/// Unicode, one with a byte past 0x7F, which it marks with U+FFFD. A string
/// of null-heavy codes it reads as UTF-16 where that scores as text, each
/// control code counting against it; any other byte by byte: by the
/// standard encoding, which gives control codes and 0x7F no character and
/// 0x27 and 0x60 curly quotes, where `font.standard` says so, else as the
/// bytes are.
pub(crate) fn read_as(font: Unmapped, bytes: &[u8]) -> Option<String> {
    let codes = codes(bytes)?;
    if font.passthrough {
        return Some(as_utf16(&codes));
    }
    if bytes.iter().any(|&byte| byte > 0x7F) {
        return None;
    }
    let nulls = bytes.iter().filter(|&&byte| byte == 0).count();
    if bytes.len() >= 4 && nulls * 4 > bytes.len() {
        let utf16 = String::from_utf16_lossy(&codes);
        if score(&utf16) > 0 {
            return Some(
                utf16
                    .chars()
                    .filter(|character| !character.is_control())
                    .collect(),
            );
        }
    }
    Some(
        bytes
            .iter()
            .filter(|&&byte| (0x20..0x7F).contains(&byte))
            .map(|&byte| match byte {
                0x27 if font.standard => '\u{2019}',
                0x60 if font.standard => '\u{2018}',
                byte => char::from(byte),
            })
            .collect(),
    )
}

/// What a string shown in `font` says, as far as can be told: codes that
/// are Unicode as the characters they are; in a collection, its codes the
/// collection gives letters, digits, or the marks amounts and dates are
/// written with, and a space for any other code. None for an odd byte, for
/// UTF-32 codes that are not four bytes each, and for another predefined
/// CMap's codes and a program's glyphs.
pub(crate) fn says(font: Unmapped, bytes: &[u8]) -> Option<String> {
    let codes = codes(bytes)?;
    match font.codes {
        Codes::Other | Codes::Glyphs => None,
        Codes::Utf16 => Some(as_utf16(&codes)),
        Codes::Utf32 => bytes.len().is_multiple_of(4).then(|| {
            bytes
                .as_chunks::<4>()
                .0
                .iter()
                .filter_map(|&code| char::from_u32(u32::from_be_bytes(code)))
                .filter(|character| !character.is_control())
                .collect()
        }),
        Codes::Cids => Some(
            codes
                .iter()
                .map(|&cid| {
                    ASCII_CIDS
                        .contains(&cid)
                        .then(|| char::from(cid as u8 + 0x1F))
                        .filter(|character| {
                            character.is_ascii_alphanumeric() || ",.-/:()".contains(*character)
                        })
                        .unwrap_or(' ')
                })
                .collect(),
        ),
    }
}

/// Whether pdf-inspector reads a string shown in `font` otherwise than it
/// says with no sign: taking its codes for Unicode, a code past the space;
/// else a string with no byte past 0x7F, which in a collection it reads
/// otherwise wherever a code is past the space, where the codes are
/// Unicode wherever it does not read them as the characters they are,
/// under another predefined CMap wherever a byte is printable and past the
/// space, and in a program's glyphs wherever one is past the first, which
/// draws nothing.
pub(crate) fn misread(font: Unmapped, bytes: &[u8]) -> bool {
    let Some(codes) = codes(bytes) else {
        return false;
    };
    if font.passthrough {
        return codes.iter().any(|&code| code > 1);
    }
    if bytes.iter().any(|&byte| byte > 0x7F) {
        return false;
    }
    match font.codes {
        Codes::Cids => codes.iter().any(|&code| code > 1),
        Codes::Utf16 | Codes::Utf32 => read_as(font, bytes) != says(font, bytes),
        Codes::Other => bytes.iter().any(|byte| (0x21..0x7F).contains(byte)),
        Codes::Glyphs => codes.iter().any(|&code| code > 0),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use lopdf::{dictionary, Stream};

    /// A descendant font in `ordering`, given by reference, embedding
    /// `program` under its key, if any.
    fn cid_font(
        document: &mut Document,
        ordering: &str,
        program: Option<(&str, Vec<u8>)>,
    ) -> Object {
        let mut descriptor = dictionary! {
            "Type" => "FontDescriptor",
            "FontName" => "KozMinPr6N-Regular",
        };
        if let Some((key, content)) = program {
            let program = document.add_object(Stream::new(dictionary! {}, content));
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

    fn type0(descendants: Object, encoding: Object) -> Dictionary {
        dictionary! {
            "Type" => "Font",
            "Subtype" => "Type0",
            "BaseFont" => "KozMinPr6N-Regular",
            "Encoding" => encoding,
            "DescendantFonts" => descendants,
        }
    }

    /// A TrueType program of 96 glyphs whose `cmap` is one format-12
    /// subtable of `groups`, each the code points from its first to its
    /// last, mapped to glyphs from its third on.
    pub(crate) fn truetype(groups: &[(u32, u32, u32)]) -> Vec<u8> {
        let mut head = Vec::new();
        for value in [0x0001_0000u32, 0x0001_0000, 0, 0x5F0F_3CF5] {
            head.extend(value.to_be_bytes());
        }
        head.extend([0, 0, 0x03, 0xE8]);
        head.extend([0; 16]);
        for value in [0i16, -200, 1000, 900, 0, 0, 2, 0, 0] {
            head.extend(value.to_be_bytes());
        }
        let mut hhea = 0x0001_0000u32.to_be_bytes().to_vec();
        for value in [880i16, -120, 0, 1000] {
            hhea.extend(value.to_be_bytes());
        }
        hhea.extend([0; 22]);
        hhea.extend(1u16.to_be_bytes());
        let mut maxp = 0x0000_5000u32.to_be_bytes().to_vec();
        maxp.extend(96u16.to_be_bytes());
        let mut cmap = Vec::new();
        for value in [0u16, 1, 3, 10] {
            cmap.extend(value.to_be_bytes());
        }
        cmap.extend(12u32.to_be_bytes());
        cmap.extend([0, 12, 0, 0]);
        let groups_len = u32::try_from(groups.len()).unwrap();
        for value in [16 + 12 * groups_len, 0, groups_len] {
            cmap.extend(value.to_be_bytes());
        }
        for (first, last, glyph) in groups {
            for value in [first, last, glyph] {
                cmap.extend(value.to_be_bytes());
            }
        }
        let tables = [
            (b"cmap", cmap),
            (b"head", head),
            (b"hhea", hhea),
            (b"maxp", maxp),
        ];
        let mut program = 0x0001_0000u32.to_be_bytes().to_vec();
        program.extend([0, 4, 0, 64, 0, 2, 0, 0]);
        let mut offset = 12 + 16 * tables.len();
        let mut data = Vec::new();
        for (tag, table) in &tables {
            program.extend(*tag);
            program.extend(0u32.to_be_bytes());
            for value in [offset, table.len()] {
                program.extend(u32::try_from(value).unwrap().to_be_bytes());
            }
            data.extend(table);
            data.resize(data.len().next_multiple_of(4), 0);
            offset = 12 + 16 * tables.len() + data.len();
        }
        program.extend(data);
        program
    }

    /// How `font` is judged where a page names it, or, `on_page` false,
    /// where only a form giving its resources by reference names it.
    fn judged_where(
        build: impl FnOnce(&mut Document) -> Dictionary,
        on_page: bool,
    ) -> Option<Unmapped> {
        judging_where(build, on_page).0
    }

    /// How `font` is judged (see `judged_where`), with what was read to
    /// judge it.
    fn judging_where(
        build: impl FnOnce(&mut Document) -> Dictionary,
        on_page: bool,
    ) -> (Option<Unmapped>, CjkFonts) {
        judging_after(0, build, on_page)
    }

    /// How `font` is judged (see `judged_where`) once `read` bytes of maps
    /// and programs have been read, with what was read to judge it.
    fn judging_after(
        read: usize,
        build: impl FnOnce(&mut Document) -> Dictionary,
        on_page: bool,
    ) -> (Option<Unmapped>, CjkFonts) {
        let mut document = Document::with_version("1.7");
        let font = build(&mut document);
        let font = document.add_object(font);
        let fonts = dictionary! { "F1" => font };
        let resources = if on_page {
            dictionary! { "Font" => fonts }
        } else {
            let own = document.add_object(dictionary! { "Font" => fonts });
            let form = document.add_object(Stream::new(
                dictionary! {
                    "Type" => "XObject",
                    "Subtype" => "Form",
                    "BBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
                    "Resources" => own,
                },
                b"BT /F1 12 Tf <0035> Tj ET".to_vec(),
            ));
            dictionary! { "XObject" => dictionary! { "Fm1" => form } }
        };
        let pages = document.new_object_id();
        let page = document.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Resources" => resources,
        });
        document.objects.insert(
            pages,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page.into()],
                "Count" => 1,
            }),
        );
        let catalog = document.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages });
        document.trailer.set("Root", catalog);
        let font = document.get_dictionary(font).unwrap();
        let mut fonts = CjkFonts {
            read,
            ..CjkFonts::default()
        };
        (fonts.font(&document, font), fonts)
    }

    fn judged(build: impl FnOnce(&mut Document) -> Dictionary) -> Option<Unmapped> {
        judged_where(build, true)
    }

    const BYTES: Option<Unmapped> = Some(Unmapped {
        passthrough: false,
        codes: Codes::Cids,
        standard: true,
        judged: true,
    });

    /// A font whose `codes` pdf-inspector reads byte by byte as they are,
    /// under a predefined CMap lopdf cannot read.
    fn bytes_as(codes: Codes) -> Option<Unmapped> {
        Some(Unmapped {
            passthrough: false,
            codes,
            standard: false,
            judged: true,
        })
    }

    #[test]
    fn fonts_pdf_inspector_finds_no_map_for_are_unmapped() {
        for ordering in ["Japan1", "GB1", "CNS1"] {
            for encoding in ["Identity-H", "Identity-V"] {
                let font = judged(|document| {
                    type0(
                        cid_font(document, ordering, None),
                        Object::Name(encoding.into()),
                    )
                });
                assert_eq!(font, BYTES, "{ordering} {encoding}");
            }
        }
        // A bare CFF program, or junk, has no map of its own.
        let font = judged(|document| {
            let program = ("FontFile3", b"\x01\x00\x04\x02 not an sfnt".to_vec());
            type0(
                cid_font(document, "Japan1", Some(program)),
                "Identity-H".into(),
            )
        });
        assert_eq!(font, BYTES);
        // An encoding given by reference, and a ToUnicode that is no map.
        let font = judged(|document| {
            let encoding = document.add_object(Object::Name(b"Identity-H".to_vec()));
            type0(cid_font(document, "Japan1", None), encoding.into())
        });
        assert_eq!(font, BYTES);
        for map in [Object::Name(b"Identity-H".to_vec()), Object::Null] {
            let font = judged(|document| {
                let mut font = type0(cid_font(document, "Korea1", None), "Identity-H".into());
                font.set("ToUnicode", map);
                font
            });
            assert_eq!(font, BYTES);
        }
        // An empty ToUnicode stream.
        let font = judged(|document| {
            let map = document.add_object(Stream::new(dictionary! {}, Vec::new()));
            let mut font = type0(cid_font(document, "GB1", None), "Identity-H".into());
            font.set("ToUnicode", map);
            font
        });
        assert_eq!(font, BYTES);
        // Codes of a predefined CMap lopdf cannot read, with no map: UCS-2
        // and UTF-16, UTF-32, and two bytes from 0x21 to 0x7E.
        for (ordering, encoding, codes) in [
            ("Japan1", "UniJIS-UCS2-H", Codes::Utf16),
            ("Japan1", "UniJIS-UTF16-H", Codes::Utf16),
            ("Japan1", "UniJIS2004-UTF16-V", Codes::Utf16),
            ("CNS1", "UniCNS-UTF16-H", Codes::Utf16),
            ("Korea1", "UniKS-UTF16-H", Codes::Utf16),
            ("GB1", "UniGB-UTF16-V", Codes::Utf16),
            ("Japan1", "UniJIS-UTF32-H", Codes::Utf32),
            ("Japan1", "H", Codes::Other),
            ("Japan1", "V", Codes::Other),
            ("GB1", "GB-H", Codes::Other),
            ("Korea1", "KSC-H", Codes::Other),
            ("Japan1", "Hiragana", Codes::Other),
        ] {
            let font = judged(|document| {
                type0(
                    cid_font(document, ordering, None),
                    Object::Name(encoding.into()),
                )
            });
            assert_eq!(font, bytes_as(codes), "{encoding}");
        }
        // Widths given mostly past 0x41 take the codes for Unicode.
        let font = judged(|document| {
            let descendants = cid_font(document, "Japan1", None);
            let Object::Array(descendants) = &descendants else {
                unreachable!()
            };
            let id = descendants[0].as_reference().unwrap();
            let descendant = document.get_object_mut(id).unwrap().as_dict_mut().unwrap();
            descendant.set(
                "W",
                vec![
                    1.into(),
                    95.into(),
                    500.into(),
                    231.into(),
                    632.into(),
                    500.into(),
                ],
            );
            type0(Object::Array(descendants.clone()), "Identity-H".into())
        });
        assert_eq!(
            font,
            Some(Unmapped {
                passthrough: true,
                ..BYTES.unwrap()
            })
        );
    }

    #[test]
    fn fonts_pdf_inspector_looks_up_no_map_for_are_unmapped() {
        // Korean, which its table reads on a page, and a program with a map,
        // named only by a form giving its resources by reference, which
        // pdf-inspector collects no fonts from.
        let korean = |document: &mut Document| {
            type0(cid_font(document, "Korea1", None), "Identity-H".into())
        };
        assert_eq!(judged_where(korean, true), None);
        assert_eq!(judged_where(korean, false), BYTES);
        // Korean whose descendant has no font descriptor, which it looks up
        // no map for.
        let font = judged(|document| {
            let descendants = cid_font(document, "Korea1", None);
            let Object::Array(descendants) = &descendants else {
                unreachable!()
            };
            let id = descendants[0].as_reference().unwrap();
            let descendant = document.get_object_mut(id).unwrap().as_dict_mut().unwrap();
            descendant.remove(b"FontDescriptor");
            type0(Object::Array(descendants.clone()), "Identity-H".into())
        });
        assert_eq!(font, BYTES);
    }

    #[test]
    fn fonts_pdf_inspector_does_not_collect_are_read_by_no_fallback() {
        // A map it cannot parse, over a program whose map covers every code
        // point 2,000 times over: on a page, pdf-inspector reads the font
        // by the program; named only by a form giving its resources by
        // reference, it files no map for it and never reads the program,
        // nor does the check.
        let groups = vec![(0, 0x10_FFFF, 1); 2_000];
        let garbled = |document: &mut Document| {
            let map =
                document.add_object(Stream::new(dictionary! {}, b"garbage, not a cmap".to_vec()));
            let program = ("FontFile2", truetype(&groups));
            let mut font = type0(
                cid_font(document, "Japan1", Some(program)),
                "Identity-H".into(),
            );
            font.set("ToUnicode", map);
            font
        };
        let (font, fonts) = judging_where(garbled, false);
        assert_eq!(font, BYTES);
        assert!(fonts.programs.is_empty());
        let mapped = |document: &mut Document| {
            let map = document.add_object(Stream::new(dictionary! {}, b"garbage".to_vec()));
            let program = ("FontFile2", truetype(&[(0x20, 0x7E, 1)]));
            let mut font = type0(
                cid_font(document, "Japan1", Some(program)),
                "Identity-H".into(),
            );
            font.set("ToUnicode", map);
            font
        };
        assert_eq!(judged_where(mapped, true), None);
        assert_eq!(judged_where(mapped, false), BYTES);
        // Korean, which its table reads only where it collects the font.
        let korean = |document: &mut Document| {
            let map = document.add_object(Stream::new(dictionary! {}, b"garbage".to_vec()));
            let mut font = type0(cid_font(document, "Korea1", None), "Identity-H".into());
            font.set("ToUnicode", map);
            font
        };
        assert_eq!(judged_where(korean, true), None);
        assert_eq!(judged_where(korean, false), BYTES);
    }

    /// A Japan1 font under `Identity-H` whose ToUnicode map is `map`.
    fn with_map(map: &'static [u8]) -> impl Fn(&mut Document) -> Dictionary {
        move |document: &mut Document| {
            let map = document.add_object(Stream::new(dictionary! {}, map.to_vec()));
            let mut font = type0(cid_font(document, "Japan1", None), "Identity-H".into());
            font.set("ToUnicode", map);
            font
        }
    }

    #[test]
    fn maps_of_fonts_pdf_inspector_does_not_collect_are_read_as_lopdf_reads_them() {
        // Maps with no `/CIDInit /ProcSet findresource begin` header, or
        // no entry such as `/CMapName`, which pdf-inspector's own parser
        // reads where it collects the font, and lopdf's grammar, which
        // reads the fonts it does not, rejects.
        let bare = b"begincmap 1 begincodespacerange <0000> <FFFF> endcodespacerange \
            1 beginbfrange <0001> <005F> <0020> endbfrange endcmap";
        let nameless = b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap \
            1 begincodespacerange <0000> <FFFF> endcodespacerange \
            1 beginbfrange <0001> <005F> <0020> endbfrange endcmap \
            CMapName currentdict /CMap defineresource pop end end";
        for map in [&bare[..], &nameless[..]] {
            assert_eq!(judged_where(with_map(map), true), None);
            assert_eq!(judged_where(with_map(map), false), BYTES);
        }
        let whole = b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap \
            /CMapName /Adobe-Identity-UCS def \
            1 begincodespacerange <0000> <FFFF> endcodespacerange \
            1 beginbfrange <0001> <005F> <0020> endbfrange endcmap \
            CMapName currentdict /CMap defineresource pop end end";
        assert_eq!(judged_where(with_map(whole), false), None);
        // A font not typed as a font lopdf gives no encoding, and its bytes
        // read as they are.
        let untyped = |document: &mut Document| {
            let mut font = with_map(bare)(document);
            font.remove(b"Type");
            font
        };
        assert_eq!(
            judged_where(untyped, false),
            Some(Unmapped {
                standard: false,
                ..BYTES.unwrap()
            })
        );
    }

    #[test]
    fn fonts_of_programs_pdf_inspector_does_not_collect_are_read_byte_by_byte() {
        // A font in Adobe's Identity ordering, whose codes are its program's
        // glyphs, read through the program's map where pdf-inspector
        // collects it, and byte by byte where it does not; so too with a
        // map lopdf's grammar rejects, and not with one it parses.
        let program = || ("FontFile2", truetype(&[(0x20, 0x7E, 1)]));
        let glyphs = |map: Option<&'static [u8]>| {
            move |document: &mut Document| {
                let mut font = type0(
                    cid_font(document, "Identity", Some(program())),
                    "Identity-H".into(),
                );
                if let Some(map) = map {
                    let map = document.add_object(Stream::new(dictionary! {}, map.to_vec()));
                    font.set("ToUnicode", map);
                }
                font
            }
        };
        let read = Some(Unmapped {
            codes: Codes::Glyphs,
            ..BYTES.unwrap()
        });
        assert_eq!(judged_where(glyphs(None), true), None);
        assert_eq!(judged_where(glyphs(None), false), read);
        let bare = b"begincmap 1 begincodespacerange <0000> <FFFF> endcodespacerange \
            1 beginbfrange <0001> <005F> <0020> endbfrange endcmap";
        assert_eq!(judged_where(glyphs(Some(bare)), true), None);
        assert_eq!(judged_where(glyphs(Some(bare)), false), read);
        let whole = b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap \
            /CMapName /Adobe-Identity-UCS def \
            1 begincodespacerange <0000> <FFFF> endcodespacerange \
            1 beginbfrange <0001> <005F> <0020> endbfrange endcmap \
            CMapName currentdict /CMap defineresource pop end end";
        assert_eq!(judged_where(glyphs(Some(whole)), false), None);
        // With no program a viewer has no glyphs to show either.
        let bare_font = |document: &mut Document| {
            type0(cid_font(document, "Identity", None), "Identity-H".into())
        };
        assert_eq!(judged_where(bare_font, false), None);
        // Glyphs whose widths are given as code points' are those characters.
        let code_points = |document: &mut Document| {
            let font = glyphs(None)(document);
            let Ok(Object::Array(descendants)) = font.get(b"DescendantFonts") else {
                unreachable!()
            };
            let id = descendants[0].as_reference().unwrap();
            let descendant = document.get_object_mut(id).unwrap().as_dict_mut().unwrap();
            descendant.set("W", vec![65.into(), 90.into(), 600.into()]);
            font
        };
        assert_eq!(
            judged_where(code_points, false),
            Some(Unmapped {
                codes: Codes::Utf16,
                ..BYTES.unwrap()
            })
        );
    }

    #[test]
    fn fonts_whose_programs_are_past_the_bytes_read_are_not_judged() {
        // A program decoding past `MAX_STREAM_BYTES` may hold a map.
        let font = judged(|document| {
            let mut program = Stream::new(dictionary! {}, vec![0; MAX_STREAM_BYTES + 1]);
            program.compress().unwrap();
            let program = document.add_object(program);
            let descendants = cid_font(document, "Japan1", None);
            let Object::Array(descendants) = &descendants else {
                unreachable!()
            };
            let id = descendants[0].as_reference().unwrap();
            let descriptor = document
                .get_object(id)
                .unwrap()
                .as_dict()
                .unwrap()
                .get(b"FontDescriptor")
                .unwrap()
                .as_reference()
                .unwrap();
            let descriptor = document
                .get_object_mut(descriptor)
                .unwrap()
                .as_dict_mut()
                .unwrap();
            descriptor.set("FontFile2", program);
            type0(Object::Array(descendants.clone()), "Identity-H".into())
        });
        assert_eq!(
            font,
            Some(Unmapped {
                judged: false,
                ..BYTES.unwrap()
            })
        );
    }

    #[test]
    fn small_programs_are_read_past_the_bytes_a_document_reads() {
        // Past the bytes a document reads, a subset's program is still
        // read: one with no map leaves the font's text read byte by byte,
        // one with a map reads it; past the small streams' bytes, or past
        // a small stream's, no program is read.
        let small = |map: bool| {
            move |document: &mut Document| {
                let groups: &[(u32, u32, u32)] = if map { &[(0x20, 0x7E, 1)] } else { &[] };
                let program = ("FontFile2", truetype(groups));
                type0(
                    cid_font(document, "Japan1", Some(program)),
                    "Identity-H".into(),
                )
            }
        };
        let unjudged = Some(Unmapped {
            judged: false,
            ..BYTES.unwrap()
        });
        assert_eq!(judging_after(MAX_READ_BYTES, small(false), true).0, BYTES);
        assert_eq!(judging_after(MAX_READ_BYTES, small(true), true).0, None);
        let spent = MAX_READ_BYTES + MAX_SMALL_READ_BYTES;
        assert_eq!(judging_after(spent, small(false), true).0, unjudged);
        let large = |document: &mut Document| {
            let mut program = truetype(&[]);
            program.resize(MAX_SMALL_STREAM_BYTES + 1, 0);
            type0(
                cid_font(document, "Japan1", Some(("FontFile2", program))),
                "Identity-H".into(),
            )
        };
        assert_eq!(judging_after(0, large, true).0, BYTES);
        assert_eq!(judging_after(MAX_READ_BYTES, large, true).0, unjudged);
        // Unicode text whose map is past the bytes read is judged by what
        // it says, which its reading with no map, where it is ASCII, is.
        let ucs2 = |document: &mut Document| {
            let mut map = Stream::new(dictionary! {}, vec![b' '; MAX_STREAM_BYTES + 1]);
            map.compress().unwrap();
            let map = document.add_object(map);
            let mut font = type0(cid_font(document, "Japan1", None), "UniJIS-UCS2-H".into());
            font.set("ToUnicode", map);
            font
        };
        assert_eq!(judged(ucs2), bytes_as(Codes::Utf16));
    }

    #[test]
    fn fonts_read_through_a_map_are_not_unmapped() {
        // Korean, which pdf-inspector keeps a table for.
        let font =
            judged(|document| type0(cid_font(document, "Korea1", None), "Identity-H".into()));
        assert_eq!(font, None);
        // A ToUnicode map it parses.
        let font = judged(|document| {
            let map = b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap \
                1 begincodespacerange <0000> <FFFF> endcodespacerange \
                1 beginbfchar <0035> <0054> endbfchar endcmap CMapName currentdict \
                /CMap defineresource pop end end";
            let map = document.add_object(Stream::new(dictionary! {}, map.to_vec()));
            let mut font = type0(cid_font(document, "Japan1", None), "Identity-H".into());
            font.set("ToUnicode", map);
            font
        });
        assert_eq!(font, None);
        // Chinese under the CMaps lopdf reads as UTF-16; CMaps whose single
        // bytes are ASCII, which read an ASCII string as it is and have
        // pdf-inspector mark any other; and a CMap no viewer knows either.
        for encoding in [
            "UniGB-UCS2-H",
            "UniGB-UTF16-H",
            "90ms-RKSJ-H",
            "EUC-V",
            "GBK-EUC-H",
            "ETen-B5-H",
            "KSCms-UHC-H",
            "KSC-Johab-H",
            "UniJIS-UTF8-H",
            "Roman",
            "Private-H",
        ] {
            let font = judged(|document| {
                // A map it cannot parse changes nothing.
                let map = document.add_object(Stream::new(dictionary! {}, b"garbage".to_vec()));
                let mut font = type0(
                    cid_font(document, "Japan1", None),
                    Object::Name(encoding.into()),
                );
                font.set("ToUnicode", map);
                font
            });
            assert_eq!(font, None, "{encoding}");
        }
        // A simple font, and one outside the collections.
        assert_eq!(
            judged(
                |_| dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica" }
            ),
            None
        );
        let font =
            judged(|document| type0(cid_font(document, "Identity", None), "Identity-H".into()));
        assert_eq!(font, None);
    }

    #[test]
    fn strings_read_with_no_sign_are_told_as_read_and_as_said() {
        // "Total 52,000.00" in CIDs 1-95.
        let cids: Vec<u8> = "Total 52,000.00"
            .bytes()
            .flat_map(|character| u16::from(character - 0x1F).to_be_bytes())
            .collect();
        let font = BYTES.unwrap();
        assert_eq!(says(font, &cids).as_deref(), Some("Total 52,000.00"));
        assert_eq!(read_as(font, &cids).as_deref(), Some("5PUBM"));
        assert!(misread(font, &cids));
        // HIRAGANA LETTER A in Adobe-Japan1, beside a dollar sign whose glyph
        // differs among the collections, read byte by byte.
        let kana = [0x03, 0x4B, 0x00, 0x05, 0x00, 0x16];
        assert_eq!(says(font, &kana).as_deref(), Some("  5"));
        assert_eq!(read_as(font, &kana).as_deref(), Some("K"));
        // A byte past 0x7F reads as U+FFFD, which pdf-inspector marks.
        assert_eq!(read_as(font, &[0x04, 0x9F, 0x00, 0x16]), None);
        assert!(!misread(font, &[0x04, 0x9F, 0x00, 0x16]));
        assert_eq!(says(font, &[0x00]), None);
        // The standard encoding gives 0x7F no character.
        assert_eq!(
            read_as(font, &[0x31, 0x7F, 0x7F, 0x41]).as_deref(),
            Some("1A")
        );
        // A null-heavy string reads as UTF-16 only where that scores as
        // text with its control codes counted, as CID 1, the space, is
        // one: "Tot" and a kanji read byte by byte.
        let mixed = [0x00, 0x35, 0x00, 0x50, 0x00, 0x55, 0x00, 0x01, 0x04, 0x65];
        assert_eq!(read_as(font, &mixed).as_deref(), Some("5PUe"));
        // Codes taken for Unicode read as the characters of their values.
        let passthrough = Unmapped {
            passthrough: true,
            ..font
        };
        assert_eq!(
            read_as(passthrough, &[0x04, 0xB0, 0x04, 0xB1]).as_deref(),
            Some("Ұұ")
        );
        assert!(misread(passthrough, &[0x04, 0xB0]));
        // UCS-2 and UTF-16 ASCII reads right as UTF-16; kanji byte by byte,
        // its quotes straight.
        let utf16 = bytes_as(Codes::Utf16).unwrap();
        assert!(!misread(utf16, &[0x00, 0x41, 0x00, 0x42]));
        assert_eq!(
            says(utf16, &[0x4F, 0x4F, 0x6C, 0x11]).as_deref(),
            Some("住民")
        );
        assert_eq!(
            read_as(utf16, &[0x4F, 0x4F, 0x6C, 0x11]).as_deref(),
            Some("OOl")
        );
        assert!(misread(utf16, &[0x4F, 0x4F, 0x6C, 0x11]));
        assert_eq!(read_as(utf16, &[0x30, 0x27]).as_deref(), Some("0'"));
        // UTF-32 ASCII reads right byte by byte, its null bytes dropped;
        // kanji do not.
        let utf32 = bytes_as(Codes::Utf32).unwrap();
        let ascii = [0, 0, 0, 0x41, 0, 0, 0, 0x42];
        assert_eq!(says(utf32, &ascii).as_deref(), Some("AB"));
        assert!(!misread(utf32, &ascii));
        let kanji = [0, 0, 0x4F, 0x4F, 0, 0, 0x6C, 0x11];
        assert_eq!(says(utf32, &kanji).as_deref(), Some("住民"));
        assert!(misread(utf32, &kanji));
        // The two-byte codes of `H` read as ASCII: "住民" as "=;L1", which
        // says nothing the check can tell.
        let jis = bytes_as(Codes::Other).unwrap();
        let codes = [0x3D, 0x3B, 0x4C, 0x31];
        assert_eq!(read_as(jis, &codes).as_deref(), Some("=;L1"));
        assert_eq!(says(jis, &codes), None);
        assert!(misread(jis, &codes));
        assert!(!misread(jis, &[0x20, 0x20]));
        // A program's glyphs read as their bytes, by the standard encoding,
        // and say nothing the check can tell but where one is drawn.
        let glyphs = Unmapped {
            codes: Codes::Glyphs,
            ..font
        };
        assert_eq!(read_as(glyphs, &cids).as_deref(), Some("5PUBM"));
        assert_eq!(says(glyphs, &cids), None);
        assert!(misread(glyphs, &[0x00, 0x03]));
        assert!(!misread(glyphs, &[0x00, 0x00]));
    }

    #[test]
    fn utf16_cmaps_are_told_by_name() {
        for name in [
            "UniJIS-UCS2-H",
            "UniJIS-UCS2-HW-V",
            "UniGB-UCS2-V",
            "UniKS-UCS2-H",
            "UniJIS-UTF16-H",
            "UniJIS2004-UTF16-V",
        ] {
            assert!(utf16_cmap(name.as_bytes()), "{name}");
        }
        for name in [
            "Identity-H",
            "UniJIS-UTF32-H",
            "UniJIS-UTF8-H",
            "90ms-RKSJ-V",
            "UniJIS-UCS2",
        ] {
            assert!(!utf16_cmap(name.as_bytes()), "{name}");
        }
    }
}
