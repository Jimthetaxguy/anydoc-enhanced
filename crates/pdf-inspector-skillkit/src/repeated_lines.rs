//! Lines pdf-inspector 1.24.0 drops as running headers or footers.
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
    // Numbers that count: "3 of 7", "3/7" but not a date's "3/7/2025".
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
        if first <= second
            && !(slash && (chained(beside(second_at, true)) || chained(index.checked_sub(1))))
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
        let paged = word_is(
            before.and_then(|before| {
                // A page word, a mark such as "." or ":" after it aside.
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
            &PAGE_WORDS,
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
            && match what {
                Piece::Roman(value, lower) => *lower && *value <= MAX_ROMAN_FOLIO,
                _ => true,
            };
        let offset = value
            .map(|value| value - i64::from(page))
            .filter(|offset| offset.abs() <= MAX_FOLIO_OFFSET);
        // After a page word or between dashes, a number no higher than the
        // page's own numbers it too, as a bundle's documents restart.
        let restarted =
            (paged || framed) && value.is_some_and(|value| (1..=i64::from(page)).contains(&value));
        match offset.filter(|_| paged || framed || folio) {
            _ if restarted => masked.push('#'),
            Some(offset) => masked.push_str(&format!("#{offset}#")),
            None => {
                counts_only = false;
                masked.push_str(piece);
            }
        }
    }
    (masked, counts_only && !text.is_empty())
}

/// A text with its white space, table pipes, emphasis and escape marks,
/// heading marks, and underline and break tags left out, as it is looked
/// for in the Markdown; and, by byte, whether anything was left out just
/// before it.
fn bare_marked(text: &str) -> (String, Vec<bool>) {
    const TAGS: [&str; 4] = ["<u>", "</u>", "<br>", "<br/>"];
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
        rest = &rest[character.len_utf8()..];
        if character.is_whitespace() || matches!(character, '|' | '*' | '_' | '`' | '\\' | '#') {
            broken = true;
            continue;
        }
        bare.push(character);
        breaks.push(broken);
        breaks.extend(std::iter::repeat_n(false, character.len_utf8() - 1));
        broken = false;
    }
    (bare, breaks)
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

/// The texts of `patterns`, bare, that the Markdown shows, each where it
/// does not run on into a number: a text starting with a digit found after
/// none, and one ending with a digit found before none, but where white
/// space or a mark stood between them.
pub(crate) fn found_in<'a>(patterns: &[&'a str], markdown: &str) -> HashSet<&'a str> {
    if patterns.is_empty() {
        return HashSet::new();
    }
    let Ok(automaton) = aho_corasick::AhoCorasick::new(patterns) else {
        return HashSet::new();
    };
    let (haystack, breaks) = bare_marked(markdown);
    let bytes = haystack.as_bytes();
    let joined = |at: usize| at > 0 && at < bytes.len() && !breaks[at];
    automaton
        .find_overlapping_iter(&haystack)
        .filter(|found| {
            let pattern = patterns[found.pattern().as_usize()].as_bytes();
            let (start, end) = (found.start(), found.end());
            // A digit at either end of the text that another digit joins.
            let runs_on_before = pattern.first().is_some_and(u8::is_ascii_digit)
                && joined(start)
                && bytes[start - 1].is_ascii_digit();
            let runs_on_after = pattern.last().is_some_and(u8::is_ascii_digit)
                && joined(end)
                && bytes[end].is_ascii_digit();
            !(runs_on_before || runs_on_after)
        })
        .map(|found| patterns[found.pattern().as_usize()])
        .collect()
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
/// its size, and its text, where its font can be read.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct EdgeRun {
    pub(crate) y: f32,
    pub(crate) x: f32,
    pub(crate) size: f32,
    pub(crate) text: Option<String>,
}

/// The height, to the point, the gate sets a run at.
fn height(run: &EdgeRun) -> i64 {
    run.y.round() as i64
}

/// The runs of a page at its `SCAN_EDGE_HEIGHTS` highest and lowest
/// heights, or all of them on a page of few.
pub(crate) fn edge_runs(mut runs: Vec<EdgeRun>) -> Vec<EdgeRun> {
    let mut heights: Vec<i64> = runs.iter().map(height).collect();
    heights.sort_unstable();
    heights.dedup();
    if heights.len() > SCAN_EDGE_HEIGHTS * 2 {
        let low = heights[SCAN_EDGE_HEIGHTS - 1];
        let high = heights[heights.len() - SCAN_EDGE_HEIGHTS];
        runs.retain(|run| {
            let height = height(run);
            height <= low || height >= high
        });
    }
    runs
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
        let mut by_height: std::collections::BTreeMap<i64, Vec<&EdgeRun>> =
            std::collections::BTreeMap::new();
        for run in runs {
            if run.text.is_none() {
                return true;
            }
            by_height.entry(height(run)).or_default().push(run);
        }
        let text = |run: &EdgeRun| run.text.as_deref().unwrap_or_default().trim().to_string();
        let on_page: Vec<Vec<&EdgeRun>> = by_height
            .into_values()
            .filter(|band| {
                let parts: Vec<String> = band.iter().map(|run| bare(&text(run))).collect();
                !(rows.contains(&parts.concat())
                    || parts
                        .iter()
                        .all(|part| part.is_empty() || cells.contains(part)))
            })
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
            let index = bands.len();
            bands.push((
                *page,
                band.iter()
                    .map(|run| {
                        let (masked, counts_only) = masked(&text(run), *page);
                        (text(run), masked, counts_only)
                    })
                    .collect(),
            ));
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
    keyed.values().any(|occurrences| {
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
                size: 10.0,
                text: Some(text.to_string()),
            })
            .collect();
        runs.extend(beside.iter().map(|(index, text)| EdgeRun {
            y: 740.0 - 16.0 * *index as f32,
            x: 400.0,
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
        assert_eq!(edge_runs(tables[0].1.clone()).len(), 24);
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
}
