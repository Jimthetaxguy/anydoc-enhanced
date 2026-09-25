//! What a workbook's number formats do to the values AnyDoc 0.2.4 converts.
//!
//! AnyDoc renders a cell through its format code when it can parse the code
//! and falls back to General when it cannot (`sheet::numfmt`). Three things
//! go wrong for a reader of the Markdown, and each converts without a word:
//!
//! - A negative value whose format marks it only by a colour, such as
//!   `#,##0;[Red]#,##0`, renders as a positive number: the colour is gone,
//!   and the section shows no sign.
//! - A value its format hides (an empty section, as in `;;;`) is shown by
//!   no spreadsheet application, while the workbook still holds it; hidden
//!   rows and columns are refused for the same reason. A zero a format
//!   hides is the common "hide zeros" idiom and is not counted.
//! - A date whose format AnyDoc cannot resolve renders as its serial
//!   number: a built-in locale date format outside AnyDoc's table, or a
//!   code its parser rejects.
//!
//! The parse here follows AnyDoc's grammar closely enough to tell which
//! codes it rejects, section by section; the number layout inside a section
//! is not needed and is not checked.

/// How a cell's value meets its format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum CellClass {
    Negative,
    Zero,
    Positive,
    Text,
}

/// What converting a cell of some class through a format loses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct FormatLoss {
    /// The format hides the value, which the workbook still holds.
    pub(super) hidden: bool,
    /// The Markdown shows something other than what the format shows: a
    /// negative value without its sign, or a date as a serial number.
    pub(super) misrendered: bool,
}

/// AnyDoc's built-in format table (ECMA-376 §18.8.30); ids 5-8, 23-36, and
/// 41-44 and above 49 are absent there, so they render as General.
fn builtin_code(id: u32) -> Option<&'static str> {
    Some(match id {
        1 => "0",
        2 => "0.00",
        3 => "#,##0",
        4 => "#,##0.00",
        9 => "0%",
        10 => "0.00%",
        11 => "0.00E+00",
        12 => "# ?/?",
        13 => "# ??/??",
        14 => "mm-dd-yy",
        15 => "d-mmm-yy",
        16 => "d-mmm",
        17 => "mmm-yy",
        18 => "h:mm AM/PM",
        19 => "h:mm:ss AM/PM",
        20 => "h:mm",
        21 => "h:mm:ss",
        22 => "m/d/yy h:mm",
        37 => "#,##0 ;(#,##0)",
        38 => "#,##0 ;[Red](#,##0)",
        39 => "#,##0.00;(#,##0.00)",
        40 => "#,##0.00;[Red](#,##0.00)",
        45 => "mm:ss",
        46 => "[h]:mm:ss",
        47 => "mmss.0",
        48 => "##0.0E+0",
        49 => "@",
        _ => return None,
    })
}

/// Built-in ids that are locale date and time formats (East Asian and
/// Thai), which AnyDoc cannot resolve without the file's own code.
fn builtin_locale_date(id: u32) -> bool {
    matches!(id, 27..=36 | 50..=58 | 71..=81)
}

/// One `;`-separated section, as far as the checks need it.
#[derive(Default, Debug)]
struct Section {
    /// No token renders: nothing but brackets (a colour, a condition, a
    /// locale) or nothing at all.
    empty: bool,
    condition: bool,
    colour: bool,
    /// A `-`, parenthesis, or minus sign it shows, or a `CR` or `DR`.
    sign: bool,
    /// The text placeholder `@`.
    text: bool,
    date: bool,
}

/// A format code as AnyDoc reads it.
#[derive(Debug)]
struct Code {
    sections: Vec<Section>,
    /// Whether AnyDoc's parser accepts the code; otherwise every cell using
    /// it renders as General.
    parses: bool,
}

const COLOURS: [&str; 8] = [
    "black", "blue", "cyan", "green", "magenta", "red", "white", "yellow",
];

/// Split a code on `;` outside quotes, brackets, and the escapes that take
/// the next character; `None` when a quote or bracket is left open.
fn split_sections(code: &str) -> Option<Vec<String>> {
    let mut sections = vec![String::new()];
    let mut characters = code.chars();
    while let Some(character) = characters.next() {
        let current = sections.last_mut().expect("a section");
        match character {
            ';' => sections.push(String::new()),
            '"' | '[' => {
                let close = if character == '"' { '"' } else { ']' };
                current.push(character);
                loop {
                    let next = characters.next()?;
                    current.push(next);
                    if next == close {
                        break;
                    }
                }
            }
            '\\' | '_' | '*' => {
                current.push(character);
                current.push(characters.next()?);
            }
            other => current.push(other),
        }
    }
    Some(sections)
}

fn has_sign(literal: &str) -> bool {
    literal.contains(['-', '(', ')', '\u{2212}']) || {
        let upper = literal.to_ascii_uppercase();
        upper.contains("CR") || upper.contains("DR")
    }
}

/// Read one section; `None` where AnyDoc's parser rejects it.
fn parse_section(section: &str) -> Option<Section> {
    let characters: Vec<char> = section.chars().collect();
    let mut parsed = Section::default();
    let mut tokens = 0usize;
    // Tokens a date section or a text section may not hold.
    let (mut digits, mut exponent, mut bare_digits, mut general) = (false, false, false, false);
    let mut index = 0;
    while index < characters.len() {
        let character = characters[index];
        match character {
            '[' => {
                let end = characters[index..].iter().position(|&c| c == ']')? + index;
                let inner: String = characters[index + 1..end].iter().collect();
                index = end + 1;
                match inner.chars().next()? {
                    '<' | '>' | '=' => {
                        if parsed.condition {
                            return None;
                        }
                        let operand = inner.trim_start_matches(['<', '>', '=']);
                        operand.trim().parse::<f64>().ok()?;
                        parsed.condition = true;
                    }
                    '$' => {
                        // `[$sym-lcid]`: the symbol renders, the locale does not.
                        let symbol = inner[1..].split('-').next().unwrap_or("");
                        if !symbol.is_empty() {
                            tokens += 1;
                            parsed.sign |= has_sign(symbol);
                        }
                    }
                    first @ ('h' | 'H' | 'm' | 'M' | 's' | 'S')
                        if inner.chars().all(|c| c.eq_ignore_ascii_case(&first)) =>
                    {
                        parsed.date = true;
                        tokens += 1;
                    }
                    _ => {
                        let lower = inner.to_ascii_lowercase();
                        let colour = COLOURS.contains(&lower.as_str())
                            || lower
                                .strip_prefix("color")
                                .and_then(|number| number.trim().parse::<u32>().ok())
                                .is_some_and(|number| (1..=56).contains(&number));
                        if !colour {
                            return None;
                        }
                        parsed.colour = true;
                    }
                }
                continue;
            }
            '"' => {
                let end = characters[index + 1..].iter().position(|&c| c == '"')? + index + 1;
                let literal: String = characters[index + 1..end].iter().collect();
                if !literal.is_empty() {
                    tokens += 1;
                    parsed.sign |= has_sign(&literal);
                }
                index = end + 1;
                continue;
            }
            '\\' => {
                let escaped = *characters.get(index + 1)?;
                tokens += 1;
                parsed.sign |= has_sign(&escaped.to_string());
                index += 2;
                continue;
            }
            // A width to skip, or a character to fill with: neither shows
            // the character.
            '_' | '*' => {
                characters.get(index + 1)?;
                tokens += usize::from(character == '_');
                index += 2;
                continue;
            }
            '0' | '#' | '?' | '.' | ',' | '%' => {
                digits |= matches!(character, '0' | '#' | '?' | '.');
                tokens += 1;
            }
            '@' => {
                parsed.text = true;
                tokens += 1;
            }
            'E' | 'e' if matches!(characters.get(index + 1), Some('+' | '-')) => {
                exponent = true;
                tokens += 1;
                index += 2;
                continue;
            }
            'y' | 'Y' | 'd' | 'D' | 'h' | 'H' | 's' | 'S' | 'm' | 'M' => {
                parsed.date = true;
                tokens += 1;
                while index < characters.len() && characters[index].eq_ignore_ascii_case(&character)
                {
                    index += 1;
                }
                continue;
            }
            'g' | 'G' => {
                let word: String = characters[index..characters.len().min(index + 7)]
                    .iter()
                    .collect();
                if !word.eq_ignore_ascii_case("general") {
                    return None;
                }
                general = true;
                tokens += 1;
                index += 7;
                continue;
            }
            'a' | 'A' => {
                let rest: String = characters[index..].iter().collect::<String>();
                let length = ["AM/PM", "A/P"]
                    .iter()
                    .find(|marker| {
                        rest.len() >= marker.len()
                            && rest
                                .get(..marker.len())
                                .is_some_and(|start| start.eq_ignore_ascii_case(marker))
                    })
                    .map(|marker| marker.len())?;
                parsed.date = true;
                tokens += 1;
                index += length;
                continue;
            }
            '1'..='9' => {
                bare_digits = true;
                tokens += 1;
                while index < characters.len() && characters[index].is_ascii_digit() {
                    index += 1;
                }
                continue;
            }
            '$' | '-' | '+' | '(' | ')' | ':' | ' ' | '/' => {
                tokens += 1;
                parsed.sign |= matches!(character, '-' | '(' | ')');
            }
            _ => return None,
        }
        index += 1;
    }
    if general && (digits || exponent || bare_digits || parsed.text || parsed.date) {
        return None;
    }
    if !general && parsed.date && (parsed.text || exponent || bare_digits) {
        return None;
    }
    if !general && !parsed.date && parsed.text && (digits || exponent || bare_digits) {
        return None;
    }
    parsed.empty = tokens == 0;
    Some(parsed)
}

fn parse(code: &str) -> Code {
    let rejected = |sections| Code {
        sections,
        parses: false,
    };
    if code.is_empty() {
        return rejected(Vec::new());
    }
    let Some(parts) = split_sections(code) else {
        return rejected(Vec::new());
    };
    let parsed: Vec<Option<Section>> = parts.iter().map(|part| parse_section(part)).collect();
    // What each section would be had the whole parsed, for the rejected
    // code's dates: a rejected section counts as a date when it names one.
    let sections: Vec<Section> = parsed
        .iter()
        .zip(&parts)
        .map(|(section, part)| match section {
            Some(section) => Section {
                date: section.date,
                ..Section::default()
            },
            None => Section {
                date: part
                    .chars()
                    .any(|c| matches!(c, 'y' | 'Y' | 'd' | 'D' | 'm' | 'M' | 'h' | 'H')),
                ..Section::default()
            },
        })
        .collect();
    if parts.len() > 4 || parsed.iter().any(Option::is_none) {
        return rejected(sections);
    }
    let parsed: Vec<Section> = parsed.into_iter().flatten().collect();
    if parsed.iter().filter(|section| section.condition).count() > 2 {
        return rejected(sections);
    }
    let last = parsed.len() - 1;
    for (index, section) in parsed.iter().enumerate() {
        if section.text && (index != last || section.condition) {
            return rejected(sections);
        }
        if parsed.len() == 4 && index == 3 && !section.text && !section.empty {
            return rejected(sections);
        }
    }
    Code {
        sections: parsed,
        parses: true,
    }
}

/// What converting a cell of `class` loses through format `id`, whose own
/// code, if the file defines one, is `code`.
pub(super) fn loss(id: u32, code: Option<&str>, class: CellClass) -> FormatLoss {
    let numeric = class != CellClass::Text;
    let code = match code.or_else(|| builtin_code(id)) {
        Some(code) => code,
        None => {
            return FormatLoss {
                misrendered: numeric && builtin_locale_date(id),
                ..FormatLoss::default()
            };
        }
    };
    let code = parse(code);
    if !code.parses {
        return FormatLoss {
            // A date shows as its serial number.
            misrendered: numeric && code.sections.iter().any(|section| section.date),
            // Text that a literal fourth section replaces shows as it is.
            hidden: class == CellClass::Text && code.sections.len() == 4,
        };
    }
    let sections = &code.sections;
    if class == CellClass::Text {
        let text_section = match sections.last() {
            Some(section) if section.text => Some(section),
            _ if sections.len() == 4 => sections.get(3),
            _ => None,
        };
        return FormatLoss {
            hidden: text_section.is_some_and(|section| section.empty),
            ..FormatLoss::default()
        };
    }
    // Conditions pick sections by value; those codes are left alone.
    if sections.iter().any(|section| section.condition) {
        return FormatLoss::default();
    }
    let numeric_sections = match sections.last() {
        Some(section) if section.text => &sections[..sections.len() - 1],
        _ if sections.len() == 4 => &sections[..3],
        _ => &sections[..],
    };
    let index = match (numeric_sections.len(), class) {
        (0, _) => return FormatLoss::default(),
        (1, _) => 0,
        (2, CellClass::Negative) => 1,
        (2, _) => 0,
        (_, CellClass::Positive) => 0,
        (_, CellClass::Negative) => 1,
        _ => 2,
    };
    let section = &numeric_sections[index];
    FormatLoss {
        hidden: section.empty && class != CellClass::Zero,
        // The negative section renders the magnitude with only its own
        // characters: a colour alone marked it negative.
        misrendered: index == 1 && section.colour && !section.sign && !section.empty,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use CellClass::{Negative, Positive, Text, Zero};

    fn misrendered(code: &str, class: CellClass) -> bool {
        loss(164, Some(code), class).misrendered
    }

    fn hidden(code: &str, class: CellClass) -> bool {
        loss(164, Some(code), class).hidden
    }

    #[test]
    fn negatives_marked_only_by_colour_are_found() {
        for code in [
            "0.00;[Red]0.00",
            "\"$\"#,##0.00;[Red]\"$\"#,##0.00",
            "#,##0;[Red]#,##0",
            "#,##0;[Color10]#,##0;0",
        ] {
            assert!(misrendered(code, Negative), "{code}");
            assert!(!misrendered(code, Positive), "{code}");
        }
        for code in [
            "#,##0.00;[Red]\\(#,##0.00\\)",
            "0.0%;[Red]-0.0%",
            "#,##0;[Red]\"-\"#,##0",
            "#,##0;[Red]#,##0\" CR\"",
            "0.00;0.00",
            "0.00",
            "[Red]0.00",
            "[<0][Red]0.00;0.00",
        ] {
            assert!(!misrendered(code, Negative), "{code}");
        }
        // Built-in 38 and 40 wrap negatives in parentheses.
        assert!(!loss(38, None, Negative).misrendered);
    }

    #[test]
    fn values_a_format_hides_are_found() {
        for (code, class) in [
            (";;;", Positive),
            (";;;", Negative),
            (";;;", Text),
            ("0;;0", Negative),
            ("0;[Red];0", Negative),
        ] {
            assert!(hidden(code, class), "{code} {class:?}");
        }
        for (code, class) in [
            // Hiding zeros is common and hides no amount.
            ("#,##0;(#,##0);", Zero),
            ("#,##0;-#,##0;;@", Zero),
            ("0;-0;\"-\"", Zero),
            ("0;-0;0;@", Text),
            ("0", Text),
            ("0.00", Positive),
        ] {
            assert!(!hidden(code, class), "{code} {class:?}");
        }
        // A literal fourth section fails AnyDoc's parse, so the raw text
        // shows where the format shows the literal.
        assert!(hidden("0;-0;0;\"N/A\"", Text));
        assert!(!hidden("0;-0;0;\"N/A\"", Positive));
    }

    #[test]
    fn dates_anydoc_cannot_resolve_are_found() {
        // Locale date ids without a code of their own.
        for id in [27, 31, 36, 50, 57, 71, 81] {
            assert!(loss(id, None, Positive).misrendered, "{id}");
            assert!(!loss(id, None, Text).misrendered, "{id}");
        }
        for id in [0, 14, 22, 5, 7, 44] {
            assert!(!loss(id, None, Positive).misrendered, "{id}");
        }
        // A code its parser rejects: an unquoted character it does not know.
        assert!(misrendered("yyyy年m月d日", Positive));
        assert!(misrendered("[$-404]e/m/d", Positive));
        // Codes it reads, including locale tags and quoted text.
        for code in [
            "[$-409]mmmm d, yyyy;@",
            "[$-F800]dddd\\,\\ mmmm\\ dd\\,\\ yyyy",
            "yyyy\"年\"m\"月\"d\"日\"",
            "d-mmm-yy h:mm AM/PM",
            "[h]:mm:ss",
        ] {
            assert!(!misrendered(code, Positive), "{code}");
        }
        // A rejected currency code falls back to General, which keeps the
        // sign and the value.
        assert_eq!(
            loss(164, Some("£#,##0.00"), Negative),
            FormatLoss::default()
        );
    }

    #[test]
    fn the_grammar_follows_anydocs_parser() {
        for code in [
            "#,##0.00_);[Red]\\(#,##0.00\\)",
            "_(\"$\"* #,##0.00_);_(\"$\"* \\(#,##0.00\\);_(\"$\"* \"-\"??_);_(@_)",
            "General",
            "\"Total: \"General",
            "0.00E+00",
            "# ?/?",
            "[>=1000]#,##0;0",
        ] {
            assert!(parse(code).parses, "{code}");
        }
        for code in [
            "",
            "\"open",
            "[Red",
            "0;0;0;0;0",
            "Generals0",
            "@;0",
            "0@",
            "[Purple]0",
            "[<0]0;[>0]0;[=0]0",
            "0;0;0;0",
        ] {
            assert!(!parse(code).parses, "{code}");
        }
    }
}
