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
//! Both are read from the Markdown and reported, never repaired. A repeat
//! counts where it stands as the detector leaves it: on a line of its own,
//! after a label or a line of form fields ("Acct: 5678 Period: April 2025"),
//! which the detector keeps out of a table, or as a whole emphasized span;
//! a sentence that restates the first row does not. A cell holding two
//! amounts is a candidate, which the page's positioned text decides (see
//! `merged_on_one_line`): amounts of separate runs on one baseline were
//! merged, while amounts stacked one above the other, or written as one
//! run, stand as the page sets them. Where the text cannot be read there,
//! a candidate counts beside an empty cell, or in a column whose other rows
//! hold one amount.

/// What the checks found.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct TableFindings {
    pub(crate) row_repeated: bool,
    /// Body cells holding two or more amounts.
    pub(crate) merged: Vec<MergedCell>,
}

/// A body cell holding two or more amounts.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct MergedCell {
    /// Its amounts, in order.
    pub(crate) amounts: Vec<String>,
    /// The text of the row's other cells.
    pub(crate) row: Vec<String>,
    /// Whether the table alone shows signs of a merge: an empty cell beside
    /// it, or another body row holding exactly one amount in its column.
    pub(crate) in_table: bool,
}

/// Merged-cell candidates read per document.
const MAX_MERGED_CELLS: usize = 256;

use crate::text_paints::Placed;

/// The least length, in characters, of a row whose repeat counts.
const MIN_REPEATED_ROW_CHARS: usize = 8;
/// The most words a row of form fields before a repeat may hold.
const MAX_FORM_TOKENS: usize = 24;

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
        merged_cells(&rows, &mut found.merged);
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
    starts_line || prefix.ends_with(':') || emphasized || after_form_fields(paragraph, repeat)
}

/// Whether the text on the line before `repeat` is a row of form fields,
/// which the detector keeps out of a table: labels ending in a colon, each
/// with a value of up to three words ("Acct: 5678 Period: April 2025").
fn after_form_fields(paragraph: &[&str], repeat: &str) -> bool {
    let Some(line) = paragraph.last().map(|line| normalize(line)) else {
        return false;
    };
    if !ends_with_words(&line, repeat) {
        return false;
    }
    let tokens: Vec<&str> = line[..line.len() - repeat.len()]
        .split_whitespace()
        .collect();
    if tokens.is_empty() || tokens.len() > MAX_FORM_TOKENS {
        return false;
    }
    // Whether the tokens from each position on are form fields.
    let mut fields = vec![false; tokens.len() + 1];
    fields[tokens.len()] = true;
    for start in (0..tokens.len()).rev() {
        fields[start] = (1..=3).any(|label| {
            let end = start + label;
            end <= tokens.len()
                && tokens[start..end - 1]
                    .iter()
                    .all(|token| !token.contains(':'))
                && tokens[end - 1].len() > 1
                && tokens[end - 1].ends_with(':')
                && (0..=3).any(|value| {
                    end + value <= tokens.len()
                        && tokens[end..end + value]
                            .iter()
                            .all(|token| !token.contains(':'))
                        && fields[end + value]
                })
        });
    }
    fields[0]
}

/// The body cells holding two or more amounts, with whether the table
/// shows signs of a merge.
fn merged_cells(rows: &[Vec<String>], found: &mut Vec<MergedCell>) {
    let body = rows.get(1..).unwrap_or_default();
    for (index, row) in body.iter().enumerate() {
        for (column, cell) in row.iter().enumerate() {
            if amount_count(cell) < 2 || found.len() >= MAX_MERGED_CELLS {
                continue;
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
            let others = row
                .iter()
                .enumerate()
                .filter(|(other, text)| *other != column && !text.is_empty())
                .map(|(_, text)| normalize(text))
                .collect();
            found.push(MergedCell {
                amounts: cell.split_whitespace().map(str::to_string).collect(),
                row: others,
                in_table: beside_empty || single_below,
            });
        }
    }
}

/// How the page's positioned text decides a merged-cell candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Placement {
    /// Two of its amounts are separate runs on one baseline: merged.
    OneLine,
    /// Its amounts are read, stacked or as one run: as the page sets them.
    AsSet,
    /// Its amounts are not read as the page's own runs.
    Unread,
}

/// A run of a page's text where pdf-inspector reads it: its baseline's
/// left end, its advance, and its size.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Run<'a> {
    pub(crate) page: u32,
    pub(crate) text: &'a str,
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) width: f64,
    pub(crate) size: f64,
}

/// Where the page sets a candidate's amounts, from the positioned text of
/// the pages read and the runs their content places (see `Placed`). Two
/// consecutive amounts as separate runs on one baseline, the second to the
/// right of the first, were merged by the detector; so were amounts on two
/// lines when another cell of the row joins text from both, as the detector
/// merges rows, and amounts read as one item that a second placed run
/// starts inside, as pdf-inspector joins close runs. Amounts stacked on two
/// lines otherwise, or one plain run, stand as the page sets them. Anything
/// else, such as one run with a word gap between its strings, is unread.
pub(crate) fn merged_on_one_line(
    cell: &MergedCell,
    runs: &[Run<'_>],
    placed: &[Placed],
) -> Placement {
    let mut stacked = false;
    for pair in cell.amounts.windows(2) {
        for left in runs.iter().filter(|run| run.text.trim() == pair[0]) {
            let size = left.size.abs().max(1.0);
            let near = 0.25 * size;
            for right in runs
                .iter()
                .filter(|run| run.page == left.page && run.text.trim() == pair[1])
            {
                let across = (right.y - left.y).abs();
                if across <= near && right.x >= left.x + left.width - near {
                    return Placement::OneLine;
                }
                if across > near && across <= 4.0 * size {
                    if rows_merged(cell, runs, left, right.y) {
                        return Placement::OneLine;
                    }
                    stacked = true;
                }
            }
        }
    }
    let joined = cell.amounts.join(" ");
    let mut as_written = false;
    for run in runs
        .iter()
        .filter(|run| words(run.text).join(" ").contains(&joined))
    {
        let near = 0.25 * run.size.abs().max(1.0);
        let on_line =
            |start: &&Placed| start.page == run.page && (start.at[1] - run.y).abs() <= near;
        let starts: Vec<&Placed> = placed.iter().filter(on_line).collect();
        if starts
            .iter()
            .any(|start| start.at[0] > run.x + near && start.at[0] < run.x + run.width - near)
        {
            return Placement::OneLine;
        }
        as_written |= starts
            .iter()
            .any(|start| start.plain && (start.at[0] - run.x).abs() <= near);
    }
    if stacked || as_written {
        Placement::AsSet
    } else {
        Placement::Unread
    }
}

/// Whether another cell of a candidate's row joins text from the line of
/// `left` and the line at `other`: rows the detector merged.
fn rows_merged(cell: &MergedCell, runs: &[Run<'_>], left: &Run<'_>, other: f64) -> bool {
    let near = 0.25 * left.size.abs().max(1.0);
    cell.row.iter().any(|text| {
        let cell_words = words(text);
        let on = |y: f64| {
            runs.iter().any(|run| {
                run.page == left.page
                    && (run.y - y).abs() <= near
                    && contains_words(&cell_words, &words(run.text))
            })
        };
        on(left.y) && on(other)
    })
}

/// A text's words.
fn words(text: &str) -> Vec<&str> {
    text.split_whitespace().collect()
}

/// Whether `words` holds `part`, non-empty, as consecutive words.
fn contains_words(words: &[&str], part: &[&str]) -> bool {
    !part.is_empty() && words.windows(part.len()).any(|window| window == part)
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

    /// The amounts of the candidates a table's Markdown holds, and whether
    /// the table alone shows a merge.
    fn candidates(markdown: &str) -> Vec<(String, bool)> {
        check(markdown)
            .merged
            .into_iter()
            .map(|cell| (cell.amounts.join(" "), cell.in_table))
            .collect()
    }

    #[test]
    fn amounts_merged_into_one_cell_are_found() {
        let lots = "| Description | Proceeds | Basis | Wash sale |\n| --- | --- | --- | --- |\n| 100 sh XYZ | 2,815.50 | 2,610.25 205.25 |  |\n";
        assert_eq!(
            candidates(lots),
            vec![("2,610.25 205.25".to_string(), true)]
        );
        let ruled = "| Item | 2025 | 2024 |\n| --- | --- | --- |\n| Revenue | 236,480,212 10,024,724 |  |\n";
        assert_eq!(candidates(ruled).len(), 1);
        // One amount, a date and an amount, and counts are not candidates.
        for cell in [
            "2,610.25",
            "04/03/25 3,210.55",
            "12 34",
            "(1,250.00)",
            "100 shares",
        ] {
            let table = format!("| A | B |\n| --- | --- |\n| x | {cell} |\n");
            assert!(candidates(&table).is_empty(), "{cell}");
        }
        // A header row is not a body row, and tables inside code are not
        // read.
        assert!(candidates("| 2024 2025 | 1.5 2.5 |\n| --- | --- |\n| a | b |\n").is_empty());
        assert!(candidates("```\n| a | 1.00 2.00 |\n```\n").is_empty());
        // Two amounts in every row of a column show no merge in the table
        // alone; one row with a single amount below does.
        let stacked = "| Employer | Wages | Withheld federal / state |\n| --- | --- | --- |\n| Example Manufacturing Inc. | 52,000.00 | 6,240.00 2,600.00 |\n| Example Services LLC | 18,000.00 | 2,160.00 900.00 |\n";
        assert!(candidates(stacked).iter().all(|(_, in_table)| !in_table));
        let uneven = "| Item | Basis | Adjustment |\n| --- | --- | --- |\n| Lot 1 | 2,610.25 205.25 | 12.00 |\n| Lot 2 | 1,200.00 | 3.00 |\n";
        assert_eq!(
            candidates(uneven),
            vec![("2,610.25 205.25".to_string(), true)]
        );
        assert!(is_amount("$1,234.56") && is_amount("(12.00)") && is_amount("5.25%"));
        assert!(!is_amount("2023") && !is_amount("1,23") && !is_amount("12.") && !is_amount("a.b"));
    }

    #[test]
    fn where_the_page_sets_merged_amounts_decides_them() {
        let run = |text: &'static str, x: f64, y: f64, width: f64| Run {
            page: 1,
            text,
            x,
            y,
            width,
            size: 9.0,
        };
        let cell = |amounts: &[&str]| MergedCell {
            amounts: amounts.iter().map(|amount| amount.to_string()).collect(),
            row: vec!["Lot 1".to_string()],
            in_table: false,
        };
        let basis = cell(&["2,610.25", "205.25"]);
        // Two columns' runs on one baseline were merged.
        let columns = [
            run("2,610.25", 365.0, 685.0, 35.0),
            run("205.25", 434.5, 685.0, 27.5),
        ];
        assert_eq!(
            merged_on_one_line(&basis, &columns, &[]),
            Placement::OneLine
        );
        // Stacked one above the other, or written as one run, they stand as
        // the page sets them.
        let stacked = [
            run("2,610.25", 380.8, 693.4, 42.0),
            run("205.25", 380.8, 679.6, 33.0),
        ];
        assert_eq!(merged_on_one_line(&basis, &stacked, &[]), Placement::AsSet);
        let one_run = [run("2,610.25 205.25", 180.0, 721.0, 63.0)];
        assert_eq!(
            merged_on_one_line(
                &basis,
                &one_run,
                &[Placed {
                    page: 1,
                    at: [180.0, 721.0],
                    plain: true
                }]
            ),
            Placement::AsSet
        );
        // One item that a second run starts inside was joined from two; one
        // plain run is as written, and one with a word gap in it unread.
        let joined = [run("2,610.25 205.25", 308.9, 686.0, 59.1)];
        let start = |x: f64, plain: bool| Placed {
            page: 1,
            at: [x, 686.0],
            plain,
        };
        assert_eq!(
            merged_on_one_line(&basis, &joined, &[start(308.9, true), start(343.5, true)]),
            Placement::OneLine
        );
        assert_eq!(
            merged_on_one_line(&basis, &joined, &[start(308.9, true)]),
            Placement::AsSet
        );
        assert_eq!(
            merged_on_one_line(&basis, &joined, &[start(308.9, false)]),
            Placement::Unread
        );
        // Two rows the detector merged: the label cell joins both lines.
        let rows = MergedCell {
            row: vec!["Capital gain distributions Total income".to_string()],
            ..cell(&["12,004", "237,787,636"])
        };
        let merged_rows = [
            run("Capital gain distributions", 64.0, 597.0, 108.9),
            run("12,004", 276.5, 597.0, 30.6),
            run("Total income", 64.0, 575.0, 57.2),
            run("237,787,636", 251.5, 575.0, 56.0),
        ];
        assert_eq!(
            merged_on_one_line(&rows, &merged_rows, &[]),
            Placement::OneLine
        );
        // Amounts the runs do not show are unread; so is the second amount
        // left of the first.
        assert_eq!(merged_on_one_line(&basis, &[], &[]), Placement::Unread);
        let reversed = [
            run("2,610.25", 434.5, 685.0, 35.0),
            run("205.25", 365.0, 685.0, 27.5),
        ];
        assert_eq!(
            merged_on_one_line(&basis, &reversed, &[]),
            Placement::Unread
        );
    }

    #[test]
    fn a_repeat_after_form_fields_is_found() {
        for fields in [
            "Acct: 5678 Period: April 2025",
            "Acct: 5678 Branch: Main St Page: 1",
            "Account number: 5678",
        ] {
            let markdown = format!("{fields} 04/03/25 ACH deposit payroll 3,210.55 15,660.55\n\n|04/03/25|ACH deposit payroll|3,210.55|15,660.55|\n|---|---|---|---|\n|04/05/25|Card purchase|42.10|15,618.45|\n");
            assert!(check(&markdown).row_repeated, "{fields}");
        }
        // A sentence with a colon, or a label whose value runs long, is text
        // the page shows.
        for fields in ["Note: the first deposit was", "Summary of your account"] {
            let markdown = format!("{fields} 04/03/25 ACH deposit payroll 3,210.55 15,660.55\n\n|04/03/25|ACH deposit payroll|3,210.55|15,660.55|\n|---|---|---|---|\n");
            assert!(!check(&markdown).row_repeated, "{fields}");
        }
    }
}
