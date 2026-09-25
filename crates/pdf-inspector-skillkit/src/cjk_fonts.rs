//! Composite fonts of Adobe's Japanese, Chinese, and Korean collections
//! whose text pdf-inspector 1.24.0 reads with no map (upstream issue #573).
//!
//! A composite font with no `/ToUnicode` map, under `Identity-H` or
//! `Identity-V`, is read by the map of its embedded TrueType or OpenType
//! program, else by its collection's map. pdf-inspector bundles those maps
//! but cannot parse the Japanese and Chinese ones (it keeps the Korean one
//! as a table of its own), and never looks for one where the font names a
//! `/ToUnicode` that is no map, gives its encoding other than as a name in
//! place, or sets its descendant in place with no program. With no map, it
//! reads a string with a byte past 0x7F as U+FFFD, which marks the page
//! garbled, and any other as its bytes: "Total" as "5PUBM", with its digits
//! and punctuation dropped as control codes, and nothing marks it. Where
//! the font's widths are given mostly past code 0x41, it takes the codes
//! for Unicode, as Chromium's fonts' are, and reads every one as the
//! character of its value: "一壱溢" as "ҰұҲ". A font under a predefined
//! `Uni*-UCS2` CMap with no `/ToUnicode` map is read the same way, but
//! Chinese under `UniGB-UCS2-H`, which lopdf reads as UTF-16.

use std::collections::{HashMap, HashSet};

use lopdf::{Dictionary, Document, Object, ObjectId, Stream};
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
/// `Unmapped::judged`).
const MAX_READ_BYTES: usize = 256 << 20;
/// Bytes one map or program may decode to; past them, its font is not
/// judged.
const MAX_STREAM_BYTES: usize = 64 << 20;
/// Distinct CIDs of a font's widths pdf-inspector reads, at most.
const MAX_WIDTH_CIDS: usize = 65_536;

/// A font pdf-inspector 1.24.0 reads with no map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Unmapped {
    /// Whether it takes the codes for Unicode, and reads each one as the
    /// character of its value; else a string with a byte past 0x7F reads
    /// as U+FFFD, and any other as its bytes.
    pub(crate) passthrough: bool,
    /// Whether the codes are UCS-2, under a predefined `Uni*-UCS2` CMap,
    /// not the collection's CIDs.
    pub(crate) ucs2: bool,
    /// Whether it is told to have no map; else it may have one, as its
    /// program was past the bytes read (`MAX_STREAM_BYTES`,
    /// `MAX_READ_BYTES`), and its text is looked for only as pdf-inspector
    /// reads it with none.
    pub(crate) judged: bool,
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
    /// Japanese, Chinese, or Korean collections it finds no map for.
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

/// The keys pdf-inspector files the maps of fonts with no `/ToUnicode`
/// under, as `FontCMaps::from_doc` collects fonts: those of each page's
/// resources, its own and those it inherits, the first of each name; and
/// those of the forms these resources name, and the forms theirs name,
/// where a form gives its `/Resources` in place. A font it does not collect
/// finds a map only where one it does is filed under the same key.
fn collected_keys(document: &Document) -> HashSet<u32> {
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

/// The key pdf-inspector files a map of `font` under as it collects it,
/// where the font has no `/ToUnicode` and is under an Identity CMap named
/// in place: its descendant's program given by reference, else the
/// descendant, where it is given by reference.
fn collection_key(document: &Document, font: &Dictionary) -> Option<u32> {
    if font.get(b"ToUnicode").is_ok() || !named_identity(font) {
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
/// counted against `read`; None past `MAX_READ_BYTES` in all, or where it
/// decodes past `MAX_STREAM_BYTES`.
fn content(stream: &Stream, read: &mut usize) -> Option<Vec<u8>> {
    if *read >= MAX_READ_BYTES {
        return None;
    }
    let data = match stream.decompressed_content_with_limit(MAX_STREAM_BYTES) {
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
/// UCS-2, two bytes each, such as `UniJIS-UCS2-H` or `UniGB-UCS2-V`.
pub(crate) fn ucs2_cmap(encoding: &[u8]) -> bool {
    encoding.starts_with(b"Uni")
        && encoding.windows(6).any(|part| part == b"-UCS2-")
        && (encoding.ends_with(b"-H") || encoding.ends_with(b"-V"))
}

/// How pdf-inspector 1.24.0 reads `font`, when it is a composite font of
/// Adobe's Japanese, Chinese, or Korean collections that it finds no map
/// for, as it looks for one: a `/ToUnicode` stream it parses; one it cannot
/// parse, under an Identity CMap, the descendant's program or the Korean
/// table; with no `/ToUnicode` at all, under an Identity CMap named in
/// place, the same, and last its widths, taken for Unicode, looked up by
/// the program or by a descendant given by reference, where the
/// descendant has a font descriptor and a font it collects is filed under
/// that key (see `collected_keys`).
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
    if !COLLECTIONS.contains(&ordering) {
        return None;
    }
    let tabled = in_place && ordering == TABLED_COLLECTION;
    let encoding = font.get(b"Encoding").ok();
    let named = encoding.and_then(name);
    let ucs2 = named.is_some_and(ucs2_cmap);
    // lopdf reads Chinese under this one as UTF-16.
    if matches!(named, Some(b"UniGB-UCS2-H")) {
        return None;
    }
    let unmapped = |passthrough: bool, judged: bool| {
        Some(Unmapped {
            passthrough,
            ucs2,
            judged,
        })
    };
    let bytes = unmapped(false, true);
    let identity = |encoding: Option<&Object>| {
        matches!(encoding.and_then(name), Some(b"Identity-H" | b"Identity-V"))
    };
    match font.get(b"ToUnicode") {
        Ok(Object::Reference(id)) => {
            let Some(stream) = document
                .get_object(*id)
                .ok()
                .and_then(|map| map.as_stream().ok())
            else {
                return bytes;
            };
            let identity = identity(encoding.and_then(|encoding| resolved(document, encoding)));
            let Some(map) = content(stream, &mut fonts.read) else {
                // A map past the bytes read may parse; else, the Korean
                // table reads the font.
                return if tabled && identity {
                    None
                } else {
                    unmapped(false, false)
                };
            };
            if ToUnicodeCMap::parse(&map).is_some_and(|cmap| holds(&cmap)) {
                return None;
            }
            // A map it cannot parse, or that is empty, leaves it the
            // program's or the table, under an Identity CMap named in place
            // or by reference.
            if !identity {
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
        Ok(_) => return bytes,
        Err(_) => {}
    }
    match encoding {
        Some(Object::Name(_)) if identity(encoding) => {}
        Some(Object::Name(_)) if ucs2 => return bytes,
        // Another predefined CMap.
        Some(Object::Name(_)) => return None,
        // An encoding given by reference or as a stream of its own.
        _ => return bytes,
    }
    // No map is looked up for a descendant with no font descriptor, nor
    // found for a font no page or form it collects fonts from names.
    let Some(key) = lookup_key(document, font) else {
        return bytes;
    };
    if !fonts.collected(document, key) {
        return bytes;
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
            .chunks_exact(2)
            .map(|code| u16::from_be_bytes([code[0], code[1]]))
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
/// of null-heavy codes it reads as UTF-16 where that scores as text; any
/// other byte by byte, under the standard encoding, which gives control
/// codes and 0x7F no character, or, for UCS-2, as Latin-1.
pub(crate) fn read_as(font: Unmapped, bytes: &[u8]) -> Option<String> {
    let codes = codes(bytes)?;
    if font.passthrough {
        return Some(as_utf16(&codes));
    }
    if bytes.iter().any(|&byte| byte > 0x7F) {
        return None;
    }
    let utf16 = as_utf16(&codes);
    let nulls = bytes.iter().filter(|&&byte| byte == 0).count();
    if bytes.len() >= 4 && nulls * 4 > bytes.len() && score(&utf16) > 0 {
        return Some(utf16);
    }
    Some(
        bytes
            .iter()
            .filter(|&&byte| (0x20..0x7F).contains(&byte))
            .map(|&byte| match byte {
                0x27 if !font.ucs2 => '\u{2019}',
                0x60 if !font.ucs2 => '\u{2018}',
                byte => char::from(byte),
            })
            .collect(),
    )
}

/// What a string shown in `font` says, as far as can be told: under a
/// UCS-2 CMap, its codes as UTF-16; in a collection, its codes the
/// collection gives letters, digits, or the marks amounts and dates are
/// written with, and a space for any other code. None for an odd byte.
pub(crate) fn says(font: Unmapped, bytes: &[u8]) -> Option<String> {
    let codes = codes(bytes)?;
    if font.ucs2 {
        return Some(as_utf16(&codes));
    }
    Some(
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
    )
}

/// Whether pdf-inspector reads a string shown in `font` otherwise than it
/// says with no sign: taking its codes for Unicode, a code past the space;
/// else a string with no byte past 0x7F, which in a collection it reads
/// otherwise wherever a code is past the space, and under UCS-2 wherever
/// it does not read it as UTF-16.
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
    if font.ucs2 {
        return read_as(font, bytes) != Some(as_utf16(&codes));
    }
    codes.iter().any(|&code| code > 1)
}

#[cfg(test)]
mod tests {
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

    /// How `font` is judged where a page names it, or, `on_page` false,
    /// where only a form giving its resources by reference names it.
    fn judged_where(
        build: impl FnOnce(&mut Document) -> Dictionary,
        on_page: bool,
    ) -> Option<Unmapped> {
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
        CjkFonts::default().font(&document, font)
    }

    fn judged(build: impl FnOnce(&mut Document) -> Dictionary) -> Option<Unmapped> {
        judged_where(build, true)
    }

    const BYTES: Option<Unmapped> = Some(Unmapped {
        passthrough: false,
        ucs2: false,
        judged: true,
    });

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
        // UCS-2 codes with no map.
        let font =
            judged(|document| type0(cid_font(document, "Japan1", None), "UniJIS-UCS2-H".into()));
        assert_eq!(
            font,
            Some(Unmapped {
                passthrough: false,
                ucs2: true,
                judged: true,
            })
        );
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
                ucs2: false,
                judged: true,
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
                passthrough: false,
                ucs2: false,
                judged: false,
            })
        );
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
        // Another predefined CMap, and Chinese under the one lopdf reads.
        for encoding in ["90ms-RKSJ-H", "UniGB-UCS2-H"] {
            let font = judged(|document| {
                type0(
                    cid_font(document, "Japan1", None),
                    Object::Name(encoding.into()),
                )
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
        // Codes taken for Unicode read as the characters of their values.
        let passthrough = Unmapped {
            passthrough: true,
            ucs2: false,
            judged: true,
        };
        assert_eq!(
            read_as(passthrough, &[0x04, 0xB0, 0x04, 0xB1]).as_deref(),
            Some("Ұұ")
        );
        assert!(misread(passthrough, &[0x04, 0xB0]));
        // UCS-2 ASCII reads right as UTF-16; kanji byte by byte.
        let ucs2 = Unmapped {
            passthrough: false,
            ucs2: true,
            judged: true,
        };
        assert!(!misread(ucs2, &[0x00, 0x41, 0x00, 0x42]));
        assert_eq!(
            says(ucs2, &[0x4F, 0x4F, 0x6C, 0x11]).as_deref(),
            Some("住民")
        );
        assert_eq!(
            read_as(ucs2, &[0x4F, 0x4F, 0x6C, 0x11]).as_deref(),
            Some("OOl")
        );
        assert!(misread(ucs2, &[0x4F, 0x4F, 0x6C, 0x11]));
    }

    #[test]
    fn ucs2_cmaps_are_told_by_name() {
        for name in [
            "UniJIS-UCS2-H",
            "UniJIS-UCS2-HW-V",
            "UniGB-UCS2-V",
            "UniKS-UCS2-H",
        ] {
            assert!(ucs2_cmap(name.as_bytes()), "{name}");
        }
        for name in ["Identity-H", "UniJIS-UTF16-H", "90ms-RKSJ-V", "UniJIS-UCS2"] {
            assert!(!ucs2_cmap(name.as_bytes()), "{name}");
        }
    }
}
