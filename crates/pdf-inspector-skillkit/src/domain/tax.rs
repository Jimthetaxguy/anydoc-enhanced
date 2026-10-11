use regex::Regex;
use serde::Serialize;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaxFormType {
    W2,
    Form1099Int,
    Form1099Div,
    Form1099Misc,
    Form1099Nec,
    K1_1065,
    K1_1120S,
    Form1040,
    Form1065,
    Form1120,
    Form1120S,
    Form1099Composite,
    ScheduleA,
    ScheduleC,
    ScheduleD,
    ScheduleE,
    Unknown,
}

#[derive(Debug, Serialize)]
pub struct TaxFormIdentification {
    pub form_type: TaxFormType,
    pub confidence: f32,
    pub raw_match: String,
}

// All regexes are compile-time invariants — patterns are static literals,
// so a failure to compile is a programmer bug, not a runtime condition.
// `OnceLock` defers compilation to first use and caches the result for
// the program's lifetime, eliminating per-call regex construction.

macro_rules! static_regex {
    ($name:ident, $pattern:literal) => {
        fn $name() -> &'static Regex {
            static RE: OnceLock<Regex> = OnceLock::new();
            RE.get_or_init(|| {
                Regex::new($pattern).expect(concat!(
                    stringify!($name),
                    " regex must compile (compile-time invariant)"
                ))
            })
        }
    };
}

static_regex!(form_1099_composite_re, r"(?i)Form\s+1099\s+Composite");
static_regex!(w2_transcript_re, r"(?im)^#+\s*W-2\s+Transcript");
static_regex!(int_transcript_re, r"(?im)^#+\s*1099-INT\s+Transcript");
static_regex!(div_transcript_re, r"(?im)^#+\s*1099-DIV\s+Transcript");
static_regex!(misc_transcript_re, r"(?im)^#+\s*1099-MISC\s+Transcript");
static_regex!(nec_transcript_re, r"(?im)^#+\s*1099-NEC\s+Transcript");
static_regex!(k1_1065_re, r"(?i)Schedule\s+K-1.*Form\s+1065");
static_regex!(k1_1120s_re, r"(?i)Schedule\s+K-1.*Form\s+1120-S");
static_regex!(form_w2_re, r"(?i)Form\s+W-2");
static_regex!(wage_tax_statement_re, r"(?i)Wage\s+and\s+Tax\s+Statement");
static_regex!(form_1099_int_re, r"(?i)Form\s+1099-INT");
static_regex!(interest_income_re, r"(?i)Interest\s+Income");
static_regex!(form_1099_div_re, r"(?i)Form\s+1099-DIV");
static_regex!(
    dividends_distributions_re,
    r"(?i)Dividends\s+and\s+Distributions"
);
static_regex!(form_1099_misc_re, r"(?i)Form\s+1099-MISC");
static_regex!(form_1099_nec_re, r"(?i)Form\s+1099-NEC");
static_regex!(nonemployee_comp_re, r"(?i)Nonemployee\s+Compensation");
static_regex!(form_1040_re, r"(?i)Form\s+1040");
static_regex!(
    individual_income_re,
    r"(?i)U\.S\.\s*Individual\s+Income\s+Tax\s+Return"
);
static_regex!(form_1065_re, r"(?i)Form\s+1065");
static_regex!(
    partnership_income_re,
    r"(?i)U\.S\.\s*Return\s+of\s+Partnership\s+Income"
);
static_regex!(form_1120s_re, r"(?i)Form\s+1120-S");
static_regex!(
    s_corp_income_re,
    r"(?i)U\.S\.\s*Income\s+Tax\s+Return\s+for\s+an\s+S\s+Corporation"
);
static_regex!(form_1120_re, r"(?i)Form\s+1120");
static_regex!(
    c_corp_income_re,
    r"(?i)U\.S\.\s*Corporation\s+Income\s+Tax\s+Return"
);
static_regex!(schedule_a_re, r"(?i)Schedule\s+A.*Itemized\s+Deductions");
static_regex!(schedule_c_re, r"(?i)Schedule\s+C.*Profit\s+or\s+Loss");
static_regex!(schedule_d_re, r"(?i)Schedule\s+D.*Capital\s+Gains");
static_regex!(schedule_e_re, r"(?i)Schedule\s+E.*Supplemental\s+Income");

/// One row of the form-detection ladder. Order matters: the first
/// matching rule wins, so transcript headings (high specificity) come
/// before bare form-name mentions (lower specificity).
type Rule = (fn() -> &'static Regex, TaxFormType, f32);

const RULES: &[Rule] = &[
    (form_1099_composite_re, TaxFormType::Form1099Composite, 1.0),
    (w2_transcript_re, TaxFormType::W2, 0.95),
    (int_transcript_re, TaxFormType::Form1099Int, 0.95),
    (div_transcript_re, TaxFormType::Form1099Div, 0.95),
    (misc_transcript_re, TaxFormType::Form1099Misc, 0.95),
    (nec_transcript_re, TaxFormType::Form1099Nec, 0.95),
    (k1_1065_re, TaxFormType::K1_1065, 1.0),
    (k1_1120s_re, TaxFormType::K1_1120S, 1.0),
    (form_w2_re, TaxFormType::W2, 1.0),
    (wage_tax_statement_re, TaxFormType::W2, 0.9),
    (form_1099_int_re, TaxFormType::Form1099Int, 1.0),
    (interest_income_re, TaxFormType::Form1099Int, 0.7),
    (form_1099_div_re, TaxFormType::Form1099Div, 1.0),
    (dividends_distributions_re, TaxFormType::Form1099Div, 0.7),
    (form_1099_misc_re, TaxFormType::Form1099Misc, 1.0),
    (form_1099_nec_re, TaxFormType::Form1099Nec, 1.0),
    (nonemployee_comp_re, TaxFormType::Form1099Nec, 0.8),
    // Schedules (A/C/D/E) must be checked before the bare `form_1040_re`
    // rule: "SCHEDULE C (Form 1040) Profit or Loss From Business" contains
    // the text "Form 1040", so if the generic Form1040 rule ran first it
    // would win and schedule detection would be unreachable. Same reasoning
    // that already places the K-1 rules before the 1065/1120-S rules above.
    (schedule_a_re, TaxFormType::ScheduleA, 1.0),
    (schedule_c_re, TaxFormType::ScheduleC, 1.0),
    (schedule_d_re, TaxFormType::ScheduleD, 1.0),
    (schedule_e_re, TaxFormType::ScheduleE, 1.0),
    (form_1040_re, TaxFormType::Form1040, 1.0),
    (individual_income_re, TaxFormType::Form1040, 1.0),
    (form_1065_re, TaxFormType::Form1065, 1.0),
    (partnership_income_re, TaxFormType::Form1065, 1.0),
    (form_1120s_re, TaxFormType::Form1120S, 1.0),
    (s_corp_income_re, TaxFormType::Form1120S, 1.0),
    (form_1120_re, TaxFormType::Form1120, 1.0),
    (c_corp_income_re, TaxFormType::Form1120, 1.0),
];

/// Floor `index` down to the nearest UTF-8 char boundary in `text`, capped
/// at `text.len()`. Used to truncate `text` for a byte-range slice without
/// risking a panic when the naive cut point lands inside a multi-byte
/// character (e.g. `text[..text.len().min(5000)]` when byte 5000 straddles
/// a multi-byte char such as '§' or '—').
fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut i = index.min(text.len());
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn match_rules(search_text: &str) -> TaxFormIdentification {
    for (re_fn, form_type, confidence) in RULES {
        if let Some(m) = re_fn().find(search_text) {
            return TaxFormIdentification {
                form_type: *form_type,
                confidence: *confidence,
                raw_match: m.as_str().to_string(),
            };
        }
    }
    TaxFormIdentification {
        form_type: TaxFormType::Unknown,
        confidence: 0.0,
        raw_match: String::new(),
    }
}

pub fn identify_tax_form(
    path: impl AsRef<std::path::Path>,
) -> Result<TaxFormIdentification, crate::SkillkitError> {
    let info = crate::process(path)?;
    Ok(identify_tax_form_markdown(
        info.markdown.as_deref().unwrap_or_default(),
    ))
}

/// Identify the form from Markdown already produced by `pdf_to_markdown`.
pub fn identify_tax_form_markdown(markdown: &str) -> TaxFormIdentification {
    let cut = floor_char_boundary(markdown, 5000);
    match_rules(&markdown[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn first_pattern_hit(text: &str) -> (TaxFormType, f32) {
        // Mirrors the production ladder so the 2026-04-15 validation
        // fixes (TurboTax transcript headings + Form 1099 Composite)
        // can be locked in without real PDFs.
        let cut = floor_char_boundary(text, 5000);
        let search_text = &text[..cut];
        let id = match_rules(search_text);
        (id.form_type, id.confidence)
    }

    #[test]
    fn turbotax_w2_transcript_heading_matches() {
        let md = "# W-2 Transcript\n\nDisclaimer: ...";
        assert_eq!(first_pattern_hit(md), (TaxFormType::W2, 0.95));
    }

    #[test]
    fn turbotax_1099_div_transcript_heading_matches() {
        let md = "# 1099-DIV Transcript\n\nDisclaimer: ...";
        assert_eq!(first_pattern_hit(md), (TaxFormType::Form1099Div, 0.95));
    }

    #[test]
    fn broker_form_1099_composite_matches() {
        let md = "Some preamble\n\n# FORM 1099 COMPOSITE & YEAR-END SUMMARY\n";
        assert_eq!(first_pattern_hit(md), (TaxFormType::Form1099Composite, 1.0));
    }

    #[test]
    fn entity_return_forms_match_before_generic_1120() {
        assert_eq!(
            first_pattern_hit("Form 1065\nU.S. Return of Partnership Income"),
            (TaxFormType::Form1065, 1.0)
        );
        assert_eq!(
            first_pattern_hit("Form 1120-S\nU.S. Income Tax Return for an S Corporation"),
            (TaxFormType::Form1120S, 1.0)
        );
        assert_eq!(
            first_pattern_hit("Form 1120\nU.S. Corporation Income Tax Return"),
            (TaxFormType::Form1120, 1.0)
        );
    }

    #[test]
    fn unknown_when_no_pattern_matches() {
        let md = "PAYEE NAME 1 ANY STREET\n0.00\n0.00";
        assert_eq!(first_pattern_hit(md), (TaxFormType::Unknown, 0.0));
    }

    #[test]
    fn schedule_c_matches_before_generic_form_1040() {
        assert_eq!(
            first_pattern_hit("SCHEDULE C (Form 1040) Profit or Loss From Business"),
            (TaxFormType::ScheduleC, 1.0)
        );
    }

    #[test]
    fn schedule_d_matches_before_generic_form_1040() {
        assert_eq!(
            first_pattern_hit("SCHEDULE D (Form 1040) Capital Gains and Losses"),
            (TaxFormType::ScheduleD, 1.0)
        );
    }

    #[test]
    fn schedule_a_matches_before_generic_form_1040() {
        assert_eq!(
            first_pattern_hit("SCHEDULE A (Form 1040) Itemized Deductions"),
            (TaxFormType::ScheduleA, 1.0)
        );
    }

    #[test]
    fn schedule_e_matches_before_generic_form_1040() {
        assert_eq!(
            first_pattern_hit("SCHEDULE E (Form 1040) Supplemental Income and Loss"),
            (TaxFormType::ScheduleE, 1.0)
        );
    }

    #[test]
    fn does_not_panic_when_multibyte_char_straddles_byte_5000() {
        // Build a >5000-byte string where a 2-byte UTF-8 char ('§') occupies
        // bytes 4999-5000, so byte offset 5000 lands mid-character and
        // `text[..5000]` (the old, unsafe cut) would panic. The match itself
        // happens on the marker within the first 4999 bytes so we can also
        // assert correct classification survives the truncation.
        let marker = "Form 1099-NEC\n";
        let pad_len = 4999usize.saturating_sub(marker.len());
        let mut md = String::new();
        md.push_str(marker);
        md.push_str(&"a".repeat(pad_len));
        md.push('§');
        md.push_str(&"b".repeat(200));

        // Sanity-check the fixture actually exercises the boundary bug.
        assert!(md.len() > 5000);
        assert!(!md.is_char_boundary(5000));

        // Must not panic, and the earlier marker must still be found.
        assert_eq!(first_pattern_hit(&md), (TaxFormType::Form1099Nec, 1.0));
    }

    #[test]
    fn floor_char_boundary_walks_back_to_valid_boundary() {
        let s = "a§b"; // 'a' (1 byte), '§' (2 bytes: idx 1-2), 'b' (1 byte, idx 3)
        assert_eq!(floor_char_boundary(s, 0), 0);
        assert_eq!(floor_char_boundary(s, 1), 1);
        assert_eq!(floor_char_boundary(s, 2), 1); // mid-'§' floors back to 1
        assert_eq!(floor_char_boundary(s, 3), 3);
        assert_eq!(floor_char_boundary(s, 100), s.len()); // capped at text.len()
    }
}
