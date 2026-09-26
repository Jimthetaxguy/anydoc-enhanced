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
/// finds its end: past the first `EI` with white space on either side,
/// looked for from the first byte after the white space following `BI`.
/// Where there is none, counting goes on from there, as it does, so that
/// the operators after an image written without that white space are
/// counted, which lopdf decodes all the same.
fn past_image(content: &[u8], mut at: usize) -> usize {
    while content.get(at).is_some_and(|&byte| white(byte)) {
        at += 1;
    }
    content[at..]
        .windows(4)
        .position(|word| white(word[0]) && word[1..3] == *b"EI" && white(word[3]))
        .map_or(at, |found| at + found + 3)
}

/// Hand `visit` each operator `content` holds, as lopdf reads one, with
/// the name standing last before it, if one does, until `visit` says to
/// stop: each word of letters, `*`, `'`, and `"` standing outside a
/// string, a name, and a comment, `true`, `false`, and `null` aside, with
/// an inline image, from its `BI` to its `EI`, one operator.
fn walk(content: &[u8], mut visit: impl FnMut(&[u8], Option<&[u8]>) -> bool) {
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
                if !visit(word, name.take().map(|(from, to)| &content[from..to])) {
                    return;
                }
                if word == b"BI" && content.get(at).is_none_or(|byte| white(*byte)) {
                    at = past_image(content, at);
                }
            }
            _ => at += 1,
        }
    }
}

/// The operators `content` holds, counted up to `limit` (see `walk`).
pub(crate) fn operators(content: &[u8], limit: usize) -> usize {
    let mut count = 0;
    if limit > 0 {
        walk(content, |_, _| {
            count += 1;
            count < limit
        });
    }
    count
}

/// XObjects drawn a content stream names, noted at most.
const MAX_DRAWN_NAMES: usize = 64;

/// What a content stream shows, read without decoding it: whether it shows
/// text, and, where it does not, the names of the XObjects it draws, the
/// first `MAX_DRAWN_NAMES` of them, some of which may.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Census {
    pub(crate) shows_text: bool,
    pub(crate) drawn: Vec<Vec<u8>>,
}

pub(crate) fn census(content: &[u8]) -> Census {
    let mut census = Census::default();
    walk(content, |operator, name| {
        match operator {
            b"Tj" | b"TJ" | b"'" | b"\"" => {
                census.shows_text = true;
                census.drawn.clear();
                return false;
            }
            b"Do" => {
                if let Some(name) =
                    name.filter(|name| !census.drawn.iter().any(|drawn| drawn == name))
                {
                    if census.drawn.len() < MAX_DRAWN_NAMES {
                        census.drawn.push(name.to_vec());
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

/// The modes a viewer sets at each `Tr` in `content`, in order, as pdfium
/// reads the stream (see `viewer_mode`): its words run to white space or a
/// delimiter, so a number written against a letter, as `1e3`, is an
/// operator of its own, where lopdf reads a number and an operator, and a
/// `Tr` after it has no operand.
pub(crate) fn viewer_modes(content: &[u8]) -> Vec<Option<i64>> {
    let mut modes = Vec::new();
    // The operand standing last: a number, or another object.
    let mut last: Option<Option<f32>> = None;
    let mut at = 0;
    while at < content.len() {
        match content[at] {
            byte if white(byte) => at += 1,
            b'%' => {
                while at < content.len() && !matches!(content[at], b'\r' | b'\n') {
                    at += 1;
                }
            }
            b'(' => {
                at = past_string(content, at);
                last = Some(None);
            }
            b'<' if content.get(at + 1) == Some(&b'<') => {
                // A dictionary, strings and all, to its closing brackets.
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
                                at += 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                    at += 1;
                }
                last = Some(None);
            }
            b'<' => {
                while at < content.len() && content[at] != b'>' {
                    at += 1;
                }
                at += 1;
                last = Some(None);
            }
            b'[' => {
                // An array, strings and all, to its closing bracket.
                let mut depth = 0usize;
                while at < content.len() {
                    match content[at] {
                        b'(' => {
                            at = past_string(content, at);
                            continue;
                        }
                        b'[' => depth += 1,
                        b']' => {
                            depth -= 1;
                            if depth == 0 {
                                at += 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                    at += 1;
                }
                last = Some(None);
            }
            b']' | b'{' | b'}' | b')' | b'>' => {
                at += 1;
                last = Some(None);
            }
            b'/' => {
                at += 1;
                while at < content.len() && !white(content[at]) && !delimiter(content[at]) {
                    at += 1;
                }
                last = Some(None);
            }
            _ => {
                let start = at;
                while at < content.len() && !white(content[at]) && !delimiter(content[at]) {
                    at += 1;
                }
                let word = &content[start..at];
                if word
                    .iter()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'+' | b'-' | b'.'))
                {
                    last = Some(Some(viewer_number(word)));
                } else if matches!(word, b"true" | b"false" | b"null") {
                    last = Some(None);
                } else {
                    if word == b"Tr" {
                        modes.push(viewer_mode(last.flatten()));
                    } else if word == b"BI" {
                        at = past_image(content, at);
                    }
                    last = None;
                }
            }
        }
    }
    modes
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
                drawn: Vec::new()
            }
        );
        let drawn = census(b"q /Fm1 Do /Im2 Do /Fm1 Do (x) pop Q");
        assert_eq!(
            drawn,
            Census {
                shows_text: false,
                drawn: vec![b"Fm1".to_vec(), b"Im2".to_vec()]
            }
        );
        // A string's text and a comment's are not operators.
        assert!(!census(b"% (a) Tj\n[(Tj)] pop").shows_text);
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
            assert_eq!(modes, [mode], "{word}");
        }
        // A number written against a letter is an operator of its own, and
        // the `Tr` after it has none, as a name is none.
        assert_eq!(viewer_modes(b"1e3 Tr 3e0 Tr /X Tr Tr"), [Some(0); 4]);
        assert_eq!(viewer_modes(b"(3) 3 Tr [1 Tr] Tr"), [Some(3), Some(0)]);
    }

    #[test]
    fn counting_stops_at_the_limit() {
        let content = b"q Q ".repeat(1_000);
        assert_eq!(operators(&content, usize::MAX), 2_000);
        assert_eq!(operators(&content, 11), 11);
    }
}
