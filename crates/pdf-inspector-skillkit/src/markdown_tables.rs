//! Tables pdf-inspector 1.25.0 may have misread, found in its Markdown.
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
//! **Amounts pushed out of their rows.** A column the grid drops, such as a
//! 1099-B's sparse wash-sale adjustments or a long statement's amounts,
//! follows the table instead, apart from the rows the page sets them on:
//! an amount a line, several to a line, after the column's heading, or as
//! a table of their own beside their currency signs (open upstream #424).
//!
//! All are read from the Markdown and reported, never repaired. A repeat
//! counts where it stands as the detector leaves it: on a line of its own,
//! after a label or a line of form fields ("Acct: 5678 Period: April 2025"),
//! which the detector keeps out of a table, or as a whole emphasized span;
//! a row of three amounts or more counts wherever it ends the text, as
//! after the heading words the detector left there too. A sentence that
//! restates a shorter first row does not. A cell holding two
//! amounts is a candidate, which the page's positioned text decides (see
//! `Layout::placement`): amounts of separate runs on one baseline, each
//! under a heading of its own in the table's header, were merged, while
//! amounts stacked one above the other, or written as one run, stand as the
//! page sets them. Where the text cannot be read there, a candidate counts
//! beside an empty cell, or in a column whose other rows hold one amount.
//! Amounts right after a table, which its cells do not hold, count where
//! the page sets most of them on the lines of its body rows (see
//! `Layout::detached`).

/// What the checks found.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct TableFindings {
    pub(crate) row_repeated: bool,
    /// Body cells holding two or more amounts.
    pub(crate) merged: Vec<MergedCell>,
    /// Amounts right after a table.
    pub(crate) detached: Vec<Detached>,
}

/// Amounts the Markdown shows right after a table, and the table's body
/// cells that hold more than amounts, such as its labels and dates.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Detached {
    pub(crate) amounts: Vec<String>,
    pub(crate) labels: Vec<String>,
}

/// Tables read for amounts after them, and the amounts read after each.
const MAX_DETACHED_TABLES: usize = 64;
const MAX_DETACHED_AMOUNTS: usize = 64;
/// Letters and digits a body cell needs to place a row's line, so that a
/// stray mark, such as a footnote number, does not.
const MIN_LABEL_CHARACTERS: usize = 3;

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

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};

use crate::text_paints::Placed;

/// The least length, in characters, of a row whose repeat counts.
const MIN_REPEATED_ROW_CHARS: usize = 8;
/// Amounts a repeated row holds to count wherever it ends the text.
const MIN_REPEATED_AMOUNTS: usize = 3;
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
        if found.detached.len() < MAX_DETACHED_TABLES {
            // An amount the table holds, such as a total restating a row,
            // is not one pushed out of it.
            let held: HashSet<String> = rows
                .iter()
                .skip(1)
                .flatten()
                .flat_map(|cell| cell.split_whitespace())
                .map(normalize)
                .filter(|token| is_amount(token))
                .collect();
            let amounts: Vec<String> = amounts_after(lines.clone())
                .into_iter()
                .filter(|amount| !held.contains(amount))
                .collect();
            if !amounts.is_empty() {
                let labels = rows
                    .iter()
                    .skip(1)
                    .flatten()
                    .filter(|cell| {
                        amount_count(cell) == 0
                            && cell.chars().filter(|c| c.is_alphanumeric()).count()
                                >= MIN_LABEL_CHARACTERS
                    })
                    .map(|cell| normalize(cell))
                    .collect();
                found.detached.push(Detached { amounts, labels });
            }
        }
        paragraph.clear();
        closed = false;
    }
    found
}

/// The amounts `lines` open with, blank lines aside, as pdf-inspector sets
/// out a column the grid drops: lines of amounts, one or more to a line and
/// each with a credit or debit mark if any ("12.40 CR"), which may follow a
/// heading line, such as the column's own ("## Amount"); or a table of
/// amounts alone, with the currency signs set apart ("|$|12.40|").
fn amounts_after<'a>(lines: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut amounts = Vec::new();
    let mut headed = false;
    let mut lines = lines.map(str::trim).peekable();
    while let Some(line) = lines.next() {
        if line.is_empty() {
            continue;
        }
        if is_table_row(line) {
            if !amounts.is_empty() {
                break;
            }
            let mut rows = vec![line];
            while let Some(&next) = lines.peek() {
                if !is_table_row(next) {
                    break;
                }
                rows.push(next);
                lines.next();
            }
            return table_amounts(&rows);
        }
        if line.starts_with('#') && amounts.is_empty() && !headed {
            headed = true;
            continue;
        }
        let Some(found) = line_amounts(&normalize(line)) else {
            break;
        };
        for amount in found {
            if amounts.len() >= MAX_DETACHED_AMOUNTS {
                return amounts;
            }
            amounts.push(amount);
        }
    }
    amounts
}

/// The amounts a line holds, when it holds nothing else but a credit or
/// debit mark after an amount.
fn line_amounts(line: &str) -> Option<Vec<String>> {
    let mut amounts = Vec::new();
    let mut marked = true;
    for token in line.split_whitespace() {
        if is_amount(token) {
            amounts.push(token.to_string());
            marked = false;
        } else if !marked && (token.eq_ignore_ascii_case("CR") || token.eq_ignore_ascii_case("DR"))
        {
            marked = true;
        } else {
            return None;
        }
    }
    (!amounts.is_empty()).then_some(amounts)
}

/// The amounts a table holds, up to `MAX_DETACHED_AMOUNTS`, when its cells
/// hold nothing else but currency signs; none otherwise.
fn table_amounts(rows: &[&str]) -> Vec<String> {
    let mut amounts = Vec::new();
    for row in rows.iter().filter(|row| !is_separator(row)) {
        for cell in cells(row) {
            for token in cell.split_whitespace().map(normalize) {
                if is_amount(&token) {
                    if amounts.len() < MAX_DETACHED_AMOUNTS {
                        amounts.push(token);
                    }
                } else if !matches!(token.as_str(), "$" | "\u{20ac}" | "\u{a3}") {
                    return Vec::new();
                }
            }
        }
    }
    amounts
}

/// Whether the text block ends with `repeat` where the detector leaves a
/// row it misreads: starting a line, after a label ending in a colon, or
/// as a whole emphasized span at the end of the last line; or anywhere, for
/// a row of `MIN_REPEATED_AMOUNTS` amounts or more.
fn repeats_as_detected(paragraph: &[&str], repeat: &str) -> bool {
    let before = normalize(&paragraph.join(" "));
    if !ends_with_words(&before, repeat) {
        return false;
    }
    if repeat
        .split_whitespace()
        .filter(|token| is_amount(token))
        .count()
        >= MIN_REPEATED_AMOUNTS
    {
        return true;
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

/// Steps placing candidates may take per document; past them, a candidate
/// is unread. Placing the amounts after tables takes steps of its own, as
/// many.
const MAX_PLACEMENT_STEPS: usize = 4_000_000;
/// How far above the lowest line of a table's header its other lines sit,
/// in sizes: a heading of three lines.
const HEADER_LINES: f64 = 2.5;

/// The positioned text of the pages read, indexed for placing candidates:
/// runs by their text, a word they hold, and their line, whether each may
/// head a column, and the placed run starts by line.
pub(crate) struct Layout<'a> {
    runs: &'a [Run<'a>],
    /// Each run's words.
    words: Vec<Vec<&'a str>>,
    /// Whether each run holds more than amounts, as a heading does.
    heading: Vec<bool>,
    /// Runs by their text, by page and baseline.
    by_text: HashMap<&'a str, Vec<usize>>,
    /// Runs by page and text, by baseline.
    by_page_text: HashMap<(u32, &'a str), Vec<usize>>,
    /// Runs by the first word they hold, by page and baseline.
    by_word: HashMap<&'a str, Vec<usize>>,
    /// Runs by page, by baseline.
    lines: HashMap<u32, Vec<usize>>,
    /// Placed run starts by page, by baseline.
    placed: HashMap<u32, Vec<Placed>>,
    /// What the header above two amounts decided, by their page, line, and
    /// ends in quarter points: the pages repeat a cell's amounts in row
    /// after row.
    headed: RefCell<HashMap<(u32, [i64; 5]), bool>>,
    steps: Cell<usize>,
    /// The steps placing the amounts after tables took.
    detached_steps: Cell<usize>,
}

impl<'a> Layout<'a> {
    pub(crate) fn new(runs: &'a [Run<'a>], placed: &[Placed]) -> Self {
        let words: Vec<Vec<&'a str>> = runs.iter().map(|run| words(run.text)).collect();
        let heading = runs
            .iter()
            .map(|run| amount_count(run.text) == 0 && !run.text.trim().is_empty())
            .collect();
        let mut order: Vec<usize> = (0..runs.len()).collect();
        order.sort_by(|&a, &b| {
            (runs[a].page, runs[a].y)
                .partial_cmp(&(runs[b].page, runs[b].y))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut layout = Layout {
            runs,
            words,
            heading,
            by_text: HashMap::new(),
            by_page_text: HashMap::new(),
            by_word: HashMap::new(),
            lines: HashMap::new(),
            placed: HashMap::new(),
            headed: RefCell::new(HashMap::new()),
            steps: Cell::new(0),
            detached_steps: Cell::new(0),
        };
        let mut firsts: HashSet<&str> = HashSet::new();
        for &index in &order {
            let run = &runs[index];
            let text = run.text.trim();
            layout.by_text.entry(text).or_default().push(index);
            layout
                .by_page_text
                .entry((run.page, text))
                .or_default()
                .push(index);
            firsts.clear();
            for &word in &layout.words[index] {
                if firsts.insert(word) {
                    layout.by_word.entry(word).or_default().push(index);
                }
            }
            layout.lines.entry(run.page).or_default().push(index);
        }
        for start in placed {
            layout.placed.entry(start.page).or_default().push(*start);
        }
        for starts in layout.placed.values_mut() {
            starts.sort_by(|a, b| {
                a.at[1]
                    .partial_cmp(&b.at[1])
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }
        layout
    }

    /// Take a step placing candidates, while steps are left.
    fn step(&self) -> bool {
        take_step(&self.steps)
    }

    /// The runs of `indices`, ordered by baseline, from `low` to `high`.
    fn band<'b>(&self, indices: &'b [usize], low: f64, high: f64) -> &'b [usize] {
        let from = indices.partition_point(|&index| self.runs[index].y < low);
        let to = indices.partition_point(|&index| self.runs[index].y <= high);
        &indices[from..to.max(from)]
    }

    /// Where the page sets a candidate's amounts. Two consecutive amounts
    /// as separate runs on one baseline, the second to the right of the
    /// first, were merged by the detector where the table's header heads
    /// them as two columns (see `columns_headed`); so were amounts on two
    /// lines when another cell of the row joins text from both, as the
    /// detector merges rows, and amounts read as one item that a second
    /// placed run starts inside, as pdf-inspector joins close runs, under
    /// such a header. Without it, runs side by side are unread: an amount
    /// with its percentage in parentheses is one cell's text. Amounts
    /// stacked on two lines otherwise, or one plain run, stand as the page
    /// sets them. Anything else, such as one run with a word gap between its
    /// strings, is unread, as is every candidate once the steps run out.
    pub(crate) fn placement(&self, cell: &MergedCell) -> Placement {
        let mut stacked = false;
        let mut beside = false;
        for pair in cell.amounts.windows(2) {
            for &left_index in self.by_text.get(pair[0].as_str()).into_iter().flatten() {
                let left = &self.runs[left_index];
                let size = left.size.abs().max(1.0);
                let near = 0.25 * size;
                let Some(rights) = self.by_page_text.get(&(left.page, pair[1].as_str())) else {
                    continue;
                };
                for &right_index in self.band(rights, left.y - 4.0 * size, left.y + 4.0 * size) {
                    if !self.step() {
                        return Placement::Unread;
                    }
                    let right = &self.runs[right_index];
                    let across = (right.y - left.y).abs();
                    if across <= near {
                        if right.x < left.x + left.width - near {
                            continue;
                        }
                        let first = [left.x, left.x + left.width];
                        let second = [right.x, right.x + right.width];
                        if self.columns_headed(left.page, left.y, first, second, size) {
                            return Placement::OneLine;
                        }
                        beside = true;
                    } else {
                        if self.rows_merged(cell, left, right.y) {
                            return Placement::OneLine;
                        }
                        stacked = true;
                    }
                }
            }
        }
        let first = cell.amounts.first().map(String::as_str).unwrap_or_default();
        let amounts: Vec<&str> = cell.amounts.iter().map(String::as_str).collect();
        let mut as_written = false;
        for &index in self.by_word.get(first).into_iter().flatten() {
            if !self.step() {
                return Placement::Unread;
            }
            if !contains_words(&self.words[index], &amounts) {
                continue;
            }
            let run = &self.runs[index];
            let size = run.size.abs().max(1.0);
            let near = 0.25 * size;
            let starts = self
                .placed
                .get(&run.page)
                .map(Vec::as_slice)
                .unwrap_or_default();
            let from = starts.partition_point(|start| start.at[1] < run.y - near);
            for start in &starts[from..] {
                if start.at[1] > run.y + near || !self.step() {
                    break;
                }
                let inside = start.at[0] > run.x + near && start.at[0] < run.x + run.width - near;
                if inside {
                    // The first amount ends a word gap or so before the
                    // second starts.
                    let first = [run.x, start.at[0] - near];
                    let second = [start.at[0], run.x + run.width];
                    if self.columns_headed(run.page, run.y, first, second, size) {
                        return Placement::OneLine;
                    }
                    beside = true;
                }
                as_written |= start.plain && (start.at[0] - run.x).abs() <= near;
            }
        }
        if beside {
            Placement::Unread
        } else if stacked || as_written {
            Placement::AsSet
        } else {
            Placement::Unread
        }
    }

    /// Whether the page sets most of the amounts the Markdown shows after a
    /// table, of those it reads, on the lines of the table's body rows: each
    /// on a line that, read left to right, holds a whole label cell, such as
    /// a row's date or description. Amounts that only share a baseline with
    /// a row by chance, as a column beside the table may, are fewer. The
    /// steps this takes are its own, so tables are read after any number of
    /// candidates were placed; once they run out, no table is detached.
    pub(crate) fn detached(&self, table: &Detached) -> bool {
        let labels: HashSet<Vec<&str>> = table.labels.iter().map(|label| words(label)).collect();
        let mut lengths: Vec<usize> = labels
            .iter()
            .map(Vec::len)
            .filter(|&length| length > 0)
            .collect();
        lengths.sort_unstable();
        lengths.dedup();
        let mut beside: Vec<usize> = Vec::new();
        let mut line_words: Vec<&str> = Vec::new();
        let (mut read, mut on_rows) = (0, 0);
        for amount in &table.amounts {
            let runs = self.by_text.get(amount.as_str());
            read += usize::from(runs.is_some());
            'runs: for &index in runs.into_iter().flatten() {
                let run = &self.runs[index];
                let near = 0.25 * run.size.abs().max(1.0);
                let Some(line) = self.lines.get(&run.page) else {
                    continue;
                };
                beside.clear();
                for &other in self.band(line, run.y - near, run.y + near) {
                    if !take_step(&self.detached_steps) {
                        return false;
                    }
                    if other != index {
                        beside.push(other);
                    }
                }
                beside.sort_by(|&a, &b| {
                    self.runs[a]
                        .x
                        .partial_cmp(&self.runs[b].x)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                line_words.clear();
                for &other in &beside {
                    line_words.extend(&self.words[other]);
                }
                for &length in &lengths {
                    for window in line_words.windows(length) {
                        if !take_step(&self.detached_steps) {
                            return false;
                        }
                        if labels.contains(window) {
                            on_rows += 1;
                            break 'runs;
                        }
                    }
                }
            }
        }
        on_rows > 0 && 2 * on_rows > read
    }

    /// Whether the table's header heads two amounts on the line at `y`,
    /// each given by its left and right ends, as two columns: on the
    /// header's lowest line, the first above them holding text over them,
    /// or a line just above it (`HEADER_LINES`), one heading lies over the
    /// first amount and ends before the second starts, and another,
    /// starting and ending past it, lies over the second, as a 1099-B's
    /// "Cost basis" and "Wash sale" head theirs, joined by pdf-inspector or
    /// not (see `heading_parts`). One heading over both, such as "Gain/loss
    /// (percent)" over an amount and its percentage, heads one column, and
    /// text further up, such as a page's folio, heads none.
    fn columns_headed(
        &self,
        page: u32,
        y: f64,
        first: [f64; 2],
        second: [f64; 2],
        size: f64,
    ) -> bool {
        let quarter = |value: f64| (4.0 * value).round() as i64;
        let key = (
            page,
            [y, first[0], first[1], second[0], second[1]].map(quarter),
        );
        if let Some(&headed) = self.headed.borrow().get(&key) {
            return headed;
        }
        let headed = self.header_heads(page, y, first, second, size);
        self.headed.borrow_mut().insert(key, headed);
        headed
    }

    /// [`Layout::columns_headed`], worked out.
    fn header_heads(
        &self,
        page: u32,
        y: f64,
        first: [f64; 2],
        second: [f64; 2],
        size: f64,
    ) -> bool {
        let near = 0.25 * size;
        let Some(line) = self.lines.get(&page) else {
            return false;
        };
        let over = |index: usize| {
            let run = &self.runs[index];
            self.heading[index] && run.x < second[1] && run.x + run.width > first[0]
        };
        // The header's lowest line is the first above holding text over the
        // amounts; the lines between hold the amounts of the rows above.
        let above = line.partition_point(|&index| self.runs[index].y <= y + near);
        let mut lowest = None;
        for &index in &line[above..] {
            if !self.step() {
                return false;
            }
            if over(index) {
                lowest = Some(self.runs[index].y);
                break;
            }
        }
        let Some(lowest) = lowest else {
            return false;
        };
        let header = self.band(line, lowest - near, lowest + HEADER_LINES * size);
        let mut parts = Vec::new();
        for &index in header {
            if over(index) && !self.heading_parts(index, header, &mut parts) {
                return false;
            }
        }
        parts.iter().any(|one| {
            spans_over(*one, first)
                && one[1] <= second[0] + near
                && parts.iter().any(|other| {
                    other[0] >= one[0] + near
                        && other[1] >= one[1] + near
                        && spans_over(*other, second)
                })
        })
    }

    /// Add a heading run of the header to `parts`, by the left and right
    /// ends of each heading it holds. Where pdf-inspector joins runs the
    /// page places apart, fewer runs than the heading has words, each run's
    /// text is a heading, from where it starts to where the next starts;
    /// otherwise, as when the page places every word or glyph apart, the
    /// run is one heading. A placed run that starts another heading of the
    /// header belongs to that heading. False once the steps run out.
    fn heading_parts(&self, index: usize, header: &[usize], parts: &mut Vec<[f64; 2]>) -> bool {
        let run = &self.runs[index];
        let near = 0.25 * run.size.abs().max(1.0);
        let (left, right) = (run.x, run.x + run.width);
        let starts = self
            .placed
            .get(&run.page)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let from = starts.partition_point(|start| start.at[1] < run.y - near);
        let mut cuts: Vec<f64> = Vec::new();
        for start in &starts[from..] {
            if start.at[1] > run.y + near {
                break;
            }
            if !self.step() {
                return false;
            }
            let x = start.at[0];
            if x <= left + near || x >= right - near {
                continue;
            }
            let mut begins_other = false;
            for &other in header {
                if !self.step() {
                    return false;
                }
                let other_run = &self.runs[other];
                if other != index
                    && (other_run.x - x).abs() <= near
                    && (other_run.y - run.y).abs() <= near
                {
                    begins_other = true;
                    break;
                }
            }
            if !begins_other {
                cuts.push(x);
            }
        }
        cuts.sort_by(f64::total_cmp);
        cuts.dedup_by(|later, earlier| *later - *earlier <= near);
        if cuts.is_empty() || cuts.len() + 1 >= self.words[index].len() {
            parts.push([left, right]);
            return true;
        }
        let mut from = left;
        for cut in cuts {
            parts.push([from, cut]);
            from = cut;
        }
        parts.push([from, right]);
        true
    }

    /// Whether another cell of a candidate's row joins text from the line of
    /// `left` and the line at `other`: rows the detector merged. The text
    /// from each line must be a different part of the cell, so a cell of one
    /// word, such as a fee of "0.00" on every row, joins nothing.
    fn rows_merged(&self, cell: &MergedCell, left: &Run<'_>, other: f64) -> bool {
        let near = 0.25 * left.size.abs().max(1.0);
        let Some(line) = self.lines.get(&left.page) else {
            return false;
        };
        cell.row.iter().any(|text| {
            let cell_words = words(text);
            // Where the runs on the line at `y` fall in the cell's words.
            let parts = |y: f64| -> Vec<(usize, usize)> {
                self.band(line, y - near, y + near)
                    .iter()
                    .filter(|_| self.step())
                    .flat_map(|&index| {
                        let part = &self.words[index];
                        positions(&cell_words, part)
                            .into_iter()
                            .map(move |start| (start, start + part.len()))
                    })
                    .collect()
            };
            let (here, there) = (parts(left.y), parts(other));
            here.iter()
                .any(|a| there.iter().any(|b| a.1 <= b.0 || b.1 <= a.0))
        })
    }
}

/// Take a step counted in `steps`, while steps are left.
fn take_step(steps: &Cell<usize>) -> bool {
    let taken = steps.get() + 1;
    steps.set(taken);
    taken <= MAX_PLACEMENT_STEPS
}

/// Whether a heading lies over an amount, each given by its left and right
/// ends: they overlap by half the narrower of the two at least.
fn spans_over(heading: [f64; 2], amount: [f64; 2]) -> bool {
    let overlap = heading[1].min(amount[1]) - heading[0].max(amount[0]);
    overlap > 0.0 && 2.0 * overlap >= (heading[1] - heading[0]).min(amount[1] - amount[0])
}

/// Where the page sets a candidate's amounts (see [`Layout::placement`]).
#[cfg(test)]
pub(crate) fn merged_on_one_line(
    cell: &MergedCell,
    runs: &[Run<'_>],
    placed: &[Placed],
) -> Placement {
    Layout::new(runs, placed).placement(cell)
}

/// A text's words.
fn words(text: &str) -> Vec<&str> {
    text.split_whitespace().collect()
}

/// Whether `words` holds `part`, non-empty, as consecutive words.
fn contains_words(words: &[&str], part: &[&str]) -> bool {
    !part.is_empty() && words.windows(part.len()).any(|window| window == part)
}

/// Where `words` holds `part`, non-empty, as consecutive words.
fn positions(words: &[&str], part: &[&str]) -> Vec<usize> {
    if part.is_empty() {
        return Vec::new();
    }
    words
        .windows(part.len())
        .enumerate()
        .filter(|(_, window)| *window == part)
        .map(|(start, _)| start)
        .collect()
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
        // A row of three amounts or more repeats wherever it ends the text,
        // as after the heading words of upstream #531's equity statement.
        let headed = "STATEMENT OF CHANGES IN EQUITY Share capital Total equity Balance at the start of the period 2,848 2,000 848 0 2,406 2,000 406 0\n\n|Balance at the start of the period|2,848|2,000|848||0|2,406|2,000|406||0|\n|---|---|---|---|---|---|---|---|---|---|---|\n|Total equity|3,394|2,000|1,394||0|2,848|2,000|848||0|\n";
        assert!(check(headed).row_repeated);
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
        // Two columns' runs on one baseline, each under a heading of its own,
        // were merged; without the headings, or under the second's alone,
        // they are unread.
        let heading = |x: f64| run("Wash sale", x, 701.0, 42.0);
        let columns = [
            run("Cost basis", 352.5, 701.0, 42.0),
            heading(419.5),
            run("2,610.25", 365.0, 685.0, 35.0),
            run("205.25", 434.5, 685.0, 27.5),
        ];
        assert_eq!(
            merged_on_one_line(&basis, &columns, &[]),
            Placement::OneLine
        );
        assert_eq!(
            merged_on_one_line(&basis, &columns[2..], &[]),
            Placement::Unread
        );
        assert_eq!(
            merged_on_one_line(&basis, &columns[1..], &[]),
            Placement::Unread
        );
        // Headings that overlap, the second right-aligned over its narrower
        // amounts, head two columns too.
        let overlapping = [
            run("Cost basis", 358.0, 701.0, 42.0),
            run("Wash sale", 388.5, 701.0, 42.0),
            run("2,610.25", 365.0, 685.0, 35.0),
            run("205.25", 403.0, 685.0, 27.5),
        ];
        assert_eq!(
            merged_on_one_line(&basis, &overlapping, &[]),
            Placement::OneLine
        );
        // Headings pdf-inspector joins from two runs of whole words head two
        // columns; one the page places word by word heads one.
        let joined_headings = [
            run("Cost basis Wash sale", 358.0, 701.0, 87.0),
            run("2,610.25", 365.0, 685.0, 35.0),
            run("205.25", 403.0, 685.0, 27.5),
        ];
        let header_start = |x: f64| Placed {
            page: 1,
            at: [x, 701.0],
            plain: true,
        };
        assert_eq!(
            merged_on_one_line(
                &basis,
                &joined_headings,
                &[header_start(358.0), header_start(403.0)]
            ),
            Placement::OneLine
        );
        let by_word = [358.0, 377.5, 403.0, 425.0].map(header_start);
        assert_eq!(
            merged_on_one_line(&basis, &joined_headings, &by_word),
            Placement::Unread
        );
        // An amount with its percentage, both under one heading, is one
        // cell's text, whether the heading spans both or is right-aligned
        // over them, on two lines; text further up, such as a folio over
        // the percentages, heads nothing.
        let gain = cell(&["1,234.56", "(9.02%)"]);
        let percent = [
            run("Gain/loss (percent)", 430.0, 712.0, 76.0),
            run("1,234.56", 430.0, 698.0, 35.0),
            run("(9.02%)", 468.0, 698.0, 31.5),
        ];
        assert_eq!(merged_on_one_line(&gain, &percent, &[]), Placement::Unread);
        let right_aligned = [
            run("Unrealized", 497.0, 723.0, 43.0),
            run("Market value", 348.5, 712.0, 51.5),
            run("gain/(loss)", 498.5, 712.0, 41.5),
            run("1,234.56", 470.5, 698.0, 35.0),
            run("(9.02%)", 508.5, 698.0, 31.5),
        ];
        assert_eq!(
            merged_on_one_line(&gain, &right_aligned, &[]),
            Placement::Unread
        );
        let folio = [
            run("Page 1", 511.5, 770.0, 28.5),
            run("Gain/loss (percent)", 443.4, 712.0, 76.0),
            run("1,234.56", 470.5, 698.0, 35.0),
            run("(9.02%)", 508.5, 698.0, 31.5),
        ];
        assert_eq!(merged_on_one_line(&gain, &folio, &[]), Placement::Unread);
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
        // One item that a second run starts inside, under headings over
        // each, was joined from two; one plain run is as written, and one
        // with a word gap in it unread.
        let joined = [
            run("2,610.25 205.25", 308.9, 686.0, 59.1),
            run("Wash", 347.5, 700.0, 21.0),
            run("Cost basis", 300.0, 700.0, 42.0),
        ];
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
            merged_on_one_line(
                &basis,
                &joined[..1],
                &[start(308.9, true), start(343.5, true)]
            ),
            Placement::Unread
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
        // A cell of one word on both lines, such as a fee of "0.00" on every
        // row, joins nothing: the amounts stand stacked.
        let zeros = MergedCell {
            row: vec!["Sample Fund A".to_string(), "0.00".to_string()],
            ..cell(&["0.00", "0.00"])
        };
        let fees = [
            run("Sample Fund A", 72.0, 702.0, 110.0),
            run("0.00", 330.0, 702.0, 17.5),
            run("0.00 0.00", 400.0, 702.0, 37.5),
            run("Sample Fund B", 72.0, 689.0, 110.0),
            run("0.00", 330.0, 689.0, 17.5),
        ];
        assert_eq!(merged_on_one_line(&zeros, &fees, &[]), Placement::AsSet);
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
    fn amounts_pushed_out_of_their_rows_are_found() {
        let lots = "|Description|Sold|Proceeds|Gain|\n|---|---|---|---|\n|100 sh XYZ CORP|03/02/25|5,210.00|230.00|\n|50 sh ABC INC|03/05/25|2,405.50|0.00|\n\n205.25\n154.60\n\nTotals carry to Form 8949.\n";
        let found = check(lots);
        assert_eq!(
            found.detached,
            vec![Detached {
                amounts: vec!["205.25".to_string(), "154.60".to_string()],
                labels: vec![
                    "100 sh XYZ CORP".to_string(),
                    "03/02/25".to_string(),
                    "50 sh ABC INC".to_string(),
                    "03/05/25".to_string()
                ],
            }]
        );
        let run = |text: &'static str, x: f64, y: f64| Run {
            page: 1,
            text,
            x,
            y,
            width: 30.0,
            size: 8.0,
        };
        // An amount the page sets on a row's line was pushed out of it; one
        // it sets on a line of its own, as a total, was not.
        let table = &found.detached[0];
        let on_row = [
            run("50 sh ABC INC", 72.0, 690.0),
            run("2,405.50", 300.0, 690.0),
            run("205.25", 380.0, 690.0),
        ];
        assert!(Layout::new(&on_row, &[]).detached(table));
        let below = [
            run("50 sh ABC INC", 72.0, 690.0),
            run("205.25", 380.0, 640.0),
        ];
        assert!(!Layout::new(&below, &[]).detached(table));
        // A label the page sets word by word is read along the line; part
        // of a label is not one.
        let by_word = [
            run("50", 72.0, 690.0),
            run("sh", 84.0, 690.0),
            run("ABC", 96.0, 690.0),
            run("INC", 118.0, 690.0),
            run("205.25", 380.0, 690.0),
        ];
        assert!(Layout::new(&by_word, &[]).detached(table));
        let part = [run("ABC INC", 96.0, 690.0), run("205.25", 380.0, 690.0)];
        assert!(!Layout::new(&part, &[]).detached(table));
        // Most of the amounts read must sit on rows' lines: one of three
        // sharing a row's baseline, as a column beside the table may, does
        // not.
        let three = Detached {
            amounts: vec![
                "205.25".to_string(),
                "154.60".to_string(),
                "105.80".to_string(),
            ],
            labels: table.labels.clone(),
        };
        let aside = [
            run("50 sh ABC INC", 72.0, 690.0),
            run("205.25", 380.0, 690.0),
            run("154.60", 380.0, 640.0),
            run("105.80", 380.0, 626.0),
        ];
        assert!(!Layout::new(&aside, &[]).detached(&three));
        // A line after the table holding more than amounts ends them.
        assert!(check("|a|1.00|\n|---|---|\n|b|2.00|\nTotal 3.00\n4.00\n")
            .detached
            .is_empty());
        // pdf-inspector also sets a dropped column's amounts several to a
        // line, with a credit mark, after the column's heading, or as a
        // table of their own beside their currency signs.
        let after = |text: &str| -> Vec<Vec<String>> {
            check(&format!(
                "|Date|Description|\n|---|---|\n|03/01|Purchase at merchant 1|\n{text}"
            ))
            .detached
            .into_iter()
            .map(|table| table.amounts)
            .collect()
        };
        let amounts = |list: &[&str]| -> Vec<Vec<String>> {
            vec![list.iter().map(|amount| amount.to_string()).collect()]
        };
        assert_eq!(
            after("\n$12.40 $15.40 -18.40\n"),
            amounts(&["$12.40", "$15.40", "-18.40"])
        );
        assert_eq!(after("\n12.40 CR\n15.40\n"), amounts(&["12.40", "15.40"]));
        assert_eq!(
            after("\n## Amount\n\n12.40\n15.40\n## Payments\n\n9.10\n"),
            amounts(&["12.40", "15.40"])
        );
        assert_eq!(
            after("\n|$|12.40|\n|---|---|\n|$|15.40|\n"),
            amounts(&["12.40", "15.40"])
        );
        // A table holding more than amounts, a second heading, or a mark
        // before any amount is text of its own.
        for text in [
            "\n|$|12.40|\n|---|---|\n|Total|12.40|\n",
            "\n## Amount\n## Purchases\n12.40\n",
            "\nCR 12.40\n",
            "\n12.40 CR CR\n",
        ] {
            assert!(after(text).is_empty(), "{text}");
        }
        // An amount the table holds, as a total restating its one row, is
        // not pushed out of it; nor does a cell of a mark or two label a
        // row.
        assert!(
            check("|Item|Amount|\n|---|---|\n|Filing fee|12.40|\n\n12.40\n")
                .detached
                .is_empty()
        );
        assert_eq!(
            check("|Item|Note|Amount|\n|---|---|---|\n|Filing fee|1|12.40|\n\n9.10\n").detached,
            vec![Detached {
                amounts: vec!["9.10".to_string()],
                labels: vec!["Filing fee".to_string()],
            }]
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
