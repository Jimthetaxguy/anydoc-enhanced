//! Text set in vertical writing, which pdf-inspector 1.24.0 lays out as if
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

use lopdf::{Dictionary, Object};

/// A string shown in a font that writes vertically, on an upright line:
/// where its column stands, the height it spans, its size, what it reads
/// as where its font can be read, and its place among the page's strings.
#[derive(Clone, Debug)]
pub(crate) struct VerticalRun {
    pub(crate) x: f64,
    pub(crate) top: f64,
    pub(crate) bottom: f64,
    pub(crate) size: f64,
    pub(crate) text: Option<String>,
    pub(crate) show: u64,
}

/// What the Markdown must show of a page's columns of vertical writing for
/// them to read right.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Reading {
    /// A column with no neighbour of its size beside it, as it reads: whole.
    Alone(String),
    /// Two neighbouring columns of one size whose heights overlap, right and
    /// left, as each reads where their fonts can be read: each whole, and,
    /// where they stand as near as a passage's columns do, the right one's
    /// text and then the left one's.
    Pair(Option<(String, String)>, bool),
}

/// The least share of one column's size another's must be to stand beside
/// it in one passage: ruby, set beside its base at half its size, does not.
const SIMILAR_SIZE: f64 = 0.75;
/// How far apart, in their size, two columns of a passage stand at most;
/// labels in the cells of a table stand further apart.
const PASSAGE_PITCH: f64 = 2.5;
/// Columns of another size passed over to find a column's neighbour.
const MAX_PASSED_COLUMNS: usize = 3;

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

/// A column of a page's vertical runs.
struct Column<'a> {
    x: f64,
    size: f64,
    runs: Vec<&'a VerticalRun>,
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
        let mut runs = self.runs.clone();
        runs.sort_by(|one, other| other.top.total_cmp(&one.top));
        runs.iter().map(|run| run.text.as_deref()).collect()
    }
}

/// What the Markdown must show of a page's vertical runs (see `Reading`),
/// right to left. Runs of one size stand in one column where they start
/// within half their size of it across the page; a column's neighbour is
/// the nearest to its left of its size, past up to `MAX_PASSED_COLUMNS` of
/// another, such as ruby.
pub(crate) fn readings(runs: &[VerticalRun]) -> Vec<Reading> {
    let mut order: Vec<&VerticalRun> = runs.iter().collect();
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
            }),
        }
    }
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
        let passage = column.x - neighbour.x <= PASSAGE_PITCH * column.size.max(neighbour.size);
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
            readings(&runs),
            [
                Reading::Pair(Some(("源泉徴収".to_owned(), "住民税".to_owned())), true),
                Reading::Alone("別紙".to_owned()),
            ]
        );
        // A column standing alone reads whole; a column whose font cannot
        // be read pairs as unread, and alone is not read at all.
        assert_eq!(
            readings(&runs[..4]),
            [Reading::Alone("源泉徴収".to_owned())]
        );
        runs[5].text = None;
        assert_eq!(
            readings(&runs),
            [Reading::Pair(None, true), Reading::Alone("別紙".to_owned())]
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
            readings(&[ruby, base]),
            [
                Reading::Alone("げんせんちょうしゅうひょう".to_owned()),
                Reading::Alone("源泉徴収票の支払金額".to_owned()),
            ]
        );
        // Labels in cells five sizes apart pair, but not as a passage.
        let labels = [
            run(390.0, 630.0, "源泉徴収税額", 0),
            run(330.0, 630.0, "支払者の住所", 0),
        ];
        assert_eq!(
            readings(&labels),
            [Reading::Pair(
                Some(("源泉徴収税額".to_owned(), "支払者の住所".to_owned())),
                false
            )]
        );
    }
}
