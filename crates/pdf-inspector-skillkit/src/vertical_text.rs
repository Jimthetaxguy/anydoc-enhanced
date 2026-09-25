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
//! right.

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

/// Two neighbouring columns, right and left, as what each reads as, where
/// their fonts can be read.
pub(crate) type ColumnPair = Option<(String, String)>;

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

/// The neighbouring columns of a page's vertical runs whose heights
/// overlap, right to left: each pair as what the right column and the left
/// one read as, where their fonts can be read. Runs stand in one column
/// where they start within half their size of it across the page.
pub(crate) fn neighbours(runs: &[VerticalRun]) -> Vec<ColumnPair> {
    let mut order: Vec<&VerticalRun> = runs.iter().collect();
    order.sort_by(|one, other| other.x.total_cmp(&one.x));
    let mut columns: Vec<Column> = Vec::new();
    for run in order {
        match columns
            .last_mut()
            .filter(|column| (column.x - run.x).abs() <= 0.5 * column.size.max(run.size))
        {
            Some(column) => column.runs.push(run),
            None => columns.push(Column {
                x: run.x,
                size: run.size,
                runs: vec![run],
            }),
        }
    }
    columns
        .windows(2)
        .filter(|pair| pair[0].top().min(pair[1].top()) > pair[0].bottom().max(pair[1].bottom()))
        .map(|pair| pair[0].text().zip(pair[1].text()))
        .collect()
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
            neighbours(&runs),
            [Some(("源泉徴収".to_owned(), "住民税".to_owned()))]
        );
        // A column standing alone has no neighbour, and a column whose font
        // cannot be read pairs as unread.
        assert!(neighbours(&runs[..4]).is_empty());
        runs[5].text = None;
        assert_eq!(neighbours(&runs), [None]);
    }
}
