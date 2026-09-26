//! Lines pdf-inspector 1.25.0 drops as running headers or footers.
//!
//! In a document of three pages or more, pdf-inspector drops a line from
//! every page but the first that shows it, as a running header or footer,
//! when it finds the line among the five highest or lowest lines of enough
//! pages (three, and three in ten of the document's) at about the same
//! height; on a page of ten lines or fewer, every line counts. It compares
//! lines with the digits at either end left out, so that "Page 2" and
//! "Page 3" count as one line, and it drops with a line the lines set at
//! its height. So "Account number 87654321" heading the pages of a second
//! account is dropped as the "Account number 12345678" heading the first
//! page is kept, and the second account's pages read as the first's; a
//! name set beside a repeated label goes the same way (open upstream issue
//! #483 reports pages lost to this rule).
//!
//! The check applies the rule to the lines pdf-inspector makes of each
//! page's text, without the images, links, and page numbers it sets aside
//! first, nor the lines the Markdown shows as table rows, and names a page
//! where a line dropped says what no line kept says and the Markdown does
//! not show it. Lines that repeat as they are, such as a bank's name, are
//! dropped by design and not named; nor are lines that differ from one kept
//! only in numbers that count the pages or tell the time (see `masked`).

use std::collections::{HashMap, HashSet};

/// How many of a page's highest and lowest distinct heights count as its
/// edges.
const EDGE_LINE_COUNT: usize = 5;
/// Characters a line needs, its end digits left out, to be dropped.
const MIN_REPEATED_CHARS: usize = 10;
/// Share of the page count a line must repeat on, besides three pages.
const REPEAT_PERCENT: u32 = 30;
/// Spread of a line's heights, as a share of the pages' average span, under
/// which it sits at one height.
const MAX_HEIGHT_SPREAD: f32 = 0.05;
/// Distinct texts of lines dropped, and of their twins, looked for in the
/// Markdown; past them, the pages after the last whose texts are looked for
/// are not named.
const MAX_SOUGHT_TEXTS: usize = 65_536;
/// How far a number at either end of a line may stand from its page's own
/// number, front matter aside, and count the pages.
const MAX_FOLIO_OFFSET: i64 = 1_000;
/// Words that set a number apart as the page's.
const PAGE_WORDS: [&str; 12] = [
    "page", "pages", "pg", "p", "pp", "seite", "página", "pagina", "blatt", "sheet", "folio",
    "side",
];
/// Words that set the number after them apart as something's own, such as a
/// check's or an invoice's, which may run with the pages a set distance
/// from theirs and still say what page it is not.
const LABEL_WORDS: [&str; 30] = [
    "no",
    "nr",
    "number",
    "num",
    "check",
    "cheque",
    "invoice",
    "inv",
    "account",
    "acct",
    "id",
    "ref",
    "reference",
    "order",
    "receipt",
    "claim",
    "policy",
    "loan",
    "case",
    "ticket",
    "voucher",
    "item",
    "unit",
    "suite",
    "form",
    "document",
    "file",
    "batch",
    "transaction",
    "confirmation",
];
/// Joining a line's parts, the gate's evaluations of the texts of the
/// bands it reads, at most; past them, it lets the pages be read again.
const MAX_GATE_WORK: usize = 20_000_000;

/// The lines of one page the rule reads: those among its highest or lowest
/// distinct heights, or every line of a page with few.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct PageLines {
    pub(crate) page: u32,
    /// The height between its highest and lowest line.
    pub(crate) span: f32,
    /// Its edge lines in reading order, by height and text.
    pub(crate) edges: Vec<(f32, String)>,
}

impl PageLines {
    /// The edge lines of one page's lines, as pdf-inspector groups them, in
    /// reading order.
    pub(crate) fn new(page: u32, lines: Vec<(f32, String)>) -> Self {
        let mut heights: Vec<f32> = lines.iter().map(|(y, _)| *y).collect();
        heights.sort_by(f32::total_cmp);
        heights.dedup();
        let span = match (heights.first(), heights.last()) {
            (Some(low), Some(high)) => high - low,
            _ => 0.0,
        };
        let at_edge = |y: f32| {
            if heights.len() <= EDGE_LINE_COUNT * 2 {
                return true;
            }
            heights
                .iter()
                .position(|height| (height - y).abs() < 0.1)
                .is_some_and(|rank| {
                    rank < EDGE_LINE_COUNT || rank >= heights.len() - EDGE_LINE_COUNT
                })
        };
        let edges = lines.into_iter().filter(|(y, _)| at_edge(*y)).collect();
        PageLines { page, span, edges }
    }
}

/// A line's text as the rule compares it: single spaces, with the digits at
/// either end left out.
fn compared(text: &str) -> String {
    let spaced = text.split_whitespace().collect::<Vec<_>>().join(" ");
    spaced
        .trim_start_matches(char::is_numeric)
        .trim_start()
        .trim_end_matches(char::is_numeric)
        .trim_end()
        .to_string()
}

/// Whether a line reads as a heading, a list item, or a marked note, which
/// the rule keeps.
fn structural(text: &str) -> bool {
    let text = text.trim_start();
    let numbered = text.chars().next().is_some_and(char::is_numeric)
        && (text.contains(". ") || text.contains(") "));
    let marked = text.strip_prefix('(').is_some_and(|rest| {
        let marker: String = rest.chars().take_while(|c| c.is_alphanumeric()).collect();
        !marker.is_empty()
            && marker.chars().count() <= 3
            && marker.chars().all(|c| c.is_numeric() || c.is_lowercase())
            && rest[marker.len()..].starts_with(')')
    });
    text.starts_with('#')
        || text.starts_with("- ")
        || text.starts_with("* ")
        || text.starts_with("• ")
        || numbered
        || marked
}

/// Whether a line is one character repeated, as a rule drawn in text is.
fn decorative(text: &str) -> bool {
    let mut characters = text.chars();
    characters
        .next()
        .is_some_and(|first| characters.all(|character| character == first))
}

/// Whether a text can repeat as a running header or footer.
fn repeatable(compared: &str) -> bool {
    compared.chars().count() >= MIN_REPEATED_CHARS && !decorative(compared)
}

/// The height bucket the rule groups a page's lines by.
fn bucket(y: f32) -> i32 {
    (y * 10.0).round() as i32
}

/// Months, whose day a number after or before them gives, not a page.
const MONTHS: [&str; 24] = [
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
    "jan",
    "feb",
    "mar",
    "apr",
    "jun",
    "jul",
    "aug",
    "sep",
    "sept",
    "oct",
    "nov",
    "dec",
];
/// The largest roman numeral read as a folio, as front matter numbers its
/// pages.
const MAX_ROMAN_FOLIO: i64 = 100;

/// What a piece of a line is.
#[derive(Clone, Copy, PartialEq)]
enum Piece {
    /// ASCII digits, with their value where it fits.
    Digits(Option<i64>),
    /// A roman numeral standing as a word, lower case or not.
    Roman(i64, bool),
    Word,
    Other,
}

/// The value of a roman numeral written as it is meant to be, in one case.
fn roman(word: &str) -> Option<(i64, bool)> {
    let lower = word.chars().all(|character| "ivxlcdm".contains(character));
    let upper = word.chars().all(|character| "IVXLCDM".contains(character));
    if word.is_empty() || !(lower || upper) || word.len() > 12 {
        return None;
    }
    let value_of = |character: char| match character.to_ascii_lowercase() {
        'i' => 1,
        'v' => 5,
        'x' => 10,
        'l' => 50,
        'c' => 100,
        'd' => 500,
        _ => 1000,
    };
    let values: Vec<i64> = word.chars().map(value_of).collect();
    let value: i64 = values
        .iter()
        .enumerate()
        .map(|(index, value)| match values.get(index + 1) {
            Some(next) if next > value => -value,
            _ => *value,
        })
        .sum();
    // Only the numeral's own spelling of its value counts.
    let mut spelled = String::new();
    let mut left = value;
    for (step, letters) in [
        (1000, "m"),
        (900, "cm"),
        (500, "d"),
        (400, "cd"),
        (100, "c"),
        (90, "xc"),
        (50, "l"),
        (40, "xl"),
        (10, "x"),
        (9, "ix"),
        (5, "v"),
        (4, "iv"),
        (1, "i"),
    ] {
        while left >= step {
            spelled.push_str(letters);
            left -= step;
        }
    }
    (value > 0 && spelled == word.to_ascii_lowercase()).then_some((value, lower))
}

/// A line's pieces: runs of ASCII digits, words, and what lies between.
fn pieces(text: &str) -> Vec<(&str, Piece)> {
    let kind = |character: char| {
        if character.is_ascii_digit() {
            0
        } else if character.is_alphabetic() {
            1
        } else {
            2
        }
    };
    let mut pieces = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((index, character)) = chars.next() {
        let this = kind(character);
        if chars.peek().is_some_and(|(_, next)| kind(*next) == this) {
            continue;
        }
        let end = index + character.len_utf8();
        let piece = &text[start..end];
        let what = match this {
            0 => Piece::Digits(piece.parse().ok()),
            1 => match roman(piece) {
                Some((value, lower)) => Piece::Roman(value, lower),
                None => Piece::Word,
            },
            _ => Piece::Other,
        };
        pieces.push((piece, what));
        start = end;
    }
    pieces
}

/// A line's text as the check compares lines dropped with lines kept, with
/// the numbers that count its pages or tell the time masked, so that lines
/// alike but for them compare equal; and whether the line says nothing but
/// such numbers and the words around them. Masked are:
///
/// - both numbers counting "3 of 7" or "3/7", which restart where documents
///   are bundled;
/// - the parts of a clock time, "10:32:07";
/// - a number, or a roman numeral, after a page word ("page 3", "p. iv"),
///   set apart by dashes ("- 3 -"), or set apart at either end of the line
///   or alone on it, as a folio is, when it stands a set distance from the
///   page's number: masked with that distance, so that "Annual report 17"
///   on page 17 reads as page 1's "Annual report 1", while counts after a
///   page word ("page 25" on page 2), sequential invoice or check numbers
///   far from the page's, and a month's days are not masked. After a page
///   word or between dashes, a number no higher than the page's is masked
///   as it is, since a bundle's documents number their pages afresh.
fn masked(text: &str, page: u32) -> (String, bool) {
    let text = text.trim();
    let pieces = pieces(text);
    let number = |index: usize| -> Option<i64> {
        match pieces.get(index)?.1 {
            Piece::Digits(value) => value,
            Piece::Roman(value, _) => Some(value),
            _ => None,
        }
    };
    let spaced = |piece: &str| piece.chars().all(char::is_whitespace);
    // The pieces beside `index` past white space, backward or forward.
    let beside = |index: usize, forward: bool| -> Option<usize> {
        let mut at = index;
        loop {
            at = if forward { at + 1 } else { at.checked_sub(1)? };
            let (piece, _) = pieces.get(at)?;
            if !spaced(piece) {
                return Some(at);
            }
        }
    };
    let word_is = |index: Option<usize>, words: &[&str]| {
        index
            .and_then(|index| pieces.get(index))
            .is_some_and(|(piece, what)| {
                *what != Piece::Other && words.contains(&piece.to_lowercase().as_str())
            })
    };
    let other_is = |index: Option<usize>, test: &dyn Fn(&str) -> bool| {
        index
            .and_then(|index| pieces.get(index))
            .is_some_and(|(piece, what)| *what == Piece::Other && test(piece))
    };
    // A page word before the piece at `index`: `Some(false)` where white
    // space or a mark such as "." or ":" sets it apart, `Some(true)` where
    // it is joined to the number, as in "p2" (a field named for its page)
    // or "Plan P2".
    let page_word_at = |index: usize| {
        let glued = index
            .checked_sub(1)
            .and_then(|before| pieces.get(before))
            .is_some_and(|(_, what)| *what != Piece::Other);
        let before = beside(index, false).and_then(|before| {
            if other_is(Some(before), &|piece| {
                piece
                    .trim()
                    .chars()
                    .all(|mark| matches!(mark, '.' | ':' | '#'))
            }) {
                beside(before, false)
            } else {
                Some(before)
            }
        });
        word_is(before, &PAGE_WORDS).then_some(glued)
    };
    let paged_at = |index: usize| page_word_at(index) == Some(false);
    // Pieces that frame a number without saying anything: white space,
    // dashes, brackets, and bars.
    let framing = |at: usize| {
        pieces.get(at).is_some_and(|(piece, what)| {
            *what == Piece::Other
                && piece.chars().all(|mark| {
                    mark.is_whitespace()
                        || matches!(
                            mark,
                            '-' | '\u{2013}' | '\u{2014}' | '(' | ')' | '[' | ']' | '|' | '.'
                        )
                })
        })
    };
    // Numbers that count: "3 of 7", "3/7" but not a date's "3/7/2025", after
    // a page word or standing alone, as "Closing date 1/31" and "Loan 2 of 3"
    // do not.
    let mut counting: HashSet<usize> = HashSet::new();
    for index in 0..pieces.len() {
        let (Some(first), Some(between)) = (number(index), beside(index, true)) else {
            continue;
        };
        let slash = pieces[between].0.trim() == "/";
        if !(slash || word_is(Some(between), &["of", "von", "de"])) {
            continue;
        }
        let Some(second_at) = beside(between, true) else {
            continue;
        };
        let Some(second) = number(second_at) else {
            continue;
        };
        let chained = |at: Option<usize>| other_is(at, &|piece| piece.contains('/'));
        let alone = (0..index).all(framing) && (second_at + 1..pieces.len()).all(framing);
        if first <= second
            && !(slash && (chained(beside(second_at, true)) || chained(index.checked_sub(1))))
            && (paged_at(index) || alone)
        {
            counting.extend([index, second_at]);
        }
    }
    let mut masked = String::with_capacity(text.len());
    let mut counts_only = true;
    for (index, (piece, what)) in pieces.iter().enumerate() {
        let value = match what {
            Piece::Digits(value) => *value,
            Piece::Roman(value, _) => Some(*value),
            Piece::Word => {
                counts_only &= PAGE_WORDS.contains(&piece.to_lowercase().as_str())
                    || ["of", "von", "de"].contains(&piece.to_lowercase().as_str());
                masked.push_str(piece);
                continue;
            }
            Piece::Other => {
                masked.push_str(piece);
                continue;
            }
        };
        if counting.contains(&index) {
            masked.push('#');
            continue;
        }
        let digits = matches!(what, Piece::Digits(_));
        let before = beside(index, false);
        let after = beside(index, true);
        let colon = |piece: &str| piece == ":";
        let clock = digits
            && piece.len() <= 2
            && ((other_is(index.checked_sub(1), &colon)
                && index.checked_sub(2).and_then(number).is_some())
                || (other_is(Some(index + 1), &colon) && number(index + 2).is_some()));
        if clock {
            masked.push('#');
            continue;
        }
        let dashes = ['-', '\u{2013}', '\u{2014}'];
        let paged = paged_at(index);
        // Joined to a page word, a number counts the pages only where it is
        // the page's own: "p2.holder" on page 2, not "Plan P2" on page 6.
        let own_page = page_word_at(index) == Some(true) && value == Some(i64::from(page));
        // A label such as "Check", "No.", or "#" before the number.
        let labelled = other_is(before, &|piece| piece.trim() == "#")
            || word_is(
                before.and_then(|before| {
                    if other_is(Some(before), &|piece| {
                        piece
                            .trim()
                            .chars()
                            .all(|mark| matches!(mark, '.' | ':' | '#'))
                    }) {
                        beside(before, false)
                    } else {
                        Some(before)
                    }
                }),
                &LABEL_WORDS,
            );
        // Dashes framing a number alone, not joining it to other digits as
        // a date's do.
        let dash = |piece: &str| {
            piece.contains(dashes)
                && piece
                    .chars()
                    .all(|mark| mark.is_whitespace() || dashes.contains(&mark))
        };
        let framed = other_is(index.checked_sub(1), &dash)
            && other_is(Some(index + 1), &dash)
            && index.checked_sub(2).and_then(number).is_none()
            && number(index + 2).is_none();
        let apart = |at: Option<usize>| {
            at.is_none_or(|at| {
                pieces[at].1 == Piece::Other
                    && pieces[at].0.chars().all(|mark| {
                        mark.is_whitespace() || matches!(mark, '|' | '\u{B7}' | '\u{2022}')
                    })
            })
        };
        let dated = word_is(before, &MONTHS) || word_is(after, &MONTHS);
        let at_end = (index == 0 && apart(index.checked_add(1).filter(|at| *at < pieces.len())))
            || (index + 1 == pieces.len() && apart(index.checked_sub(1)));
        let folio = at_end
            && !dated
            && !labelled
            && match what {
                Piece::Roman(value, lower) => *lower && *value <= MAX_ROMAN_FOLIO,
                _ => true,
            };
        let offset = value
            .map(|value| value - i64::from(page))
            .filter(|offset| offset.abs() <= MAX_FOLIO_OFFSET);
        // After a page word or between dashes, a number no higher than the
        // page's own numbers it too, as a bundle's documents restart, or as
        // pages numbered from 0 run a page behind.
        let restarted =
            (paged || framed) && value.is_some_and(|value| (0..=i64::from(page)).contains(&value));
        match offset.filter(|_| paged || framed || folio) {
            _ if restarted || own_page => masked.push('#'),
            Some(offset) => masked.push_str(&format!("#{offset}#")),
            None => {
                counts_only = false;
                masked.push_str(piece);
            }
        }
    }
    (masked, counts_only && !text.is_empty())
}

/// Bytes of a link's target in the Markdown read past, at most, after its
/// text (`[text](target)`).
const MAX_LINK_TARGET: usize = 2_048;

/// A text with its white space, table pipes, emphasis, escape, and link
/// marks, heading marks, underline, strike, script, and break tags, and
/// links' targets left out, as it is looked for in the Markdown; its
/// superscript and subscript digits read as digits, set apart from what
/// stands before them, its ligatures as their letters, and the bullets
/// pdf-inspector writes as a list's "-" as that; and, by byte, whether
/// anything was left out just before it.
fn bare_marked(text: &str) -> (String, Vec<bool>) {
    const TAGS: [&str; 10] = [
        "<u>", "</u>", "<br>", "<br/>", "<s>", "</s>", "<sup>", "</sup>", "<sub>", "</sub>",
    ];
    let mut bare = String::with_capacity(text.len());
    let mut breaks = Vec::with_capacity(text.len());
    let mut broken = false;
    let mut rest = text;
    while let Some(character) = rest.chars().next() {
        if character == '<' {
            if let Some(tag) = TAGS.iter().find(|tag| rest.starts_with(**tag)) {
                rest = &rest[tag.len()..];
                broken = true;
                continue;
            }
        }
        // A link's target, after its text.
        if rest.starts_with("](") {
            let target = &rest[2..rest.len().min(2 + MAX_LINK_TARGET)];
            if let Some(end) = target.find(')') {
                rest = &rest[2 + end + 1..];
                broken = true;
                continue;
            }
        }
        rest = &rest[character.len_utf8()..];
        if character.is_whitespace()
            || matches!(character, '|' | '*' | '_' | '`' | '\\' | '#' | '[' | ']')
        {
            broken = true;
            continue;
        }
        let (read, count, apart) = folded(character);
        for (index, character) in read[..count].iter().enumerate() {
            bare.push(*character);
            breaks.push(index == 0 && (broken || apart));
            breaks.extend(std::iter::repeat_n(false, character.len_utf8() - 1));
        }
        broken = false;
    }
    (bare, breaks)
}

/// A character as it is looked for (see `bare_marked`): the characters it
/// reads as, how many, and whether it is set apart from what stands before
/// it.
fn folded(character: char) -> ([char; 3], usize, bool) {
    let digit = |value: u32| char::from_digit(value, 10).unwrap_or('0');
    let script = |value: u32| ([digit(value), ' ', ' '], 1, true);
    let letters = |text: &str| {
        let mut read = [' '; 3];
        let mut count = 0;
        for (slot, letter) in read.iter_mut().zip(text.chars()) {
            *slot = letter;
            count += 1;
        }
        (read, count, false)
    };
    match character {
        '\u{00B9}' => script(1),
        '\u{00B2}' | '\u{00B3}' => script(u32::from(character) - 0xB0),
        '\u{2070}' | '\u{2074}'..='\u{2079}' => script(u32::from(character) - 0x2070),
        '\u{2080}'..='\u{2089}' => script(u32::from(character) - 0x2080),
        '\u{FB00}' => letters("ff"),
        '\u{FB01}' => letters("fi"),
        '\u{FB02}' => letters("fl"),
        '\u{FB03}' => letters("ffi"),
        '\u{FB04}' => letters("ffl"),
        '\u{FB05}' | '\u{FB06}' => letters("st"),
        '\u{2022}' | '\u{25CB}' | '\u{25CF}' | '\u{25E6}' => (['-', ' ', ' '], 1, false),
        other => ([other, ' ', ' '], 1, false),
    }
}

/// A text as it is looked for in the Markdown (see `bare_marked`).
pub(crate) fn bare(text: &str) -> String {
    bare_marked(text).0
}

/// The rows of the Markdown's tables, and their cells, bare: pdf-inspector
/// sets the items it reads as a table's aside before it looks for running
/// headers, and a line of the page's is one of them where it is a row, or
/// where each of its items is a cell, as when two tables share the line.
pub(crate) fn table_rows(markdown: &str) -> (HashSet<String>, HashSet<String>) {
    let (mut rows, mut cells) = (HashSet::new(), HashSet::new());
    for line in markdown.lines().map(str::trim) {
        if !(line.starts_with('|') && line.ends_with('|'))
            || line
                .chars()
                .all(|character| matches!(character, '|' | '-' | ':' | ' '))
        {
            continue;
        }
        cells.extend(line.split('|').map(bare).filter(|cell| !cell.is_empty()));
        let row = bare(line);
        if !row.is_empty() {
            rows.insert(row);
        }
    }
    (rows, cells)
}

/// Bytes of the texts looked for with one automaton: past them, the texts
/// are looked for a part at a time, so that the automaton's memory stays
/// bounded however much text is looked for.
const MAX_PATTERN_BYTES: usize = 1 << 20;
/// Matches read in all: past them, the texts not yet found are taken as not
/// shown, as in a Markdown repeating one text over and over.
const MAX_MATCHES: usize = 20_000_000;

/// The texts of `patterns`, bare, that the Markdown shows, each where it
/// does not run on into a number: a text starting with a digit found after
/// none, and one ending with a digit found before none, but where white
/// space or a mark stood between them.
pub(crate) fn found_in<'a>(patterns: &[&'a str], markdown: &str) -> HashSet<&'a str> {
    let mut found = HashSet::new();
    if patterns.is_empty() {
        return found;
    }
    let (haystack, breaks) = bare_marked(markdown);
    let bytes = haystack.as_bytes();
    let joined = |at: usize| at > 0 && at < bytes.len() && !breaks[at];
    let mut matches = 0usize;
    let mut rest = patterns;
    while !rest.is_empty() && matches < MAX_MATCHES {
        let mut size = 0usize;
        let take = rest
            .iter()
            .take_while(|pattern| {
                size += pattern.len();
                size <= MAX_PATTERN_BYTES
            })
            .count()
            .max(1);
        let (part, next) = rest.split_at(take);
        rest = next;
        let Ok(automaton) = aho_corasick::AhoCorasick::new(part) else {
            continue;
        };
        for shown in automaton.find_overlapping_iter(&haystack) {
            matches += 1;
            if matches > MAX_MATCHES {
                break;
            }
            let pattern = part[shown.pattern().as_usize()];
            let (start, end) = (shown.start(), shown.end());
            // A digit at either end of the text that another digit joins.
            let runs_on_before = pattern.as_bytes().first().is_some_and(u8::is_ascii_digit)
                && joined(start)
                && bytes[start - 1].is_ascii_digit();
            let runs_on_after = pattern.as_bytes().last().is_some_and(u8::is_ascii_digit)
                && joined(end)
                && bytes[end].is_ascii_digit();
            if !(runs_on_before || runs_on_after) {
                found.insert(pattern);
            }
        }
    }
    found
}

/// Pages the running-header check reads, at most: the page scan's runs
/// for the gate, and pdf-inspector's lines past it.
pub(crate) const MAX_REPEAT_PAGES: usize = 2_000;
/// Heights at either edge of a page the page scan keeps runs at for the
/// gate, more than the rule reads, since the Markdown's table rows are set
/// aside after; and the bands at either edge the gate reads, more than the
/// rule's five, since it reads runs, not pdf-inspector's lines.
pub(crate) const SCAN_EDGE_HEIGHTS: usize = 12;
const GATE_EDGE_LINES: usize = 8;

/// A run of text a page shows, as the page scan reads it: where it starts,
/// the way its baseline runs on the page, its size, and its text, where its
/// font can be read.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct EdgeRun {
    pub(crate) y: f32,
    pub(crate) x: f32,
    pub(crate) direction: [f32; 2],
    pub(crate) size: f32,
    pub(crate) text: Option<String>,
}

/// The height, to the point, the gate sets a run at.
/// The lines pdf-inspector makes of a page's runs, top to bottom, as the
/// indexes of their runs: taken from the top, and from the left along a
/// height, a run goes on the line above when it sits less than 3 points
/// from that line's first run, unless, set more than half a point apart
/// from it, it starts where that run starts, as a line below it at the
/// margin does, or well left of the line's last run.
fn lines_of(runs: &[EdgeRun]) -> Vec<Vec<usize>> {
    let mut order: Vec<usize> = (0..runs.len()).collect();
    order.sort_by(|&one, &other| {
        runs[other]
            .y
            .total_cmp(&runs[one].y)
            .then(runs[one].x.total_cmp(&runs[other].x))
    });
    let mut lines: Vec<Vec<usize>> = Vec::new();
    for index in order {
        let run = &runs[index];
        let joins = lines.last().is_some_and(|line| {
            let (first, last) = (&runs[line[0]], &runs[line[line.len() - 1]]);
            let apart = (first.y - run.y).abs();
            apart < 3.0
                && !(apart > 0.5 && ((run.x - first.x).abs() < 5.0 || run.x < last.x - 10.0))
        });
        match lines.last_mut().filter(|_| joins) {
            Some(line) => line.push(index),
            None => lines.push(vec![index]),
        }
    }
    lines
}

/// How pdf-inspector turns a page whose runs mostly read up or down it so
/// that they read along x, as a point on the page is turned: where two
/// thirds or more of the runs set within 20 degrees of an axis are set
/// along the page's height, the way more of them read (see
/// `correct_rotated_page`, which counts shows where this counts runs).
pub(crate) fn page_turn(runs: &[EdgeRun]) -> fn([f32; 2]) -> [f32; 2] {
    const TAN_20_DEG: f32 = 0.364;
    let (mut across, mut up, mut down) = (0usize, 0usize, 0usize);
    for run in runs {
        let [a, b] = run.direction;
        if b.abs() <= a.abs() * TAN_20_DEG {
            across += 1;
        } else if a.abs() <= b.abs() * TAN_20_DEG {
            if b > 0.0 {
                up += 1;
            } else {
                down += 1;
            }
        }
    }
    let turned = up + down;
    if across + turned < 2 || turned * 3 < (across + turned) * 2 || up == down {
        |[x, y]| [x, y]
    } else if up > down {
        |[x, y]| [y, -x]
    } else {
        |[x, y]| [-y, x]
    }
}

/// Whether pdf-inspector takes the order runs are shown in as too jumbled
/// to read lines in, going by where they sit: five runs or more, among
/// whose moves of more than 50 points from one to the next there are three
/// or more, more than four in ten of them up the page
/// (`should_use_y_sorting`).
fn jumbled(at: &[[f32; 2]]) -> bool {
    if at.len() < 5 {
        return false;
    }
    let (mut up, mut down) = (0usize, 0usize);
    for pair in at.windows(2) {
        let moved = pair[1][1] - pair[0][1];
        if moved > 50.0 {
            up += 1;
        } else if moved < -50.0 {
            down += 1;
        }
    }
    up + down >= 3 && up as f32 / (up + down) as f32 > 0.4
}

/// The lines pdf-inspector makes of a page's runs, in the order it reads
/// them, as the indexes of their runs (`group_single_column`): the page
/// turned where its runs mostly read up or down it (see `page_turn`), the
/// runs taken in the order shown, or from the top and then the left where
/// that order is jumbled (see `jumbled`) or `from_the_top`, a run going on
/// the line before when it sits less than 3 points from that line's first
/// run, unless, set more than half a point apart from it, it starts where
/// that run starts or well left of the line's last run; each line then
/// read left to right, or right to left where its runs are upside down,
/// runs starting together in the order shown.
fn reading_lines(runs: &[EdgeRun], from_the_top: bool) -> Vec<Vec<usize>> {
    let turn = page_turn(runs);
    let at: Vec<[f32; 2]> = runs.iter().map(|run| turn([run.x, run.y])).collect();
    let mut order: Vec<usize> = (0..runs.len()).collect();
    if from_the_top || jumbled(&at) {
        order.sort_by(|&one, &other| {
            at[other][1]
                .total_cmp(&at[one][1])
                .then(at[one][0].total_cmp(&at[other][0]))
        });
    }
    let mut lines: Vec<Vec<usize>> = Vec::new();
    for index in order {
        let [x, y] = at[index];
        let joins = lines.last().is_some_and(|line| {
            let [first_x, first_y] = at[line[0]];
            let last_x = at[line[line.len() - 1]][0];
            let apart = (first_y - y).abs();
            apart < 3.0 && !(apart > 0.5 && ((x - first_x).abs() < 5.0 || x < last_x - 10.0))
        });
        match lines.last_mut().filter(|_| joins) {
            Some(line) => line.push(index),
            None => lines.push(vec![index]),
        }
    }
    for line in &mut lines {
        let upside_down = line.iter().all(|&index| {
            let [a, b] = turn(runs[index].direction);
            a < 0.0 && b.abs() < a.abs()
        });
        line.sort_by(|&one, &other| {
            let (one, other) = (at[one][0], at[other][0]);
            if upside_down {
                other.total_cmp(&one)
            } else {
                one.total_cmp(&other)
            }
        });
    }
    lines
}

/// Characters taken on either side of a short run of text, to look for
/// the run where it stands.
const CONTEXT_CHARS: usize = 8;
/// Characters that must stand on a side of a short run for it to be looked
/// for with them.
const MIN_CONTEXT_CHARS: usize = 2;
/// Runs a span may reach over, at most.
const MAX_SPAN_RUNS: usize = 64;

/// The texts to look for of each of `spans` of `runs`, bare, as
/// pdf-inspector reads the page's lines, one after another (see
/// `reading_lines`), in the order shown and from the top, as it may yet
/// set a line read in the order shown beside another at its height: a
/// span read from its first run to its last, from its leftmost character
/// on its lines to its rightmost; and, where the span is to be read
/// `beside` what stands with it, that text with the `CONTEXT_CHARS`
/// characters before it, and, apart, with as many after it, from the lines
/// before and after where its own ends, but not past a run whose text
/// cannot be read. None for a span a run of which cannot be read, or,
/// beside, with nothing on either side.
pub(crate) fn line_contexts(runs: &[EdgeRun], spans: &[(usize, usize, bool)]) -> Vec<Vec<String>> {
    let mut contexts: Vec<Vec<String>> = vec![Vec::new(); spans.len()];
    if spans.is_empty() {
        return contexts;
    }
    for from_the_top in [false, true] {
        let read = read_in_order(runs, &reading_lines(runs, from_the_top), spans);
        for (texts, more) in contexts.iter_mut().zip(read) {
            for text in more {
                if !texts.contains(&text) {
                    texts.push(text);
                }
            }
        }
    }
    contexts
}

/// The texts to look for of each of `spans` of `runs`, as the page reads in
/// `lines` (see `line_contexts`).
fn read_in_order(
    runs: &[EdgeRun],
    lines: &[Vec<usize>],
    spans: &[(usize, usize, bool)],
) -> Vec<Vec<String>> {
    // The page's text as read, bare, a gap where a run cannot be read, and
    // where each run's text starts and ends in it, in characters.
    let mut text: Vec<Option<char>> = Vec::new();
    let mut places: Vec<Option<(usize, usize)>> = vec![None; runs.len()];
    for &index in lines.iter().flatten() {
        let start = text.len();
        match runs[index].text.as_deref() {
            Some(read) => text.extend(bare(read).chars().map(Some)),
            None => text.push(None),
        }
        places[index] = Some((start, text.len()));
    }
    let read =
        |range: std::ops::Range<usize>| -> Option<String> { text[range].iter().copied().collect() };
    spans
        .iter()
        .map(|&(first, last, beside)| {
            if last < first || last - first > MAX_SPAN_RUNS || last >= runs.len() {
                return Vec::new();
            }
            let (from, to) = places[first..=last]
                .iter()
                .flatten()
                .fold((usize::MAX, 0), |(from, to), &(start, end)| {
                    (from.min(start), to.max(end))
                });
            let Some(span) = (from < to).then(|| read(from..to)).flatten() else {
                return Vec::new();
            };
            if !beside {
                // With a character on either side as well, so that a span
                // joined to digits on both sides, as a run slipped into a
                // number is, is found where it stands.
                let wider = (from > 0 && to < text.len())
                    .then(|| read(from - 1..to + 1))
                    .flatten();
                return [Some(span), wider].into_iter().flatten().collect();
            }
            let before = text[..from]
                .iter()
                .rev()
                .take(CONTEXT_CHARS)
                .take_while(|character| character.is_some())
                .count();
            let after = text[to..]
                .iter()
                .take(CONTEXT_CHARS)
                .take_while(|character| character.is_some())
                .count();
            // Whole, where there is text on both sides, as a digit slipped
            // between two others is found only whole; and each side apart,
            // where the other reads otherwise.
            let (before, after) = (
                (before >= MIN_CONTEXT_CHARS).then_some(before),
                (after >= MIN_CONTEXT_CHARS).then_some(after),
            );
            [
                before
                    .zip(after)
                    .and_then(|(before, after)| read(from - before..to + after)),
                before.and_then(|before| read(from - before..to)),
                after.and_then(|after| read(from..to + after)),
            ]
            .into_iter()
            .flatten()
            .collect()
        })
        .collect()
}

/// Whether the Markdown shows a line of `runs` as a table row: whole, or
/// each part as a cell of one. A run whose text the scan could not read
/// leaves it undecided, not a row.
fn is_table_row(runs: &[&EdgeRun], rows: &HashSet<String>, cells: &HashSet<String>) -> bool {
    if runs.iter().any(|run| run.text.is_none()) {
        return false;
    }
    let parts: Vec<String> = runs
        .iter()
        .map(|run| bare(run.text.as_deref().unwrap_or_default().trim()))
        .collect();
    rows.contains(&parts.concat())
        || parts
            .iter()
            .all(|part| part.is_empty() || cells.contains(part))
}

/// The rows and the cells of the Markdown's tables (see `table_rows`).
pub(crate) type Tables<'a> = (&'a HashSet<String>, &'a HashSet<String>);

/// The runs of a page on its `SCAN_EDGE_HEIGHTS` highest and lowest lines,
/// as pdf-inspector makes them (see `lines_of`), lines the Markdown shows
/// as table rows (`tables`) aside; or all of them on a page of few.
pub(crate) fn edge_runs(runs: Vec<EdgeRun>, tables: Option<Tables<'_>>) -> Vec<EdgeRun> {
    let lines: Vec<Vec<usize>> = lines_of(&runs)
        .into_iter()
        .filter(|line| {
            tables.is_none_or(|(rows, cells)| {
                let line: Vec<&EdgeRun> = line.iter().map(|&index| &runs[index]).collect();
                !is_table_row(&line, rows, cells)
            })
        })
        .collect();
    if lines.len() <= SCAN_EDGE_HEIGHTS * 2 {
        return runs;
    }
    let kept: HashSet<usize> = lines[..SCAN_EDGE_HEIGHTS]
        .iter()
        .chain(&lines[lines.len() - SCAN_EDGE_HEIGHTS..])
        .flatten()
        .copied()
        .collect();
    runs.into_iter()
        .enumerate()
        .filter(|(index, _)| kept.contains(index))
        .map(|(_, run)| run)
        .collect()
}

/// A run's text as the gate reads it: as it is, masked (see `masked`), and
/// whether it says nothing but numbers that count the pages.
type RunText = (String, String, bool);

/// Whether pdf-inspector may drop, as a running header or footer, a line
/// that says what no line it keeps says, going by the runs the page scan
/// read at the edges of `pages`, so that the pages are worth reading again
/// for its rule: some text found at the edges of enough pages, its end
/// digits left out, sits beside a run that the first page showing it does
/// not show, but for numbers that count the pages or tell the time. The
/// gate reads a band's text whole, in parts set far apart, and run by run,
/// as the lines pdf-inspector may make of it, and sets aside the bands the
/// Markdown shows as table rows (`rows`, `cells`); where the scan could not
/// read a run's text, it may drop one.
pub(crate) fn may_drop(
    pages: &[(u32, Vec<EdgeRun>)],
    page_count: u32,
    rows: &HashSet<String>,
    cells: &HashSet<String>,
) -> bool {
    if page_count < 3 {
        return false;
    }
    let threshold = 3.max(page_count * REPEAT_PERCENT / 100) as usize;
    // Each band read: its page, and each run's text, as it is and masked,
    // with whether it says nothing but numbers that count the pages.
    let mut bands: Vec<(u32, Vec<RunText>)> = Vec::new();
    let mut keyed: HashMap<String, Vec<usize>> = HashMap::new();
    for (page, runs) in pages {
        if runs.iter().any(|run| run.text.is_none()) {
            return true;
        }
        let text = |run: &EdgeRun| run.text.as_deref().unwrap_or_default().trim().to_string();
        let on_page: Vec<Vec<&EdgeRun>> = lines_of(runs)
            .into_iter()
            .map(|line| {
                line.into_iter()
                    .map(|index| &runs[index])
                    .collect::<Vec<_>>()
            })
            .filter(|band| !is_table_row(band, rows, cells))
            .collect();
        let count = on_page.len();
        for (rank, mut band) in on_page.into_iter().enumerate() {
            if count > GATE_EDGE_LINES * 2
                && rank >= GATE_EDGE_LINES
                && rank < count - GATE_EDGE_LINES
            {
                continue;
            }
            band.sort_by(|one, other| one.x.total_cmp(&other.x));
            let joined = |runs: &[&EdgeRun]| {
                runs.iter()
                    .map(|run| text(run))
                    .filter(|text| !text.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            // The band, its parts set far apart, and each of its runs.
            let mut texts = vec![joined(&band)];
            let mut start = 0;
            for index in 1..=band.len() {
                let apart = index == band.len() || {
                    let (before, run) = (band[index - 1], band[index]);
                    let width = text(before).chars().count() as f32 * before.size * 0.5;
                    run.x - (before.x + width) > 2.0 * before.size.max(run.size)
                };
                if apart {
                    texts.push(joined(&band[start..index]));
                    start = index;
                }
            }
            texts.extend(band.iter().map(|run| text(run)));
            // A band shown a glyph or a few at a time is read whole as well,
            // its runs joined without spaces, as pdf-inspector joins glyphs
            // set close.
            let glyphs = band
                .iter()
                .map(|run| text(run).chars().count())
                .sum::<usize>()
                <= 3 * band.len();
            let tight: String = band.iter().map(|run| text(run)).collect();
            if glyphs && band.len() > 1 {
                texts.push(tight.clone());
            }
            let index = bands.len();
            let mut read: Vec<RunText> = band
                .iter()
                .map(|run| {
                    let (masked, counts_only) = masked(&text(run), *page);
                    (text(run), masked, counts_only)
                })
                .collect();
            if glyphs && band.len() > 1 {
                let (masked, counts_only) = masked(&tight, *page);
                read.push((tight, masked, counts_only));
            }
            bands.push((*page, read));
            let mut keys: HashSet<String> = HashSet::new();
            for text in texts {
                let key = compared(&text);
                if !structural(text.trim()) && repeatable(&key) {
                    keys.insert(key);
                }
            }
            for key in keys {
                keyed.entry(key).or_default().push(index);
            }
        }
    }
    // Keys found in the same bands say the same of them: each list of bands
    // is weighed once, and past the work allowed the gate opens.
    let mut weighed: HashSet<&[usize]> = HashSet::new();
    let mut work = 0usize;
    keyed.values().any(|occurrences| {
        if !weighed.insert(occurrences.as_slice()) {
            return false;
        }
        work += occurrences
            .iter()
            .map(|&band| bands[band].1.len() + 1)
            .sum::<usize>();
        if work > MAX_GATE_WORK {
            return true;
        }
        let pages: HashSet<u32> = occurrences.iter().map(|&band| bands[band].0).collect();
        if pages.len() < threshold {
            return false;
        }
        let first = pages.iter().min().copied().unwrap_or_default();
        let shown: HashSet<&str> = occurrences
            .iter()
            .filter(|&&band| bands[band].0 == first)
            .flat_map(|&band| {
                bands[band]
                    .1
                    .iter()
                    .flat_map(|(text, masked, _)| [text.as_str(), masked.as_str()])
            })
            .collect();
        occurrences
            .iter()
            .filter(|&&band| bands[band].0 != first)
            .flat_map(|&band| &bands[band].1)
            .any(|(text, masked, counts_only)| {
                !counts_only && !shown.contains(text.as_str()) && !shown.contains(masked.as_str())
            })
    })
}

/// One edge line of the document.
struct Line {
    page: u32,
    y: f32,
    text: String,
    compared: String,
}

/// The pages the check names, and, when it could not look for every text
/// dropped, the last page it looked through.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Lost {
    pub(crate) pages: Vec<u32>,
    pub(crate) read_to: Option<u32>,
}

/// The pages on which pdf-inspector drops a line, as a running header or
/// footer, that says what no line it keeps says, but for numbers that count
/// the pages or tell the time, and that the Markdown does not show; the
/// rule's threshold is set by `page_count`, the pages read.
pub(crate) fn lost(pages: &[PageLines], page_count: u32, markdown: &str) -> Lost {
    if page_count < 3 {
        return Lost::default();
    }
    let mut ordered_pages: Vec<&PageLines> = pages.iter().collect();
    ordered_pages.sort_by_key(|page| page.page);
    let lines: Vec<Line> = ordered_pages
        .iter()
        .flat_map(|page| {
            page.edges.iter().map(|(y, text)| Line {
                page: page.page,
                y: *y,
                text: text.clone(),
                compared: compared(text),
            })
        })
        .collect();
    if lines.is_empty() {
        return Lost::default();
    }
    let spans: Vec<f32> = pages
        .iter()
        .filter(|page| !page.edges.is_empty())
        .map(|page| page.span)
        .collect();
    let average_span = (spans.iter().sum::<f32>() / spans.len().max(1) as f32).max(1.0);

    // Lines at one height on a page form a band; a band holding a heading
    // or a list item is kept whole, with the bands beside it.
    let mut bands: HashMap<(u32, i32), Vec<usize>> = HashMap::new();
    for (index, line) in lines.iter().enumerate() {
        bands
            .entry((line.page, bucket(line.y)))
            .or_default()
            .push(index);
    }
    let mut protected: HashSet<(u32, i32)> = HashSet::new();
    for (&(page, at), members) in &bands {
        if members
            .iter()
            .any(|&index| structural(lines[index].text.trim()))
        {
            protected.extend([(page, at - 1), (page, at), (page, at + 1)]);
        }
    }
    let band_text = |members: &[usize]| -> String {
        members
            .iter()
            .map(|&index| lines[index].text.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    };

    // How often each line, and each band of two lines or more, repeats.
    let mut pages_by_line: HashMap<&str, HashSet<u32>> = HashMap::new();
    let mut heights_by_line: HashMap<&str, Vec<f32>> = HashMap::new();
    for line in &lines {
        if structural(line.text.trim()) || !repeatable(&line.compared) {
            continue;
        }
        pages_by_line
            .entry(&line.compared)
            .or_default()
            .insert(line.page);
        heights_by_line
            .entry(&line.compared)
            .or_default()
            .push(line.y);
    }
    let mut band_keys: HashMap<(u32, i32), String> = HashMap::new();
    let mut pages_by_band: HashMap<String, HashSet<u32>> = HashMap::new();
    let mut heights_by_band: HashMap<String, Vec<f32>> = HashMap::new();
    for (&(page, at), members) in &bands {
        if members.len() < 2 {
            continue;
        }
        let text = band_text(members);
        if structural(text.trim()) {
            continue;
        }
        let key = compared(&text);
        if !repeatable(&key) {
            continue;
        }
        pages_by_band.entry(key.clone()).or_default().insert(page);
        heights_by_band
            .entry(key.clone())
            .or_default()
            .push(lines[members[0]].y);
        band_keys.insert((page, at), key);
    }
    let threshold = 3.max(page_count * REPEAT_PERCENT / 100) as usize;
    let at_one_height = |heights: &[f32]| {
        if heights.len() < 2 {
            return true;
        }
        let count = heights.len() as f32;
        let mean = heights.iter().sum::<f32>() / count;
        let variance = heights
            .iter()
            .map(|height| (height - mean).powi(2))
            .sum::<f32>()
            / count;
        variance.sqrt() / average_span < MAX_HEIGHT_SPREAD
    };
    let repeated: HashSet<&str> = pages_by_line
        .iter()
        .filter(|(key, pages)| {
            pages.len() >= threshold && !structural(key) && at_one_height(&heights_by_line[*key])
        })
        .map(|(key, _)| *key)
        .collect();
    let repeated_bands: HashSet<&str> = pages_by_band
        .iter()
        .filter(|(key, pages)| {
            pages.len() >= threshold
                && !structural(key)
                && at_one_height(&heights_by_band[key.as_str()])
        })
        .map(|(key, _)| key.as_str())
        .collect();
    if repeated.is_empty() && repeated_bands.is_empty() {
        return Lost::default();
    }

    // What the rule drops: a repeated line or band past the first page
    // showing it, and the rest of its band with it. Each band dropped keeps
    // the band the first page shows in its place, its twin.
    let mut twins: HashMap<(u32, i32), (u32, i32)> = HashMap::new();
    let mut first_line: HashMap<&str, (u32, i32)> = HashMap::new();
    for line in &lines {
        let band = (line.page, bucket(line.y));
        if protected.contains(&band) || !repeated.contains(line.compared.as_str()) {
            continue;
        }
        let first = *first_line.entry(&line.compared).or_insert(band);
        if line.page > first.0 {
            twins.entry(band).or_insert(first);
        }
    }
    let mut first_band: HashMap<&str, (u32, i32)> = HashMap::new();
    let mut keyed: Vec<(&(u32, i32), &String)> = band_keys.iter().collect();
    keyed.sort_unstable();
    for (&band, key) in &keyed {
        if repeated_bands.contains(key.as_str()) {
            first_band.entry(key.as_str()).or_insert(band);
        }
    }
    for (&band, key) in keyed {
        if protected.contains(&band) {
            continue;
        }
        if let Some(&first) = first_band.get(key.as_str()) {
            if band.0 > first.0 {
                twins.entry(band).or_insert(first);
            }
        }
    }
    if twins.is_empty() {
        return Lost::default();
    }

    // A line dropped is lost where no line or band kept says it, even with
    // the numbers that count the pages or tell the time masked, and the
    // Markdown, which shows a line of its twin, does not show it.
    let spaced = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut kept: HashSet<String> = HashSet::new();
    let mut kept_masked: HashSet<String> = HashSet::new();
    for (band, members) in &bands {
        if twins.contains_key(band) {
            continue;
        }
        let texts = members
            .iter()
            .map(|&index| spaced(&lines[index].text))
            .chain([spaced(&band_text(members))]);
        for text in texts {
            kept_masked.insert(masked(&text, band.0).0);
            kept.insert(text);
        }
    }
    let mut dropped: Vec<_> = twins.iter().collect();
    dropped.sort_unstable();
    // Each page's texts dropped, and its twins' texts, bare.
    let mut sought: Vec<(u32, Vec<String>, Vec<String>)> = Vec::new();
    for (band, twin) in dropped {
        let members = &bands[band];
        // A line dropped says something else where it is neither kept as
        // it is, nor kept but for the numbers that count the pages or tell
        // the time, nor made of them alone.
        let new = |text: &String| {
            let (masked, counts_only) = masked(text, band.0);
            !kept.contains(text) && !kept_masked.contains(&masked) && !counts_only
        };
        let whole = spaced(&band_text(members));
        if !new(&whole) {
            continue;
        }
        let texts: Vec<String> = members
            .iter()
            .map(|&index| spaced(&lines[index].text))
            .filter(new)
            .map(|text| bare(&text))
            .filter(|text| !text.is_empty())
            .collect();
        let shown: Vec<String> = bands[twin]
            .iter()
            .map(|&index| bare(&lines[index].text))
            .filter(|text| !text.is_empty())
            .collect();
        if !texts.is_empty() && !shown.is_empty() {
            sought.push((band.0, texts, shown));
        }
    }

    // Texts are looked for page by page, as many pages as the bound allows.
    let mut patterns: HashSet<&str> = HashSet::new();
    let (mut looked, mut read_to) = (0, None);
    while looked < sought.len() {
        let page = sought[looked].0;
        let end = sought[looked..]
            .iter()
            .position(|(other, ..)| *other != page)
            .map_or(sought.len(), |length| looked + length);
        let more: HashSet<&str> = sought[looked..end]
            .iter()
            .flat_map(|(_, texts, shown)| texts.iter().chain(shown))
            .map(String::as_str)
            .filter(|text| !patterns.contains(text))
            .collect();
        if patterns.len() + more.len() > MAX_SOUGHT_TEXTS {
            read_to = Some(page - 1);
            break;
        }
        patterns.extend(more);
        looked = end;
    }
    let mut patterns: Vec<&str> = patterns.into_iter().collect();
    patterns.sort_unstable();
    let found = found_in(&patterns, markdown);
    let mut named: Vec<u32> = sought[..looked]
        .iter()
        .filter(|(_, texts, shown)| {
            shown.iter().any(|text| found.contains(text.as_str()))
                && texts.iter().any(|text| !found.contains(text.as_str()))
        })
        .map(|(page, ..)| *page)
        .collect();
    named.sort_unstable();
    named.dedup();
    Lost {
        pages: named,
        read_to,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A page whose lines sit 16 points apart from the top, each given by
    /// its text.
    fn page(page: u32, texts: &[&str]) -> PageLines {
        PageLines::new(
            page,
            texts
                .iter()
                .enumerate()
                .map(|(index, text)| (740.0 - 16.0 * index as f32, text.to_string()))
                .collect(),
        )
    }

    fn statement(number: &str, rows: &[&str]) -> Vec<String> {
        let mut texts = vec![
            "Example Bank consolidated statement".to_string(),
            format!("Account number {number}"),
        ];
        texts.extend(rows.iter().map(|row| row.to_string()));
        texts
    }

    #[test]
    fn a_line_dropped_that_says_something_else_is_found() {
        let numbers = ["12345678", "87654321", "55501234"];
        let pages: Vec<PageLines> = numbers
            .iter()
            .enumerate()
            .map(|(index, number)| {
                let texts = statement(number, &["03/01 Deposit 103.00", "03/02 Deposit 110.00"]);
                let texts: Vec<&str> = texts.iter().map(String::as_str).collect();
                page(index as u32 + 1, &texts)
            })
            .collect();
        // pdf-inspector keeps the first page's header and drops the others.
        let markdown = "Example Bank consolidated statement Account number 12345678\n\n03/01 Deposit 103.00\n03/02 Deposit 110.00\n\n03/01 Deposit 103.00\n03/02 Deposit 110.00\n\n03/01 Deposit 103.00\n03/02 Deposit 110.00\n";
        assert_eq!(lost(&pages, 3, markdown).pages, vec![2, 3]);
        // Where the Markdown shows a line, it was not lost; a number that
        // runs on into another is not the one shown.
        let shown = format!("{markdown}\nAccount number 87654321\nAccount number 555012345\n");
        assert_eq!(lost(&pages, 3, &shown).pages, vec![3]);
        // Two pages give no evidence of repetition.
        assert!(lost(&pages[..2], 2, markdown).pages.is_empty());
    }

    #[test]
    fn lines_repeated_as_they_are_or_counting_pages_are_not() {
        // The same account on every page: its header repeats as it is.
        let same: Vec<PageLines> = (1..=4)
            .map(|number| {
                let texts = statement("12345678", &["Deposit 103.00"]);
                let texts: Vec<&str> = texts.iter().map(String::as_str).collect();
                page(number, &texts)
            })
            .collect();
        assert!(lost(&same, 4, "Example Bank consolidated statement")
            .pages
            .is_empty());
        // A running header numbering its pages, which the first page does
        // not show.
        let paged: Vec<PageLines> = (1..=10)
            .map(|number| {
                let header = if number == 1 {
                    "Sample annual income summary".to_string()
                } else {
                    format!("Sample annual income summary, page {number}")
                };
                page(number, &[header.as_str(), "Interest income 12.00"])
            })
            .collect();
        // pdf-inspector keeps the first page's header, and the second's,
        // the first to number its page.
        let markdown = "Sample annual income summary\n\nInterest income 12.00\n\nSample annual income summary, page 2\n";
        assert!(lost(&paged, 10, markdown).pages.is_empty());
    }

    #[test]
    fn a_band_dropped_with_a_repeated_label_carries_its_value() {
        // The label and the name are separate lines at one height: the
        // label repeats, and the name goes with it.
        let pages: Vec<PageLines> = ["Jane Sample", "John Example", "Alex Placeholder"]
            .iter()
            .enumerate()
            .map(|(index, name)| {
                PageLines::new(
                    index as u32 + 1,
                    vec![
                        (740.0, "Prepared for the partner".to_string()),
                        (740.0, name.to_string()),
                        (700.0, "Share of income 1,000.00".to_string()),
                    ],
                )
            })
            .collect();
        let markdown = "Prepared for the partner Jane Sample\n\nShare of income 1,000.00\n\nShare of income 1,000.00\n\nShare of income 1,000.00\n";
        assert_eq!(lost(&pages, 3, markdown).pages, vec![2, 3]);
        // The twin's lines read apart, as two columns are, still show it.
        let apart = "Prepared for the partner\n\nJane Sample\n\nShare of income 1,000.00\n";
        assert_eq!(lost(&pages, 3, apart).pages, vec![2, 3]);
    }

    #[test]
    fn edges_are_the_highest_and_lowest_lines_of_a_long_page() {
        let texts: Vec<String> = (0..14).map(|row| format!("Row {row}")).collect();
        let lines = page(1, &texts.iter().map(String::as_str).collect::<Vec<_>>());
        let edges: Vec<&str> = lines.edges.iter().map(|(_, text)| text.as_str()).collect();
        assert_eq!(
            edges,
            [
                "Row 0", "Row 1", "Row 2", "Row 3", "Row 4", "Row 9", "Row 10", "Row 11", "Row 12",
                "Row 13"
            ]
        );
        assert_eq!(lines.span, 16.0 * 13.0);
        assert_eq!(page(2, &["A", "B"]).edges.len(), 2);
    }

    #[test]
    fn numbers_that_count_pages_or_tell_the_time_are_masked() {
        let same = |one: (&str, u32), other: (&str, u32)| {
            masked(one.0, one.1).0 == masked(other.0, other.1).0
        };
        assert!(same(("Statement, page 2", 2), ("Statement, page 5", 5)));
        assert!(same(("Statement p. 2", 2), ("Statement p. 7", 7)));
        assert!(same(("Notice page 1 of 2", 1), ("Notice page 1 of 1", 4)));
        assert!(same(("Image sheet 2/3", 5), ("Image sheet 1/3", 4)));
        assert!(same(("Report - 3 -", 3), ("Report - 9 -", 9)));
        assert!(same(
            ("Printed 2025-04-15 10:32:07", 2),
            ("Printed 2025-04-15 10:32:08", 3)
        ));
        // A folio at either end, a set distance from the page's number.
        assert!(same(("Continued on 3", 2), ("Continued on 5", 4)));
        assert!(same(("12 Annual report", 12), ("13 Annual report", 13)));
        // Numbers that run with the pages but far from them, an amount's
        // cents, and numbers that differ otherwise say something else.
        assert!(!same(
            ("Invoice number 100231", 1),
            ("Invoice number 100232", 2)
        ));
        assert!(!same(("Check 5002", 1), ("Check 5003", 2)));
        assert!(!same(("Balance 1,000.01", 1), ("Balance 1,000.03", 3)));
        assert!(!same(("Account 12345678", 1), ("Account 87654321", 2)));
        assert!(!same(("Group 3 totals", 3), ("Group 4 totals", 4)));
        assert!(!same(("Printed 2025-04-15", 2), ("Printed 2025-04-16", 3)));
        // A count after a page word, a month's days, and a date's parts are
        // not page numbers.
        assert!(!same(
            ("Transactions on this page 25", 2),
            ("Transactions on this page 18", 3)
        ));
        assert!(!same(
            ("Statement date March 4", 1),
            ("Statement date March 5", 2)
        ));
        assert!(!same(
            ("Statement date 3/4/2025", 1),
            ("Statement date 3/5/2025", 2)
        ));
        assert!(same(("Statement page 5", 5), ("Statement page 1", 1)));
        assert!(same(("Statement page 0", 1), ("Statement page 1", 2)));
        // Counts after a page word, or standing alone, count the pages; a
        // date or an index after another word does not.
        assert!(same(("3/7", 3), ("4/7", 4)));
        assert!(same(("- 3 of 7 -", 3), ("- 4 of 7 -", 4)));
        assert!(!same(("Closing date 1/31", 1), ("Closing date 2/28", 3)));
        assert!(!same(("Loan 1 of 3", 1), ("Loan 2 of 3", 3)));
        // A labelled number running with the pages is an identifier.
        assert!(!same(("Check 1001", 1), ("Check 1002", 2)));
        assert!(!same(("Invoice No. 1001", 1), ("Invoice No. 1002", 2)));
        assert!(!same(("Check #101", 1), ("Check #102", 2)));
        // Joined to a page word, a number is the page's only where it is.
        assert!(same(("p1.holder: Jane", 1), ("p2.holder: Jane", 2)));
        assert!(!same(("Plan P1", 1), ("Plan P2", 6)));
        assert!(same(("Report - 0 -", 1), ("Report - 7 -", 8)));
        assert!(same(("Disclosures, page 2", 7), ("Disclosures, page 1", 1)));
        assert!(same(("Excerpt page 102", 2), ("Excerpt page 101", 1)));
        // Roman numerals number front matter.
        assert!(same(
            ("Front matter page ii", 2),
            ("Front matter page iii", 3)
        ));
        assert!(same(("Preface iv", 4), ("Preface vii", 7)));
        assert!(!same(("World War II", 2), ("World War III", 3)));
        // A line of nothing but a page's number counts the pages alone.
        for (text, page) in [
            ("ii", 2),
            ("Page 3", 3),
            ("- 3 -", 3),
            ("3 of 7", 3),
            ("7", 7),
        ] {
            assert!(masked(text, page).1, "{text}");
        }
        for (text, page) in [("Page 3 total", 3), ("Account 12345678", 1), ("March 4", 3)] {
            assert!(!masked(text, page).1, "{text}");
        }
        assert_eq!(roman("xiv"), Some((14, true)));
        assert_eq!(roman("IIII"), None);
        assert_eq!(roman("mix"), Some((1009, true)));
        assert_eq!(
            compared(" 3  Social security wages 88000"),
            "Social security wages"
        );
        assert!(structural("(a) Note"));
        assert!(structural("1. Introduction"));
        assert!(!structural("(all amounts in dollars)"));
    }

    #[test]
    fn sequential_numbers_heading_their_pages_are_found() {
        let pages: Vec<PageLines> = (1..=5)
            .map(|number| {
                let header = format!("Invoice number {}", 100_230 + number);
                page(
                    number,
                    &["Example Supplies Ltd", header.as_str(), "Paper 12.00"],
                )
            })
            .collect();
        let markdown = "Example Supplies Ltd\n\nInvoice number 100231\n\nPaper 12.00\n";
        assert_eq!(lost(&pages, 5, markdown).pages, vec![2, 3, 4, 5]);
    }

    /// Runs set 16 points apart from the top of a page, each given by its
    /// text, one to a height but where `beside` sets another at its right.
    fn runs(page: u32, texts: &[&str], beside: &[(usize, &str)]) -> (u32, Vec<EdgeRun>) {
        let mut runs: Vec<EdgeRun> = texts
            .iter()
            .enumerate()
            .map(|(index, text)| EdgeRun {
                y: 740.0 - 16.0 * index as f32,
                x: 72.0,
                direction: [1.0, 0.0],
                size: 10.0,
                text: Some(text.to_string()),
            })
            .collect();
        runs.extend(beside.iter().map(|(index, text)| EdgeRun {
            y: 740.0 - 16.0 * *index as f32,
            x: 400.0,
            direction: [1.0, 0.0],
            size: 10.0,
            text: Some(text.to_string()),
        }));
        (page, runs)
    }

    #[test]
    fn the_gate_opens_only_where_a_repeated_line_differs() {
        let none = HashSet::new();
        let statement = |page: u32, number: &str| {
            let header = format!("Account number {number}");
            let folio = format!("Page {page} of 3");
            runs(
                page,
                &[
                    "Example Bank consolidated statement",
                    &header,
                    "Deposit 12.00",
                    &folio,
                ],
                &[],
            )
        };
        let accounts = [
            statement(1, "12345678"),
            statement(2, "87654321"),
            statement(3, "87654321"),
        ];
        assert!(may_drop(&accounts, 3, &none, &none));
        let one = [
            statement(1, "12345678"),
            statement(2, "12345678"),
            statement(3, "12345678"),
        ];
        assert!(!may_drop(&one, 3, &none, &none));
        // A repeated label beside a name that changes.
        let named: Vec<(u32, Vec<EdgeRun>)> = ["Jane Sample", "John Example", "Alex Placeholder"]
            .iter()
            .zip(1..)
            .map(|(name, page)| {
                runs(
                    page,
                    &["Prepared for the partner", "Share 1,000.00"],
                    &[(0, name)],
                )
            })
            .collect();
        assert!(may_drop(&named, 3, &none, &none));
        // A run the scan cannot read may say anything.
        let mut unread = one.clone();
        unread[1].1[0].text = None;
        assert!(may_drop(&unread, 3, &none, &none));
        // Too few pages.
        assert!(!may_drop(&accounts[..2], 2, &none, &none));
        // Set below many table rows, a header line reaches the edge only
        // once the rows are set aside.
        let tall = |page: u32, number: &str| {
            let mut texts: Vec<String> = (0..9)
                .map(|line| format!("Example Bank header line {line}"))
                .collect();
            texts.push(format!("Account number {number}"));
            texts.extend((0..30).map(|row| format!("03/{row:02} Card purchase")));
            let texts: Vec<&str> = texts.iter().map(String::as_str).collect();
            runs(page, &texts, &[])
        };
        let tables = [
            tall(1, "12345678"),
            tall(2, "87654321"),
            tall(3, "55501234"),
        ];
        assert!(!may_drop(&tables, 3, &none, &none));
        let cells: HashSet<String> = (0..30)
            .map(|row| bare(&format!("03/{row:02} Card purchase")))
            .collect();
        assert!(may_drop(&tables, 3, &none, &cells));
        assert_eq!(edge_runs(tables[0].1.clone(), None).len(), 24);
        // Set aside first, the rows leave every other line within the edges.
        assert_eq!(
            edge_runs(tables[0].1.clone(), Some((&none, &cells))).len(),
            40
        );
        // A header drawn a glyph at a time is read whole.
        let glyphs = |page: u32, number: &str| {
            let text = format!("Account number {number}");
            let runs = text
                .chars()
                .enumerate()
                .filter(|(_, glyph)| *glyph != ' ')
                .map(|(index, glyph)| EdgeRun {
                    y: 740.0,
                    x: 72.0 + 6.0 * index as f32,
                    direction: [1.0, 0.0],
                    size: 10.0,
                    text: Some(glyph.to_string()),
                })
                .collect();
            (page, runs)
        };
        let changing = [
            glyphs(1, "12345678"),
            glyphs(2, "87654321"),
            glyphs(3, "87654321"),
        ];
        assert!(may_drop(&changing, 3, &none, &none));
        let same = [
            glyphs(1, "12345678"),
            glyphs(2, "12345678"),
            glyphs(3, "12345678"),
        ];
        assert!(!may_drop(&same, 3, &none, &none));
    }

    #[test]
    fn runs_join_into_lines_as_pdf_inspector_joins_them() {
        let run = |x: f32, y: f32| EdgeRun {
            y,
            x,
            direction: [1.0, 0.0],
            size: 10.0,
            text: Some(String::new()),
        };
        // A number set a fraction of a point below the name beside it, and a
        // run less than 3 points below a line's first that does not start at
        // its margin, are on its line; one starting at the margin is not.
        let runs = [
            run(72.0, 750.6),
            run(300.0, 750.4),
            run(72.0, 740.0),
            run(300.0, 738.2),
            run(72.0, 737.5),
        ];
        assert_eq!(lines_of(&runs), [vec![0, 1], vec![2, 3], vec![4]]);
    }

    #[test]
    fn the_markdown_decides_what_its_table_rows_hold() {
        let markdown = "| Date | Amount |\n|---|---|\n| 03/01 | 1.00 |\n\nText\n";
        let (rows, cells) = table_rows(markdown);
        assert!(rows.contains("DateAmount") && rows.contains("03/011.00"));
        assert_eq!(rows.len(), 2);
        assert!(cells.contains("03/01") && cells.contains("1.00") && !cells.contains("Text"));
        let found = found_in(
            &["Account1234", "Name", "5678"],
            "Account 12345 Name 12 5678",
        );
        assert!(!found.contains("Account1234") && found.contains("Name"));
        assert!(found.contains("5678"));
        assert_eq!(bare("| a <u>b</u> |<br>c"), "abc");
    }

    #[test]
    fn markdown_decoration_is_left_out_as_text_is_looked_for() {
        assert_eq!(bare("Price <s>$1,500</s> now"), "Price$1,500now");
        assert_eq!(bare("x<sup>2</sup> H<sub>2</sub>O"), "x2H2O");
        assert_eq!(
            bare("Do not pay at [https://pay.example](https://pay.example) now"),
            "Donotpayathttps://pay.examplenow"
        );
        assert_eq!(bare("The fee\u{B9} is \u{FB01}nal"), "Thefee1isfinal");
        assert_eq!(bare("\u{2022} Fees"), bare("- Fees"));
        // A superscript digit is set apart from a number before it.
        let found = found_in(&["1,250.00"], "Total 1,250.00\u{B9}");
        assert!(found.contains("1,250.00"));
    }

    fn line_run(x: f32, y: f32, text: Option<&str>) -> EdgeRun {
        EdgeRun {
            y,
            x,
            direction: [1.0, 0.0],
            size: 12.0,
            text: text.map(str::to_owned),
        }
    }

    #[test]
    fn short_runs_are_read_beside_their_lines_as_pdf_inspector_reads_them() {
        let holds = |runs: &[EdgeRun], span: usize, texts: &[&str]| {
            let read = line_contexts(runs, &[(span, span, true)]);
            texts
                .iter()
                .all(|text| read[0].iter().any(|read| read == text))
        };
        // Raised or lowered off the line a point or so, in the order shown,
        // as pdf-inspector keeps it on the line; a tie on x in that order.
        for lift in [1.0, 1.5, 2.5, -1.5] {
            let runs = [
                line_run(72.0, 640.0, Some("The fee is ")),
                line_run(128.0, 640.0 + lift, Some("not ")),
                line_run(128.0, 640.0, Some("refundable within")),
            ];
            assert!(holds(&runs, 1, &["Thefeeisnot", "notrefundab"]), "{lift}");
        }
        // Drawn last, where pdf-inspector sets it beside the line at its
        // height.
        let runs = [
            line_run(72.0, 640.0, Some("The fee is ")),
            line_run(128.0, 640.0, Some("refundable.")),
            line_run(72.0, 600.0, Some("Please keep this.")),
            line_run(128.0, 640.0, Some("not ")),
        ];
        assert!(holds(&runs, 3, &["undable.not"]));
        // A run that cannot be read ends the text beside, not the line.
        let runs = [
            line_run(72.0, 640.0, Some("The customer")),
            line_run(140.0, 640.0, None),
            line_run(146.0, 640.0, Some("s fee is ")),
            line_run(200.0, 640.0, Some("not ")),
            line_run(225.0, 640.0, Some("refundable")),
        ];
        assert!(holds(&runs, 3, &["sfeeisnot", "notrefundab"]));
        assert_eq!(
            line_contexts(&runs, &[(1, 1, true)]),
            [Vec::<String>::new()]
        );
        // A run alone on its line is read with the lines before and after.
        let runs = [
            line_run(72.0, 660.0, Some("Balance due")),
            line_run(72.0, 640.0, Some("$0.00")),
            line_run(72.0, 620.0, Some("Thank you")),
        ];
        assert!(holds(&runs, 1, &["lancedue$0.00", "$0.00Thankyou"]));
        // A page whose lines read up it is turned first, as pdf-inspector
        // turns it: the run shown a point along the line from the text
        // after it stays in the line.
        let up = |x: f32, y: f32, text: &str| EdgeRun {
            direction: [0.0, 1.0],
            ..line_run(x, y, Some(text))
        };
        let runs = [
            up(560.0, 72.0, "Statement of account"),
            up(480.0, 72.0, "The fee is "),
            up(480.0, 126.0, "not "),
            up(480.0, 127.0, "refundable within"),
        ];
        assert!(holds(&runs, 2, &["Thefeeisnot", "notrefundab"]));
        // A digit slipped between two others is read whole with the text
        // on either side, which alone the Markdown shows as it stands: each
        // side runs on into a number.
        let runs = [
            line_run(72.0, 640.0, Some("Total due $10")),
            line_run(150.0, 640.0, Some("0")),
            line_run(150.0, 640.0, Some("0.00 by June")),
        ];
        assert_eq!(
            line_contexts(&runs, &[(1, 1, true)])[0],
            ["aldue$1000.00byJu", "aldue$100", "00.00byJu"]
        );
        let found = found_in(
            &["aldue$1000.00byJu", "aldue$100", "00.00byJu"],
            "Total due $1000.00 by June",
        );
        assert_eq!(found, HashSet::from(["aldue$1000.00byJu"]));
        // A longer run is read with a character on either side as well.
        let runs = [
            line_run(72.0, 640.0, Some("Total due $1")),
            line_run(150.0, 640.0, Some("0,000,00")),
            line_run(190.0, 640.0, Some("0.00")),
        ];
        let read = line_contexts(&runs, &[(1, 1, false)]);
        assert_eq!(read[0], ["0,000,00", "10,000,000"]);
    }
}
