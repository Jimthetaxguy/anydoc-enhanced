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
//! page's text (it leaves out the rows of the tables it finds, which the
//! check cannot tell, so it may count lines pdf-inspector does not), and
//! names a page where a line dropped says what no line kept says, other
//! than by a page number, and the Markdown does not show it. Lines that
//! repeat as they are, such as a bank's name, are dropped by design and not
//! named.

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
/// Markdown; past them, none is.
const MAX_SOUGHT_LINES: usize = 8_192;

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

/// Whether `other`, on page `other_page`, differs from `text`, on `page`,
/// only by numbers that count the pages: every number that differs moves
/// by the pages between them.
fn counts_pages(other: &str, other_page: u32, text: &str, page: u32) -> bool {
    fn parts(text: &str) -> Vec<&str> {
        let mut parts = Vec::new();
        let mut start = 0;
        let mut digits = false;
        for (index, character) in text.char_indices() {
            let digit = character.is_ascii_digit();
            if index > start && digit != digits {
                parts.push(&text[start..index]);
                start = index;
            }
            digits = digit;
        }
        parts.push(&text[start..]);
        parts
    }
    let (ours, theirs) = (parts(text), parts(other));
    if ours.len() != theirs.len() {
        return false;
    }
    let step = i64::from(page) - i64::from(other_page);
    let mut moved = false;
    for (ours, theirs) in ours.iter().zip(&theirs) {
        if ours == theirs {
            continue;
        }
        let (Ok(ours), Ok(theirs)) = (ours.parse::<i64>(), theirs.parse::<i64>()) else {
            return false;
        };
        if ours - theirs != step {
            return false;
        }
        moved = true;
    }
    moved
}

/// A text with its white space, table pipes, emphasis and escape marks,
/// heading marks, and underline and break tags left out, as it is looked
/// for in the Markdown.
fn bare(text: &str) -> String {
    let text = text
        .replace("<u>", "")
        .replace("</u>", "")
        .replace("<br>", "")
        .replace("<br/>", "");
    text.chars()
        .filter(|character| {
            !character.is_whitespace() && !matches!(character, '|' | '*' | '_' | '`' | '\\' | '#')
        })
        .collect()
}

/// One edge line of the document.
struct Line {
    page: u32,
    y: f32,
    text: String,
    compared: String,
}

/// The pages on which pdf-inspector drops a line, as a running header or
/// footer, that says what no line it keeps says, other than by a page
/// number, and that the Markdown does not show; `page_count` is the
/// document's.
pub(crate) fn lost(pages: &[PageLines], page_count: u32, markdown: &str) -> Vec<u32> {
    if page_count < 3 {
        return Vec::new();
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
        return Vec::new();
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
        return Vec::new();
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
        return Vec::new();
    }

    // A band dropped is lost where no line or band kept says it, no band on
    // another page differs from it only by a page number, and the Markdown,
    // which shows its twin, does not show it.
    let spaced = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    // A text's shape: each run of digits as one mark, so that pages 9 and
    // 10 number their lines alike.
    let shape = |text: &str| -> String {
        let mut shape = String::with_capacity(text.len());
        for character in text.chars() {
            if !character.is_ascii_digit() {
                shape.push(character);
            } else if !shape.ends_with('\u{0}') {
                shape.push('\u{0}');
            }
        }
        shape
    };
    let mut kept: HashSet<String> = HashSet::new();
    let mut by_shape: HashMap<String, Vec<(u32, String)>> = HashMap::new();
    let mut texts: HashMap<(u32, i32), String> = HashMap::new();
    for (band, members) in &bands {
        let text = spaced(&band_text(members));
        by_shape
            .entry(shape(&text))
            .or_default()
            .push((band.0, text.clone()));
        if !twins.contains_key(band) {
            kept.extend(members.iter().map(|&index| spaced(&lines[index].text)));
            kept.insert(text.clone());
        }
        texts.insert(*band, text);
    }
    let mut sought: Vec<(u32, String, String)> = Vec::new();
    for (band, twin) in &twins {
        let text = &texts[band];
        if kept.contains(text) {
            continue;
        }
        let paged = by_shape[&shape(text)]
            .iter()
            .any(|(other_page, other)| counts_pages(other, *other_page, text, band.0));
        let (dropped, shown) = (bare(text), bare(&texts[twin]));
        if !paged && !dropped.is_empty() && !shown.is_empty() {
            sought.push((band.0, dropped, shown));
        }
    }
    let mut patterns: Vec<&str> = sought
        .iter()
        .flat_map(|(_, dropped, shown)| [dropped.as_str(), shown.as_str()])
        .collect();
    patterns.sort_unstable();
    patterns.dedup();
    if patterns.is_empty() || patterns.len() > MAX_SOUGHT_LINES {
        return Vec::new();
    }
    let Ok(automaton) = aho_corasick::AhoCorasick::new(&patterns) else {
        return Vec::new();
    };
    let found: HashSet<&str> = automaton
        .find_overlapping_iter(&bare(markdown))
        .map(|found| patterns[found.pattern().as_usize()])
        .collect();
    let mut pages: Vec<u32> = sought
        .iter()
        .filter(|(_, dropped, shown)| {
            found.contains(shown.as_str()) && !found.contains(dropped.as_str())
        })
        .map(|(page, _, _)| *page)
        .collect();
    pages.sort_unstable();
    pages.dedup();
    pages
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
        assert_eq!(lost(&pages, 3, markdown), vec![2, 3]);
        // Where the Markdown shows a line, it was not lost.
        let shown = format!("{markdown}\nAccount number 87654321\n");
        assert_eq!(lost(&pages, 3, &shown), vec![3]);
        // Two pages give no evidence of repetition.
        assert!(lost(&pages[..2], 2, markdown).is_empty());
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
        assert!(lost(&same, 4, "Example Bank consolidated statement").is_empty());
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
        assert!(lost(&paged, 10, markdown).is_empty());
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
        assert_eq!(lost(&pages, 3, markdown), vec![2, 3]);
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
    fn page_counts_are_told_from_other_numbers() {
        assert!(counts_pages("Statement, page 2", 2, "Statement, page 5", 5));
        assert!(counts_pages("Continued on 3", 2, "Continued on 5", 4));
        assert!(!counts_pages("Account 12345678", 1, "Account 87654321", 2));
        assert!(!counts_pages("Total hours 80", 1, "Total hours 76", 2));
        assert!(!counts_pages("Same 1", 1, "Same 1", 2));
        assert!(!counts_pages("Page 1", 1, "Page 2 of 3", 2));
        assert_eq!(
            compared(" 3  Social security wages 88000"),
            "Social security wages"
        );
        assert!(structural("(a) Note"));
        assert!(structural("1. Introduction"));
        assert!(!structural("(all amounts in dollars)"));
    }
}
