//! Removes personal data from text before the text is stored or sent to the LLM.
//!
//! The filter works on patterns. It finds email addresses, phone numbers and the
//! other listed forms. It does not find names or postal addresses.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The start of every placeholder that replaces removed text.
pub const PLACEHOLDER_PREFIX: &str = "[REDACTED:";

const MAX_CUSTOM_PATTERNS: usize = 50;
const MAX_PATTERN_LEN: usize = 500;
/// The compiled size limit of one custom pattern, in bytes.
const MAX_COMPILED_SIZE: usize = 1 << 20;

/// A pattern the owner adds. Text that matches it is removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomPattern {
    /// Shown in the placeholder: `[REDACTED:<label>]`.
    pub label: String,
    /// A regular expression in the syntax of the Rust `regex` crate.
    pub pattern: String,
}

/// What the privacy filter of one profile removes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivacyConfig {
    pub email: bool,
    pub phone: bool,
    /// Korean resident registration numbers.
    pub resident_number: bool,
    /// Payment card numbers. A number counts only if its check digit is right.
    pub card_number: bool,
    pub custom: Vec<CustomPattern>,
}

impl Default for PrivacyConfig {
    /// Every built-in kind is on, so a profile filters before its owner looks at the settings.
    fn default() -> Self {
        Self {
            email: true,
            phone: true,
            resident_number: true,
            card_number: true,
            custom: Vec::new(),
        }
    }
}

const EMAIL: &str = r"[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}";
// Korean numbers with a mobile, area or service prefix (or +82), and other numbers in
// international form. A number that only starts with 0, such as an order number, is not a match.
const PHONE: &str = r"(?x)
    (?: \+82[-.\s]?\d{1,2} | \b(?: 01[016789] | 02 | 0[3-6][1-5] | 070 | 080 | 050\d? ) )
        [-.\s]? \d{3,4} [-.\s]? \d{4} \b
  | \+\d{1,3} [-.\s]? \(?\d{1,4}\)? (?: [-.\s]? \d{2,4} ){2,3} \b";
const RESIDENT_NUMBER: &str = r"\b\d{6}[-\s]?[1-8]\d{6}\b";
const CARD_NUMBER: &str = r"\b\d(?:[ -]?\d){12,18}\b";

/// True if the digits of `text` pass the Luhn check that payment card numbers use.
fn luhn_valid(text: &str) -> bool {
    let digits: Vec<u32> = text.chars().filter_map(|c| c.to_digit(10)).collect();
    let sum: u32 = digits
        .iter()
        .rev()
        .enumerate()
        .map(|(i, &d)| {
            if i % 2 == 1 {
                if d > 4 { d * 2 - 9 } else { d * 2 }
            } else {
                d
            }
        })
        .sum();
    (13..=19).contains(&digits.len()) && sum.is_multiple_of(10)
}

struct Rule {
    label: String,
    regex: Regex,
    /// An extra test a match must pass.
    accept: Option<fn(&str) -> bool>,
}

/// The compiled filter of one profile.
pub struct Redactor {
    rules: Vec<Rule>,
}

/// How many pieces of text were removed, by label. It never holds the removed text.
pub type RedactionCounts = BTreeMap<String, u32>;

impl Redactor {
    /// Compiles the settings. The error says which custom pattern is wrong.
    pub fn new(config: &PrivacyConfig) -> Result<Self, String> {
        let built_in = |label: &str, pattern: &str, accept| Rule {
            label: label.to_owned(),
            regex: Regex::new(pattern).expect("a built-in pattern compiles"),
            accept,
        };
        let mut rules = Vec::new();
        if config.email {
            rules.push(built_in("EMAIL", EMAIL, None));
        }
        if config.resident_number {
            rules.push(built_in("RESIDENT_NUMBER", RESIDENT_NUMBER, None));
        }
        if config.card_number {
            rules.push(built_in(
                "CARD_NUMBER",
                CARD_NUMBER,
                Some(luhn_valid as fn(&str) -> bool),
            ));
        }
        if config.phone {
            rules.push(built_in("PHONE", PHONE, None));
        }
        if config.custom.len() > MAX_CUSTOM_PATTERNS {
            return Err(format!("at most {MAX_CUSTOM_PATTERNS} custom patterns are allowed"));
        }
        for custom in &config.custom {
            let label = custom.label.trim();
            let label_ok = !label.is_empty()
                && label.len() <= 40
                && label
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == ' ');
            if !label_ok {
                return Err(format!(
                    "the label `{}` is invalid: use 1 to 40 letters, digits, spaces, `_` or `-`",
                    custom.label
                ));
            }
            if custom.pattern.is_empty() || custom.pattern.len() > MAX_PATTERN_LEN {
                return Err(format!(
                    "the pattern of `{label}` must have 1 to {MAX_PATTERN_LEN} characters"
                ));
            }
            let regex = regex::RegexBuilder::new(&custom.pattern)
                .size_limit(MAX_COMPILED_SIZE)
                .build()
                .map_err(|e| format!("the pattern of `{label}` is not a valid regular expression: {e}"))?;
            // Such a pattern would put a placeholder between every two characters.
            if regex.is_match("") {
                return Err(format!("the pattern of `{label}` matches empty text"));
            }
            rules.push(Rule {
                label: label.to_owned(),
                regex,
                accept: None,
            });
        }
        Ok(Self { rules })
    }

    /// Replaces every match with `[REDACTED:<label>]` and adds to `counts`.
    ///
    /// All rules look at the original text. Where two matches overlap, the one that
    /// starts first wins, then the rule that comes first. A placeholder that is already
    /// in the text is never changed, so redacting twice gives the same text.
    pub fn redact(&self, text: &str, counts: &mut RedactionCounts) -> String {
        let placeholders: Vec<(usize, usize)> = text
            .match_indices(PLACEHOLDER_PREFIX)
            .filter_map(|(start, _)| text[start..].find(']').map(|len| (start, start + len + 1)))
            .collect();
        let mut matches: Vec<(usize, usize, usize)> = Vec::new();
        for (index, rule) in self.rules.iter().enumerate() {
            for found in rule.regex.find_iter(text) {
                let inside_placeholder = placeholders.iter().any(|(s, e)| found.start() < *e && *s < found.end());
                if !inside_placeholder && rule.accept.is_none_or(|accept| accept(found.as_str())) {
                    matches.push((found.start(), index, found.end()));
                }
            }
        }
        matches.sort_unstable();

        let mut out = String::with_capacity(text.len());
        let mut done = 0;
        for (start, index, end) in matches {
            if start < done {
                continue;
            }
            let label = &self.rules[index].label;
            out.push_str(&text[done..start]);
            out.push_str(PLACEHOLDER_PREFIX);
            out.push_str(label);
            out.push(']');
            *counts.entry(label.clone()).or_default() += 1;
            done = end;
        }
        out.push_str(&text[done..]);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redact_with(config: &PrivacyConfig, text: &str) -> (String, RedactionCounts) {
        let mut counts = RedactionCounts::new();
        let out = Redactor::new(config).unwrap().redact(text, &mut counts);
        (out, counts)
    }

    fn redact(text: &str) -> String {
        redact_with(&PrivacyConfig::default(), text).0
    }

    fn custom(label: &str, pattern: &str) -> PrivacyConfig {
        PrivacyConfig {
            custom: vec![CustomPattern {
                label: label.into(),
                pattern: pattern.into(),
            }],
            ..PrivacyConfig::default()
        }
    }

    #[test]
    fn emails_are_removed() {
        let cases = [
            ("mail kim.lee+test@corp.example.co.kr now", "mail [REDACTED:EMAIL] now"),
            (
                "<mailto:kim@example.com|kim@example.com>",
                "<mailto:[REDACTED:EMAIL]|[REDACTED:EMAIL]>",
            ),
            ("담당자(kim_01@example.com)에게", "담당자([REDACTED:EMAIL])에게"),
            ("a@b.io,c@d.org", "[REDACTED:EMAIL],[REDACTED:EMAIL]"),
        ];
        for (input, expected) in cases {
            assert_eq!(redact(input), expected, "{input}");
        }
    }

    #[test]
    fn phone_numbers_are_removed() {
        for number in [
            "010-1234-5678",
            "01012345678",
            "010 1234 5678",
            "010.1234.5678",
            "011-234-5678",
            "02-123-4567",
            "02-1234-5678",
            "031-123-4567",
            "0212345678",
            "+82 10-1234-5678",
            "+82-10-1234-5678",
            "+821012345678",
            "+82 2 123 4567",
            "+1 415-555-2671",
            "+1 (415) 555-2671",
            "+44 20 7946 0958",
        ] {
            assert_eq!(
                redact(&format!("call {number} today")),
                "call [REDACTED:PHONE] today",
                "{number}"
            );
        }
    }

    #[test]
    fn work_text_that_only_looks_like_personal_data_stays() {
        for text in [
            "Slack ts 1700000000.000100 in thread 1700000123.456789",
            "PROJ-1234 and OPS-20261114 are blocked",
            "released on 2026-11-14 at 09:30:15",
            "version 0.12.3456 and build 10.0.19045.3803",
            "server 192.168.10.25 port 8080, 10.0.0.5:9000",
            "took 0.0123 s, 1234567 rows, 0.5% of 20000000",
            "order 000123456 and invoice 2026-000123",
            "commit 4a0c3411f2 and id 1234567890123456",
            "user <@U012AB3CD> in <#C0123456789|general>",
            "kim at example dot com",
            "https://example.com/a@b/path",
        ] {
            assert_eq!(redact(text), text);
        }
    }

    #[test]
    fn resident_numbers_are_removed() {
        assert_eq!(
            redact("주민번호 900101-1234567 입니다"),
            "주민번호 [REDACTED:RESIDENT_NUMBER] 입니다"
        );
        assert_eq!(redact("9001011234567"), "[REDACTED:RESIDENT_NUMBER]");
        // The seventh digit of a resident number is 1 to 8.
        assert_eq!(redact("900101-9234567"), "900101-9234567");
    }

    #[test]
    fn card_numbers_are_removed_only_with_a_valid_check_digit() {
        assert_eq!(redact("card 4539 1488 0343 6467 ok"), "card [REDACTED:CARD_NUMBER] ok");
        assert_eq!(redact("4539-1488-0343-6467"), "[REDACTED:CARD_NUMBER]");
        assert_eq!(redact("4539148803436467"), "[REDACTED:CARD_NUMBER]");
        // The last digit is changed, so the check fails.
        assert_eq!(redact("4539 1488 0343 6468"), "4539 1488 0343 6468");
    }

    #[test]
    fn each_kind_can_be_turned_off() {
        let text = "kim@example.com 010-1234-5678 900101-1234567 4539148803436467";
        let off = PrivacyConfig {
            email: false,
            phone: false,
            resident_number: false,
            card_number: false,
            custom: vec![],
        };
        assert_eq!(redact_with(&off, text).0, text);
        let only_email = PrivacyConfig {
            email: true,
            ..off.clone()
        };
        assert_eq!(
            redact_with(&only_email, text).0,
            "[REDACTED:EMAIL] 010-1234-5678 900101-1234567 4539148803436467"
        );
        let only_phone = PrivacyConfig { phone: true, ..off };
        assert_eq!(
            redact_with(&only_phone, text).0,
            "kim@example.com [REDACTED:PHONE] 900101-1234567 4539148803436467"
        );
    }

    #[test]
    fn a_custom_pattern_removes_its_matches() {
        let config = custom("employee id", r"\bEMP-\d{6}\b");
        let (out, counts) = redact_with(&config, "EMP-004211 asked EMP-004212, mail kim@example.com");
        assert_eq!(
            out,
            "[REDACTED:employee id] asked [REDACTED:employee id], mail [REDACTED:EMAIL]"
        );
        assert_eq!(counts.get("employee id"), Some(&2));
        assert_eq!(counts.get("EMAIL"), Some(&1));
    }

    #[test]
    fn counts_hold_numbers_and_never_the_removed_text() {
        let (_, counts) = redact_with(
            &PrivacyConfig::default(),
            "kim@example.com, lee@example.com, 010-1234-5678",
        );
        assert_eq!(
            counts,
            RedactionCounts::from([("EMAIL".to_owned(), 2), ("PHONE".to_owned(), 1)])
        );
        let (_, none) = redact_with(&PrivacyConfig::default(), "nothing personal here");
        assert!(none.is_empty());
    }

    #[test]
    fn a_placeholder_is_not_redacted_again() {
        // A custom pattern that would match inside a placeholder leaves it whole,
        // in the same pass and in text that was redacted before.
        let config = custom("word", r"[A-Z]{5,}");
        let (out, _) = redact_with(&config, "kim@example.com SECRETWORD");
        assert_eq!(out, "[REDACTED:EMAIL] [REDACTED:word]");
        let (again, counts) = redact_with(&config, &out);
        assert_eq!(again, out);
        assert!(counts.is_empty());
        // Text that is already redacted stays the same.
        assert_eq!(
            redact(&redact("kim@example.com 010-1234-5678")),
            "[REDACTED:EMAIL] [REDACTED:PHONE]"
        );
    }

    #[test]
    fn overlapping_matches_give_one_placeholder() {
        // The custom pattern and the email rule both match the address.
        let config = custom("corp", r"\S+@corp\.example\S*");
        let (out, counts) = redact_with(&config, "to kim@corp.example.com please");
        assert_eq!(out.matches(PLACEHOLDER_PREFIX).count(), 1, "{out}");
        assert!(!out.contains("kim") && !out.contains("corp.example"), "{out}");
        assert_eq!(counts.values().sum::<u32>(), 1);
    }

    #[test]
    fn bad_custom_patterns_are_refused_with_the_reason() {
        let error = |label: &str, pattern: &str| Redactor::new(&custom(label, pattern)).err().unwrap();
        assert!(error("x", "(unclosed").contains("not a valid regular expression"));
        assert!(error("x", "a*").contains("matches empty text"));
        assert!(error("x", "").contains("1 to 500 characters"));
        assert!(error("x", &"a".repeat(501)).contains("1 to 500 characters"));
        assert!(error("", "abc").contains("label"));
        assert!(error("bad]label", "abc").contains("label"));
        // A pattern that would compile to a very large program is refused.
        assert!(error("x", r"(\w{1,100}){1,100}\d{1,1000}").contains("not a valid regular expression"));
        let many = PrivacyConfig {
            custom: (0..51)
                .map(|i| CustomPattern {
                    label: format!("p{i}"),
                    pattern: "abc".into(),
                })
                .collect(),
            ..PrivacyConfig::default()
        };
        assert!(Redactor::new(&many).err().unwrap().contains("at most 50"));
    }

    #[test]
    fn settings_from_an_older_version_get_the_defaults_per_caller() {
        // The type itself requires every field, so a typo in a field name is an error.
        assert!(serde_json::from_str::<PrivacyConfig>(r#"{"email": true}"#).is_err());
        assert!(
            serde_json::from_str::<PrivacyConfig>(
                r#"{"email":true,"phone":true,"resident_number":true,"card_number":true,"custom":[],"emial":false}"#
            )
            .is_err()
        );
        let full = serde_json::to_string(&PrivacyConfig::default()).unwrap();
        assert_eq!(
            serde_json::from_str::<PrivacyConfig>(&full).unwrap(),
            PrivacyConfig::default()
        );
    }
}
