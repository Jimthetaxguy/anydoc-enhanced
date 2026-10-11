//! What a workbook's number formats do to the values AnyDoc 0.2.4 converts.
//!
//! AnyDoc renders a cell through its format code when it can parse the code
//! and falls back to General when it cannot (`sheet::numfmt`). Four things
//! go wrong for a reader of the Markdown, and each converts without a word:
//!
//! - A negative value whose format marks it only by a colour, such as
//!   `#,##0;[Red]#,##0`, renders as a positive number: the colour is gone,
//!   and the section shows no sign. A section that shows other text than
//!   the positive one (`"Refund "`, `▼`) still marks it, and a value too
//!   small to show a digit at the section's decimals shows as zero either
//!   way.
//! - A value its format hides (an empty section, as in `;;;`) is shown by
//!   no spreadsheet application, while the workbook still holds it; hidden
//!   rows and columns are refused for the same reason. A zero a format
//!   hides is the common "hide zeros" idiom and is not counted.
//! - A date whose format AnyDoc cannot resolve renders as its serial
//!   number: a built-in locale date format outside AnyDoc's table, or a
//!   code its parser rejects. So does a built-in percentage outside the
//!   table (ids 67 and 68), a hundredth of what the format shows.
//! - A fraction whose format scales it by thousands (`# ?/?,`) renders a
//!   thousandth of the value, where LibreOffice shows the fraction
//!   unscaled.
//!
//! The parse here follows AnyDoc's grammar closely enough to tell which
//! codes it rejects, section by section; the number layout inside a section
//! is not needed and is not checked.

/// How a cell's value meets its format.
#[derive(Clone, Copy, Debug)]
pub(super) enum CellClass {
    /// A negative value, by its magnitude. A larger one shows at least as
    /// much of a format as a smaller one, so a style's largest stands for
    /// them all.
    Negative {
        magnitude: f64,
    },
    Zero,
    Positive,
    Text,
}

impl PartialEq for CellClass {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (CellClass::Negative { magnitude }, CellClass::Negative { magnitude: other }) => {
                magnitude.to_bits() == other.to_bits()
            }
            _ => std::mem::discriminant(self) == std::mem::discriminant(other),
        }
    }
}

impl Eq for CellClass {}

impl std::hash::Hash for CellClass {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        if let CellClass::Negative { magnitude } = self {
            magnitude.to_bits().hash(state);
        }
    }
}

impl CellClass {
    /// The class of a finite number.
    pub(super) fn of(number: f64) -> Self {
        if number < 0.0 {
            CellClass::Negative { magnitude: -number }
        } else if number > 0.0 {
            CellClass::Positive
        } else {
            CellClass::Zero
        }
    }
}

/// The fewest decimals `d` at which `magnitude × 10^d` rounds away from
/// zero: negative for a value of tens or more.
fn shown_from(magnitude: f64) -> i32 {
    (0.5 / magnitude).log10().ceil().clamp(-99.0, 99.0) as i32
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
/// Thai), which AnyDoc cannot resolve without the file's own code, and the
/// Thai percentages, which a reader shows as `0%` and `0.00%` and AnyDoc as
/// General.
fn builtin_misrendered(id: u32) -> bool {
    matches!(id, 27..=36 | 50..=58 | 67 | 68 | 71..=81)
}

/// One `;`-separated section, as far as the checks need it.
#[derive(Default, Debug)]
struct Section {
    /// No token renders: nothing but brackets (a colour, a condition, a
    /// locale) or nothing at all.
    empty: bool,
    condition: bool,
    colour: bool,
    /// A `-`, parenthesis, or minus sign it shows, or `CR` or `DR` as a
    /// word.
    sign: bool,
    /// The text placeholder `@`.
    text: bool,
    date: bool,
    /// The literal text it shows, without white space.
    literal: String,
    /// Decimals it shows, less three for each thousands scaling comma and
    /// plus two for each percent sign.
    decimals: i32,
    /// An exponent, which shows a digit other than zero for any value.
    exponent: bool,
    /// A fraction (`# ?/?`), which shows a value too small for a digit
    /// before its point as a fraction ("1/4").
    fraction: bool,
    /// The largest denominator a fraction shows: a fixed number (`?/16`),
    /// or the largest its placeholders hold (99 for `?/??`).
    denominator: f64,
    /// Percent signs, each of which scales the value by a hundred.
    percents: i32,
    /// Scaling commas, those after its last digit placeholder, each of
    /// which divides the value by a thousand.
    scaling: i32,
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

/// Characters a reader takes for a minus sign or accounting parentheses:
/// hyphen-minus, the minus sign, figure and en dashes, and small and
/// fullwidth forms. An em dash, which often stands for zero, is not one.
const SIGNS: [char; 10] = [
    '-', '(', ')', '\u{2212}', '\u{2012}', '\u{2013}', '\u{fe63}', '\u{ff0d}', '\u{ff08}',
    '\u{ff09}',
];

/// Whether literal text marks a value negative: a minus or parenthesis, or
/// the accounting words `CR` and `DR`, but not a currency code holding them
/// (`IDR`, `CRC`).
fn has_sign(literal: &str) -> bool {
    literal.contains(SIGNS)
        || literal
            .split(|character: char| !character.is_alphabetic())
            .any(|word| word.eq_ignore_ascii_case("CR") || word.eq_ignore_ascii_case("DR"))
}

/// The literal text a piece of a section shows, without white space.
fn push_literal(literal: &mut String, text: &str) {
    literal.extend(text.chars().filter(|character| !character.is_whitespace()));
}

/// Read one section; `None` where AnyDoc's parser rejects it.
fn parse_section(section: &str) -> Option<Section> {
    let characters: Vec<char> = section.chars().collect();
    let mut parsed = Section::default();
    let mut tokens = 0usize;
    // Tokens a date section or a text section may not hold.
    let (mut digits, mut exponent, mut bare_digits, mut general) = (false, false, false, false);
    let mut slash = false;
    // The number's decimals, its trailing (scaling) commas, and percents.
    let (mut after_point, mut placeholders_after_point) = (false, 0i32);
    let (mut commas_after_digit, mut percents) = (0i32, 0i32);
    // A fraction as AnyDoc reads one: a bar directly after an integer
    // placeholder, then the denominator's placeholders, or a fixed number
    // directly after the bar. The count of tokens read just after the last
    // integer placeholder and after the bar: the same count later means
    // nothing came between.
    let (mut placeholder_end, mut bar_end) = (None, None);
    let (mut denominator_places, mut fixed_denominator) = (0i32, None);
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
                            push_literal(&mut parsed.literal, symbol);
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
                    push_literal(&mut parsed.literal, &literal);
                }
                index = end + 1;
                continue;
            }
            '\\' => {
                let escaped = characters.get(index + 1)?.to_string();
                tokens += 1;
                parsed.sign |= has_sign(&escaped);
                push_literal(&mut parsed.literal, &escaped);
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
                let placeholder = matches!(character, '0' | '#' | '?');
                if parsed.fraction {
                    // Past the bar every placeholder is the denominator's;
                    // a point, or a placeholder after a fixed denominator,
                    // is rejected.
                    if character == '.' || (placeholder && fixed_denominator.is_some()) {
                        return None;
                    }
                    denominator_places += i32::from(placeholder);
                }
                match character {
                    '.' => after_point = true,
                    ',' => commas_after_digit += 1,
                    '%' => percents += 1,
                    _ => {
                        placeholders_after_point += i32::from(after_point);
                        commas_after_digit = 0;
                    }
                }
                tokens += 1;
                if placeholder && !after_point && !exponent && !parsed.fraction {
                    placeholder_end = Some(tokens);
                }
            }
            '@' => {
                parsed.text = true;
                tokens += 1;
            }
            'E' | 'e' if matches!(characters.get(index + 1), Some('+' | '-')) => {
                if parsed.fraction {
                    return None;
                }
                exponent = true;
                parsed.exponent = true;
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
                // Digits directly after the bar are a fixed denominator.
                let fixed = bar_end == Some(tokens);
                bare_digits = true;
                tokens += 1;
                let start = index;
                while characters.get(index).is_some_and(char::is_ascii_digit) {
                    index += 1;
                }
                if fixed {
                    let number: String = characters[start..index].iter().collect();
                    fixed_denominator = Some(number.parse::<u64>().ok()? as f64);
                }
                continue;
            }
            '$' | '-' | '+' | '(' | ')' | ':' | ' ' | '/' => {
                parsed.sign |= matches!(character, '-' | '(' | ')');
                slash |= character == '/';
                // A slash directly after an integer placeholder is a
                // fraction's bar, part of the number; any other is text the
                // section shows.
                if character == '/' && !parsed.fraction && placeholder_end == Some(tokens) {
                    parsed.fraction = true;
                    bar_end = Some(tokens + 1);
                } else {
                    push_literal(&mut parsed.literal, &character.to_string());
                }
                tokens += 1;
            }
            _ => return None,
        }
        index += 1;
    }
    // A section naming General renders the value as General, with the
    // literal text around it and whatever date letters it names ignored;
    // any other token rejects it.
    if general {
        if digits
            || exponent
            || bare_digits
            || parsed.text
            || slash
            || commas_after_digit > 0
            || percents > 0
        {
            return None;
        }
        parsed.date = false;
    }
    if !general && parsed.date && (parsed.text || exponent || bare_digits) {
        return None;
    }
    if !general && !parsed.date && parsed.text && (digits || exponent || bare_digits || slash) {
        return None;
    }
    // A bar with no denominator after it is rejected.
    if parsed.fraction && denominator_places == 0 && fixed_denominator.is_none() {
        return None;
    }
    parsed.empty = tokens == 0;
    parsed.decimals = placeholders_after_point + 2 * percents - 3 * commas_after_digit;
    parsed.denominator = fixed_denominator.unwrap_or_else(|| 10f64.powi(denominator_places) - 1.0);
    parsed.percents = percents;
    parsed.scaling = commas_after_digit;
    Some(parsed)
}

/// Whether a section AnyDoc rejects names a date: a date letter outside
/// quotes, escapes, and brackets other than an elapsed-time one (`[h]`).
fn names_date(section: &str) -> bool {
    let mut characters = section.chars();
    while let Some(character) = characters.next() {
        match character {
            '"' => {
                characters.by_ref().find(|&next| next == '"');
            }
            '\\' | '_' | '*' => {
                characters.next();
            }
            '[' => {
                let inner: String = characters
                    .by_ref()
                    .take_while(|&next| next != ']')
                    .collect();
                let mut letters = inner.chars();
                if letters.next().is_some_and(|first| {
                    matches!(first.to_ascii_lowercase(), 'h' | 'm' | 's')
                        && letters.all(|next| next.eq_ignore_ascii_case(&first))
                }) {
                    return true;
                }
            }
            'y' | 'Y' | 'd' | 'D' | 'm' | 'M' | 'h' | 'H' => return true,
            _ => {}
        }
    }
    false
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
                date: names_date(part),
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
                misrendered: numeric && builtin_misrendered(id),
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
        (2, CellClass::Negative { .. }) => 1,
        (2, _) => 0,
        (_, CellClass::Positive) => 0,
        (_, CellClass::Negative { .. }) => 1,
        _ => 2,
    };
    let section = &numeric_sections[index];
    if let CellClass::Negative { magnitude } = class {
        // Too small to show a digit: a zero, however it is marked. A
        // fraction shows zero below half its smallest step, one over its
        // largest denominator, as AnyDoc rounds the numerator of the value
        // its percent signs scale. AnyDoc also divides it by a thousand for
        // each scaling comma, where LibreOffice shows the fraction unscaled
        // (-0.2 in `# ?/?,` as 1/5), so the commas are left out.
        let zero = if section.fraction {
            magnitude * 100f64.powi(section.percents) * section.denominator < 0.5
        } else {
            !section.exponent && shown_from(magnitude) > section.decimals
        };
        if zero {
            return FormatLoss::default();
        }
    }
    // A fraction AnyDoc divides by a thousand for each scaling comma, where
    // LibreOffice shows it unscaled: 0.2 in `# ?/?,` shows as 0, not 1/5.
    // A negative value too small to show a fraction either way has passed.
    let scaled_fraction = section.fraction && section.scaling > 0 && class != CellClass::Zero;
    FormatLoss {
        hidden: section.empty && class != CellClass::Zero,
        // The negative section renders the magnitude with only its own
        // characters: a colour alone marked it negative, with no sign and
        // no text of its own.
        misrendered: scaled_fraction
            || (index == 1
                && section.colour
                && !section.sign
                && !section.empty
                && section.literal == numeric_sections[0].literal),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use CellClass::{Positive, Text, Zero};

    /// A negative value that shows at any decimals.
    const NEGATIVE: CellClass = CellClass::Negative { magnitude: 2.0 };

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
            assert!(misrendered(code, NEGATIVE), "{code}");
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
            assert!(!misrendered(code, NEGATIVE), "{code}");
        }
        // Built-in 38 and 40 wrap negatives in parentheses.
        assert!(!loss(38, None, NEGATIVE).misrendered);
        // A currency code holding `CR` or `DR` is no accounting marker.
        for code in [
            "\"IDR \"#,##0;[Red]\"IDR \"#,##0",
            "[$IDR-421]#,##0;[Red][$IDR-421]#,##0",
            "\"CRC \"#,##0;[Red]\"CRC \"#,##0",
        ] {
            assert!(misrendered(code, NEGATIVE), "{code}");
        }
        assert!(!misrendered("#,##0;[Red]#,##0\" DR\"", NEGATIVE));
        // Text of its own, or an arrow, marks the section.
        for code in [
            "\"Balance due \"$#,##0;[Red]\"Refund \"$#,##0;\"Even\"",
            "[$-409]\\▲0.0%;[RED]\\▼0.0%",
        ] {
            assert!(parse(code).parses, "{code}");
            assert!(!misrendered(code, NEGATIVE), "{code}");
        }
        assert!(misrendered("$#,##0;[Red]$#,##0", NEGATIVE));
        // AnyDoc renders a section naming General and date letters as
        // General, so the colour alone marks the negative there too.
        for code in [
            "General;[Red]General s",
            "General;[Red]General A/P",
            "General;[Red]General a/p",
            "General;[Red]General d",
            "General;[Red]General [h]",
            "0.00;[Red]General s",
            "#,##0;[Red]General S",
        ] {
            assert!(parse(code).parses, "{code}");
            assert!(misrendered(code, NEGATIVE), "{code}");
            assert!(!misrendered(code, Positive), "{code}");
        }
        assert!(!misrendered("General;[Red]-General s", NEGATIVE));
        // A value too small to show a digit shows as zero either way.
        let class = CellClass::of;
        for (code, value, expected) in [
            ("#,##0.00;[Red]#,##0.00", -2.91e-11, false),
            ("#,##0.00;[Red]#,##0.00", -0.004, false),
            ("#,##0.00;[Red]#,##0.00", -0.006, true),
            ("#,##0.00;[Red]#,##0.00", -1250.0, true),
            ("0%;[Red]0%", -0.004, false),
            ("0%;[Red]0%", -0.006, true),
            ("#,##0,;[Red]#,##0,", -400.0, false),
            ("#,##0,;[Red]#,##0,", -600.0, true),
            ("0.00E+00;[Red]0.00E+00", -2.91e-11, true),
            // A fraction shows a quarter as "1/4", and zero below half its
            // smallest step: 1/18 for one digit, 1/198 for two, and for a
            // fixed denominator, 1/4 for halves, 1/20 for tenths, 1/32 for
            // sixteenths.
            ("# ?/?;[Red]# ?/?", -0.25, true),
            ("# ??/??;[Red]# ??/??", -0.25, true),
            ("# ?/?;[Red]# ?/?", -5.55e-17, false),
            ("# ?/?;[Red]# ?/?", -0.052, false),
            ("# ?/?;[Red]# ?/?", -0.06, true),
            ("?/?;[Red]?/?", -0.03, false),
            ("# ??/??;[Red]# ??/??", -0.004, false),
            ("# ??/??;[Red]# ??/??", -0.006, true),
            ("# ?/16;[Red]# ?/16", -0.001, false),
            ("# ??/16;[Red]# ??/16", -0.03, false),
            ("# ?/16;[Red]# ?/16", -0.1, true),
            ("# ?/2;[Red]# ?/2", -0.2, false),
            ("# ?/2;[Red]# ?/2", -0.3, true),
            ("# ?/10;[Red]# ?/10", -0.049, false),
            ("# ?/10;[Red]# ?/10", -0.06, true),
            ("# ?/?;[Red]# ?/?", -1.5, true),
            // Every placeholder past the bar is the denominator's: 2/67
            // shows as "2/6 7".
            ("# ?/? ?;[Red]# ?/? ?", -0.03, true),
            // Percent signs scale the value; scaling commas are left out.
            ("# ?/?%;[Red]# ?/?%", -0.004, true),
            ("# ?/?%;[Red]# ?/?%", -0.0004, false),
            ("# ?/?,;[Red]# ?/?,", -0.2, true),
            // A fraction's bar is not text marking the section; a slash
            // anywhere else is.
            ("0.00;[Red]# ?/?", -0.2, true),
            ("0.00;[Red]0/?", -0.25, true),
            ("?*x/?;[Red]?*x/?", -0.25, true),
            ("0.00;[Red]-# ?/?", -0.2, false),
            ("0.00;[Red]0.0/0", -0.25, false),
            ("?/?;[Red]?/?/", -0.25, false),
            // A scaling comma shows a fraction a thousandth of its value,
            // unless it shows zero either way.
            ("# ?/?,", -0.2, true),
            ("# ?/?,", -0.0001, false),
            ("# ?/8,", -1500.0, true),
        ] {
            assert_eq!(
                loss(164, Some(code), class(value)).misrendered,
                expected,
                "{code} {value}"
            );
        }
    }

    #[test]
    fn values_a_format_hides_are_found() {
        for (code, class) in [
            (";;;", Positive),
            (";;;", NEGATIVE),
            (";;;", Text),
            ("0;;0", NEGATIVE),
            ("0;[Red];0", NEGATIVE),
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
        // sign and the value, whatever colours it names.
        for code in [
            "£#,##0.00",
            "[Green]▲#,##0;[Red]▼#,##0",
            "#,##0.00 €;[Red]-#,##0.00 €",
            "[Magenta]#,##0 £",
        ] {
            assert!(!parse(code).parses, "{code}");
            for class in [NEGATIVE, Positive] {
                assert_eq!(
                    loss(164, Some(code), class),
                    FormatLoss::default(),
                    "{code}"
                );
            }
        }
        // A section naming General and date letters shows General, as
        // LibreOffice shows it too.
        for code in [
            "General d",
            "General yyyy",
            "[h]General",
            "General h;General",
        ] {
            assert!(parse(code).parses, "{code}");
            assert!(!misrendered(code, Positive), "{code}");
        }
        // A fraction scaled by thousands shows a thousandth of any value
        // but zero; other formats scale alike on both sides.
        for code in ["# ?/?,", "# ?/?,;[Red]# ?/?,", "?/8,"] {
            assert!(misrendered(code, Positive), "{code}");
            assert!(!misrendered(code, Zero), "{code}");
            assert!(!misrendered(code, Text), "{code}");
        }
        assert!(!misrendered("#,##0,", Positive));
        assert!(!misrendered("# ?/?", Positive));
        // Built-in percentages outside AnyDoc's table show a hundredth.
        for id in [67, 68] {
            assert!(loss(id, None, Positive).misrendered, "{id}");
            assert!(!loss(id, None, Text).misrendered, "{id}");
        }
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
            "# ??/16",
            "0.00/",
            "[>=1000]#,##0;0",
            "General s",
            "\"x\"General A/P",
            "_(General_)",
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
            // A fraction without a denominator directly after its bar, or
            // with more after a fixed one.
            "# ?/",
            "# ?/ 16",
            "# ?/16 ?",
            "?/?.0",
            "?/?E+0",
            // General beside any number token, and text beside a slash.
            "General%",
            "General,",
            "General/",
            "0 General",
            "@/",
        ] {
            assert!(!parse(code).parses, "{code}");
        }
    }
}
