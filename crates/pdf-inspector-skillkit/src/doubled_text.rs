//! Text pdf-inspector's Markdown shows twice, to confirm the pages the
//! repeat scan (`text_paints`) reports.
//!
//! The scan reads the content streams, so it also finds repeats that
//! pdf-inspector drops before its Markdown: a doubled header or footer it
//! strips as page furniture (keeping only the first of lines repeated across
//! pages), a white copy it hides inside a form, a copy its clip hides. A
//! reported page stands only while the Markdown shows twice the text
//! pdf-inspector reads where the page's repeated runs start; each doubled
//! occurrence counts for one page.

use pdf_inspector::TextItem;

use crate::text_paints::Repeat;

/// Words a repeated run may span: a doubled header line runs to a dozen.
const MAX_REPEATED_WORDS: usize = 64;
/// Doubled occurrences read from one document.
const MAX_OCCURRENCES: usize = 10_000;

/// Text the Markdown shows doubled, one entry per occurrence, with white
/// space removed: a run of words repeated at once ("84.19 84.19", "Total due
/// 12.00 Total due 12.00"), a word written twice without a space
/// ("84.1984.19"), and a word whose every character is doubled
/// ("TToottaall", kept doubled, as the page's own text writes it).
pub(crate) fn doubled(markdown: &str) -> Vec<String> {
    let mut found = Vec::new();
    // Text outside tables runs on across lines, as a heading repeated on
    // the next line does; each table cell is read alone, since text doubled
    // across two cells is two values.
    let mut running: Vec<String> = Vec::new();
    for line in markdown.lines() {
        let line = without_tags(line);
        let line: String = line
            .chars()
            .filter(|character| !matches!(character, '*' | '_' | '`' | '\\'))
            .collect();
        let trimmed = line.trim();
        if trimmed.starts_with('|') {
            doubled_words(&running, &mut found);
            running.clear();
            for cell in trimmed.split('|') {
                doubled_words(&words(cell), &mut found);
            }
        } else {
            running.extend(words(trimmed));
        }
        if found.len() >= MAX_OCCURRENCES {
            return found;
        }
    }
    doubled_words(&running, &mut found);
    found
}

/// A line with its HTML tags (`<u>`, `<br>`) read as spaces.
fn without_tags(line: &str) -> String {
    let mut plain = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(start) = rest.find('<') {
        let tag = &rest[start..];
        let end = tag.find('>').filter(|&end| {
            let name = tag[1..end].trim_start_matches('/');
            !name.is_empty()
                && name.chars().all(|character| {
                    character.is_ascii_alphanumeric() || character == ' ' || character == '/'
                })
        });
        match end {
            Some(end) => {
                plain.push_str(&rest[..start]);
                plain.push(' ');
                rest = &tag[end + 1..];
            }
            None => {
                plain.push_str(&rest[..start + 1]);
                rest = &tag[1..];
            }
        }
    }
    plain.push_str(rest);
    plain
}

/// A line's words, without Markdown's heading, list, quote, and rule marks.
fn words(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter(|word| {
            !word
                .chars()
                .all(|character| matches!(character, '#' | '>' | '-' | '+' | ':'))
        })
        .map(str::to_string)
        .collect()
}

/// The doubled text in a run of words.
fn doubled_words(words: &[String], found: &mut Vec<String>) {
    let mut index = 0;
    while index < words.len() && found.len() < MAX_OCCURRENCES {
        let repeated = (1..=MAX_REPEATED_WORDS).find(|&count| {
            index + 2 * count <= words.len()
                && words[index + count] == words[index]
                && words[index..index + count] == words[index + count..index + 2 * count]
        });
        if let Some(count) = repeated {
            let text = words[index..index + count].concat();
            if substantial(&text) {
                found.push(text);
            }
            index += 2 * count;
            continue;
        }
        let word: Vec<char> = words[index].chars().collect();
        let half = word.len() / 2;
        if word.len().is_multiple_of(2) && half >= 2 && word[..half] == word[half..] {
            let text: String = word[..half].iter().collect();
            if substantial(&text) {
                found.push(text);
            }
        } else if word.len() >= 6 && every_character_doubled(&words[index]) {
            found.push(words[index].clone());
        }
        index += 1;
    }
}

/// Text worth counting: a number, or a word of four characters or more.
fn substantial(text: &str) -> bool {
    text.chars().any(|character| character.is_ascii_digit()) || text.chars().count() >= 4
}

/// The reported pages, in order, that the Markdown's doubled text confirms.
/// A page counts when text doubled in the Markdown is the text of an item
/// pdf-inspector reads where one of the page's repeated runs starts, or,
/// when it reads none there, any text of the page. Each occurrence counts
/// for the first page left that it matches.
pub(crate) fn confirm(
    pages: &[u32],
    repeats: &[(u32, Repeat)],
    items: &[TextItem],
    markdown: &str,
) -> Vec<u32> {
    let mut occurrences = doubled(markdown);
    let mut confirmed = Vec::new();
    for &page in pages {
        let on_page: Vec<&TextItem> = items.iter().filter(|item| item.page == page).collect();
        let at_repeats: Vec<String> = on_page
            .iter()
            .filter(|item| {
                repeats
                    .iter()
                    .any(|(repeated, repeat)| *repeated == page && starts_within(item, repeat))
            })
            .map(|item| compact(&item.text))
            .filter(|text| !text.is_empty())
            .collect();
        let page_text: String = on_page.iter().map(|item| compact(&item.text)).collect();
        let found = occurrences.iter().position(|occurrence| {
            if at_repeats.is_empty() {
                return page_text.contains(occurrence.as_str());
            }
            at_repeats.iter().any(|text| {
                text.contains(occurrence.as_str())
                    || ((text.chars().count() >= 2 || every_character_doubled(occurrence))
                        && occurrence.contains(text.as_str()))
            })
        });
        if let Some(found) = found {
            occurrences.remove(found);
            confirmed.push(page);
        }
    }
    confirmed
}

/// Whether a repeated run starts within an item's box, on its baseline.
fn starts_within(item: &TextItem, repeat: &Repeat) -> bool {
    let near = (0.5 * repeat.size).max(1.0);
    let (left, baseline) = (f64::from(item.x), f64::from(item.y));
    let right = left + f64::from(item.width);
    (baseline - repeat.at[1]).abs() <= near
        && repeat.at[0] >= left - near
        && repeat.at[0] <= right + near
}

/// Text with its white space removed.
fn compact(text: &str) -> String {
    text.chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

/// Whether each character of a word is written twice ("TToottaall").
fn every_character_doubled(word: &str) -> bool {
    let characters: Vec<char> = word.chars().collect();
    characters.len() >= 2
        && characters.len().is_multiple_of(2)
        && characters.chunks(2).all(|pair| pair[0] == pair[1])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doubled_text_is_found_in_its_shapes() {
        assert_eq!(doubled("Balance 84.19 84.19 due"), vec!["84.19"]);
        assert_eq!(
            doubled("**Total due 12.00** Total due 12.00\n"),
            vec!["Totaldue12.00"]
        );
        assert_eq!(doubled("| 84.1984.19 |"), vec!["84.19"]);
        assert_eq!(doubled("# TToottaall"), vec!["TToottaall"]);
        // Headings repeated on their own lines are doubled; two cells of one
        // value are not.
        assert_eq!(
            doubled("## Key Deadlines\n\n## Key Deadlines\n"),
            vec!["KeyDeadlines"]
        );
        assert!(doubled("|Balance|14,956.36|14,956.36|\n|---|---|---|").is_empty());
        // A doubled header line runs long, and tags do not hide a repeat.
        assert_eq!(
            doubled("Example Tax Advisors LLP · 100 Main Street · Springfield Example Tax Advisors LLP · 100 Main Street · Springfield Terms"),
            vec!["ExampleTaxAdvisorsLLP·100MainStreet·Springfield"]
        );
        assert_eq!(
            doubled("approaching.<u>Deadline: April 15 Deadline: April 15</u> Please"),
            vec!["Deadline:April15"]
        );
        // Short words, and prose without repeats, are not doubled text.
        assert!(doubled("the the cat").is_empty());
        assert!(doubled("A statement of account for the year").is_empty());
    }
}
