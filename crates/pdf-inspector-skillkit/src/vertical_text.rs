//! Text set in vertical writing, which pdf-inspector 1.25.0 lays out as if
//! it were horizontal (upstream issue #575).
//!
//! A font under a CMap for vertical writing (`Identity-V`, `UniJIS-UCS2-V`,
//! and the like) advances down the page, and Japanese or Chinese columns set
//! with it read top to bottom, right to left. pdf-inspector groups the
//! glyphs into lines by their height instead, so columns standing side by
//! side read row by row across them where each glyph is placed on its own
//! ("住源源 民泉泉 税徴徴" for three columns), and out of order, left to
//! right, where each column is one string. A column standing alone reads
//! whole, but where lines of horizontal text at its glyphs' heights run
//! through it.

use std::collections::HashSet;

use lopdf::{Dictionary, Object};

use crate::repeated_lines::EdgeRun;

/// A string shown in a font that writes vertically, on an upright line:
/// where its column stands, the height it spans, its size, what it reads
/// as where its font can be read, its place among the page's strings, the
/// page's run it was noted in, where it was (see `EdgeRun`), and whether it
/// is a glyph of a font that writes across, set on its own in a column of
/// vertical writing emulated glyph by glyph (see `note_glyph`).
#[derive(Clone, Debug)]
pub(crate) struct VerticalRun {
    pub(crate) x: f64,
    pub(crate) top: f64,
    pub(crate) bottom: f64,
    pub(crate) size: f64,
    pub(crate) text: Option<String>,
    pub(crate) show: u64,
    pub(crate) edge: Option<usize>,
    pub(crate) emulated: bool,
}

impl VerticalRun {
    /// A glyph of a font that writes across, set on its own on an upright
    /// line with its origin at `x` and `y`: its box, as a Japanese or
    /// Chinese font's, reaching nine tenths of its size above its baseline
    /// and a tenth below, its column standing at its middle.
    pub(crate) fn glyph(
        (x, y, size): (f64, f64, f64),
        text: Option<String>,
        show: u64,
        edge: Option<usize>,
    ) -> Self {
        VerticalRun {
            x: x + 0.5 * size,
            top: y + 0.9 * size,
            bottom: y - 0.1 * size,
            size,
            text,
            show,
            edge,
            emulated: true,
        }
    }

    /// Whether `next`, a glyph noted after this one, stands below it in its
    /// column: of its size, at its place across the page, and below it by
    /// up to `MAX_COLUMN_GAP` sizes, as a glyph stands past digits or a
    /// word set sideways in the column between them.
    fn above(&self, next: &VerticalRun) -> bool {
        let (size, drop) = (self.size.max(next.size), self.top - next.top);
        self.emulated
            && next.emulated
            && similar(self.size, next.size)
            && (self.x - next.x).abs() <= 0.5 * size
            && drop > 0.0
            && drop <= MAX_COLUMN_GAP * size
    }

    /// Whether `next`, the glyph shown right after this one, goes on down
    /// its column: below it by up to one and a half sizes, as a column's
    /// glyphs are set a size apart, or a little more.
    fn goes_on(&self, next: &VerticalRun) -> bool {
        self.show + 1 == next.show
            && self.above(next)
            && self.top - next.top <= 1.5 * self.size.max(next.size)
    }
}

/// Whether a text is a Japanese or Chinese character: an ideograph, kana,
/// or a mark of their punctuation.
fn japanese_or_chinese(text: &str) -> bool {
    let mut characters = text.chars();
    matches!(
        (characters.next(), characters.next()),
        (
            Some(
                '\u{3000}'..='\u{30FF}'
                | '\u{3400}'..='\u{4DBF}'
                | '\u{4E00}'..='\u{9FFF}'
                | '\u{F900}'..='\u{FAFF}'
                | '\u{FF01}'..='\u{FF60}',
            ),
            None,
        )
    )
}

/// Note `glyph`, a glyph of a font that writes across set on its own (see
/// `VerticalRun::glyph`), among a page's `runs`: vertical writing emulated
/// in such a font, as LibreOffice and browsers draw it, places each glyph
/// of a column a size or so below the last, where horizontal text sets the
/// next one beside it. It goes on the column the last glyph noted stands
/// in, where it goes on down it (see `VerticalRun::goes_on`); else it may
/// start a column of its own where it is a Japanese or Chinese character,
/// and the last glyph noted is let go where it started none and stands
/// below none's glyphs (see `VerticalRun::above`). The bytes of text let go
/// are returned.
pub(crate) fn note_glyph(runs: &mut Vec<VerticalRun>, glyph: VerticalRun) -> usize {
    if runs.last().is_some_and(|last| last.goes_on(&glyph)) {
        runs.push(glyph);
        return 0;
    }
    let alone = match &runs[..] {
        [.., before, last] => last.emulated && !before.above(last),
        [last] => last.emulated,
        [] => false,
    };
    let mut let_go = 0;
    if alone {
        let_go = runs
            .pop()
            .and_then(|last| last.text)
            .map_or(0, |text| text.len());
    }
    if glyph.text.as_deref().is_some_and(japanese_or_chinese) {
        runs.push(glyph);
    }
    let_go
}

/// What the Markdown must show of a page's columns of vertical writing for
/// them to read right.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Reading {
    /// A column with no neighbour of its size beside it, as it reads: whole.
    Alone(String),
    /// Two neighbouring columns of one size whose heights overlap, right and
    /// left, as each reads where their fonts can be read: each whole, and,
    /// where they stand as near as a passage's columns do and not over a
    /// row of values, as a table's header labels do, the right one's text
    /// and then the left one's.
    Pair(Option<(String, String)>, bool),
}

/// The cells the Markdown's tables set side by side, bare (see
/// `repeated_lines::bare`): each cell of a row with the next that holds
/// text, the left one first.
pub(crate) fn neighbouring_cells(markdown: &str) -> HashSet<(String, String)> {
    let mut neighbours = HashSet::new();
    for line in markdown.lines().map(str::trim) {
        if !(line.starts_with('|') && line.ends_with('|'))
            || line
                .chars()
                .all(|character| matches!(character, '|' | '-' | ':' | ' '))
        {
            continue;
        }
        let cells: Vec<String> = line
            .split('|')
            .map(crate::repeated_lines::bare)
            .filter(|cell| !cell.is_empty())
            .collect();
        neighbours.extend(
            cells
                .windows(2)
                .map(|pair| (pair[0].clone(), pair[1].clone())),
        );
    }
    neighbours
}

/// The least share of one column's size another's must be to stand beside
/// it in one passage: ruby, set beside its base at half its size, does not.
const SIMILAR_SIZE: f64 = 0.75;
/// How far below the glyph before it, in their size, a glyph of vertical
/// writing emulated in a font that writes across stands at most in its
/// column, past what is set between them (see `VerticalRun::above`).
const MAX_COLUMN_GAP: f64 = 4.0;
/// How far apart, in their size, two columns of a passage stand at most,
/// its leading wide; labels in the cells of a table may stand nearer (see
/// `check_vertical_text`).
const PASSAGE_PITCH: f64 = 4.0;
/// Columns of another size passed over to find a column's neighbour.
const MAX_PASSED_COLUMNS: usize = 3;
/// Characters a horizontal run holds at most to be read in a column of
/// vertical writing it stands in, as digits set across the column
/// (tate-chu-yoko) are.
const MAX_ACROSS_CHARS: usize = 4;

/// Whether two sizes are near enough for their text to be one passage.
fn similar(one: f64, other: f64) -> bool {
    one.min(other) >= SIMILAR_SIZE * one.max(other)
}

/// Whether `font` writes vertically: a composite font under a CMap named
/// for vertical writing.
pub(crate) fn writes_vertically(font: &Dictionary) -> bool {
    let named = |key: &[u8]| font.get(key).and_then(Object::as_name).ok();
    named(b"Subtype") == Some(b"Type0".as_slice())
        && named(b"Encoding").is_some_and(|encoding| encoding.ends_with(b"-V"))
}

/// A column of a page's vertical runs, and the horizontal runs set across
/// it, each with the height its text reads at.
struct Column<'a> {
    x: f64,
    size: f64,
    runs: Vec<&'a VerticalRun>,
    across: Vec<(f64, &'a str)>,
}

impl Column<'_> {
    fn top(&self) -> f64 {
        self.runs.iter().map(|run| run.top).fold(f64::MIN, f64::max)
    }

    fn bottom(&self) -> f64 {
        self.runs
            .iter()
            .map(|run| run.bottom)
            .fold(f64::MAX, f64::min)
    }

    /// What the column reads as, top to bottom, where every run's font can
    /// be read.
    fn text(&self) -> Option<String> {
        let mut texts: Vec<(f64, &str)> = self
            .runs
            .iter()
            .map(|run| Some((run.top, run.text.as_deref()?)))
            .collect::<Option<_>>()?;
        texts.extend(self.across.iter().copied());
        texts.sort_by(|one, other| other.0.total_cmp(&one.0));
        Some(texts.into_iter().map(|(_, text)| text).collect())
    }

    /// Where a horizontal run stands across the column, if it does:
    /// upright, short, no larger than its glyphs, starting within its width
    /// to the left of its line or half of it to the right, with its
    /// baseline between its top and its foot, or below its foot where the
    /// top of the run's glyphs reaches within a quarter of the column's
    /// size of it, as digits set in the column's last cell do.
    fn holds(&self, run: &EdgeRun) -> Option<Across> {
        let (x, y, size) = (f64::from(run.x), f64::from(run.y), f64::from(run.size));
        let short = run.text.as_deref().is_some_and(|text| {
            let text = text.trim();
            !text.is_empty() && text.chars().count() <= MAX_ACROSS_CHARS
        });
        if !(upright(run)
            && size <= 1.05 * self.size
            && (self.x - self.size..=self.x + 0.5 * self.size).contains(&x)
            && short)
        {
            return None;
        }
        let (top, bottom) = (self.top(), self.bottom());
        if (bottom..=top).contains(&y) {
            Some(Across::Within)
        } else if y < bottom && y + 0.8 * size >= bottom - 0.25 * self.size {
            Some(Across::Below)
        } else {
            None
        }
    }
}

/// Where a horizontal run stands across a column (see `Column::holds`).
#[derive(Clone, Copy, PartialEq)]
enum Across {
    /// Between the column's top and its foot.
    Within,
    /// Below its foot, in the cell after its last glyph.
    Below,
}

/// Whether a run is upright, its line running along the page.
fn upright(run: &EdgeRun) -> bool {
    let [a, b] = run.direction;
    a > 0.0 && b.abs() <= 0.1 * a
}

/// What the Markdown must show of a page's vertical runs (see `Reading`),
/// right to left, with the page's horizontal runs set across a column read
/// in it (see `Column::holds`) among `across`, the page's runs, the
/// vertical ones among them aside. A glyph of a font that writes across
/// stands in a column only where the glyph before it goes on down to it or
/// it goes on down to the next (see `VerticalRun::goes_on`), or it stands
/// below a glyph of a column noted just before it (see `VerticalRun::above`).
/// Runs of one size stand in one column where they start within half their
/// size of it across the page; a column's neighbour is the nearest to its
/// left of its size, past up to `MAX_PASSED_COLUMNS` of another, such as
/// ruby.
pub(crate) fn readings(runs: &[VerticalRun], across: &[EdgeRun]) -> Vec<Reading> {
    let mut in_column: Vec<bool> = Vec::with_capacity(runs.len());
    for (index, run) in runs.iter().enumerate() {
        let before = index.checked_sub(1).map(|before| &runs[before]);
        in_column.push(
            !run.emulated
                || before.is_some_and(|before| before.goes_on(run))
                || runs.get(index + 1).is_some_and(|next| run.goes_on(next))
                || (in_column.last() == Some(&true)
                    && before.is_some_and(|before| before.above(run))),
        );
    }
    let runs: Vec<&VerticalRun> = runs
        .iter()
        .zip(in_column)
        .filter_map(|(run, in_column)| in_column.then_some(run))
        .collect();
    let mut order = runs.clone();
    order.sort_by(|one, other| other.x.total_cmp(&one.x));
    let mut columns: Vec<Column> = Vec::new();
    for run in order {
        let near = |column: &&mut Column| {
            (column.x - run.x).abs() <= 0.5 * column.size.max(run.size)
                && similar(column.size, run.size)
        };
        match columns.last_mut().filter(near) {
            Some(column) => column.runs.push(run),
            None => columns.push(Column {
                x: run.x,
                size: run.size,
                runs: vec![run],
                across: Vec::new(),
            }),
        }
    }
    let vertical: HashSet<usize> = runs.iter().filter_map(|run| run.edge).collect();
    // The baselines of the horizontal runs, low to high, with where each
    // starts: a run below a column's foot that another, standing apart from
    // the column, lines up with as a row, as a table's values stand under
    // its labels, is not read in the column.
    let mut baselines: Vec<(f64, f64)> = across
        .iter()
        .enumerate()
        .filter(|(index, run)| !vertical.contains(index) && upright(run))
        .map(|(_, run)| (f64::from(run.y), f64::from(run.x)))
        .collect();
    baselines.sort_by(|one, other| one.0.total_cmp(&other.0));
    let in_row = |run: &EdgeRun, column: &Column| {
        let (y, near) = (f64::from(run.y), 0.25 * f64::from(run.size));
        let from = baselines.partition_point(|(baseline, _)| *baseline < y - near);
        baselines[from..]
            .iter()
            .take_while(|(baseline, _)| *baseline <= y + near)
            .any(|(_, x)| (x - column.x).abs() > column.size)
    };
    for (index, run) in across.iter().enumerate() {
        if vertical.contains(&index) {
            continue;
        }
        let Some((column, place)) = columns
            .iter_mut()
            .find_map(|column| column.holds(run).map(|place| (column, place)))
        else {
            continue;
        };
        if place == Across::Below && in_row(run, column) {
            continue;
        }
        let text = run.text.as_deref().unwrap_or_default();
        // It reads at the top of its glyphs.
        column
            .across
            .push((f64::from(run.y) + 0.8 * f64::from(run.size), text));
    }
    // The horizontal runs showing text, by where they start across the
    // page: two neighbouring columns each with one starting under it, below
    // its foot, the two on one baseline, stand over a row of values, as a
    // table's header labels do, and read left to right as its row does.
    let mut starts: Vec<(f64, f64)> = across
        .iter()
        .enumerate()
        .filter(|(index, run)| {
            !vertical.contains(index)
                && upright(run)
                && run
                    .text
                    .as_deref()
                    .is_some_and(|text| !text.trim().is_empty())
        })
        .map(|(_, run)| (f64::from(run.x), f64::from(run.y)))
        .collect();
    starts.sort_by(|one, other| one.0.total_cmp(&other.0));
    let under = |column: &Column| -> Vec<f64> {
        let (from, bottom) = (
            starts.partition_point(|(x, _)| *x < column.x - column.size),
            column.bottom(),
        );
        starts[from..]
            .iter()
            .take_while(|(x, _)| *x <= column.x + 0.5 * column.size)
            .filter(|(_, y)| *y < bottom)
            .map(|(_, y)| *y)
            .collect()
    };
    let over_values = |right: &Column, left: &Column| {
        let (lefts, near) = (under(left), 0.25 * right.size.max(left.size));
        under(right)
            .iter()
            .any(|y| lefts.iter().any(|other| (other - y).abs() <= near))
    };
    let mut paired = vec![false; columns.len()];
    let mut readings = Vec::new();
    for (right, column) in columns.iter().enumerate() {
        let Some(left) = (right + 1..columns.len())
            .take(MAX_PASSED_COLUMNS + 1)
            .find(|&left| similar(column.size, columns[left].size))
        else {
            continue;
        };
        let neighbour = &columns[left];
        if column.top().min(neighbour.top()) <= column.bottom().max(neighbour.bottom()) {
            continue;
        }
        paired[right] = true;
        paired[left] = true;
        let passage = column.x - neighbour.x <= PASSAGE_PITCH * column.size.max(neighbour.size)
            && !over_values(column, neighbour);
        readings.push(Reading::Pair(column.text().zip(neighbour.text()), passage));
    }
    readings.extend(
        columns
            .iter()
            .zip(paired)
            .filter(|(_, paired)| !paired)
            .filter_map(|(column, _)| column.text().map(Reading::Alone)),
    );
    readings
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::dictionary;

    fn run(x: f64, top: f64, text: &str, show: u64) -> VerticalRun {
        let glyphs = text.chars().count() as f64;
        VerticalRun {
            x,
            top,
            bottom: top - 12.0 * glyphs,
            size: 12.0,
            text: Some(text.to_owned()),
            show,
            edge: None,
            emulated: false,
        }
    }

    #[test]
    fn fonts_under_a_vertical_cmap_write_vertically() {
        let font = |encoding: &str| {
            dictionary! {
                "Type" => "Font",
                "Subtype" => "Type0",
                "Encoding" => Object::Name(encoding.as_bytes().to_vec()),
            }
        };
        assert!(writes_vertically(&font("Identity-V")));
        assert!(writes_vertically(&font("UniJIS-UCS2-V")));
        assert!(!writes_vertically(&font("Identity-H")));
        assert!(!writes_vertically(&dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "Encoding" => "WinAnsiEncoding",
        }));
    }

    #[test]
    fn neighbouring_columns_pair_right_to_left() {
        // Two columns set glyph by glyph, 18 pt apart, and one far below.
        let mut runs = Vec::new();
        for (index, glyph) in "源泉徴収".chars().enumerate() {
            runs.push(run(
                500.0,
                720.0 - 12.0 * index as f64,
                &glyph.to_string(),
                0,
            ));
        }
        for (index, glyph) in "住民税".chars().enumerate() {
            runs.push(run(
                482.0,
                720.0 - 12.0 * index as f64,
                &glyph.to_string(),
                0,
            ));
        }
        runs.push(run(464.0, 300.0, "別紙", 0));
        assert_eq!(
            readings(&runs, &[]),
            [
                Reading::Pair(Some(("源泉徴収".to_owned(), "住民税".to_owned())), true),
                Reading::Alone("別紙".to_owned()),
            ]
        );
        // A column standing alone reads whole; a column whose font cannot
        // be read pairs as unread, and alone is not read at all.
        assert_eq!(
            readings(&runs[..4], &[]),
            [Reading::Alone("源泉徴収".to_owned())]
        );
        runs[5].text = None;
        assert_eq!(
            readings(&runs, &[]),
            [Reading::Pair(None, true), Reading::Alone("別紙".to_owned())]
        );
    }

    #[test]
    fn glyphs_set_down_a_column_in_a_font_that_writes_across_stand_in_it() {
        let glyphs = |runs: &mut Vec<VerticalRun>, at: &[(f64, f64, &str)], from: u64| {
            let mut let_go = 0;
            for (show, &(x, y, glyph)) in (from..).zip(at) {
                let glyph = VerticalRun::glyph((x, y, 12.0), Some(glyph.to_owned()), show, None);
                let_go += note_glyph(runs, glyph);
            }
            let_go
        };
        // Two columns set glyph by glyph, 21 pt apart, as LibreOffice sets
        // vertical writing in a font that writes across.
        let mut runs = Vec::new();
        let column = |x: f64, text: &'static str| -> Vec<(f64, f64, &'static str)> {
            let glyphs = text
                .char_indices()
                .map(|(at, glyph)| &text[at..at + glyph.len_utf8()]);
            (0..)
                .zip(glyphs)
                .map(|(row, glyph)| (x, 710.0 - 12.0 * f64::from(row), glyph))
                .collect()
        };
        let mut both = column(494.0, "源泉徴収");
        both.extend(column(473.0, "住民税"));
        assert_eq!(glyphs(&mut runs, &both, 0), 0);
        assert_eq!(
            readings(&runs, &[]),
            [Reading::Pair(
                Some(("源泉徴収".to_owned(), "住民税".to_owned())),
                true
            )]
        );
        // A glyph set on its own after them stands in no column; the one
        // before it is let go when another comes.
        glyphs(&mut runs, &[(300.0, 400.0, "別")], 7);
        assert_eq!(runs.len(), 8);
        assert_eq!(readings(&runs, &[]).len(), 1);
        assert_eq!(glyphs(&mut runs, &[(100.0, 400.0, "紙")], 8), "別".len());
        assert_eq!(runs.len(), 8);
        // Glyphs set one by one along a line across the page, or down one
        // but shown apart, or starting with no Japanese or Chinese
        // character, stand in no column: each is let go at the next.
        let mut line = Vec::new();
        let across = [
            (72.0, 700.0, "源"),
            (84.0, 700.0, "泉"),
            (96.0, 700.0, "徴"),
        ];
        assert_eq!(glyphs(&mut line, &across, 0), 2 * "源".len());
        assert!(readings(&line, &[]).is_empty());
        let mut apart = Vec::new();
        for (show, glyph) in [(0, (72.0, 700.0, "源")), (2, (72.0, 688.0, "泉"))] {
            glyphs(&mut apart, &[glyph], show);
        }
        assert!(readings(&apart, &[]).is_empty());
        let mut latin = Vec::new();
        glyphs(&mut latin, &[(72.0, 700.0, "S"), (72.0, 688.0, "a")], 0);
        assert!(latin.is_empty());
        // A column standing alone reads whole.
        let mut alone = Vec::new();
        glyphs(&mut alone, &column(494.0, "源泉徴収"), 0);
        assert_eq!(
            readings(&alone, &[]),
            [Reading::Alone("源泉徴収".to_owned())]
        );
        // A glyph shown apart from the one above it in its column, as past
        // digits set sideways between them, stands in the column up to four
        // sizes below it; further down, in none.
        let mut dated = Vec::new();
        let date = [
            (0, (494.0, 700.0, "令")),
            (1, (494.0, 688.0, "和")),
            (4, (494.0, 659.2, "年")),
            (6, (494.0, 636.4, "月")),
            (8, (494.0, 613.6, "日")),
            (9, (494.0, 601.6, "に")),
            (12, (494.0, 540.0, "発")),
            (14, (100.0, 400.0, "別")),
        ];
        for (show, glyph) in date {
            glyphs(&mut dated, &[glyph], show);
        }
        assert_eq!(dated.len(), 7);
        assert_eq!(
            readings(&dated, &[]),
            [Reading::Alone("令和年月日に".to_owned())]
        );
    }

    #[test]
    fn ruby_and_labels_apart_do_not_read_as_a_passage() {
        // Ruby at half size beside its base: each stands alone.
        let mut ruby = run(409.0, 700.0, "げんせんちょうしゅうひょう", 0);
        ruby.size = 6.0;
        ruby.bottom = 700.0 - 6.0 * 13.0;
        let base = run(400.0, 700.0, "源泉徴収票の支払金額", 0);
        assert_eq!(
            readings(&[ruby, base], &[]),
            [
                Reading::Alone("げんせんちょうしゅうひょう".to_owned()),
                Reading::Alone("源泉徴収票の支払金額".to_owned()),
            ]
        );
        // Columns of a passage with wide leading, three sizes apart.
        let wide = [
            run(500.0, 720.0, "源泉徴収票の支払金額", 0),
            run(464.0, 720.0, "源泉徴収税額は十六万", 0),
        ];
        assert!(matches!(
            readings(&wide, &[])[..],
            [Reading::Pair(Some(_), true)]
        ));
        // Digits set across a column (tate-chu-yoko) read in it, where they
        // stand: "令和12年5月1日".
        let date = [
            run(300.0, 700.0, "令和", 0),
            run(300.0, 664.0, "年", 0),
            run(300.0, 640.0, "月", 0),
            run(300.0, 616.0, "日", 0),
        ];
        let across = |x: f32, y: f32, text: &str| EdgeRun {
            y,
            x,
            direction: [1.0, 0.0],
            size: 10.0,
            text: Some(text.to_owned()),
        };
        let digits = [
            across(294.0, 668.0, "12"),
            across(297.0, 644.0, "5"),
            across(297.0, 620.0, "1"),
            across(72.0, 460.0, "Line 0 of the notice"),
        ];
        assert_eq!(
            readings(&date, &digits),
            [Reading::Alone("令和12年5月1日".to_owned())]
        );
        // Below a column's foot, digits set in the cell after its last glyph
        // read in it, glyph by glyph too; a folio set further down does not.
        let era = [run(300.0, 700.0, "平成", 0)];
        let digits = |runs: &[(f32, f32, &str, f32)]| -> Vec<EdgeRun> {
            runs.iter()
                .map(|&(x, y, text, size)| EdgeRun {
                    size,
                    ..across(x, y, text)
                })
                .collect()
        };
        assert_eq!(
            readings(&era, &digits(&[(295.0, 667.0, "31", 10.0)])),
            [Reading::Alone("平成31".to_owned())]
        );
        assert_eq!(
            readings(
                &era,
                &digits(&[(295.0, 667.0, "3", 10.0), (300.5, 667.0, "1", 10.0)])
            ),
            [Reading::Alone("平成31".to_owned())]
        );
        assert_eq!(
            readings(&era, &digits(&[(296.0, 664.0, "12", 9.0)])),
            [Reading::Alone("平成".to_owned())]
        );
        // Values under labels, touching their feet, line up as a row: they
        // are the table's, not the labels', and the labels stand over them
        // as a table's header does, not as a passage.
        let labels = [run(230.0, 696.0, "源泉", 0), run(200.0, 696.0, "支払", 0)];
        assert_eq!(
            readings(
                &labels,
                &digits(&[(196.0, 665.0, "5.2", 7.0), (226.0, 665.0, "162", 7.0)])
            ),
            [Reading::Pair(
                Some(("源泉".to_owned(), "支払".to_owned())),
                false
            )]
        );
        // Header labels three sizes apart over a row of values set well
        // below them are a table's; over lines of text across the page,
        // or a value under one column alone, they read as a passage.
        let header = [
            run(236.0, 696.0, "源泉徴収税額", 0),
            run(200.0, 696.0, "支払金額", 0),
        ];
        let passage = |across: &[EdgeRun]| match readings(&header, across)[..] {
            [Reading::Pair(_, passage)] => passage,
            _ => unreachable!("one pair"),
        };
        assert!(!passage(&digits(&[
            (196.0, 590.0, "5,200", 7.0),
            (232.0, 590.0, "162", 7.0)
        ])));
        assert!(passage(&digits(&[
            (72.0, 590.0, "Line 0 of the notice", 10.0),
            (72.0, 576.0, "Line 1 of the notice", 10.0)
        ])));
        assert!(passage(&digits(&[
            (196.0, 590.0, "5,200", 7.0),
            (232.0, 560.0, "162", 7.0)
        ])));
        // Labels in cells five sizes apart pair, but not as a passage.
        let labels = [
            run(390.0, 630.0, "源泉徴収税額", 0),
            run(330.0, 630.0, "支払者の住所", 0),
        ];
        assert_eq!(
            readings(&labels, &[]),
            [Reading::Pair(
                Some(("源泉徴収税額".to_owned(), "支払者の住所".to_owned())),
                false
            )]
        );
    }
}
