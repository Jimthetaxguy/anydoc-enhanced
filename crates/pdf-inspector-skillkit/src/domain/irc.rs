//! Title 26 (Internal Revenue Code) section parser over pdf-inspector
//! Markdown.
//!
//! pdf-inspector renders section headings as Markdown headings
//! (`# §1398. Title`) and enumerated provisions as headings, bold runs, or
//! plain paragraphs (`## (a) Title`, `**(2) Title** Body`, `(A) body`). The
//! parser normalizes those forms, tracks the subsection → paragraph →
//! subparagraph → clause → subclause hierarchy so every provision carries
//! its full citation label (`(d)(2)(A)(i)`), and keeps the editorial and
//! statutory notes the U.S. Code prints after each section out of the
//! statutory text.

use regex::Regex;
use serde::Serialize;
use std::sync::OnceLock;

use crate::SkillkitError;

#[derive(Debug, Clone, Serialize)]
pub struct IrcSection {
    pub section_number: String,
    pub title: String,
    /// Statutory text of the section through its source credit. Notes
    /// printed after the section are reported separately in `notes`.
    pub content: String,
    pub subsections: Vec<IrcSubsection>,
    pub char_offset: usize,
    pub char_length: usize,
    /// True for a `[§N. Repealed. …]` placeholder heading.
    pub repealed: bool,
    /// Editorial and statutory notes printed after the section text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IrcSubsection {
    /// Full citation path within the section, such as `(d)(2)(A)(i)`.
    pub label: String,
    /// Heading carried by the provision, when it has one.
    pub title: Option<String>,
    /// Text of this provision, excluding nested provisions.
    pub content: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct IrcParseResult {
    pub subtitle: Option<String>,
    pub chapter: Option<String>,
    pub subchapter: Option<String>,
    pub sections: Vec<IrcSection>,
    pub total_sections: usize,
}

// IRC structural patterns are static literals — compile once and reuse.
// All `expect` calls fire only on programmer error in the pattern itself,
// not on any runtime input.

fn section_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(?:§{1,2}\s*|SEC\.\s*|SECTION\s+)(\d+[A-Z]*(?:[-–]\d+[A-Z]*)?)\.\s*(.*)$")
            .expect("IRC section regex must compile (compile-time invariant)")
    })
}

fn enumerator_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // A label is followed by whitespace, a bold closer, or nothing:
        // `(1), July 1, 1972` and `(1)—` are wrapped citation fragments.
        Regex::new(r"^\(([a-z]{1,7}|[0-9]{1,3}|[A-Z]{1,7})\)(\s.*|\*\*.*)?$")
            .expect("IRC enumerator regex must compile (compile-time invariant)")
    })
}

fn subtitle_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\bSUBTITLE\s+([A-Z])")
            .expect("IRC subtitle regex must compile (compile-time invariant)")
    })
}

fn chapter_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\bCHAPTER\s+([IVX0-9]+)")
            .expect("IRC chapter regex must compile (compile-time invariant)")
    })
}

fn subchapter_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\bSUBCHAPTER\s+([A-Z])")
            .expect("IRC subchapter regex must compile (compile-time invariant)")
    })
}

pub fn parse_irc_sections(
    path: impl AsRef<std::path::Path>,
) -> Result<IrcParseResult, SkillkitError> {
    let info = crate::process(&path)?;
    Ok(parse_irc_markdown_with_source(
        info.markdown.as_deref().unwrap_or_default(),
        path.as_ref(),
    ))
}

/// Parse IRC sections from Markdown already produced by `pdf_to_markdown`.
pub fn parse_irc_markdown(markdown: &str) -> IrcParseResult {
    parse_markdown(markdown, "")
}

/// Like [`parse_irc_markdown`], falling back to the source file name for a
/// subtitle, chapter, or subchapter the text does not state.
pub fn parse_irc_markdown_with_source(markdown: &str, source: &std::path::Path) -> IrcParseResult {
    parse_markdown(markdown, &source.to_string_lossy().to_uppercase())
}

/// One Markdown line with heading markers and a leading bold opener removed.
struct Line<'a> {
    text: &'a str,
    heading: bool,
    bold: bool,
}

impl<'a> Line<'a> {
    fn new(raw: &'a str) -> Self {
        let trimmed = raw.trim();
        let unmarked = trimmed.trim_start_matches('#');
        let heading = unmarked.len() != trimmed.len();
        let unmarked = unmarked.trim_start();
        match unmarked.strip_prefix("**") {
            Some(rest) => Self {
                text: rest.trim_start(),
                heading,
                bold: true,
            },
            None => Self {
                text: unmarked,
                heading,
                bold: false,
            },
        }
    }

    /// Text with a trailing bold closer removed.
    fn plain(&self) -> &'a str {
        self.text.trim_end().trim_end_matches("**").trim_end()
    }

    /// `(number, title, repealed)` when the line opens a section.
    fn section_heading(&self) -> Option<(String, String, bool)> {
        let (text, bracketed) = match self.plain().strip_prefix('[') {
            Some(rest) => (rest.trim_start(), true),
            None => (self.plain(), false),
        };
        let caps = section_re().captures(text)?;
        let mut title = caps[2].trim();
        if bracketed {
            title = title.strip_suffix(']').unwrap_or(title).trim_end();
        }
        // Citation runs inside notes can also begin with `§` (for example
        // `§4(a), title XV, …`); an unmarked line only opens a section when
        // it reads like a heading.
        let looks_like_heading =
            self.heading || self.bold || bracketed || title.starts_with(|c: char| c.is_uppercase());
        if !looks_like_heading {
            return None;
        }
        let repealed = title
            .get(..8)
            .is_some_and(|word| word.eq_ignore_ascii_case("repealed"));
        Some((caps[1].to_string(), title.to_string(), repealed))
    }

    /// U.S. Code note headings that end a section's statutory text.
    fn opens_notes(&self) -> bool {
        if !(self.heading || self.bold) {
            return false;
        }
        let text = self.plain();
        matches!(
            text,
            "Editorial Notes"
                | "Statutory Notes and Related Subsidiaries"
                | "Statutory Notes"
                | "Executive Documents"
                | "Amendments"
                | "References in Text"
                | "Codification"
                | "Prior Provisions"
        ) || text.starts_with("Effective Date")
    }

    /// `(token, title, body)` when the line opens an enumerated provision.
    fn enumerator(&self) -> Option<(&'a str, Option<String>, &'a str)> {
        // `[(b) Repealed. …]` keeps its label so the sequence continues.
        let (text, bracketed) = match self.text.strip_prefix('[') {
            Some(rest) => (rest, true),
            None => (self.text, false),
        };
        let caps = enumerator_re().captures(text)?;
        let token = caps.get(1)?.as_str();
        let rest = caps.get(2).map_or("", |rest| rest.as_str().trim_start());
        let (title, body) = if self.bold {
            match rest.split_once("**") {
                Some((title, body)) => (title.trim(), body.trim()),
                None => (rest.trim(), ""),
            }
        } else if self.heading || bracketed {
            (rest.trim().trim_end_matches("**").trim_end(), "")
        } else {
            // A cross-reference wrapped onto its own line, such as
            // `(2) of section 3121(d).` or `(1) and (2)`, is not a provision.
            const REFERENCE_CONTINUATIONS: [&str; 4] = ["of ", "and (", "or (", "through ("];
            if REFERENCE_CONTINUATIONS
                .iter()
                .any(|prefix| rest.starts_with(prefix))
            {
                return None;
            }
            ("", rest.trim())
        };
        let title = if bracketed {
            title.strip_suffix(']').unwrap_or(title).trim_end()
        } else {
            title
        };
        let title = (!title.is_empty()).then(|| title.to_string());
        Some((token, title, body))
    }
}

/// Provision depth, from subsection `(a)` down to subitem `(AA)`.
const LEVELS: usize = 7;
const SUBSECTION: usize = 0;
const PARAGRAPH: usize = 1;
const SUBPARAGRAPH: usize = 2;
const CLAUSE: usize = 3;
const SUBCLAUSE: usize = 4;
const ITEM: usize = 5;
const SUBITEM: usize = 6;

fn roman_value(token: &str) -> Option<u32> {
    let mut total = 0u32;
    let mut previous = 0u32;
    for ch in token.chars().rev() {
        let value = match ch.to_ascii_lowercase() {
            'i' => 1,
            'v' => 5,
            'x' => 10,
            'l' => 50,
            'c' => 100,
            'd' => 500,
            'm' => 1000,
            _ => return None,
        };
        if value < previous {
            total = total.checked_sub(value)?;
        } else {
            total += value;
            previous = value;
        }
    }
    (total > 0).then_some(total)
}

fn next_letter(token: &str) -> Option<char> {
    let mut chars = token.chars();
    let first = chars.next()?;
    if chars.next().is_some() || !first.is_ascii_alphabetic() || first.eq_ignore_ascii_case(&'z') {
        return None;
    }
    char::from_u32(first as u32 + 1)
}

/// Assign a depth to an enumerator token. Letters that are also Roman
/// numerals (`i`, `v`, `x`, `I`, …) are resolved by which sequence they
/// continue; when both fit, a preceding lead-in (text ending in a dash or
/// colon, as in "…as 2 taxable years—") marks a clause list.
fn level_for(
    token: &str,
    stack: &[Option<String>; LEVELS],
    lead_in: bool,
    plain: bool,
) -> Option<usize> {
    if token.bytes().all(|b| b.is_ascii_digit()) {
        return Some(PARAGRAPH);
    }
    let lower = token.bytes().all(|b| b.is_ascii_lowercase());
    let upper = token.bytes().all(|b| b.is_ascii_uppercase());
    if !lower && !upper {
        return None;
    }
    let (alpha, roman_level, doubled_level) = if lower {
        (SUBSECTION, CLAUSE, ITEM)
    } else {
        (SUBPARAGRAPH, SUBCLAUSE, SUBITEM)
    };
    let roman = roman_value(token);
    let continues_roman = roman.is_some()
        && match &stack[roman_level] {
            Some(previous) => roman_value(previous).map(|value| value + 1) == roman,
            None => roman == Some(1) && stack[roman_level - 1].is_some(),
        };
    let bytes = token.as_bytes();
    if bytes.len() > 1 {
        let doubled = bytes.len() == 2 && bytes[0] == bytes[1];
        // Items such as `(aa)` nest under subclauses; elsewhere a doubled
        // token that is also a numeral (`ii`, `xx`) is a clause.
        return if continues_roman {
            Some(roman_level)
        } else if doubled && stack[doubled_level - 1].is_some() {
            Some(doubled_level)
        } else if roman.is_some() {
            Some(roman_level)
        } else if doubled {
            Some(doubled_level)
        } else {
            None
        };
    }
    let continues_alpha = match &stack[alpha] {
        Some(previous) => next_letter(previous) == token.chars().next(),
        None => matches!(token, "a" | "A"),
    };
    Some(match (continues_alpha, continues_roman) {
        (true, true) if lead_in => roman_level,
        (true, _) => alpha,
        (false, true) => roman_level,
        // Continuing neither sequence: `(i)` opens a clause list (for
        // example in a subsection's flush text), and an unheaded numeral is
        // more likely a clause whose predecessors sat inline.
        (false, false) if roman == Some(1) || (plain && roman.is_some()) => roman_level,
        (false, false) => alpha,
    })
}

fn label_for(stack: &[Option<String>; LEVELS], level: usize) -> String {
    stack[..=level]
        .iter()
        .flatten()
        .map(|token| format!("({token})"))
        .collect()
}

struct OpenSection {
    offset: usize,
    number: String,
    title: String,
    repealed: bool,
    content: String,
    notes: String,
    in_notes: bool,
    subsections: Vec<IrcSubsection>,
    stack: [Option<String>; LEVELS],
    lead_in: bool,
}

impl OpenSection {
    fn finish(self) -> IrcSection {
        let content = self.content.trim().to_string();
        let notes = self.notes.trim();
        IrcSection {
            section_number: format!("§{}", self.number),
            title: self.title,
            char_length: content.len(),
            content,
            subsections: self
                .subsections
                .into_iter()
                .map(|mut sub| {
                    sub.content = sub.content.trim().to_string();
                    sub
                })
                .collect(),
            char_offset: self.offset,
            repealed: self.repealed,
            notes: (!notes.is_empty()).then(|| notes.to_string()),
        }
    }

    fn push_text(&mut self, raw: &str, body: &str) {
        self.content.push_str(raw);
        self.content.push('\n');
        let body = body.trim_end();
        if body.is_empty() {
            return;
        }
        self.lead_in = body.ends_with(['—', '–', ':']);
        if let Some(sub) = self.subsections.last_mut() {
            if !sub.content.is_empty() {
                sub.content.push('\n');
            }
            sub.content.push_str(body);
        }
    }
}

fn parse_markdown(text: &str, path_hint: &str) -> IrcParseResult {
    let mut sections = Vec::new();
    let mut open: Option<OpenSection> = None;
    let mut offset = 0usize;

    for raw in text.split_inclusive('\n') {
        let line_offset = offset;
        offset += raw.len();
        let raw = raw.trim_end_matches(['\n', '\r']);
        let line = Line::new(raw);

        if let Some((number, title, repealed)) = line.section_heading() {
            if let Some(section) = open.take() {
                sections.push(section.finish());
            }
            let mut content = String::from(raw);
            content.push('\n');
            open = Some(OpenSection {
                offset: line_offset,
                number,
                title,
                repealed,
                content,
                notes: String::new(),
                in_notes: false,
                subsections: Vec::new(),
                stack: Default::default(),
                lead_in: false,
            });
            continue;
        }
        // Chapter headings, tables of contents, and chapter-level notes
        // before the first section are not section text.
        let Some(section) = open.as_mut() else {
            continue;
        };
        if section.in_notes || line.opens_notes() {
            section.in_notes = true;
            section.notes.push_str(raw);
            section.notes.push('\n');
            continue;
        }
        if let Some((token, title, body)) = line.enumerator() {
            let plain = !(line.heading || line.bold);
            if let Some(level) = level_for(token, &section.stack, section.lead_in, plain) {
                section.stack[level] = Some(token.to_string());
                for deeper in &mut section.stack[level + 1..] {
                    *deeper = None;
                }
                section.subsections.push(IrcSubsection {
                    label: label_for(&section.stack, level),
                    title,
                    content: String::new(),
                });
                section.lead_in = false;
                section.push_text(raw, body);
                continue;
            }
        }
        section.push_text(raw, line.plain());
    }
    if let Some(section) = open {
        sections.push(section.finish());
    }

    let total_sections = sections.len();
    IrcParseResult {
        subtitle: extract_heading(subtitle_re(), "Subtitle", text, 5, path_hint),
        chapter: extract_heading(chapter_re(), "Chapter", text, 10, path_hint),
        subchapter: extract_heading(subchapter_re(), "Subchapter", text, 15, path_hint),
        sections,
        total_sections,
    }
}

/// Read a structural heading from the document's opening lines, falling back
/// to the caller-supplied path when the text does not name one.
fn extract_heading(
    re: &Regex,
    label: &str,
    text: &str,
    lines: usize,
    path_hint: &str,
) -> Option<String> {
    let opening: String = text.lines().take(lines).collect::<Vec<_>>().join(" ");
    re.captures(&opening)
        .or_else(|| re.captures(path_hint))
        .and_then(|caps| caps.get(1))
        .map(|value| format!("{label} {}", value.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const USC_MARKDOWN: &str = "\
## 26 USC Subtitle A, CHAPTER 1, Subchapter V: Title 11 Cases

Sec.

1398. Rules relating to individuals' title 11 cases.
## Editorial Notes

# Amendments

**1980**—Pub. L. 96–589 added subchapter V.

# §1398. Rules relating to individuals' title 11 cases

## (a) Cases to which section applies

This section shall apply to any case under chapter 7 or chapter 11.

**(b) Exceptions where case is dismissed, etc.**
## (1) Section does not apply where case is dismissed

This section shall not apply if the case is dismissed.

**(2) Section does not apply at partnership level** For purposes of subsection (a), a partnership shall not be treated as an individual.
**(d) Taxable year of debtors**
**(2) Election to terminate debtor's year when case commences**
## (A) In general

The debtor may elect to treat the taxable year as 2 taxable years—

(i) the first of which ends on the day before the commencement date, and (ii) the second of which begins on the commencement date.
**(B) Spouse may join in election** The spouse may join the election.
**(h) Administration expenses**
**(2) Carryback of excess administrative costs**
**(D) Deductions allowed only to estate** The deductions shall be allowable only to the estate.
## (i) Debtor succeeds to tax attributes of estate

The debtor shall succeed to the tax attributes of the estate.

**(j) Other special rules**
**(2) Treatment of certain carrybacks**
## (C) Carryback and carryback year defined

For purposes of this paragraph—

**(i) Carryback** The term \"carryback\" means a net operating loss carryback.
## (ii) Carryback year

The term \"carryback year\" means the taxable year to which a carryback is carried. (Added Pub. L. 96–589, §3(a)(1), Dec. 24, 1980, 94 Stat. 3397.)

## Editorial Notes

# Amendments

**1986**—Subsec. (c). Pub. L. 99–514 substituted \"basic standard deduction\".
(1), July 1, 1972, 86 Stat. 420.
§4(a), title XV, §§15301(a), June 18, 2008.

# §1399. No separate taxable entities for partnerships, corporations, etc.

Except in any case to which section 1398 applies, no separate taxable entity shall result.

# [§1400. Repealed. Pub. L. 115–97, title I, §13614(b), Dec. 22, 2017, 131 Stat. 2187]
";

    fn labels(section: &IrcSection) -> Vec<&str> {
        section
            .subsections
            .iter()
            .map(|sub| sub.label.as_str())
            .collect()
    }

    #[test]
    fn test_parse_nonexistent_file_returns_error() {
        let result = super::parse_irc_sections("/nonexistent/file.pdf");
        assert!(result.is_err());
    }

    #[test]
    fn markdown_heading_sections_are_found() {
        let result = parse_irc_markdown(USC_MARKDOWN);
        let numbers: Vec<_> = result
            .sections
            .iter()
            .map(|section| section.section_number.as_str())
            .collect();
        assert_eq!(numbers, ["§1398", "§1399", "§1400"]);
        assert_eq!(result.total_sections, 3);
        assert_eq!(result.subtitle.as_deref(), Some("Subtitle A"));
        assert_eq!(result.chapter.as_deref(), Some("Chapter 1"));
        assert_eq!(result.subchapter.as_deref(), Some("Subchapter V"));
        assert_eq!(
            result.sections[0].title,
            "Rules relating to individuals' title 11 cases"
        );
    }

    #[test]
    fn provisions_carry_full_citation_labels() {
        let result = parse_irc_markdown(USC_MARKDOWN);
        assert_eq!(
            labels(&result.sections[0]),
            [
                "(a)",
                "(b)",
                "(b)(1)",
                "(b)(2)",
                "(d)",
                "(d)(2)",
                "(d)(2)(A)",
                "(d)(2)(A)(i)",
                "(d)(2)(B)",
                "(h)",
                "(h)(2)",
                "(h)(2)(D)",
                "(i)",
                "(j)",
                "(j)(2)",
                "(j)(2)(C)",
                "(j)(2)(C)(i)",
                "(j)(2)(C)(ii)",
            ]
        );
    }

    #[test]
    fn provision_titles_and_bodies_are_split() {
        let result = parse_irc_markdown(USC_MARKDOWN);
        let find = |label: &str| {
            result.sections[0]
                .subsections
                .iter()
                .find(|sub| sub.label == label)
                .unwrap_or_else(|| panic!("missing {label}"))
        };
        let heading = find("(a)");
        assert_eq!(
            heading.title.as_deref(),
            Some("Cases to which section applies")
        );
        assert!(heading.content.starts_with("This section shall apply"));
        let bold = find("(b)(2)");
        assert_eq!(
            bold.title.as_deref(),
            Some("Section does not apply at partnership level")
        );
        assert!(bold.content.starts_with("For purposes of subsection (a)"));
        let plain = find("(d)(2)(A)(i)");
        assert_eq!(plain.title, None);
        assert!(plain.content.starts_with("the first of which ends"));
    }

    #[test]
    fn notes_are_kept_out_of_statutory_text() {
        let result = parse_irc_markdown(USC_MARKDOWN);
        let section = &result.sections[0];
        assert!(section.content.ends_with("94 Stat. 3397.)"));
        assert!(!section.content.contains("Editorial Notes"));
        let notes = section.notes.as_deref().expect("notes");
        assert!(notes.starts_with("## Editorial Notes"));
        assert!(notes.contains("basic standard deduction"));
        // Citation runs in the notes are neither provisions nor sections.
        assert!(labels(section).iter().all(|label| *label != "(1)"));
        assert_eq!(result.sections[1].section_number, "§1399");
        assert_eq!(result.sections[1].notes, None);
    }

    #[test]
    fn repealed_placeholders_are_flagged() {
        let result = parse_irc_markdown(USC_MARKDOWN);
        let repealed = &result.sections[2];
        assert!(repealed.repealed);
        assert!(repealed.title.starts_with("Repealed. Pub. L. 115–97"));
        assert!(!repealed.title.ends_with(']'));
        assert!(!result.sections[0].repealed);
    }

    #[test]
    fn char_offsets_point_at_section_headings() {
        let result = parse_irc_markdown(USC_MARKDOWN);
        for section in &result.sections {
            assert!(USC_MARKDOWN[section.char_offset..].starts_with("# "));
        }
    }

    #[test]
    fn plain_sec_and_section_forms_still_parse() {
        let result =
            parse_irc_markdown("SEC. 101. SHORT TITLE.\nBody.\nSECTION 102. PURPOSE.\nMore.\n");
        let numbers: Vec<_> = result
            .sections
            .iter()
            .map(|section| section.section_number.as_str())
            .collect();
        assert_eq!(numbers, ["§101", "§102"]);
    }

    #[test]
    fn wrapped_citation_fragments_are_not_provisions() {
        let markdown = "# §1401. Rate of tax\n\
            **(c) Relief** During any period the tax shall not apply. (Aug. 16, 1954, ch. 736; Pub. L. 92–336, §203(b)\n\
            (1), July 1, 1972, 86 Stat. 420.)\n\
            The adjustment under paragraph\n\
            (1)—\n";
        let result = parse_irc_markdown(markdown);
        assert_eq!(labels(&result.sections[0]), ["(c)"]);
    }

    #[test]
    fn wrapped_cross_references_and_repealed_subsections() {
        let markdown = "# §1563. Definitions and special rules\n\
            ## (a) Controlled group of corporations\n\
            ## [(b) Repealed. Pub. L. 94–455, title X, §1052(c)(5), Oct. 4, 1976, 90 Stat. 1648]\n\
            ## (c) Other definitions and rules\n\
            ## (1) Employee defined\n\
            The term employee has the meaning given by paragraph\n\
            (2) of section 3121(d).\n\
            ## (2) Operating rules\n";
        let result = parse_irc_markdown(markdown);
        let section = &result.sections[0];
        assert_eq!(labels(section), ["(a)", "(b)", "(c)", "(c)(1)", "(c)(2)"]);
        assert!(section.subsections[1]
            .title
            .as_deref()
            .is_some_and(|title| title.starts_with("Repealed.") && !title.ends_with(']')));
        assert!(section.subsections[3]
            .content
            .ends_with("(2) of section 3121(d)."));
    }

    #[test]
    fn clause_lists_in_flush_text_are_clauses() {
        let markdown = "# §1402. Definitions\n\
            ## (a) Net earnings from self-employment\n\
            (13) there shall be excluded the distributive share of a limited partner;\n\
            If the taxable year of a partner is different, the share is based on the partnership year.\n\
            (i) in the case of an individual, the gross income may be deemed; or (ii) in any other case.\n\
            (v) in the case of any such trade or business, the gross receipts.\n\
            **(b) Self-employment income**\n";
        let result = parse_irc_markdown(markdown);
        assert_eq!(
            labels(&result.sections[0]),
            ["(a)", "(a)(13)", "(a)(13)(i)", "(a)(13)(v)", "(b)"]
        );
    }

    #[test]
    fn roman_numerals_follow_their_sequence() {
        assert_eq!(roman_value("iv"), Some(4));
        assert_eq!(roman_value("xiii"), Some(13));
        assert_eq!(roman_value("IX"), Some(9));
        assert_eq!(roman_value("aa"), None);
        let mut stack: [Option<String>; LEVELS] = Default::default();
        stack[SUBSECTION] = Some("u".into());
        stack[PARAGRAPH] = Some("1".into());
        stack[SUBPARAGRAPH] = Some("A".into());
        stack[CLAUSE] = Some("iv".into());
        // (v) continues both (u) and (iv); a lead-in marks the clause list.
        assert_eq!(level_for("v", &stack, true, false), Some(CLAUSE));
        assert_eq!(level_for("v", &stack, false, false), Some(SUBSECTION));
        assert_eq!(level_for("vi", &stack, false, false), Some(CLAUSE));
        assert_eq!(level_for("aa", &stack, false, false), Some(ITEM));
        assert_eq!(level_for("B", &stack, false, false), Some(SUBPARAGRAPH));
        assert_eq!(level_for("I", &stack, false, false), Some(SUBCLAUSE));
        assert_eq!(level_for("AA", &stack, false, false), Some(SUBITEM));
        assert_eq!(level_for("ab", &stack, false, false), None);
    }
}
