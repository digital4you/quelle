//! Deterministic last line of defence before a query leaves for a paid,
//! external search engine. The planner is already told to keep personal
//! details and secrets out of search terms; these rules make that hard for
//! identifiers with a recognisable shape. Checksummed identifiers are only
//! removed when the checksum holds, which keeps false positives rare.

use std::sync::LazyLock;

use regex::Regex;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Finding {
    Secret,
    Email,
    Ahv,
    Iban,
    Card,
    Phone,
}

impl Finding {
    pub fn code(self) -> &'static str {
        match self {
            Self::Secret => "secret",
            Self::Email => "email",
            Self::Ahv => "ahv",
            Self::Iban => "iban",
            Self::Card => "card",
            Self::Phone => "phone",
        }
    }
}

struct Rule {
    finding: Finding,
    pattern: Regex,
    valid: fn(&str) -> bool,
}

fn always(_: &str) -> bool {
    true
}

static RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    let rule = |finding, pattern: &str, valid| Rule {
        finding,
        pattern: Regex::new(pattern).expect("privacy pattern"),
        valid,
    };
    // Order matters: secrets and emails first so their digits are not
    // reported as phone or card numbers.
    vec![
        rule(
            Finding::Secret,
            concat!(
                r"-----BEGIN [A-Z ]+-----",
                r"|\b(?:sk|pk|rk)-[A-Za-z0-9_-]{16,}",
                r"|\b(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{20,}|\bgithub_pat_[A-Za-z0-9_]{20,}",
                r"|\bglpat-[A-Za-z0-9_-]{20,}",
                r"|\bxox[abprs]-[A-Za-z0-9-]{10,}",
                r"|\b(?:AKIA|ASIA)[0-9A-Z]{16}\b",
                r"|\bAIza[0-9A-Za-z_-]{35}",
                r"|\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
                r"|(?i:\b(?:password|passwort|passwd|pwd|kennwort|token|api[_-]?key|secret)\s*[:=]\s*\S+)",
            ),
            always,
        ),
        rule(Finding::Secret, r"\b[A-Za-z0-9_+/=-]{32,}\b", high_entropy),
        rule(
            Finding::Email,
            r"(?i)\b[a-z0-9._%+-]+@[a-z0-9-]+(?:\.[a-z0-9-]+)*\.[a-z]{2,}\b",
            always,
        ),
        rule(
            Finding::Ahv,
            r"\b756[ .]?\d{4}[ .]?\d{4}[ .]?\d{2}\b",
            ahv_valid,
        ),
        rule(
            Finding::Iban,
            r"\b[A-Z]{2}\d{2}(?: ?[A-Z0-9]){11,30}\b",
            iban_valid,
        ),
        rule(
            Finding::Card,
            r"\b[2-6]\d{3}(?:[ -]?\d){9,15}\b",
            card_valid,
        ),
        rule(
            Finding::Phone,
            concat!(
                r"(?:\+|\b00)\d{2,3}[ ./-]?(?:\(0\)[ ./-]?)?\d{1,3}(?:[ ./-]?\d){6,9}\b",
                r"|\b0[1-9]\d(?:[ ./-]?\d){7}\b",
            ),
            always,
        ),
    ]
});

fn digits(text: &str) -> Vec<u32> {
    text.chars().filter_map(|c| c.to_digit(10)).collect()
}

/// Swiss social security number: EAN-13 check digit.
fn ahv_valid(text: &str) -> bool {
    let digits = digits(text);
    if digits.len() != 13 {
        return false;
    }
    let sum: u32 = digits[..12]
        .iter()
        .enumerate()
        .map(|(index, digit)| if index % 2 == 0 { *digit } else { digit * 3 })
        .sum();
    (10 - sum % 10) % 10 == digits[12]
}

/// ISO 13616 mod-97 check.
fn iban_valid(text: &str) -> bool {
    let compact = text
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_uppercase();
    if !(15..=34).contains(&compact.len()) {
        return false;
    }
    let (head, tail) = compact.split_at(4);
    let mut remainder = 0u32;
    for c in tail.chars().chain(head.chars()) {
        let value = match c {
            '0'..='9' => c as u32 - '0' as u32,
            'A'..='Z' => c as u32 - 'A' as u32 + 10,
            _ => return false,
        };
        let width = if value >= 10 { 100 } else { 10 };
        remainder = (remainder * width + value) % 97;
    }
    remainder == 1
}

/// Payment card: 13 to 19 digits passing the Luhn check.
fn card_valid(text: &str) -> bool {
    let digits = digits(text);
    if !(13..=19).contains(&digits.len()) {
        return false;
    }
    let sum: u32 = digits
        .iter()
        .rev()
        .enumerate()
        .map(|(index, digit)| {
            if index % 2 == 1 {
                let doubled = digit * 2;
                if doubled > 9 { doubled - 9 } else { doubled }
            } else {
                *digit
            }
        })
        .sum();
    sum.is_multiple_of(10)
}

/// Long tokens mixing upper case, lower case and digits look like keys;
/// ordinary words, hyphenated compounds and hex hashes rarely do.
fn high_entropy(text: &str) -> bool {
    let upper = text.chars().any(|c| c.is_ascii_uppercase());
    let lower = text.chars().any(|c| c.is_ascii_lowercase());
    let digit_count = text.chars().filter(char::is_ascii_digit).count();
    upper && lower && digit_count >= 4
}

/// Removes recognisable personal identifiers and secrets from a search
/// query. Returns the cleaned text and what was removed, without values.
pub fn scrub(text: &str) -> (String, Vec<Finding>) {
    let mut text = text.to_owned();
    let mut findings = Vec::new();
    for rule in RULES.iter() {
        let mut found = false;
        let cleaned = rule
            .pattern
            .replace_all(&text, |capture: &regex::Captures<'_>| {
                if (rule.valid)(&capture[0]) {
                    found = true;
                    " ".to_owned()
                } else {
                    capture[0].to_owned()
                }
            })
            .into_owned();
        if found {
            findings.push(rule.finding);
            text = cleaned;
        }
    }
    findings.sort_unstable();
    findings.dedup();
    (
        text.split_whitespace().collect::<Vec<_>>().join(" "),
        findings,
    )
}
