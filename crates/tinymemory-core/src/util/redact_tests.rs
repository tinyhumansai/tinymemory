//! Tests for the surrounding module.

use super::*;

// ── redact ───────────────────────────────────────────────────────────────

#[test]
fn redact_returns_eight_hex_chars() {
    let r = redact("alice@example.com");
    assert_eq!(r.len(), 8, "must be 8 hex chars; got {r:?}");
    assert!(r.chars().all(|c| c.is_ascii_hexdigit()), "must be hex");
}

#[test]
fn redact_is_stable_across_calls() {
    assert_eq!(redact("alice@example.com"), redact("alice@example.com"));
}

#[test]
fn redact_is_different_for_different_inputs() {
    assert_ne!(redact("alice@example.com"), redact("bob@example.com"));
}

#[test]
fn redact_empty_string_does_not_panic() {
    let r = redact("");
    assert_eq!(r.len(), 8);
}

// ── redact_endpoint ─────────────────────────────────────────────────────

#[test]
fn redact_endpoint_strips_path_and_query() {
    assert_eq!(
        redact_endpoint("http://localhost:11434/api/chat"),
        "localhost:11434"
    );
}

#[test]
fn redact_endpoint_strips_credentials() {
    assert_eq!(
        redact_endpoint("https://user:pass@example.com/foo"),
        "example.com"
    );
}

#[test]
fn redact_endpoint_no_scheme_passthrough() {
    // No "://" present — treat the whole string as host/path; still strip path.
    assert_eq!(redact_endpoint("localhost:11434/api"), "localhost:11434");
}

#[test]
fn redact_endpoint_just_host() {
    assert_eq!(redact_endpoint("https://example.com"), "example.com");
}

#[test]
fn redact_endpoint_strips_fragment() {
    assert_eq!(redact_endpoint("http://host:9090/path#frag"), "host:9090");
}

#[test]
fn redact_endpoint_strips_query() {
    assert_eq!(redact_endpoint("http://host/path?q=1"), "host");
}

#[test]
fn redact_endpoint_empty_does_not_panic() {
    let r = redact_endpoint("");
    // Empty input: no scheme, no host — returns empty string.
    assert_eq!(r, "");
}

#[test]
fn redact_endpoint_ollama_style() {
    assert_eq!(
        redact_endpoint("http://127.0.0.1:11434/v1/chat/completions"),
        "127.0.0.1:11434"
    );
}
