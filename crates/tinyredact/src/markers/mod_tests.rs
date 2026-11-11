//! Tests for the credential-marker rules: one-time-secret URLs and `Bearer`
//! values, including the short ones the regex set leaves alone.
//!
//! Ported case for case from OpenCompany's `harness/built_in/redact_tests.rs`,
//! split so each rule names what it protects.

use super::*;

fn redact(text: &str) -> String {
    redact_credential_markers(text).into_owned()
}

#[test]
fn a_one_time_secret_url_loses_its_key_but_keeps_the_prose() {
    // The key is stripped; the surrounding sentence — the context an agent
    // needs to understand that a link was shared — is kept.
    assert_eq!(
        redact("here it is https://ots.example/secret/AbCdEf123456 open it"),
        "here it is https://ots.example/secret/[REDACTED] open it"
    );
}

#[test]
fn a_bearer_token_in_prose_is_redacted() {
    assert_eq!(
        redact("auth with Bearer sk-verylongsecrettoken please"),
        "auth with Bearer [REDACTED] please"
    );
}

#[test]
fn a_jwt_is_consumed_whole_across_its_dots() {
    assert_eq!(
        redact(
            "auth with Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.aSignature0123456789 please"
        ),
        "auth with Bearer [REDACTED] please"
    );
}

#[test]
fn base64_punctuation_does_not_end_the_credential() {
    // `+`, `/`, `~` and `=` are part of opaque keys; stopping at the first one
    // would leak the remainder.
    assert_eq!(
        redact("auth with Bearer aGVsbG8r/d29ybGQ=andtheRestOfTheKey please"),
        "auth with Bearer [REDACTED] please"
    );
}

#[test]
fn a_short_token_shaped_bearer_value_is_still_a_secret() {
    // Any non-empty bearer value is a valid credential; a digit marks a short
    // one like `s3cret` as token-shaped rather than prose.
    assert_eq!(
        redact("auth with Bearer s3cret please"),
        "auth with Bearer [REDACTED] please"
    );
}

#[test]
fn a_digit_free_bearer_value_of_six_characters_is_redacted() {
    assert_eq!(
        redact("auth with Bearer secret please"),
        "auth with Bearer [REDACTED] please"
    );
}

#[test]
fn the_scheme_matches_case_insensitively() {
    // RFC 9110's auth-scheme ABNF is case-insensitive.
    assert_eq!(
        redact("auth with bearer sk-longsecret please"),
        "auth with bearer [REDACTED] please"
    );
    assert_eq!(
        redact("auth with BEARER sk-longsecret please"),
        "auth with BEARER [REDACTED] please"
    );
}

#[test]
fn lower_case_bearer_in_its_english_sense_is_left_alone() {
    assert_eq!(
        redact("the bearer bond matures in June"),
        "the bearer bond matures in June"
    );
    assert_eq!(
        redact("the standard bearer candidate won the race"),
        "the standard bearer candidate won the race"
    );
    assert_eq!(
        redact("the ring bearer walked down the aisle"),
        "the ring bearer walked down the aisle"
    );
}

#[test]
fn a_backtick_or_quote_wrapper_does_not_hide_the_credential() {
    assert_eq!(
        redact("auth with Bearer `sk-verylongsecret` please"),
        "auth with Bearer `[REDACTED]` please"
    );
    assert_eq!(
        redact("auth with Bearer \"sk-verylongsecret\" please"),
        "auth with Bearer \"[REDACTED]\" please"
    );
}

#[test]
fn every_fragment_of_a_space_separated_credential_is_redacted() {
    assert_eq!(
        redact("auth with Bearer firstpart secondpart please"),
        "auth with Bearer [REDACTED] [REDACTED] please"
    );
}

#[test]
fn trailing_prose_after_a_single_token_survives() {
    assert_eq!(
        redact("auth with Bearer sk-abc123 please"),
        "auth with Bearer [REDACTED] please"
    );
}

#[test]
fn text_without_a_marker_is_borrowed_through_untouched() {
    assert!(matches!(
        redact_credential_markers("nothing secret here"),
        Cow::Borrowed(_)
    ));
}

#[test]
fn a_value_too_short_to_be_a_secret_is_left_alone() {
    assert_eq!(redact("Bearer or not"), "Bearer or not");
}

#[test]
fn extra_whitespace_after_the_scheme_does_not_leak_the_credential() {
    assert_eq!(
        redact("auth with Bearer   sk-longsecret please"),
        "auth with Bearer   [REDACTED] please"
    );
}

#[test]
fn short_prose_words_after_bearer_survive_with_or_without_a_dot() {
    assert_eq!(redact("Bearer key. Please"), "Bearer key. Please");
    assert_eq!(redact("Bearer token please"), "Bearer token please");
}

#[test]
fn the_redaction_count_matches_the_values_replaced() {
    let (text, hits) =
        redact_counted("Bearer firstpart secondpart and https://x.example/secret/k3y1234");
    assert_eq!(
        text,
        "Bearer [REDACTED] [REDACTED] and https://x.example/secret/[REDACTED]"
    );
    assert_eq!(hits, 3);
    assert_eq!(redact_counted("plain prose").1, 0);
}
