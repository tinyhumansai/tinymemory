//! Detection rules for the local PII detector.
//!
//! Two flavours of rule, both compiled once into a process-wide static:
//!
//! * **Pattern rules** — a [`Regex`] plus an optional structural `validator`
//!   (Luhn and complete-card grouping for cards, octet range for IPv4) that
//!   rejects shape-matches that aren't actually valid identifiers.
//! * **Keyword rules** — an alternation of domain terms wrapped in word
//!   boundaries, used for the topical categories (medical / legal / financial /
//!   family) where there is no single canonical value shape.
//!
//! All matching is case-insensitive and offline; nothing here performs I/O.

use std::sync::LazyLock;

use regex::Regex;

use super::types::PiiCategory;

/// A single detection rule: matches [`regex`](Rule::regex), attributes hits to
/// [`category`](Rule::category), and — if present — only counts a match when
/// [`validator`](Rule::validator) confirms it.
pub(crate) struct Rule {
    pub category: PiiCategory,
    pub regex: Regex,
    /// Optional structural check applied to each raw match before it counts.
    pub validator: Option<fn(&str) -> bool>,
}

/// Compile a built-in rule, all of which are exercised by safety fixtures.
///
/// # Panics
/// Panics on a malformed built-in pattern, which is a developer error.
#[allow(
    clippy::expect_used,
    reason = "only built-in patterns are passed and the safety fixtures compile every rule"
)]
fn compile_builtin(pattern: &str) -> Regex {
    Regex::new(pattern).expect("a built-in identification-risk rule is a valid regex")
}

/// Build a keyword rule: a case-insensitive, word-boundaried alternation over
/// `terms`, all attributed to `category`.
fn keyword_rule(category: PiiCategory, terms: &[&str]) -> Rule {
    let alternation = terms.join("|");
    let pattern = format!(r"(?i)\b(?:{alternation})\b");
    Rule {
        category,
        regex: compile_builtin(&pattern),
        validator: None,
    }
}

/// Build a pattern rule from a raw regex string.
fn pattern_rule(category: PiiCategory, pattern: &str, validator: Option<fn(&str) -> bool>) -> Rule {
    Rule {
        category,
        regex: compile_builtin(pattern),
        validator,
    }
}

/// Luhn checksum validation for candidate payment-card numbers. Strips
/// separators first; requires 13–19 digits.
pub(crate) fn is_luhn_valid(raw: &str) -> bool {
    let digits: Vec<u8> = raw
        .bytes()
        .filter(|b| b.is_ascii_digit())
        .map(|b| b - b'0')
        .collect();
    if !(13..=19).contains(&digits.len()) {
        return false;
    }
    let mut sum = 0u32;
    // Double every second digit from the right.
    for (i, &d) in digits.iter().rev().enumerate() {
        let mut v = d as u32;
        if i % 2 == 1 {
            v *= 2;
            if v > 9 {
                v -= 9;
            }
        }
        sum += v;
    }
    sum.is_multiple_of(10)
}

/// Accept a run of digits and single separators as a payment card only when
/// it is complete as written: either a PAN of 13–19 digits that passes Luhn,
/// or such a PAN followed by one adjacent group of 3–4 digits after the last
/// separator, the way a CVV is written next to the card it belongs to. The
/// run is judged as a unit — the whole run first, then the split at its last
/// separator — so a Luhn-valid head of something longer is not a card, and
/// an unlabelled CVV is set beside the PAN rather than folded into it.
pub(crate) fn is_complete_card(raw: &str) -> bool {
    if is_luhn_valid(raw) {
        return true;
    }
    let Some((pan, cvv)) = raw.rsplit_once([' ', '-']) else {
        return false;
    };
    (3..=4).contains(&cvv.len()) && cvv.bytes().all(|b| b.is_ascii_digit()) && is_luhn_valid(pan)
}

/// Validate that a dotted-quad match is a real IPv4 address (each octet 0–255).
pub(crate) fn is_ipv4(raw: &str) -> bool {
    let octets: Vec<&str> = raw.split('.').collect();
    octets.len() == 4
        && octets
            .iter()
            .all(|o| !o.is_empty() && o.parse::<u32>().is_ok_and(|n| n <= 255))
}

/// The full, process-wide rule set. Compiled lazily on first [`scan`] call and
/// reused for the life of the process.
///
/// [`scan`]: super::detector::scan
pub(crate) fn rules() -> &'static [Rule] {
    static RULES: LazyLock<Vec<Rule>> = LazyLock::new(build_rules);
    &RULES
}

fn build_rules() -> Vec<Rule> {
    vec![
        // --- structured identifiers -------------------------------------
        // Email.
        pattern_rule(
            PiiCategory::Email,
            r"\b[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}\b",
            None,
        ),
        // US Social Security Number (3-2-4, dash or space separated). The
        // separator requirement keeps it distinct from phone numbers (3-3-4)
        // and from bare 9-digit blobs.
        pattern_rule(
            PiiCategory::NationalId,
            r"\b\d{3}[-\s]\d{2}[-\s]\d{4}\b",
            None,
        ),
        // Payment card: a run of 13 or more digits with at most one space or
        // hyphen between any two of them. The pattern has no upper bound on
        // purpose: it consumes the whole run so the validator can judge it as
        // a unit — a complete 13–19 digit PAN that passes Luhn, or one with a
        // single adjacent 3–4 digit CVV group after its last separator — and
        // an overlong run is left alone rather than matched on a Luhn-valid
        // head that happens to end at a separator. Redaction replaces the
        // whole span, CVV group included.
        pattern_rule(
            PiiCategory::CreditCard,
            r"\b\d(?:[ -]?\d){12,}\b",
            Some(is_complete_card),
        ),
        // IBAN bank account: 2-letter country + 2 check digits + 11–30 alnum.
        pattern_rule(
            PiiCategory::BankAccount,
            r"\b[A-Z]{2}\d{2}[A-Z0-9]{11,30}\b",
            None,
        ),
        // US-style phone number (3-3-4 groups, requires a separator so it
        // doesn't swallow arbitrary 10-digit numbers).
        pattern_rule(
            PiiCategory::PhoneNumber,
            r"\b(?:\+?1[-.\s]?)?\(?\d{3}\)?[-.\s]\d{3}[-.\s]\d{4}\b",
            None,
        ),
        // Legacy international phone-like pattern. Its repetition counts
        // separators too; normalized digit bounds remain follow-up #263.
        pattern_rule(PiiCategory::PhoneNumber, r"\+\d(?:[\d\s\-]{6,16}\d)", None),
        // IPv4 address (octet-validated).
        pattern_rule(
            PiiCategory::IpAddress,
            r"\b(?:\d{1,3}\.){3}\d{1,3}\b",
            Some(is_ipv4),
        ),
        // Street / postal address (number + words + street-type keyword).
        pattern_rule(
            PiiCategory::PostalAddress,
            r"(?i)\b\d{1,6}\s+[A-Za-z0-9.'\- ]{2,40}?\b(?:street|st|avenue|ave|road|rd|boulevard|blvd|lane|ln|drive|dr|court|ct|way|place|pl|terrace|ter|circle|cir|highway|hwy)\b",
            None,
        ),
        // Date of birth (explicit context — pattern-based, not a keyword list).
        pattern_rule(
            PiiCategory::DateOfBirth,
            r"(?i)\b(?:date of birth|d\.?o\.?b\.?|birth ?date|born on)\b",
            None,
        ),
        // --- sensitive topical context (keyword-based) ------------------
        keyword_rule(PiiCategory::Passport, &["passport"]),
        keyword_rule(
            PiiCategory::Medical,
            &[
                "patient",
                "diagnosis",
                "diagnosed",
                "prescription",
                "prescribed",
                "symptom",
                "symptoms",
                "medication",
                "medications",
                "treatment",
                "physician",
                "hospital",
                "clinic",
                "disease",
                "medical record",
                "health record",
                "mrn",
                "blood pressure",
                "dosage",
                "therapy",
                "biopsy",
                "oncology",
                "cardiology",
                "psychiatric",
                "mental health",
                "allergy",
                "allergies",
            ],
        ),
        keyword_rule(
            PiiCategory::Legal,
            &[
                "plaintiff",
                "defendant",
                "attorney",
                "lawsuit",
                "litigation",
                "case number",
                "docket",
                "subpoena",
                "settlement",
                "indictment",
                "deposition",
                "legal counsel",
                "prosecution",
                "testimony",
                "affidavit",
                "arraignment",
                "felony",
                "misdemeanor",
                "custody",
                "court order",
            ],
        ),
        keyword_rule(
            PiiCategory::Financial,
            &[
                "bank account",
                "account number",
                "routing number",
                "salary",
                "annual income",
                "credit score",
                "tax id",
                "invoice",
                "sort code",
                "swift code",
                "wire transfer",
                "mortgage",
                "loan balance",
                "net worth",
                "iban",
            ],
        ),
        keyword_rule(
            PiiCategory::Family,
            &[
                "spouse",
                "wife",
                "husband",
                "daughter",
                "son",
                "children",
                "minor child",
                "maiden name",
                "next of kin",
                "dependent",
                "marital status",
                "guardian",
                "sibling",
            ],
        ),
    ]
}
