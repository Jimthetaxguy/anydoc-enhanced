//! The operators a content stream holds, counted without decoding it.
//!
//! lopdf's decoder holds every operation of a stream at once, so a stream of
//! a few million operators, a few kilobytes compressed, takes hundreds of
//! megabytes to decode. pdf-inspector 1.25.0 counts a page's or a form's
//! operators first and reads nothing of one holding more than a million;
//! the scan counts them the same way before it decodes a stream, to pass
//! over what pdf-inspector passes over and to charge its work limits first.

/// Operators pdf-inspector 1.25.0 reads in one page's or form's content at
/// most: it reads nothing of a content stream holding more.
pub(crate) const MAX_READ_OPERATORS: usize = 1_000_000;
/// Bytes of a page's content, its streams together, and of a form's, that
/// pdf-inspector 1.25.0 decodes at most: it reads nothing of content past
/// them.
pub(crate) const MAX_READ_BYTES: usize = 64 << 20;

/// Whether a byte is white space in a content stream.
fn white(byte: u8) -> bool {
    matches!(byte, b'\0' | b'\t' | b'\n' | b'\x0c' | b'\r' | b' ')
}

/// Whether a byte can stand in an operator, as lopdf reads one.
fn operator_byte(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || matches!(byte, b'*' | b'\'' | b'"')
}

/// Whether a byte ends a name.
fn delimiter(byte: u8) -> bool {
    matches!(
        byte,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}

/// Where a literal string starting at `at` ends: past its closing
/// parenthesis, nested ones and escapes counted.
fn past_string(content: &[u8], mut at: usize) -> usize {
    let mut depth = 0usize;
    while at < content.len() {
        match content[at] {
            b'\\' => at += 1,
            b'(' => depth += 1,
            b')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return at + 1;
                }
            }
            _ => {}
        }
        at += 1;
    }
    at
}

/// Where an inline image whose `BI` ends at `at` ends, as pdf-inspector
/// finds its end when it counts operators: past the first `EI` with white
/// space on either side, looked for from the first byte after the white
/// space following `BI`, where `BI` stands before white space. Where there
/// is none, counting goes on from there, as it does, so that the operators
/// after an image written without that white space are counted, which
/// lopdf decodes all the same.
fn past_counted_image(content: &[u8], mut at: usize) -> Option<usize> {
    if content.get(at).is_some_and(|&byte| !white(byte)) {
        return Some(at);
    }
    while content.get(at).is_some_and(|&byte| white(byte)) {
        at += 1;
    }
    Some(
        content[at..]
            .windows(4)
            .position(|word| white(word[0]) && word[1..3] == *b"EI" && white(word[3]))
            .map_or(at, |found| at + found + 3),
    )
}

/// Whether a byte is white space between the parts of an operation, as
/// lopdf reads a content stream: it takes no other there.
fn lopdf_white(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n')
}

fn past_lopdf_white(content: &[u8], mut at: usize) -> usize {
    while content.get(at).copied().is_some_and(lopdf_white) {
        at += 1;
    }
    at
}

/// Past the white space and comments at `at`.
fn past_space(content: &[u8], mut at: usize) -> usize {
    loop {
        match content.get(at) {
            Some(&byte) if white(byte) => at += 1,
            Some(b'%') => {
                while at < content.len() && !matches!(content[at], b'\r' | b'\n') {
                    at += 1;
                }
            }
            _ => return at,
        }
    }
}

/// Where a dictionary starting at `at` ends: past its closing brackets,
/// the strings in it read whole.
fn past_dictionary(content: &[u8], mut at: usize) -> usize {
    let mut depth = 0usize;
    while at < content.len() {
        match content[at] {
            b'(' => {
                at = past_string(content, at);
                continue;
            }
            b'<' if content.get(at + 1) == Some(&b'<') => {
                depth += 1;
                at += 1;
            }
            b'>' if content.get(at + 1) == Some(&b'>') => {
                depth = depth.saturating_sub(1);
                at += 1;
                if depth == 0 {
                    return at + 1;
                }
            }
            _ => {}
        }
        at += 1;
    }
    at
}

/// Where an array starting at `at` ends: past its closing bracket, the
/// strings in it read whole.
fn past_array(content: &[u8], mut at: usize) -> usize {
    let mut depth = 0usize;
    while at < content.len() {
        match content[at] {
            b'(' => {
                at = past_string(content, at);
                continue;
            }
            b'[' => depth += 1,
            b']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return at + 1;
                }
            }
            _ => {}
        }
        at += 1;
    }
    at
}

/// A token of a content stream, as pdfium reads one: its words run to
/// white space or a delimiter, so a number written against a letter, as
/// `1e3` or `3Tr`, is a word of its own, where lopdf reads a number and an
/// operator.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Token<'c> {
    /// A word of digits, signs, and points, as pdfium reads it (see
    /// `viewer_number`), and whether it holds no point.
    Number(f32, bool),
    Name(&'c [u8]),
    /// A word that is no number, `true`, `false`, or `null`: an operator,
    /// or, among an inline image's entries, `ID`.
    Keyword(&'c [u8]),
    /// An array, with the name it starts with, where it does.
    Array(Option<&'c [u8]>),
    True,
    /// Another object: a string, a dictionary, `false`, `null`, or a
    /// delimiter standing alone.
    Other,
    End,
}

/// The token at `at`, past white space and comments, where it starts, and
/// where it ends.
fn viewer_token(content: &[u8], at: usize) -> (Token<'_>, usize, usize) {
    let start = past_space(content, at);
    let Some(&byte) = content.get(start) else {
        return (Token::End, start, start);
    };
    let word_end = |mut at: usize| {
        while at < content.len() && !white(content[at]) && !delimiter(content[at]) {
            at += 1;
        }
        at
    };
    match byte {
        b'(' => (Token::Other, start, past_string(content, start)),
        b'<' if content.get(start + 1) == Some(&b'<') => {
            (Token::Other, start, past_dictionary(content, start))
        }
        b'<' => {
            let end = content[start..]
                .iter()
                .position(|&byte| byte == b'>')
                .map_or(content.len(), |found| start + found + 1);
            (Token::Other, start, end)
        }
        b'[' => {
            let first = past_space(content, start + 1);
            let name = (content.get(first) == Some(&b'/'))
                .then(|| &content[first + 1..word_end(first + 1)]);
            (Token::Array(name), start, past_array(content, start))
        }
        b'/' => {
            let end = word_end(start + 1);
            (Token::Name(&content[start + 1..end]), start, end)
        }
        b')' | b'>' | b']' | b'{' | b'}' => (Token::Other, start, start + 1),
        _ => {
            let end = word_end(start);
            let word = &content[start..end];
            let token = if word
                .iter()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'+' | b'-' | b'.'))
            {
                Token::Number(viewer_number(word), !word.contains(&b'.'))
            } else if word == b"true" {
                Token::True
            } else if matches!(word, b"false" | b"null") {
                Token::Other
            } else {
                Token::Keyword(word)
            };
            (token, start, end)
        }
    }
}

/// What an inline image's dictionary says of the data after its `ID`, each
/// entry by its key, abbreviated or not.
#[derive(Default)]
struct ImageEntries<'c> {
    width: Option<Token<'c>>,
    height: Option<Token<'c>>,
    bits: Option<Token<'c>>,
    colour: Option<Token<'c>>,
    mask: Option<Token<'c>>,
    filtered: bool,
}

impl<'c> ImageEntries<'c> {
    fn set(&mut self, key: &[u8], value: Token<'c>) {
        match key {
            b"W" | b"Width" => self.width = Some(value),
            b"H" | b"Height" => self.height = Some(value),
            b"BPC" | b"BitsPerComponent" => self.bits = Some(value),
            b"CS" | b"ColorSpace" => self.colour = Some(value),
            b"IM" | b"ImageMask" => self.mask = Some(value),
            b"F" | b"Filter" => self.filtered = true,
            _ => {}
        }
    }

    /// The bytes of data pdfium reads: for an image not filtered, its rows,
    /// each of its width's samples of its colour space's components, each
    /// of its bits, a sample and a component a bit where it names no colour
    /// space. `None` where it takes none, filtered or of no size, or where
    /// that cannot be told, as for a colour space among the resources.
    fn viewer_length(&self) -> Option<usize> {
        if self.filtered {
            return None;
        }
        let whole = |value: Option<Token<'_>>| match value {
            Some(Token::Number(value, _)) if (1.0..2_147_483_648.0).contains(&value) => {
                Some(value as usize)
            }
            _ => None,
        };
        let (bits, components) = match self.colour {
            None => (1, 1),
            Some(Token::Name(b"DeviceGray" | b"G") | Token::Array(Some(b"Indexed" | b"I"))) => {
                (whole(self.bits)?, 1)
            }
            Some(Token::Array(Some(b"CalGray" | b"Separation"))) => (whole(self.bits)?, 1),
            Some(Token::Name(b"DeviceRGB" | b"RGB") | Token::Array(Some(b"CalRGB" | b"Lab"))) => {
                (whole(self.bits)?, 3)
            }
            Some(Token::Name(b"DeviceCMYK" | b"CMYK")) => (whole(self.bits)?, 4),
            _ => return None,
        };
        whole(self.width)?
            .checked_mul(bits)?
            .checked_mul(components)?
            .div_ceil(8)
            .checked_mul(whole(self.height)?)
    }

    /// The bytes of data lopdf reads: for an image not filtered, of whole
    /// numbers of width, height, and bits, and a device colour space it
    /// names as lopdf spells one, or a mask, its rows. `None` where it
    /// looks for `EI` instead.
    fn lopdf_length(&self) -> Option<usize> {
        if self.filtered {
            return None;
        }
        let integer = |value: Option<Token<'_>>| match value {
            Some(Token::Number(value, true)) if value >= 0.0 => Some(value as usize),
            _ => None,
        };
        let components = match (self.mask, self.colour) {
            (Some(Token::True), _) => 1,
            (_, Some(Token::Name(b"DeviceGray" | b"Gray"))) => 1,
            (_, Some(Token::Name(b"DeviceRGB" | b"RGB"))) => 3,
            (_, Some(Token::Name(b"DeviceRGBA" | b"RGBA" | b"DeviceCMYK" | b"CMYK"))) => 4,
            _ => return None,
        };
        integer(self.width)?
            .checked_mul(components)?
            .checked_mul(integer(self.bits)?)?
            .div_ceil(8)
            .checked_mul(integer(self.height)?)
    }
}

/// Where an inline image whose `BI` ends at `at` ends, as pdfium reads one:
/// its entries up to `ID`; one byte of white space; the data, as many
/// bytes as `ImageEntries::viewer_length` gives; and every token after, up
/// to `EI`, which is how the data is passed over where its length is not
/// told. `None` where pdfium reads no further: at a word other than `ID`
/// among the entries.
fn viewer_past_image(content: &[u8], mut at: usize) -> Option<usize> {
    let mut entries = ImageEntries::default();
    loop {
        let (token, _, end) = viewer_token(content, at);
        at = end;
        match token {
            Token::Keyword(b"ID") => break,
            Token::Keyword(_) => return None,
            Token::Name(key) => {
                let (value, _, end) = viewer_token(content, at);
                entries.set(key, value);
                at = end;
            }
            Token::End => return Some(at),
            _ => break,
        }
    }
    if content.get(at).is_some_and(|&byte| white(byte)) {
        at += 1;
    }
    if let Some(length) = entries.viewer_length() {
        at = at.saturating_add(length).min(content.len());
    }
    loop {
        let (token, _, end) = viewer_token(content, at);
        at = end;
        if matches!(token, Token::End | Token::Keyword(b"EI")) {
            return Some(at);
        }
    }
}

/// Where an inline image whose `BI` ends at `at` ends, as lopdf decodes
/// one: its entries up to `ID` and the white space after; the data, as
/// many bytes as `ImageEntries::lopdf_length` gives, and `EI`; or, where
/// its length is not told, up to the first `EI` with a space, a carriage
/// return, or a line feed on either side. `None` where lopdf decodes no
/// further.
fn lopdf_past_image(content: &[u8], at: usize) -> Option<usize> {
    let mut entries = ImageEntries::default();
    let mut at = past_lopdf_white(content, at);
    while content.get(at) == Some(&b'/') {
        let (Token::Name(key), _, end) = viewer_token(content, at) else {
            return None;
        };
        let (value, _, end) = viewer_token(content, end);
        entries.set(key, value);
        at = past_space(content, end);
    }
    if !content[at..].starts_with(b"ID") {
        return None;
    }
    let data = past_lopdf_white(content, at + 2);
    if let Some(end) = entries
        .lopdf_length()
        .and_then(|length| data.checked_add(length))
        .filter(|&end| end <= content.len())
    {
        let end = past_lopdf_white(content, end);
        return content[end..]
            .starts_with(b"EI")
            .then(|| past_lopdf_white(content, end + 2));
    }
    let spaced = |byte: u8| matches!(byte, b' ' | b'\r' | b'\n');
    content[data..]
        .windows(4)
        .position(|word| spaced(word[0]) && word[1..3] == *b"EI" && spaced(word[3]))
        .map(|found| past_lopdf_white(content, data + found + 3))
}

/// Hand `visit` each operator `content` holds, as lopdf reads one, where it
/// starts, and the name standing last before it, if one does, until
/// `visit` says to stop: each word of letters, `*`, `'`, and `"` standing
/// outside a string, a name, and a comment, `true`, `false`, and `null`
/// aside, with an inline image, from its `BI` to where `past_image` says it
/// ends, one operator, and nothing read past where it says reading stops.
fn walk(
    content: &[u8],
    past_image: fn(&[u8], usize) -> Option<usize>,
    mut visit: impl FnMut(&[u8], usize, Option<&[u8]>) -> bool,
) {
    let mut at = 0;
    let mut name: Option<(usize, usize)> = None;
    while at < content.len() {
        match content[at] {
            b'%' => {
                while at < content.len() && !matches!(content[at], b'\r' | b'\n') {
                    at += 1;
                }
            }
            b'(' => at = past_string(content, at),
            b'<' if content.get(at + 1) == Some(&b'<') => at += 2,
            b'<' => {
                at += 1;
                while at < content.len() && content[at] != b'>' {
                    at += 1;
                }
                at += 1;
            }
            b'/' => {
                at += 1;
                let start = at;
                while at < content.len() && !white(content[at]) && !delimiter(content[at]) {
                    at += 1;
                }
                name = Some((start, at));
            }
            byte if operator_byte(byte) => {
                let start = at;
                while at < content.len() && operator_byte(content[at]) {
                    at += 1;
                }
                let word = &content[start..at];
                if matches!(word, b"true" | b"false" | b"null") {
                    continue;
                }
                if !visit(
                    word,
                    start,
                    name.take().map(|(from, to)| &content[from..to]),
                ) {
                    return;
                }
                if word == b"BI" {
                    match past_image(content, at) {
                        Some(end) => at = end,
                        None => return,
                    }
                }
            }
            _ => at += 1,
        }
    }
}

/// The operators `content` holds, counted up to `limit`, as pdf-inspector
/// counts them (see `walk` and `past_counted_image`).
pub(crate) fn operators(content: &[u8], limit: usize) -> usize {
    let mut count = 0;
    if limit > 0 {
        walk(content, past_counted_image, |_, _, _| {
            count += 1;
            count < limit
        });
    }
    count
}

/// The operand standing last before an operator, as pdfium keeps it.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Operand<'c> {
    Number(f32),
    Name(&'c [u8]),
    Other,
}

/// Hand `visit` each operator `content` holds as pdfium reads the stream
/// (see `Token`), where it starts, and the operand standing last before
/// it, until `visit` says to stop, passing over inline images as pdfium
/// does (see `viewer_past_image`).
fn viewer_walk<'c>(
    content: &'c [u8],
    mut visit: impl FnMut(&'c [u8], usize, Option<Operand<'c>>) -> bool,
) {
    let mut last = None;
    let mut at = 0;
    loop {
        let (token, start, end) = viewer_token(content, at);
        at = end;
        match token {
            Token::End => return,
            Token::Number(value, _) => last = Some(Operand::Number(value)),
            Token::Name(name) => last = Some(Operand::Name(name)),
            Token::Keyword(operator) => {
                if !visit(operator, start, last.take()) {
                    return;
                }
                if operator == b"BI" {
                    match viewer_past_image(content, at) {
                        Some(end) => at = end,
                        None => return,
                    }
                }
            }
            Token::Array(_) | Token::True | Token::Other => last = Some(Operand::Other),
        }
    }
}

/// XObjects drawn a content stream names, noted at most.
const MAX_DRAWN_NAMES: usize = 64;
/// Graphics states saved that the census keeps the render mode of.
const MAX_SAVED_MODES: usize = 256;

/// What a content stream shows, as a viewer reads it, without decoding it:
/// whether it shows text a viewer paints, and, where it does not, the names
/// of the XObjects it draws, some of which may, the first
/// `MAX_DRAWN_NAMES` of them, and whether it draws more.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Census {
    pub(crate) shows_text: bool,
    pub(crate) drawn: Vec<Vec<u8>>,
    pub(crate) more_drawn: bool,
}

/// What `content` shows (see `Census`): text shown in any render mode but
/// 3, which paints nothing, as a viewer sets it (see `viewer_mode`), from
/// mode 0 at the start.
pub(crate) fn census(content: &[u8]) -> Census {
    let mut census = Census::default();
    let mut mode = 0;
    let mut saved: Vec<i64> = Vec::new();
    let mut unsaved = 0usize;
    viewer_walk(content, |operator, _, last| {
        match operator {
            b"q" if saved.len() < MAX_SAVED_MODES => saved.push(mode),
            b"q" => unsaved += 1,
            b"Q" if unsaved > 0 => unsaved -= 1,
            b"Q" => mode = saved.pop().unwrap_or(mode),
            b"Tr" => {
                if let Some(set) = viewer_mode(match last {
                    Some(Operand::Number(value)) => Some(value),
                    _ => None,
                }) {
                    mode = set;
                }
            }
            b"Tj" | b"TJ" | b"'" | b"\"" if mode != 3 => {
                census.shows_text = true;
                census.drawn.clear();
                census.more_drawn = false;
                return false;
            }
            b"Do" => {
                if let Some(Operand::Name(name)) = last {
                    if !census.drawn.iter().any(|drawn| drawn == name) {
                        if census.drawn.len() < MAX_DRAWN_NAMES {
                            census.drawn.push(name.to_vec());
                        } else {
                            census.more_drawn = true;
                        }
                    }
                }
            }
            _ => {}
        }
        true
    });
    census
}

/// The render mode a viewer sets from a `Tr` operand, as pdfium reads it:
/// the number as a float, cut to a whole number, and a number at or past
/// 2^31 either way, or none, as 0; a mode outside 0 to 7 leaves the mode as
/// it was (`None`).
pub(crate) fn viewer_mode(operand: Option<f32>) -> Option<i64> {
    const LIMIT: f32 = 2_147_483_648.0;
    let value = operand.unwrap_or(0.0);
    let mode = if !(-LIMIT..LIMIT).contains(&value) {
        0
    } else {
        value.trunc() as i64
    };
    (0..=7).contains(&mode).then_some(mode)
}

/// A number as pdfium reads a word of digits, signs, and points: with a
/// point, as far as it reads as a decimal; else as an optional sign and the
/// digits after it, where one past 2^32, or signed past 2^31, reads as 0.
fn viewer_number(word: &[u8]) -> f32 {
    let (negative, digits) = match word.first() {
        Some(b'-') => (true, &word[1..]),
        Some(b'+') => (false, &word[1..]),
        _ => (false, word),
    };
    let signed = digits.len() < word.len();
    if word.contains(&b'.') {
        let decimal: Vec<u8> = digits
            .iter()
            .copied()
            .enumerate()
            .take_while(|&(at, byte)| {
                byte.is_ascii_digit() || (byte == b'.' && !digits[..at].contains(&b'.'))
            })
            .map(|(_, byte)| byte)
            .collect();
        let value: f32 = std::str::from_utf8(&decimal)
            .ok()
            .and_then(|decimal| format!("0{decimal}0").parse().ok())
            .unwrap_or(0.0);
        return if negative { -value } else { value };
    }
    let mut value: u64 = 0;
    for byte in digits.iter().take_while(|byte| byte.is_ascii_digit()) {
        value = value * 10 + u64::from(byte - b'0');
        if value > u64::from(u32::MAX) {
            return 0.0;
        }
    }
    let limit = if negative { 1 << 31 } else { (1 << 31) - 1 };
    if signed && value > limit {
        return 0.0;
    }
    if negative {
        -(value as f32)
    } else {
        value as f32
    }
}

/// Where each `Tr` in `content` stands, as pdfium reads the stream (see
/// `viewer_walk`), and the mode a viewer sets there (see `viewer_mode`):
/// the `Tr` after a number written against a letter, as `1e3`, has none,
/// and a name is none either.
pub(crate) fn viewer_modes(content: &[u8]) -> Vec<(usize, Option<i64>)> {
    let mut modes = Vec::new();
    viewer_walk(content, |operator, at, last| {
        if operator == b"Tr" {
            modes.push((
                at,
                viewer_mode(match last {
                    Some(Operand::Number(value)) => Some(value),
                    _ => None,
                }),
            ));
        }
        true
    });
    modes
}

/// The modes a viewer sets at the first `count` `Tr`s lopdf decodes in
/// `content`, in order, each found where lopdf reads it (see `walk` and
/// `lopdf_past_image`): the mode pdfium sets at a `Tr` it reads there (see
/// `viewer_modes`), and none, leaving the mode as it was, at one it does
/// not read, as in `3Tr`, one word to pdfium, or in data it passes over as
/// an image's. `None` where fewer are found.
pub(crate) fn viewer_modes_read(content: &[u8], count: usize) -> Option<Vec<Option<i64>>> {
    if count == 0 {
        return Some(Vec::new());
    }
    let viewed: std::collections::HashMap<usize, Option<i64>> =
        viewer_modes(content).into_iter().collect();
    let mut modes = Vec::with_capacity(count);
    walk(content, lopdf_past_image, |operator, at, _| {
        if operator == b"Tr" {
            modes.push(viewed.get(&at).copied().flatten());
        }
        modes.len() < count
    });
    (modes.len() == count).then_some(modes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The operations lopdf decodes of `content`.
    fn decoded(content: &[u8]) -> usize {
        lopdf::content::Content::decode(content)
            .map(|content| content.operations.len())
            .unwrap_or_default()
    }

    #[test]
    fn operators_are_counted_as_lopdf_decodes_them() {
        for content in [
            &b"q 1 0 0 1 72 720 cm BT /F1 12 Tf (Total \\) due (a)) Tj ET Q"[..],
            b"BT /F1 12 Tf [(A) -120 (B)] TJ T* (C) ' 1 2 (D) \" ET",
            b"/P << /MCID 0 /Alt (x y) >> BDC 0 0 1 rg 0 0 10 10 re f EMC",
            b"% a comment with Tj in it\nq Q <48656C6C6F> Tj",
            b"q BI /W 2 /H 1 /BPC 8 /CS /G ID \x00\xFF EI Q",
            // An image whose data follows `ID` with no white space, which
            // lopdf reads all the same, and the operators after it.
            b"q BI /W 1 /H 1 /BPC 8 /CS /G IDx EI Q q Q",
            b"true false null 1 w",
        ] {
            assert_eq!(operators(content, usize::MAX), decoded(content));
        }
    }

    #[test]
    fn images_without_their_white_space_leave_the_rest_counted() {
        // No `EI` with white space on either side: pdf-inspector goes on
        // counting from after `BI`, where lopdf decodes every operation.
        let content = [
            &b"BI /W 1 /H 1 /BPC 8 /CS /G ID xEI "[..],
            &b"q Q ".repeat(500),
        ]
        .concat();
        // `BI`, and `ID` and `xEI` read as operators, as pdf-inspector reads them.
        assert_eq!(operators(&content, usize::MAX), 1_003);
        assert!(operators(&content, usize::MAX) >= decoded(&content));
    }

    #[test]
    fn what_a_stream_shows_is_told_without_decoding_it() {
        let shown = census(b"q 0 0 m 1 1 l S /Fm1 Do BT /F1 9 Tf (Total) Tj ET Q");
        assert_eq!(
            shown,
            Census {
                shows_text: true,
                ..Census::default()
            }
        );
        let drawn = census(b"q /Fm1 Do /Im2 Do /Fm1 Do (x) pop Q");
        assert_eq!(
            drawn,
            Census {
                drawn: vec![b"Fm1".to_vec(), b"Im2".to_vec()],
                ..Census::default()
            }
        );
        // A string's text and a comment's are not operators.
        assert!(!census(b"% (a) Tj\n[(Tj)] pop").shows_text);
        // Text painted invisibly shows nothing, but where a mode a viewer
        // paints is set again, or restored.
        assert!(!census(b"BT 3 Tr (x) Tj ET BT (y) Tj ET").shows_text);
        assert!(census(b"3 Tr q 0 Tr (x) Tj Q").shows_text);
        assert!(census(b"q 3 Tr Q (x) Tj").shows_text);
        assert!(!census(b"q 0 Tr Q 3 Tr 9 Tr (x) Tj").shows_text);
        // Past the names kept, what else is drawn is not told.
        let many: Vec<u8> = (0..=MAX_DRAWN_NAMES)
            .flat_map(|name| format!("/Fm{name} Do ").into_bytes())
            .collect();
        let crowded = census(&many);
        assert_eq!(crowded.drawn.len(), MAX_DRAWN_NAMES);
        assert!(crowded.more_drawn);
    }

    #[test]
    fn inline_images_are_passed_over_as_each_reader_passes_them() {
        // Data that holds `EI` and a `Tr`, of the length its entries give:
        // pdfium and lopdf both pass over it by that length, and read the
        // one `Tr` after it.
        let data = [&b"\nEI 3 Tr ("[..], &[b'x'; 30]].concat();
        let content = [
            &b"q BI /W 40 /H 1 /BPC 8 /CS /DeviceGray ID "[..],
            &data,
            b" EI Q BT 3 0 Tr (The fee) Tj ET",
        ]
        .concat();
        assert_eq!(
            viewer_modes(&content)
                .into_iter()
                .map(|(_, mode)| mode)
                .collect::<Vec<_>>(),
            [Some(0)]
        );
        assert_eq!(viewer_modes_read(&content, 1), Some(vec![Some(0)]));
        // pdfium reads `/G` as gray; lopdf, which does not, looks for the
        // first `EI` with white space around it, and reads the `Tr` in the
        // data, which pdfium does not.
        let data = [&b"x\nEI 3 Tr ("[..], &[b'x'; 29]].concat();
        let spelled = [
            &b"q BI /W 40 /H 1 /BPC 8 /CS /G ID "[..],
            &data,
            b" EI Q BT 3 0 Tr (The fee) Tj ET",
        ]
        .concat();
        let read: Vec<Vec<lopdf::Object>> = lopdf::content::Content::decode(&spelled)
            .unwrap()
            .operations
            .into_iter()
            .filter(|operation| operation.operator == "Tr")
            .map(|operation| operation.operands)
            .collect();
        assert_eq!(read, [vec![lopdf::Object::Integer(3)]]);
        assert_eq!(viewer_modes(&spelled).len(), 1);
        assert_eq!(viewer_modes_read(&spelled, 1), Some(vec![None]));
        // A filtered image's data is read as tokens up to `EI`.
        let filtered = b"BI /W 4 /H 4 /F /AHx ID 00ff00ff> EI 3 Tr";
        assert_eq!(viewer_modes(filtered).len(), 1);
        // A word other than `ID` among the entries stops pdfium's reading.
        assert!(viewer_modes(b"BI /W 4 Tj ID x EI 3 Tr").is_empty());
    }

    #[test]
    fn modes_a_viewer_sets_are_found_where_lopdf_reads_each_tr() {
        // lopdf reads `3Tr` as a number and a `Tr`, which pdfium reads as a
        // word of its own, setting no mode.
        let glued = b"BT 3Tr (The fee is not refundable) Tj 2 Tr ET";
        assert_eq!(viewer_modes_read(glued, 2), Some(vec![None, Some(2)]));
        assert_eq!(
            viewer_modes_read(b"BT 3 Tr 1e3 Tr ET", 2),
            Some(vec![Some(3), Some(0)])
        );
        // Where lopdf decodes more than are found, none are given.
        assert_eq!(viewer_modes_read(b"BT 3 Tr ET", 2), None);
        assert_eq!(viewer_modes_read(b"BT ET", 0), Some(Vec::new()));
    }

    #[test]
    fn render_modes_are_read_as_a_viewer_reads_them() {
        // As pdfium paints each: a number as a float cut to a whole number,
        // one at or past 2^31 as 0, and a mode past 7 leaving it as it was.
        for (word, mode) in [
            ("0", Some(0)),
            ("2.9", Some(2)),
            ("3.", Some(3)),
            (".5", Some(0)),
            ("-0.5", Some(0)),
            ("+2", Some(2)),
            ("00000000000000000002", Some(2)),
            ("--1", Some(0)),
            ("8", None),
            ("-1", None),
            ("2000000000", None),
            ("2147483645", Some(0)),
            ("4294967295", Some(0)),
            ("99999999999", Some(0)),
        ] {
            let modes = viewer_modes(format!("BT {word} Tr ET").as_bytes());
            assert_eq!(modes, [(3 + word.len() + 1, mode)], "{word}");
        }
        let modes = |content: &[u8]| -> Vec<Option<i64>> {
            viewer_modes(content)
                .into_iter()
                .map(|(_, mode)| mode)
                .collect()
        };
        // A number written against a letter is an operator of its own, and
        // the `Tr` after it has none, as a name is none.
        assert_eq!(modes(b"1e3 Tr 3e0 Tr /X Tr Tr"), [Some(0); 4]);
        assert_eq!(modes(b"(3) 3 Tr [1 Tr] Tr"), [Some(3), Some(0)]);
    }

    #[test]
    fn counting_stops_at_the_limit() {
        let content = b"q Q ".repeat(1_000);
        assert_eq!(operators(&content, usize::MAX), 2_000);
        assert_eq!(operators(&content, 11), 11);
    }
}
