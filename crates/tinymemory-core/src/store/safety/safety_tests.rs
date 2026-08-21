//! Tests for the surrounding module.

//! Byte-parity guard over the crate scrubber: every secret/PII pattern the
//! host used to redact must still be redacted after the port.
use super::*;
use serde_json::json;

const REDACTED_SECRET: &str = "[REDACTED_SECRET]";
const REDACTED_PRIVATE_KEY: &str = "[REDACTED_PRIVATE_KEY]";
const MAX_JSON_SANITIZE_DEPTH: usize = 128;

fn private_key_fixture(kind: &str, body: &str) -> String {
    format!("-----BEGIN {kind}-----\n{body}\n-----END {kind}-----")
}

#[test]
fn sanitize_text_redacts_bearer_and_openai_key() {
    let input = "Authorization: Bearer abcdefghijklmnop and sk-1234567890123456789012345";
    let sanitized = sanitize_text(input);
    assert!(sanitized.value.contains("Bearer [REDACTED]"));
    assert!(!sanitized.value.contains("sk-1234567890123456789012345"));
    assert!(sanitized.report.text_redactions >= 2);
}

#[test]
fn sanitize_text_blocks_private_key_blocks() {
    let input = private_key_fixture("PRIVATE KEY", "abc");
    let sanitized = sanitize_text(&input);
    assert!(sanitized.value.contains(REDACTED_PRIVATE_KEY));
    assert!(sanitized.report.blocked_secret_hits >= 1);
}

#[test]
fn sanitize_json_redacts_sensitive_keys_and_nested_strings() {
    let input = json!({
        "token": "abc123",
        "nested": { "notes": "Bearer supersecretvalue", "ok": "hello" },
        "arr": ["sk-1234567890123456789012345", "safe"]
    });
    let sanitized = sanitize_json(&input);
    assert_eq!(sanitized.value["token"], json!(REDACTED_SECRET));
    assert_eq!(sanitized.value["nested"]["ok"], json!("hello"));
    assert!(sanitized.value["nested"]["notes"]
        .as_str()
        .unwrap_or_default()
        .contains("[REDACTED]"));
    assert!(sanitized.report.key_redactions >= 1);
    assert!(sanitized.report.text_redactions >= 2);
}

#[test]
fn sanitize_json_redacts_common_sensitive_key_variants() {
    let input = json!({
        "db_password": "p@ss", "secret_key": "abc123",
        "api_secret": "def456", "monkey": "banana"
    });
    let sanitized = sanitize_json(&input);
    assert_eq!(sanitized.value["db_password"], json!(REDACTED_SECRET));
    assert_eq!(sanitized.value["secret_key"], json!(REDACTED_SECRET));
    assert_eq!(sanitized.value["api_secret"], json!(REDACTED_SECRET));
    assert_eq!(sanitized.value["monkey"], json!(REDACTED_SECRET));
    assert!(sanitized.report.key_redactions >= 4);
}

#[test]
fn has_likely_secret_detects_common_patterns() {
    assert!(has_likely_secret("api_key=abc123"));
    assert!(has_likely_secret("Bearer abcdefghijklmnopqrstuvwxyz"));
    let slack_token = format!("{}{}-1234567890-abcdef-ghijklmnop", "xo", "xb");
    assert!(has_likely_secret(&slack_token));
    assert!(has_likely_secret("glpat-aaaaaaaaaaaaaaaaaaaa"));
    assert!(has_likely_secret("SG.aaaaaaaaaaaaaaaa.bbbbbbbbbbbbbbbb"));
    assert!(!has_likely_secret("I prefer rust"));
}

#[test]
fn sanitize_text_redacts_more_provider_secrets() {
    let input = "auth=Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ== stripe=sk_live_12345678901234567890 npm=npm_abcdefghijklmnopqrstuvwxyz";
    let sanitized = sanitize_text(input);
    assert!(!sanitized.value.contains("sk_live_12345678901234567890"));
    assert!(!sanitized.value.contains("npm_abcdefghijklmnopqrstuvwxyz"));
    assert!(sanitized.value.contains("[REDACTED]"));
    assert!(sanitized.report.text_redactions >= 2);
}

#[test]
fn sanitize_text_redacts_oauth_url_style_params() {
    let input =
        "https://example.com/callback?access_token=abcd1234&refresh_token=efgh5678&id_token=jwt";
    let sanitized = sanitize_text(input);
    assert!(!sanitized.value.contains("abcd1234"));
    assert!(!sanitized.value.contains("efgh5678"));
    assert!(!sanitized.value.contains("id_token=jwt"));
    assert!(sanitized.report.text_redactions >= 3);
}

#[test]
fn sanitize_text_redacts_multiline_private_key_blocks() {
    let key_kind = format!("{} PRIVATE KEY", "OPENSSH");
    let input = format!(
        "BEGIN\n{}\nEND",
        private_key_fixture(&key_kind, "line1\nline2")
    );
    let sanitized = sanitize_text(&input);
    assert!(!sanitized.value.contains(&key_kind));
    assert!(sanitized.value.contains(REDACTED_PRIVATE_KEY));
    assert!(sanitized.report.blocked_secret_hits >= 1);
}

#[test]
fn sanitize_text_also_redacts_pii_after_secrets() {
    let input = "Token sk-abcdefghijklmnopqrstuvwxyz; CPF 111.444.777-35; phone +15551234567";
    let sanitized = sanitize_text(input);
    assert!(!sanitized.value.contains("sk-abcdefghijklmnopqrstuvwxyz"));
    assert!(!sanitized.value.contains("111.444.777-35"));
    assert!(!sanitized.value.contains("+15551234567"));
    assert!(sanitized.value.contains("[REDACTED_PII_CPF]"));
    assert!(sanitized.value.contains("[REDACTED_PII_PHONE]"));
    assert!(sanitized.report.text_redactions >= 1);
    assert_eq!(sanitized.report.pii_redactions, 2);
}

#[test]
fn sanitize_json_propagates_pii_redaction_into_nested_strings() {
    let input = json!({
        "note": "Cliente RFC VECJ880326XK4 confirmado",
        "meta": { "cuit": "20-11111111-2" }
    });
    let sanitized = sanitize_json(&input);
    assert!(sanitized.value["note"]
        .as_str()
        .unwrap_or_default()
        .contains("[REDACTED_PII_RFC]"));
    assert!(sanitized.value["meta"]["cuit"]
        .as_str()
        .unwrap_or_default()
        .contains("[REDACTED_PII_CUIT]"));
    assert!(sanitized.report.pii_redactions >= 2);
}

#[test]
fn sanitize_json_redacts_values_beyond_max_depth() {
    let mut nested = json!("leaf");
    for _ in 0..(MAX_JSON_SANITIZE_DEPTH + 2) {
        nested = json!({ "nested": nested });
    }
    let sanitized = sanitize_json(&nested);
    assert!(sanitized.report.depth_redactions >= 1);
    assert!(sanitized
        .value
        .to_string()
        .contains(&format!("\"{REDACTED_SECRET}\"")));
}

/// #5164: identifiers are storage addresses, so canonicalization follows
/// the **strict** boundary predicate. Formatted / keyword-gated national IDs
/// are rewritten; the bare digit-run shapes the scanners build identifiers
/// out of are left alone (rewriting those maps distinct contacts onto one
/// `(namespace, key)` and the upsert silently overwrites).
#[test]
fn canonical_identifier_rewrites_only_strict_pii() {
    for identifier in [
        "ssn-123-45-6789",
        "cliente-RFC-VECJ880326XK4",
        "cuit-20-11111111-2",
        "user/111.444.777-35",
    ] {
        let canonical = canonical_identifier(identifier);
        assert_ne!(
            canonical, identifier,
            "strict PII identifier must be canonicalized: {identifier}"
        );
        assert!(
            canonical.contains("[REDACTED_PII_"),
            "expected a redaction placeholder, got: {canonical}"
        );
    }

    for identifier in [
        // WhatsApp group JID / 1:1 JID / broadcast, iMessage E.164 chat id,
        // telegram numeric peer id, padded ms timestamp, plain namespaces.
        "12025551234-1543890267@g.us:2026-05-30",
        "12025551234@c.us:2026-05-30",
        "imessage:+12025551234:2026-05-30",
        "4123456789:2026-05-30",
        "accepted:000001747729035001",
        "memory/global/preferences",
        "skill-gmail",
    ] {
        assert_eq!(
            canonical_identifier(identifier),
            identifier,
            "scanner-built identifier must keep its identity: {identifier}"
        );
    }
}

/// Read paths canonicalize unconditionally, so the transform has to be a
/// fixed point on its own output.
#[test]
fn canonical_identifier_is_idempotent() {
    for identifier in ["ssn-123-45-6789", "cliente-RFC-VECJ880326XK4", "safe-key"] {
        let once = canonical_identifier(identifier);
        assert_eq!(canonical_identifier(&once), once, "not idempotent: {once}");
    }
}

/// `canonical_document_key` single-sources the write-path transform, trim
/// included — otherwise `Memory::get` would address an untrimmed key that
/// `upsert_document` never wrote.
#[test]
fn canonical_document_key_trims_before_canonicalizing() {
    assert_eq!(canonical_document_key("  doc-a  "), "doc-a");
    assert_eq!(
        canonical_document_key("  ssn-123-45-6789  "),
        canonical_identifier("ssn-123-45-6789")
    );
    assert_eq!(canonical_document_key("   "), "");
}

#[test]
fn sanitize_document_input_preserves_taint() {
    let input = NamespaceDocumentInput {
        namespace: "ns".into(),
        key: "k".into(),
        title: "Bearer secret123456789 visible title".into(),
        content: "content with sk-abcdefghijklmnopqrstuvwxyz".into(),
        source_type: "sync".into(),
        priority: "normal".into(),
        tags: vec!["tag1".into()],
        metadata: json!({"safe": "value"}),
        category: "core".into(),
        session_id: None,
        document_id: None,
        taint: crate::MemoryTaint::ExternalSync,
    };
    let sanitized = sanitize_document_input(input);
    assert_eq!(
        sanitized.value.taint,
        crate::MemoryTaint::ExternalSync,
        "taint must survive sanitization unchanged"
    );
    assert!(sanitized.report.text_redactions >= 1);
}
