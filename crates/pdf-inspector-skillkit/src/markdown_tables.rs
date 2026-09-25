//! Tables pdf-inspector 1.24.0 may have misread, found in its Markdown.
//!
//! **A first row repeated above its table.** The heuristic table detector
//! leaves out of a table's region any item within a flat 15 points of a row
//! it skipped, even an item it already placed in the first row it kept. So
//! the first rows of a compact table, such as a statement's opening balance
//! or first deposit, also end the paragraph before it, and their amounts
//! appear twice (open upstream #406, #531).
//!
//! **Amounts merged into one cell.** A sparse column is merged into a
//! numeric neighbour, and a ruled table places a whole item by its centre,
//! so adjacent columns of amounts, such as a 1099-B's wash-sale adjustments
//! beside the cost basis, can land in one cell (open upstream #424).
//!
//! Both are read from the Markdown alone and reported, never repaired. A
//! repeat counts where it stands as the detector leaves it: on a line of
//! its own, after a label, or as a whole emphasized span; a sentence that
//! restates the first row does not. A cell holding two amounts counts only
//! beside an empty cell, or in a column whose other rows hold one amount: a
//! column that stacks two amounts by design holds two in every row.

/// What the checks found.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct TableFindings {
    pub(crate) row_repeated: bool,
    pub(crate) values_merged: bool,
}

/// The least length, in characters, of a row whose repeat counts.
const MIN_REPEATED_ROW_CHARS: usize = 8;

/// Check the tables of pdf-inspector's Markdown.
pub(crate) fn check(markdown: &str) -> TableFindings {
    let mut found = TableFindings::default();
    // The text block before the next table, and whether a blank line ended
    // it.
    let mut paragraph: Vec<&str> = Vec::new();
    let mut closed = false;
    let mut in_fence = false;
    let mut lines = markdown.lines().peekable();
    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            paragraph.clear();
            closed = false;
            continue;
        }
        if in_fence {
            continue;
        }
        if trimmed.is_empty() {
            closed = true;
            continue;
        }
        if !is_table_row(trimmed) {
            if closed {
                paragraph.clear();
                closed = false;
            }
            paragraph.push(trimmed);
            continue;
        }
        let mut rows = vec![trimmed];
        while let Some(next) = lines.peek().map(|next| next.trim()) {
            if !is_table_row(next) {
                break;
            }
            rows.push(next);
            lines.next();
        }
        let rows: Vec<Vec<String>> = rows
            .into_iter()
            .filter(|row| !is_separator(row))
            .map(cells)
            .collect();
        for row in rows.iter().take(2) {
            let shown: Vec<&str> = row
                .iter()
                .map(String::as_str)
                .filter(|cell| !cell.is_empty())
                .collect();
            let joined = normalize(&shown.join(" "));
            found.row_repeated |= joined.chars().count() > MIN_REPEATED_ROW_CHARS
                && joined.contains(|character: char| character.is_ascii_digit())
                && repeats_as_detected(&paragraph, &joined);
        }
        found.values_merged |= merged_cell(&rows);
        paragraph.clear();
        closed = false;
    }
    found
}

/// Whether the text block ends with `repeat` where the detector leaves a
/// row it misreads: starting a line, after a label ending in a colon, or
/// as a whole emphasized span at the end of the last line.
fn repeats_as_detected(paragraph: &[&str], repeat: &str) -> bool {
    let before = normalize(&paragraph.join(" "));
    if !ends_with_words(&before, repeat) {
        return false;
    }
    let prefix = before[..before.len() - repeat.len()].trim_end();
    // The text of the block's first lines, line by line.
    let mut lines_before = String::new();
    let mut starts_line = false;
    for line in paragraph {
        if lines_before == prefix {
            starts_line = true;
            break;
        }
        if lines_before.len() > prefix.len() {
            break;
        }
        let line = normalize(line);
        if !line.is_empty() && !lines_before.is_empty() {
            lines_before.push(' ');
        }
        lines_before.push_str(&line);
    }
    let emphasized = paragraph.last().is_some_and(|line| {
        ["**", "__", "*", "_"].iter().any(|marker| {
            line.strip_suffix(marker)
                .and_then(|inner| inner.rsplit_once(marker))
                .is_some_and(|(_, span)| normalize(span) == repeat)
        })
    });
    starts_line || prefix.ends_with(':') || emphasized
}

/// Whether a body cell holding two or more amounts shows signs of a merge:
/// an empty cell beside it, or another body row holding exactly one amount
/// in its column.
fn merged_cell(rows: &[Vec<String>]) -> bool {
    let body = rows.get(1..).unwrap_or_default();
    body.iter().enumerate().any(|(index, row)| {
        row.iter().enumerate().any(|(column, cell)| {
            if amount_count(cell) < 2 {
                return false;
            }
            let beside_empty = [column.checked_sub(1), Some(column + 1)]
                .into_iter()
                .flatten()
                .filter_map(|neighbour| row.get(neighbour))
                .any(String::is_empty);
            let single_below = body
                .iter()
                .enumerate()
                .filter(|(other, _)| *other != index)
                .filter_map(|(_, other)| other.get(column))
                .any(|other| amount_count(other) == 1);
            beside_empty || single_below
        })
    })
}

fn is_table_row(line: &str) -> bool {
    line.starts_with('|') && line.ends_with('|') && line.len() > 1
}

fn is_separator(row: &str) -> bool {
    row.contains('-')
        && row
            .chars()
            .all(|character| matches!(character, '|' | '-' | ':' | ' '))
}

/// A row's cells, split on pipes a backslash does not escape.
fn cells(row: &str) -> Vec<String> {
    let inner = &row[1..row.len() - 1];
    let mut cells = vec![String::new()];
    let mut escaped = false;
    for character in inner.chars() {
        match character {
            '|' if !escaped => cells.push(String::new()),
            _ => cells.last_mut().expect("a cell").push(character),
        }
        escaped = character == '\\' && !escaped;
    }
    cells.iter().map(|cell| cell.trim().to_string()).collect()
}

/// Text as a reader sees it: no emphasis markers or escapes, and single
/// spaces.
fn normalize(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\\' if characters
                .peek()
                .is_some_and(|next| next.is_ascii_punctuation()) =>
            {
                plain.extend(characters.next());
            }
            '*' | '_' | '`' => {}
            _ => plain.push(character),
        }
    }
    plain.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether `text` ends with `words`, at a word boundary.
fn ends_with_words(text: &str, words: &str) -> bool {
    !words.is_empty()
        && text
            .strip_suffix(words)
            .is_some_and(|rest| rest.is_empty() || rest.ends_with(' '))
}

/// How many amounts a cell holds, when it holds nothing else.
fn amount_count(cell: &str) -> usize {
    let tokens: Vec<&str> = cell.split_whitespace().collect();
    if tokens.iter().all(|token| is_amount(token)) {
        tokens.len()
    } else {
        0
    }
}

/// An amount: digits grouped by commas or with decimals, with an optional
/// sign, currency sign, parentheses, or percent sign.
fn is_amount(token: &str) -> bool {
    let token = token.trim_start_matches(['(', '-', '+', '\u{2212}']);
    let token = token.trim_start_matches(['$', '\u{20ac}', '\u{a3}']);
    let token = token.trim_end_matches([')', '%', '-']);
    let (whole, decimals) = match token.split_once('.') {
        Some((whole, decimals)) => (whole, Some(decimals)),
        None => (token, None),
    };
    if decimals.is_some_and(|decimals| {
        decimals.is_empty() || !decimals.bytes().all(|b| b.is_ascii_digit())
    }) {
        return false;
    }
    let groups: Vec<&str> = whole.split(',').collect();
    let grouped = groups.len() > 1
        && (1..=3).contains(&groups[0].len())
        && groups[1..].iter().all(|group| group.len() == 3);
    let digits = !whole.is_empty()
        && groups
            .iter()
            .all(|group| !group.is_empty() && group.bytes().all(|b| b.is_ascii_digit()));
    digits && (grouped || decimals.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_first_row_repeated_above_its_table_is_found() {
        let statement = "Deposits and other credits\n\n04/03/25 ACH deposit payroll 3,210.55 15,660.55\n\n| Date | Description | Amount | Balance |\n| --- | --- | --- | --- |\n| 04/03/25 | ACH deposit payroll | 3,210.55 | 15,660.55 |\n| 04/05/25 | Card purchase | 42.10 | 15,618.45 |\n";
        assert!(check(statement).row_repeated);
        // The repeat may end a longer paragraph, and use emphasis.
        let inline = "Summary of your account **Balance at the start of the period 12,450.00**\n| Balance at the start of the period | 12,450.00 |\n| --- | --- |\n| Deposits | 3,210.55 |\n";
        assert!(check(inline).row_repeated);
        // A paragraph ending otherwise, a header repeated without a number,
        // and part of a word are not repeats.
        for markdown in [
            "Opening balance 12,450.00 as of April 1\n\n| Date | Amount |\n| --- | --- |\n| 04/03 | 3,210.55 |\n",
            "Columns: Date Description Amount\n\n| Date | Description | Amount |\n| --- | --- | --- |\n| x | y | z |\n",
            "Paid on 104/03/25 ACH 3,210.55\n\n| 04/03/25 | ACH | 3,210.55 |\n| --- | --- | --- |\n",
        ] {
            assert!(!check(markdown).row_repeated, "{markdown}");
        }
        // The detector leaves a repeat after a label, as on pages of the
        // Internal Revenue Code; a sentence that restates the row is text
        // the page shows.
        let labelled = "**Beginning after: And before: Percent:** December 31, 1983 January 1, 1988 11.40\n\n|December 31, 1983|January 1, 1988|11.40|\n|---|---|---|\n";
        assert!(check(labelled).row_repeated);
        let sentence = "One Form W-2 was received from Example Manufacturing Inc. 52,000.00\n\n|Example Manufacturing Inc.|52,000.00|\n|---|---|\n|Total wages|52,000.00|\n";
        assert!(!check(sentence).row_repeated);
    }

    #[test]
    fn amounts_merged_into_one_cell_are_found() {
        let lots = "| Description | Proceeds | Basis | Wash sale |\n| --- | --- | --- | --- |\n| 100 sh XYZ | 2,815.50 | 2,610.25 205.25 |  |\n";
        assert!(check(lots).values_merged);
        let ruled = "| Item | 2025 | 2024 |\n| --- | --- | --- |\n| Revenue | 236,480,212 10,024,724 |  |\n";
        assert!(check(ruled).values_merged);
        // One amount, a date and an amount, and counts are not merged.
        for cell in [
            "2,610.25",
            "04/03/25 3,210.55",
            "12 34",
            "(1,250.00)",
            "100 shares",
        ] {
            let table = format!("| A | B |\n| --- | --- |\n| x | {cell} |\n");
            assert!(!check(&table).values_merged, "{cell}");
        }
        // A header row is not a body row.
        assert!(!check("| 2024 2025 | 1.5 2.5 |\n| --- | --- |\n| a | b |\n").values_merged);
        // A column that stacks two amounts in every row does so by design;
        // one row with a single amount below shows a merge.
        let stacked = "| Employer | Wages | Withheld federal / state |\n| --- | --- | --- |\n| Example Manufacturing Inc. | 52,000.00 | 6,240.00 2,600.00 |\n| Example Services LLC | 18,000.00 | 2,160.00 900.00 |\n";
        assert!(!check(stacked).values_merged);
        let price = "| Item | Price | Total |\n| --- | --- | --- |\n| Return preparation | 750.00 (75.00) | 675.00 |\n";
        assert!(!check(price).values_merged);
        let uneven = "| Item | Basis | Adjustment |\n| --- | --- | --- |\n| Lot 1 | 2,610.25 205.25 | 12.00 |\n| Lot 2 | 1,200.00 | 3.00 |\n";
        assert!(check(uneven).values_merged);
        // Tables inside code are not read.
        assert!(!check("```\n| a | 1.00 2.00 |\n```\n").values_merged);
        assert!(is_amount("$1,234.56") && is_amount("(12.00)") && is_amount("5.25%"));
        assert!(!is_amount("2023") && !is_amount("1,23") && !is_amount("12.") && !is_amount("a.b"));
    }
}
