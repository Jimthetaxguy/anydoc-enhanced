use regex::Regex;
use serde::Serialize;
use std::sync::OnceLock;

use crate::SkillkitError;

#[derive(Debug, Clone, Serialize)]
pub struct SecSection {
    pub name: String,
    pub item_number: String,
    pub content: String,
    pub char_offset: usize,
    pub char_length: usize,
}

// SEC filing structural patterns are static literals — compile once,
// reuse across every call. The `expect` only fires if the pattern source
// itself is malformed, which is a programmer error caught at first use.

fn part_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)^\s*#{0,4}\s*\*{0,2}\s*PART\s+([IVX]+)\s*\*{0,2}\s*$")
            .expect("PART regex must compile (compile-time invariant)")
    })
}

fn item_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)^\s*#{0,4}\s*\*{0,2}\s*ITEM\s+(?P<num>\d+[A-Z]?)\s*\.?\s*(?P<title>.*)")
            .expect("ITEM regex must compile (compile-time invariant)")
    })
}

pub fn split_sec_filing(
    path: impl AsRef<std::path::Path>,
) -> Result<Vec<SecSection>, SkillkitError> {
    let info = crate::process(&path)?;
    Ok(split_sec_markdown(
        info.markdown.as_deref().unwrap_or_default(),
    ))
}

/// Split Markdown already produced by `pdf_to_markdown` into filing sections.
pub fn split_sec_markdown(text: &str) -> Vec<SecSection> {
    if text.is_empty() {
        return vec![];
    }

    let part_re = part_re();
    let item_re = item_re();

    let mut sections = Vec::new();
    let mut current_offset = 0;
    let mut current_name = String::new();
    let mut current_item_num = String::new();
    let mut current_content = String::new();
    let mut in_section = false;

    let line_endoffsets: Vec<usize> = text.match_indices('\n').map(|(i, _)| i).collect();

    for (line_num, line) in text.lines().enumerate() {
        let line_offset = if line_num == 0 {
            0
        } else {
            line_endoffsets
                .get(line_num - 1)
                .map(|&o| o + 1)
                .unwrap_or(0)
        };

        if let Some(caps) = part_re.captures(line) {
            if in_section && !current_content.trim().is_empty() {
                sections.push(SecSection {
                    name: current_name.clone(),
                    item_number: current_item_num.clone(),
                    content: current_content.trim().to_string(),
                    char_offset: current_offset,
                    char_length: current_content.trim().len(),
                });
            }

            current_offset = line_offset;
            current_name = caps[0].trim().to_string();
            current_item_num = format!("PART_{}", &caps[1]);
            current_content = line.to_string();
            current_content.push('\n');
            in_section = true;
        } else if let Some(caps) = item_re.captures(line) {
            if in_section && !current_content.trim().is_empty() {
                sections.push(SecSection {
                    name: current_name.clone(),
                    item_number: current_item_num.clone(),
                    content: current_content.trim().to_string(),
                    char_offset: current_offset,
                    char_length: current_content.trim().len(),
                });
            }

            current_offset = line_offset;
            let num = &caps["num"];
            let title = caps
                .name("title")
                .map(|m| m.as_str().trim())
                .unwrap_or("")
                .to_string();
            current_item_num = num.to_string();
            current_name = if title.is_empty() {
                format!("Item {}", num)
            } else {
                format!("Item {} - {}", num, title)
            };
            current_content = line.to_string();
            current_content.push('\n');
            in_section = true;
        } else if in_section {
            current_content.push_str(line);
            current_content.push('\n');
        }
    }

    if in_section && !current_content.trim().is_empty() {
        sections.push(SecSection {
            name: current_name,
            item_number: current_item_num,
            content: current_content.trim().to_string(),
            char_offset: current_offset,
            char_length: current_content.trim().len(),
        });
    }

    sections
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_nonexistent_file_returns_error() {
        let result = super::split_sec_filing("/nonexistent/file.pdf");
        // Should fail with FileNotFound since the file doesn't exist
        assert!(result.is_err());
    }

    #[test]
    fn item_regex_matches_markdown_h2_heading() {
        // 2026-04-15 validation finding: Apple 10-K rendered as
        // `## Item 1.    Business`. Pre-fix regex required `*` or empty
        // prefix and missed all H2-prefixed items.
        let caps = item_re().captures("## Item 1.    Business").unwrap();
        assert_eq!(&caps["num"], "1");
        assert_eq!(caps["title"].trim(), "Business");
    }

    #[test]
    fn item_regex_matches_bold_inline_heading() {
        let caps = item_re().captures("**Item 1A.    Risk Factors**").unwrap();
        assert_eq!(&caps["num"], "1A");
    }

    #[test]
    fn item_regex_does_not_match_table_of_contents_row() {
        // TOC rows like `|Item 1.|Business|1|` start with `|`, which is
        // not in the allowed prefix set, so they must not match.
        assert!(item_re().captures("|Item 1.|Business|1|").is_none());
    }

    #[test]
    fn part_regex_matches_markdown_heading() {
        assert!(part_re().is_match("## PART I"));
        assert!(part_re().is_match("PART II"));
        assert!(part_re().is_match("**PART III**"));
    }
}
