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

/// Where an inline image whose `BI` ends at `at` ends: past the `EI` that
/// follows white space after its `ID`.
fn past_image(content: &[u8], at: usize) -> usize {
    let Some(data) = content[at..]
        .windows(3)
        .position(|word| word[..2] == *b"ID" && white(word[2]))
        .map(|found| at + found + 3)
    else {
        return content.len();
    };
    content[data..]
        .windows(4)
        .position(|word| white(word[0]) && word[1..3] == *b"EI" && white(word[3]))
        .map_or(content.len(), |found| data + found + 3)
}

/// The operators `content` holds, counted up to `limit`: each word of
/// letters, `*`, `'`, and `"` standing outside a string, a name, and a
/// comment, `true`, `false`, and `null` aside, with an inline image, from
/// its `BI` to its `EI`, counted once.
pub(crate) fn operators(content: &[u8], limit: usize) -> usize {
    let mut count = 0;
    let mut at = 0;
    while at < content.len() && count < limit {
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
                while at < content.len() && !white(content[at]) && !delimiter(content[at]) {
                    at += 1;
                }
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
                count += 1;
                if word == b"BI" && content.get(at).is_none_or(|byte| white(*byte)) {
                    at = past_image(content, at);
                }
            }
            _ => at += 1,
        }
    }
    count
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
            b"true false null 1 w",
        ] {
            assert_eq!(operators(content, usize::MAX), decoded(content));
        }
    }

    #[test]
    fn counting_stops_at_the_limit() {
        let content = b"q Q ".repeat(1_000);
        assert_eq!(operators(&content, usize::MAX), 2_000);
        assert_eq!(operators(&content, 11), 11);
    }
}
