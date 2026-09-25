//! Text pdf-inspector's Markdown shows twice, to confirm the pages the
//! repeat scan (`text_paints`) reports.
//!
//! The scan reads the content streams, so it also finds repeats that
//! pdf-inspector drops before its Markdown: a doubled header or footer it
//! strips as page furniture (keeping only the first of lines repeated across
//! pages), a white copy it hides inside a form, a copy its clip hides. A
//! reported page stands only while the Markdown shows twice the text
//! pdf-inspector reads where the page's repeated runs start; each doubled
//! occurrence counts for one page. Past the pages read again, a page stands
//! when the text of one of its repeated runs, as its font reads it, is
//! doubled text left over; a page whose runs its fonts do not read stands
//! while any is left. Short words doubled glyph by glyph, as a page number
//! "11" reads, and numbers such as a year "2020", count only where a page's
//! text at a repeat is that number alone or one paint of it.

use pdf_inspector::TextItem;

use crate::text_paints::Repeat;

/// Words a repeated run may span: a doubled header line runs to a dozen.
const MAX_REPEATED_WORDS: usize = 64;
/// Doubled occurrences read from one document.
const MAX_OCCURRENCES: usize = 10_000;
/// Characters read and compared per document looking for text repeated
/// with no space between its copies.
const MAX_JOINED_STEPS: usize = 20_000_000;
/// The longest text repeated with no space between its copies, in
/// characters.
const MAX_JOINED_CHARACTERS: usize = 128;
/// The least word doubled glyph by glyph, in characters, that names a page
/// not read again: shorter, it reads as a page number ("11", "22").
const MIN_UNCHECKED_GLYPHS: usize = 6;
/// Digits shown twice that read as a number of their own: a year "2020" is
/// "20" twice, a box "11" is "1" doubled glyph by glyph.
const MIN_PLAIN_DIGITS: usize = 6;

/// Text the Markdown shows doubled, one entry per occurrence, with white
/// space removed: a run of words repeated at once ("84.19 84.19", "Total due
/// 12.00 Total due 12.00"), a word written twice without a space
/// ("84.1984.19"), words written twice with no space between the copies
/// ("Total due 1,234.56Total due 1,234.56"), and a word whose every
/// character is doubled ("TToottaall", kept doubled, as the page's own text
/// writes it): six characters or more, or fewer with a digit or a currency
/// sign ("22", "$$55").
pub(crate) fn doubled(markdown: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut steps = 0usize;
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
            doubled_words(&running, &mut found, &mut steps);
            running.clear();
            for cell in trimmed.split('|') {
                doubled_words(&words(cell), &mut found, &mut steps);
            }
        } else {
            running.extend(words(trimmed));
        }
        if found.len() >= MAX_OCCURRENCES {
            return found;
        }
    }
    doubled_words(&running, &mut found, &mut steps);
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

/// The doubled text in a run of words; `steps` counts the characters read
/// looking for copies with no space between them.
fn doubled_words(words: &[String], found: &mut Vec<String>, steps: &mut usize) {
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
        if let Some((text, count)) = joined_twice(&words[index..], steps) {
            if substantial(&text) {
                found.push(text);
            }
            index += count;
            continue;
        }
        let word: Vec<char> = words[index].chars().collect();
        let half = word.len() / 2;
        if word.len().is_multiple_of(2) && half >= 2 && word[..half] == word[half..] {
            let text: String = word[..half].iter().collect();
            if substantial(&text) {
                found.push(text);
            }
        } else if every_character_doubled(&words[index])
            && (word.len() >= 6
                || word
                    .iter()
                    .any(|character| character.is_ascii_digit() || currency(*character)))
        {
            found.push(words[index].clone());
        }
        index += 1;
    }
}

/// Text the first words repeat with no space between its copies, the seam
/// inside a word, as a run painted again at once reads ("Total due
/// 1,234.56Total due 1,234.56"): the text once and the words it spans.
fn joined_twice(words: &[String], steps: &mut usize) -> Option<(String, usize)> {
    let mut joined: Vec<char> = Vec::new();
    // Where each word after the first starts in `joined`.
    let mut starts: Vec<usize> = Vec::new();
    for (count, word) in words.iter().enumerate().take(2 * MAX_REPEATED_WORDS) {
        if count > 0 {
            starts.push(joined.len());
        }
        let before = joined.len();
        joined.extend(word.chars());
        *steps += joined.len() - before;
        if joined.len() > 2 * MAX_JOINED_CHARACTERS || *steps > MAX_JOINED_STEPS {
            return None;
        }
        let half = joined.len() / 2;
        if count == 0 || !joined.len().is_multiple_of(2) || starts.contains(&half) {
            continue;
        }
        let same = joined[..half]
            .iter()
            .zip(&joined[half..])
            .take_while(|(first, second)| first == second)
            .count();
        *steps += same + 1;
        if same == half {
            return Some((joined[..half].iter().collect(), count + 1));
        }
    }
    None
}

/// A currency sign.
fn currency(character: char) -> bool {
    matches!(character, '$' | '\u{20ac}' | '\u{a3}' | '\u{a5}')
}

/// Text worth counting: a number, or a word of four characters or more.
fn substantial(text: &str) -> bool {
    text.chars().any(|character| character.is_ascii_digit()) || text.chars().count() >= 4
}

/// The reported pages, in order, that the Markdown's doubled text confirms.
/// Of the first `checked` pages, whose text `items` holds, a page counts
/// when text doubled in the Markdown is where pdf-inspector reads one of
/// the page's repeated runs start: an item there shows it twice, or lies
/// within it, as each paint's own item does. When pdf-inspector reads no
/// item there, the page's text must show it twice. Each occurrence counts
/// for the first page left that it matches. A page past `checked` counts
/// when the text of one of its repeated runs is an occurrence left; then,
/// pages whose repeated runs have no text count while occurrences are left.
/// Words shorter than `MIN_UNCHECKED_GLYPHS` doubled glyph by glyph, and
/// numbers, name none of them.
pub(crate) fn confirm(
    pages: &[u32],
    checked: usize,
    repeats: &[(u32, Repeat)],
    items: &[TextItem],
    markdown: &str,
) -> Vec<u32> {
    let checked = checked.min(pages.len());
    let mut occurrences = doubled(markdown);
    let mut confirmed = Vec::new();
    for &page in &pages[..checked] {
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
            let twice = shown_twice(occurrence);
            // A number that reads as itself confirms only the text at a
            // repeat that is that number alone, or one paint of it.
            if reads_as_number(&twice) {
                let once = painted_once(occurrence);
                return at_repeats
                    .iter()
                    .any(|text| *text == twice || *text == once);
            }
            if at_repeats.is_empty() {
                return page_text.contains(twice.as_str());
            }
            at_repeats.iter().any(|text| {
                text.contains(twice.as_str())
                    || ((text.chars().count() >= 2 || every_character_doubled(occurrence))
                        && occurrence.contains(text.as_str()))
            })
        });
        if let Some(found) = found {
            occurrences.remove(found);
            confirmed.push(page);
        }
    }
    occurrences.retain(|occurrence| {
        (!every_character_doubled(occurrence) || occurrence.chars().count() >= MIN_UNCHECKED_GLYPHS)
            && !reads_as_number(&shown_twice(occurrence))
    });
    let mut unread = Vec::new();
    for &page in &pages[checked..] {
        let texts: Vec<String> = repeats
            .iter()
            .filter(|(repeated, _)| *repeated == page)
            .filter_map(|(_, repeat)| repeat.text.as_deref().map(compact))
            .filter(|text| !text.is_empty())
            .collect();
        if texts.is_empty() {
            unread.push(page);
            continue;
        }
        let found = occurrences.iter().position(|occurrence| {
            let once = painted_once(occurrence);
            texts.iter().any(|text| {
                text.contains(once.as_str())
                    || ((text.chars().count() >= 2 || every_character_doubled(occurrence))
                        && once.contains(text.as_str()))
            })
        });
        if let Some(found) = found {
            occurrences.remove(found);
            confirmed.push(page);
        }
    }
    confirmed.extend(unread.into_iter().take(occurrences.len()));
    confirmed.sort_unstable();
    confirmed
}

/// Doubled text as one paint shows it: a word whose every character is
/// doubled with each written once, other text as it is.
fn painted_once(occurrence: &str) -> String {
    if every_character_doubled(occurrence) {
        occurrence.chars().step_by(2).collect()
    } else {
        occurrence.to_string()
    }
}

/// Doubled text as the page's items show it: a word whose every character
/// is doubled as it is, other text twice over.
fn shown_twice(occurrence: &str) -> String {
    if every_character_doubled(occurrence) {
        occurrence.to_string()
    } else {
        occurrence.repeat(2)
    }
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

/// Text with its white space removed, and the marks `words` leaves out, so
/// a page's text reads as the Markdown's doubled text does.
fn compact(text: &str) -> String {
    words(text).concat()
}

/// Whether doubled text, as the page shows it, is digits few enough to read
/// as a number of their own.
fn reads_as_number(shown: &str) -> bool {
    shown.chars().count() < MIN_PLAIN_DIGITS
        && shown.chars().all(|character| character.is_ascii_digit())
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
        assert_eq!(
            doubled("## Total due 1,234.56Total due 1,234.56"),
            vec!["Totaldue1,234.56"]
        );
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
        // Short values doubled glyph by glyph, with a digit or a currency
        // sign.
        assert_eq!(doubled("Dependents claimed: 22"), vec!["22"]);
        assert_eq!(doubled("Fee: $$55 and 1122"), vec!["$$55", "1122"]);
        // Short words, and prose without repeats, are not doubled text.
        assert!(doubled("the the cat").is_empty());
        assert!(doubled("A statement of account for the year").is_empty());
        assert!(doubled("see all book").is_empty());
    }

    #[test]
    fn pages_not_read_again_count_while_long_doubled_text_is_left() {
        let markdown = "Total due 12.00 Total due 12.00\n\n## Page 22\n\n| 04/06 04/06 |\n";
        // Without the pages' text, each doubled text names one page, in
        // order, but for a short word doubled glyph by glyph.
        assert_eq!(confirm(&[3, 7, 9], 0, &[], &[], markdown), vec![3, 7]);
        assert!(confirm(&[3, 7], 0, &[], &[], "## Page 22\n\nBox 11\n").is_empty());
        // Nor does a year, which reads as a number shown twice.
        assert!(confirm(&[3], 0, &[], &[], "Tax year 2020 summary\n").is_empty());
        // A page whose repeated run the font reads counts for its own text,
        // wherever it falls among the pages.
        let run = |page: u32, text: &str| {
            (
                page,
                Repeat {
                    at: [72.0, 700.0],
                    size: 10.0,
                    text: Some(text.to_string()),
                },
            )
        };
        let repeats = [run(65, "Page header"), run(90, "Total due 1,234.56")];
        assert_eq!(
            confirm(
                &[65, 90],
                0,
                &repeats,
                &[],
                "Total due 1,234.56 Total due 1,234.56\n"
            ),
            vec![90]
        );
    }

    /// An item of page 1 at `x`, on the baseline at 700.
    fn item(text: &str, x: f32, width: f32) -> TextItem {
        TextItem {
            text: text.to_string(),
            x,
            y: 700.0,
            width,
            height: 10.0,
            rotation: 0.0,
            advance_known: true,
            font: String::new(),
            font_tag: String::new(),
            legacy_symbol_rewrite: false,
            font_size: 10.0,
            page: 1,
            is_bold: false,
            is_italic: false,
            font_weight: None,
            bold_source: None,
            fixed_pitch: None,
            fill_color: None,
            stroke_color: None,
            render_mode: None,
            is_underline: false,
            is_strikeout: false,
            item_type: pdf_inspector::types::ItemType::Text,
            mcid: None,
            baseline_shift: 0.0,
        }
    }

    #[test]
    fn a_repeat_is_confirmed_by_the_text_at_it() {
        let at = |x: f64| {
            (
                1,
                Repeat {
                    at: [x, 700.0],
                    size: 10.0,
                    text: None,
                },
            )
        };
        // A line with a dash between its dates reads as the Markdown shows
        // it doubled.
        let period = "Statement period 09/01/2025 - 09/30/2025";
        let doubled_period = [item(&format!("{period} {period}"), 72.0, 380.0)];
        let markdown = format!("# {period} {period}\n");
        assert_eq!(
            confirm(&[1], 1, &[at(72.0)], &doubled_period, &markdown),
            vec![1]
        );
        // A year or a box number is confirmed only by the number alone at
        // the repeat, not by a header holding it.
        let header = [item("Tax year 2020 statement", 72.0, 120.0)];
        assert!(confirm(&[1], 1, &[at(72.0)], &header, "Tax year 2020\n").is_empty());
        let page_number = [item("55", 300.0, 10.0)];
        assert_eq!(
            confirm(&[1], 1, &[at(300.0)], &page_number, "Page 55\n"),
            vec![1]
        );
        // Each paint its own item, a value doubled glyph by glyph counts.
        let paints = [item("2", 200.0, 6.7), item("2", 200.6, 6.7)];
        assert_eq!(
            confirm(&[1], 1, &[at(200.0)], &paints, "Dependents claimed: 22\n"),
            vec![1]
        );
    }
}
