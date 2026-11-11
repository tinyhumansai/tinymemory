//! Fixture + boundary tests for the local PII detector.
//!
//! Coverage intent:
//! * every category detects its canonical form,
//! * false-positive guards (benign text, invalid card/IP) do NOT flag,
//! * false-negative / recall guards (varied formats, in-sentence values) DO
//!   flag — recall matters more than precision here,
//! * risk-level escalation behaves as specified,
//! * detection is deterministic + offline (no external egress on classify),
//! * category identifiers serialize to stable snake_case strings.

use super::rules::{
    has_international_phone_digit_count, is_complete_card, is_iban, is_ipv4, is_luhn_valid,
};
use super::*;

// ---------------------------------------------------------------------------
// Per-category detection (recall / true positives)
// ---------------------------------------------------------------------------

#[test]
fn detects_email() {
    let r = scan("reach me at alice.smith@example.com any time");
    assert!(r.has_category(PiiCategory::Email));
    assert!(r.is_sensitive());
}

#[test]
fn detects_national_id_ssn() {
    // Dash- and space-separated both count (recall).
    for raw in ["SSN 123-45-6789", "ssn is 123 45 6789"] {
        let r = scan(raw);
        assert!(r.has_category(PiiCategory::NationalId), "missed in {raw:?}");
        assert_eq!(r.level, RiskLevel::High, "strong id must be High: {raw:?}");
    }
}

#[test]
fn detects_valid_credit_card() {
    // 4111 1111 1111 1111 is the canonical Visa test number (Luhn-valid).
    for raw in [
        "card 4111 1111 1111 1111 exp 12/26",
        "4111-1111-1111-1111",
        "4111111111111111",
    ] {
        let r = scan(raw);
        assert!(r.has_category(PiiCategory::CreditCard), "missed in {raw:?}");
        assert_eq!(r.level, RiskLevel::High);
    }
}

#[test]
fn detects_card_with_adjacent_cvv_group() {
    // A CVV is often written right after the card, with the same separator.
    // The run is PAN + CVV; the PAN still has to pass Luhn on its own.
    for raw in [
        "4111 1111 1111 1111 123",
        "4111-1111-1111-1111-123",
        "card 4111 1111 1111 1111 1234 exp 12/26",
    ] {
        let r = scan(raw);
        assert!(r.has_category(PiiCategory::CreditCard), "missed in {raw:?}");
        assert_eq!(r.level, RiskLevel::High, "{raw:?}");
        assert_eq!(
            r.hits,
            vec![CategoryHit {
                category: PiiCategory::CreditCard,
                count: 1
            }],
            "{raw:?}"
        );
    }
}

#[test]
fn detects_card_with_labelled_cvv() {
    // A label between the card and its CVV ends the run at the card, so the
    // PAN is judged on its own and counted once.
    let r = scan("card 4111 1111 1111 1111 cvv 123");
    assert_eq!(
        r.hits,
        vec![CategoryHit {
            category: PiiCategory::CreditCard,
            count: 1
        }]
    );
}

#[test]
fn detects_iban_bank_account() {
    let r = scan("transfer to GB82WEST12345698765432 please");
    assert!(r.has_category(PiiCategory::BankAccount));
    assert_eq!(r.level, RiskLevel::High);
}

#[test]
fn detects_grouped_and_lower_case_ibans() {
    // The groups of four an IBAN is ordinarily written in, with either
    // separator, and the lower-case spelling a form might hand back.
    for raw in [
        "GB82 WEST 1234 5698 7654 32",
        "GB82-WEST-1234-5698-7654-32",
        "gb82 west 1234 5698 7654 32",
        "DE89 3704 0044 0532 0130 00",
        "FR76 3000 6000 0112 3456 7890 189",
    ] {
        let r = scan(raw);
        assert!(
            r.has_category(PiiCategory::BankAccount),
            "missed {raw:?}: {r:?}"
        );
        assert_eq!(r.level, RiskLevel::High, "strong id must be High: {raw:?}");
    }
}

#[test]
fn an_iban_with_a_bad_checksum_is_not_a_bank_account() {
    for raw in [
        "GB82WEST12345698765433",
        "GB82 WEST 1234 5698 7654 33",
        "AB12CDEFGHIJKLMNOP",
    ] {
        let r = scan(raw);
        assert!(
            !r.has_category(PiiCategory::BankAccount),
            "flagged {raw:?}: {r:?}"
        );
    }
}

#[test]
fn iban_redaction_covers_the_whole_grouped_value() {
    assert_eq!(
        redact_identifiers("pay GB82 WEST 1234 5698 7654 32 today"),
        "pay [redacted] today"
    );
    assert_eq!(
        redact_identifiers("pay GB82WEST12345698765432 today"),
        "pay [redacted] today"
    );
    // The word after a grouped value is not swallowed into it.
    assert_eq!(
        redact_identifiers("pay DE89 3704 0044 0532 0130 00 today"),
        "pay [redacted] today"
    );
}

#[test]
fn detects_phone_numbers_varied_formats() {
    for raw in [
        "call 415-555-1234",
        "(415) 555 1234",
        "+1 415.555.1234",
        "reach +44 20 7946 0958 in london",
    ] {
        let r = scan(raw);
        assert!(
            r.has_category(PiiCategory::PhoneNumber),
            "missed in {raw:?}"
        );
    }
}

#[test]
fn international_phone_numbers_carry_seven_to_fifteen_digits() {
    for raw in [
        "+1234567",
        "+123456789012345",
        "+44 20 7946 0958",
        "+1-415-555-0100",
    ] {
        let r = scan(raw);
        assert!(r.has_category(PiiCategory::PhoneNumber), "missed {raw:?}");
    }
    for raw in [
        "+123456",
        "+1234567890123456",
        "+123456789012345678",
        "+1      2",
    ] {
        let r = scan(raw);
        assert!(
            !r.has_category(PiiCategory::PhoneNumber),
            "flagged {raw:?}: {r:?}"
        );
    }
}

#[test]
fn an_overlong_number_is_not_matched_on_a_prefix() {
    // 18 digits: no 15-digit (or shorter) head of it may count as a phone
    // number, so the value is reported clean and left as written.
    let text = "ref +123456789012345678 end";
    assert_eq!(scan(text).level, RiskLevel::None);
    assert_eq!(redact_identifiers(text), text);
}

#[test]
fn an_overlong_separated_number_is_not_matched_on_a_prefix() {
    // 16 digits with a separator after the fifteenth: the rule must consume
    // the whole run and reject it, not stop at the separator and accept the
    // 15-digit head.
    for text in [
        "ref +123456789012345-6 end",
        "ref +123456789012345 6 end",
        "ref +1-234-567-890-123-456 end",
    ] {
        assert_eq!(scan(text).level, RiskLevel::None, "flagged {text:?}");
        assert_eq!(redact_identifiers(text), text);
    }
}

#[test]
fn only_ascii_digits_make_up_an_international_phone_number() {
    // Seven ASCII digits followed by nine Arabic-Indic ones: 16 digits in one
    // run, past the 15-digit bound, so it is not a phone number.
    let text = "ref +1234567\u{661}\u{662}\u{663}\u{664}\u{665}\u{666}\u{667}\u{668}\u{669} end";
    assert!(
        !scan(text).has_category(PiiCategory::PhoneNumber),
        "flagged {text:?}"
    );
}

#[test]
fn international_phone_separators_do_not_span_lines_or_tabs() {
    // Only a space or a hyphen joins two digit groups. A newline or a tab
    // ends the candidate, so two short runs on separate lines are not glued
    // into one phone number.
    for text in ["+1234\n567", "+1234\t567", "+123 456\n789 012"] {
        assert!(
            !scan(text).has_category(PiiCategory::PhoneNumber),
            "flagged {text:?}"
        );
    }
}

#[test]
fn international_phone_redaction_covers_the_whole_number() {
    assert_eq!(
        redact_identifiers("call +44 20 7946 0958 now"),
        "call [redacted] now"
    );
    assert_eq!(
        redact_identifiers("call +123456789012345 now"),
        "call [redacted] now"
    );
}

#[test]
fn detects_ipv4_address() {
    let r = scan("client connected from 192.168.10.24 last night");
    assert!(r.has_category(PiiCategory::IpAddress));
}

#[test]
fn detects_postal_address() {
    let r = scan("ship to 1600 Pennsylvania Avenue, Washington");
    assert!(r.has_category(PiiCategory::PostalAddress));
}

#[test]
fn detects_date_of_birth_context() {
    for raw in [
        "date of birth: 1990-04-12",
        "DOB 04/12/1990",
        "he was born on the 3rd",
    ] {
        let r = scan(raw);
        assert!(
            r.has_category(PiiCategory::DateOfBirth),
            "missed in {raw:?}"
        );
    }
}

#[test]
fn detects_passport() {
    let r = scan("upload a scan of your passport");
    assert!(r.has_category(PiiCategory::Passport));
    assert_eq!(r.level, RiskLevel::High); // strong identifier
}

#[test]
fn detects_medical_context() {
    let r = scan("the patient was diagnosed and prescribed medication");
    assert!(r.has_category(PiiCategory::Medical));
    assert!(r.level >= RiskLevel::Medium);
}

#[test]
fn detects_legal_context() {
    let r = scan("the plaintiff filed a lawsuit; the defendant's attorney responded");
    assert!(r.has_category(PiiCategory::Legal));
    assert!(r.level >= RiskLevel::Medium);
}

#[test]
fn detects_financial_context() {
    let r = scan("please send your bank account and routing number");
    assert!(r.has_category(PiiCategory::Financial));
    assert!(r.level >= RiskLevel::Medium);
}

#[test]
fn detects_family_context() {
    let r = scan("my spouse and our minor child are dependents");
    assert!(r.has_category(PiiCategory::Family));
    assert!(r.is_sensitive());
}

// ---------------------------------------------------------------------------
// False-positive guards
// ---------------------------------------------------------------------------

#[test]
fn benign_text_is_not_flagged() {
    for raw in [
        "let's grab coffee at 3pm tomorrow",
        "the build passed and the tests are green",
        "refactor the parser to be faster",
        "version 1.2.3 shipped yesterday", // dotted, but only 3 octets — not IPv4
    ] {
        let r = scan(raw);
        assert_eq!(r.level, RiskLevel::None, "false positive on {raw:?}: {r:?}");
        assert!(r.categories.is_empty(), "categories on {raw:?}: {r:?}");
    }
}

#[test]
fn luhn_invalid_number_is_not_a_card() {
    // 16 digits but fails the Luhn checksum → must NOT be a CreditCard.
    let r = scan("order id 1234 5678 9012 3456 confirmed");
    assert!(
        !r.has_category(PiiCategory::CreditCard),
        "Luhn-invalid number flagged as card: {r:?}"
    );
}

#[test]
fn a_run_that_starts_with_a_card_is_a_card() {
    // A Luhn-valid PAN followed by more digits is still a card: the run is
    // flagged whole rather than passed over because it is longer than one.
    for raw in [
        "4111 1111 1111 1111 003-1",
        "4111 1111 1111 1111 1111 1111",
        "4111 1111 1111 1111 12",
    ] {
        let r = scan(raw);
        assert!(r.has_category(PiiCategory::CreditCard), "missed in {raw:?}");
        assert_eq!(r.level, RiskLevel::High, "{raw:?}");
    }
    // With no separator there is no head to try: an overlong run of digits is
    // judged whole and is not a card.
    let r = scan("41111111111111110031");
    assert!(!r.has_category(PiiCategory::CreditCard), "{r:?}");
}

#[test]
fn out_of_range_octets_are_not_ipv4() {
    let r = scan("coords 300.400.500.600 are nonsense");
    assert!(
        !r.has_category(PiiCategory::IpAddress),
        "bad octets flagged: {r:?}"
    );
}

#[test]
fn phone_shaped_ssn_not_double_counted_as_ssn() {
    // 3-3-4 grouping is a phone, not a 3-2-4 SSN.
    let r = scan("call 415-555-1234");
    assert!(r.has_category(PiiCategory::PhoneNumber));
    assert!(!r.has_category(PiiCategory::NationalId));
}

fn count_of(r: &PiiScanResult, category: PiiCategory) -> usize {
    r.hits
        .iter()
        .find(|hit| hit.category == category)
        .map_or(0, |hit| hit.count)
}

#[test]
fn a_value_matched_by_two_rules_counts_once() {
    // `+1 415-555-0100` is both a US-format and an international phone
    // number. It is one identifier, so it is one hit.
    let r = scan("call +1 415-555-0100");
    assert_eq!(count_of(&r, PiiCategory::PhoneNumber), 1, "{r:?}");
    assert_eq!(r.level, RiskLevel::Low);
}

#[test]
fn distinct_values_in_one_category_each_count() {
    let r = scan("+1 415-555-0100, 415-555-0199 or +44 20 7946 0958");
    assert_eq!(count_of(&r, PiiCategory::PhoneNumber), 3, "{r:?}");
    // The same value written twice is two occurrences.
    let r = scan("415-555-0100 then again 415-555-0100");
    assert_eq!(count_of(&r, PiiCategory::PhoneNumber), 2, "{r:?}");
}

#[test]
fn a_doubly_matched_value_is_redacted_whole() {
    assert_eq!(
        redact_identifiers("call +1 415-555-0100 now"),
        "call [redacted] now"
    );
    assert_eq!(
        redact_identifiers("call +1 415.555.1234 now"),
        "call [redacted] now"
    );
}

// ---------------------------------------------------------------------------
// Risk-level escalation
// ---------------------------------------------------------------------------

#[test]
fn single_weak_identifier_is_low() {
    let r = scan("email me: bob@example.org");
    assert_eq!(r.level, RiskLevel::Low);
    assert_eq!(r.categories, vec![PiiCategory::Email]);
}

#[test]
fn co_occurring_identifiers_escalate() {
    // Email + phone together → beyond a single weak signal.
    let r = scan("bob@example.org / 415-555-1234");
    assert!(r.has_category(PiiCategory::Email));
    assert!(r.has_category(PiiCategory::PhoneNumber));
    assert!(
        r.level >= RiskLevel::Medium,
        "expected escalation, got {r:?}"
    );
}

#[test]
fn strong_identifier_forces_high_even_alone() {
    let r = scan("123-45-6789");
    assert_eq!(r.level, RiskLevel::High);
    assert!(r.score >= PiiCategory::NationalId.weight());
}

#[test]
fn medical_record_shape_is_high() {
    let r = scan("patient John Doe, DOB 01/02/1980, diagnosed; SSN 111-22-3333");
    assert_eq!(r.level, RiskLevel::High);
    assert!(r.has_category(PiiCategory::Medical));
    assert!(r.has_category(PiiCategory::NationalId));
    assert!(r.has_category(PiiCategory::DateOfBirth));
}

#[test]
fn score_increases_with_more_categories() {
    let one = scan("bob@example.org").score;
    let two = scan("bob@example.org and patient diagnosed").score;
    assert!(
        two > one,
        "adding a category should raise the score: {one} !< {two}"
    );
}

// ---------------------------------------------------------------------------
// Offline / determinism (no external egress on classify)
// ---------------------------------------------------------------------------

#[test]
fn scan_is_deterministic_and_pure() {
    // Detection performs zero I/O: it is a pure function of its input, so
    // repeated calls must be byte-for-byte identical. This is the unit-test
    // proxy for "no external egress occurs during classify" — there is no
    // async, no network client, and nothing in this module to mock away.
    let input = "patient bob@example.org, card 4111 1111 1111 1111, SSN 123-45-6789";
    let first = scan(input);
    let second = scan(input);
    assert_eq!(first, second);
}

#[test]
fn scan_runs_without_a_tokio_runtime() {
    // A plain `#[test]` has no Tokio runtime installed; the fact that `scan`
    // returns here at all proves it does not spawn async work / touch the
    // network to classify.
    let r = scan("SSN 123-45-6789");
    assert_eq!(r.level, RiskLevel::High);
}

// ---------------------------------------------------------------------------
// Stable serde identifiers (contract for S2 / RPC / event bus)
// ---------------------------------------------------------------------------

#[test]
fn category_identifiers_are_stable_snake_case() {
    let cases = [
        (PiiCategory::Email, "email"),
        (PiiCategory::PhoneNumber, "phone_number"),
        (PiiCategory::NationalId, "national_id"),
        (PiiCategory::CreditCard, "credit_card"),
        (PiiCategory::BankAccount, "bank_account"),
        (PiiCategory::IpAddress, "ip_address"),
        (PiiCategory::PostalAddress, "postal_address"),
        (PiiCategory::DateOfBirth, "date_of_birth"),
        (PiiCategory::Passport, "passport"),
        (PiiCategory::Medical, "medical"),
        (PiiCategory::Legal, "legal"),
        (PiiCategory::Financial, "financial"),
        (PiiCategory::Family, "family"),
    ];
    for (cat, expected) in cases {
        assert_eq!(cat.as_str(), expected);
        assert_eq!(
            serde_json::to_string(&cat).unwrap(),
            format!("\"{expected}\"")
        );
    }
}

#[test]
fn risk_levels_are_ordered_and_stable() {
    assert!(RiskLevel::None < RiskLevel::Low);
    assert!(RiskLevel::Low < RiskLevel::Medium);
    assert!(RiskLevel::Medium < RiskLevel::High);
    for (lvl, s) in [
        (RiskLevel::None, "none"),
        (RiskLevel::Low, "low"),
        (RiskLevel::Medium, "medium"),
        (RiskLevel::High, "high"),
    ] {
        assert_eq!(lvl.as_str(), s);
        assert_eq!(serde_json::to_string(&lvl).unwrap(), format!("\"{s}\""));
    }
}

// ---------------------------------------------------------------------------
// Validator unit tests
// ---------------------------------------------------------------------------

#[test]
fn luhn_validator_accepts_known_good_and_rejects_bad() {
    assert!(is_luhn_valid("4111111111111111")); // Visa test number
    assert!(is_luhn_valid("4111 1111 1111 1111"));
    assert!(!is_luhn_valid("1234567890123456"));
    assert!(!is_luhn_valid("12345")); // too short
}

#[test]
fn complete_card_validator_accepts_pan_or_pan_with_cvv_group() {
    // A complete PAN, grouped or not.
    assert!(is_complete_card("4111111111111111"));
    assert!(is_complete_card("4111 1111 1111 1111"));
    // A complete PAN followed by one adjacent group of 3–4 digits.
    assert!(is_complete_card("4111 1111 1111 1111 123"));
    assert!(is_complete_card("4111-1111-1111-1111-1234"));
    // The whole run is Luhn-valid at 19 digits: a complete PAN as written.
    assert!(is_complete_card("4111 1111 1111 1111 003"));
    // Luhn-invalid PAN with a CVV-shaped tail.
    assert!(!is_complete_card("1234 5678 9012 3456 123"));
    // A complete PAN followed by more digits: the run starts with a card.
    assert!(is_complete_card("4111 1111 1111 1111 12"));
    assert!(is_complete_card("4111 1111 1111 1111 12345"));
    assert!(is_complete_card("4111 1111 1111 1111 003-1"));
    // No separator, so no head to try, and 20 digits is not a PAN.
    assert!(!is_complete_card("41111111111111110031"));
    // A valid PAN after another number on the same run is still found.
    assert!(is_complete_card("1234567890123456 4111 1111 1111 1111"));
    assert!(is_complete_card("1234 5678 9012 3456 4111 1111 1111 1111"));
    // A group of non-ASCII digits is skipped without panicking.
    assert!(is_complete_card(
        "\u{661}\u{662}\u{663}\u{664} 4111 1111 1111 1111"
    ));
}

#[test]
fn a_card_after_another_number_on_the_same_run_is_found_and_redacted() {
    for (text, redacted) in [
        (
            "invoice 1234567890123456 4111 1111 1111 1111",
            "invoice [redacted]",
        ),
        (
            "ids 1234 5678 9012 3456 4111 1111 1111 1111 end",
            "ids [redacted] end",
        ),
    ] {
        let r = scan(text);
        assert!(
            r.has_category(PiiCategory::CreditCard),
            "missed in {text:?}"
        );
        assert_eq!(redact_identifiers(text), redacted, "{text:?}");
    }
}

#[test]
fn a_long_separated_digit_run_is_judged_in_linear_work() {
    // Every group start is tried, but each stops past 19 digits, so a long run
    // of single-digit groups is cheap and is not a card.
    let run = "1 ".repeat(20_000);
    assert!(!is_complete_card(run.trim_end()));
}

#[test]
fn international_phone_validator_bounds_digit_count() {
    assert!(has_international_phone_digit_count("+1234567"));
    assert!(has_international_phone_digit_count("+1 234 567 890 123 45"));
    assert!(!has_international_phone_digit_count("+123456"));
    assert!(!has_international_phone_digit_count("+1234567890123456"));
}

#[test]
fn iban_validator_checks_the_mod_97_remainder() {
    assert!(is_iban("GB82WEST12345698765432"));
    assert!(is_iban("GB82 WEST 1234 5698 7654 32"));
    assert!(is_iban("de89 3704 0044 0532 0130 00"));
    assert!(!is_iban("GB82WEST12345698765433")); // one digit off
    assert!(!is_iban("G882WEST12345698765432")); // digit in the country code
    assert!(!is_iban("GB82WEST123")); // too short
}

#[test]
fn ipv4_validator_bounds_octets() {
    assert!(is_ipv4("192.168.0.1"));
    assert!(is_ipv4("255.255.255.255"));
    assert!(!is_ipv4("256.1.1.1"));
    assert!(!is_ipv4("1.2.3"));
}

// ---------------------------------------------------------------------------
// Redaction
// ---------------------------------------------------------------------------

#[test]
fn redact_identifiers_replaces_structured_values_only() {
    let out = redact_identifiers(
        "user jane.doe@example.com logged in from 203.0.113.7, call +1 415-555-0100 about the patient",
    );
    assert!(!out.contains("jane.doe@example.com"), "{out}");
    assert!(!out.contains("203.0.113.7"), "{out}");
    assert!(!out.contains("415-555-0100"), "{out}");
    assert!(out.contains("[redacted]"), "{out}");
    // Topical keywords are context, not identifiers: kept.
    assert!(out.contains("patient"), "{out}");
}

#[test]
fn redact_identifiers_replaces_the_whole_card_span() {
    // An adjacent CVV group belongs to the card: the span is replaced whole.
    assert_eq!(
        redact_identifiers("pay with 4111 1111 1111 1111 123 today"),
        "pay with [redacted] today"
    );
    // A labelled CVV ends the run at the card; only the card is structured.
    assert_eq!(
        redact_identifiers("card 4111 1111 1111 1111 cvv 123"),
        "card [redacted] cvv 123"
    );
}

#[test]
fn redact_identifiers_replaces_a_run_that_starts_with_a_card_whole() {
    // The valid PAN at the head is not left in place: the whole run goes.
    for text in [
        "ref 4111 1111 1111 1111 003-1 noted",
        "ref 4111 1111 1111 1111 1111 1111 noted",
    ] {
        assert_eq!(redact_identifiers(text), "ref [redacted] noted", "{text:?}");
    }
    let text = "ref 41111111111111110031 noted";
    assert_eq!(redact_identifiers(text), text);
}

#[test]
fn redact_identifiers_keeps_values_that_fail_validation() {
    // Not a valid IPv4 address, not a Luhn-valid card: left as written.
    let text = "version 999.1.1.1 build 1234567890123456";
    assert_eq!(redact_identifiers(text), text);
}
