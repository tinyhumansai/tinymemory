//! CortexDB adapter unit tests.
//!
//! These cover the pure functions the adapter reconstructs the contract
//! with — scope mapping, the fold, and the two shapes a stored envelope
//! comes back in. Behaviour against a live engine is in
//! `tests/live_remote_engines.rs`.

// Test-only, and deliberately narrow: `expect` here names the invariant a
// failing setup step or assertion violated, which is the diagnostic a reader of
// the failure output needs. Production paths in this crate return `Result`.
#![allow(clippy::expect_used)]

use super::*;

#[test]
fn credentials_require_https_except_on_loopback() {
    assert!(CortexMemory::api("http://memory.example.com", "test-key").is_err());
    assert!(CortexMemory::api("https://memory.example.com", "test-key").is_ok());
    assert!(CortexMemory::api("http://127.0.0.1:3141", "test-key").is_ok());
    assert!(CortexMemory::api("http://[::1]:3141", "test-key").is_ok());
    assert!(CortexMemory::api("http://localhost:3141", "test-key").is_ok());
}
#[test]
fn a_namespace_maps_to_a_scope_and_back() {
    let namespace = "oc/acme-0123456789abcdef0123456789abcdef/facts";
    let scope = CortexDialect::scope_of(namespace).expect("maps");
    assert_eq!(
        scope,
        "tm:oc/tm:acme-0123456789abcdef0123456789abcdef/tm:facts"
    );
    assert_eq!(
        CortexDialect::namespace_of(&scope).as_deref(),
        Some(namespace),
        "the round trip is load-bearing: a scope we cannot map back yields zero hits \
         silently, because the host re-checks every returned record against the \
         namespace it asked for"
    );
}

#[test]
fn a_segment_cortex_would_reject_is_encoded_rather_than_refused() {
    // The contract allows characters a Cortex scope id does not, and `:` is the
    // one that matters: it addresses a namespace *section*. Refusing it would
    // make whole sections unstorable, and collapsing it would silently
    // re-address the namespace out of its section — which is exactly the
    // regression `assert_namespaces_preserve_their_section` exists to catch.
    for namespace in [
        "conversation:tinymemory-conformance/cortex/section-thread",
        "oc/has space/facts",
        "oc/ünicode/facts",
    ] {
        let mapped = CortexDialect::scope_of(namespace);
        assert!(
            mapped.is_ok(),
            "`{namespace}` should encode, not refuse: {mapped:?}"
        );
        let scope = mapped.expect("checked on the line above");
        assert_eq!(
            CortexDialect::namespace_of(&scope).as_deref(),
            Some(namespace),
            "`{namespace}` did not survive the round trip through `{scope}`"
        );
    }

    // A scope id is capped at 128 characters, and encoding doubles the ones it
    // applies to — so the ceiling is real, and lower for an encoded segment.
    assert!(CortexDialect::scope_of(&format!("oc/{}", "x".repeat(129))).is_err());
    assert!(CortexDialect::scope_of(&format!("oc/{}", ":".repeat(65))).is_err());
    assert!(CortexDialect::scope_of("").is_err());

    // A scope this adapter did not write must not decode into a namespace.
    assert_eq!(CortexDialect::namespace_of("user:alice/notes"), None);
}

#[test]
fn the_fold_keeps_the_newest_write_per_key() {
    // The whole contract this adapter reconstructs: the engine holds both
    // versions, and a caller must see only the second.
    let events = vec![
        json!({
            "id": "evt_1", "wal_offset": 10,
            "content": { "text": r#"{"k":"billing-owner","c":"Ana"}"# },
            "context": { "recorded_at": "2026-09-02T00:00:00Z" }
        }),
        json!({
            "id": "evt_2", "wal_offset": 20,
            "content": { "text": r#"{"k":"billing-owner","c":"Dev"}"# },
            "context": { "recorded_at": "2026-09-02T00:01:00Z" }
        }),
        json!({
            "id": "evt_3", "wal_offset": 30,
            "content": { "text": r#"{"k":"oncall","c":"Priya"}"# },
            "context": { "recorded_at": "2026-09-02T00:02:00Z" }
        }),
    ];
    let folded = CortexDialect::fold("oc/acme/facts", &events);
    assert_eq!(
        folded.len(),
        2,
        "one row per logical key, not one per write"
    );
    let owner = folded
        .iter()
        .find(|e| e.key == "billing-owner")
        .expect("key");
    assert_eq!(owner.content, "Dev", "the later write wins");
    assert_eq!(owner.remote_id, "evt_2");
}

#[test]
fn the_fold_ignores_events_this_adapter_did_not_write() {
    // A scope can hold events written by someone using CortexDB directly.
    // Those are not ours to interpret, and must not become phantom records.
    let events = vec![
        json!({ "id": "evt_1", "wal_offset": 1,
                "content": { "text": "just a sentence someone typed" } }),
        json!({ "id": "evt_2", "wal_offset": 2,
                "content": { "text": r#"{"unrelated":"json"}"# } }),
    ];
    assert!(CortexDialect::fold("oc/acme/facts", &events).is_empty());
}

#[test]
fn taint_survives_the_envelope() {
    let events = vec![json!({
        "id": "evt_1", "wal_offset": 1,
        "content": { "text": r#"{"k":"a","c":"b","t":"external_sync"}"# }
    })];
    let folded = CortexDialect::fold("oc/acme/facts", &events);
    assert_eq!(folded[0].taint, MemoryTaint::ExternalSync);
}

#[test]
fn a_cursor_is_escaped_far_beyond_the_characters_a_scope_carries() {
    // A scope only ever holds `:` and `/`, so an escape list would cover it.
    assert_eq!(
        urlencoding("tm:oc/tm:acme-1/tm:facts"),
        "tm%3Aoc%2Ftm%3Aacme-1%2Ftm%3Afacts"
    );
    // The cursor is opaque engine output, and these are the characters that
    // would silently reshape a query string rather than fail.
    assert_eq!(urlencoding("a+b&c=d#e?f"), "a%2Bb%26c%3Dd%23e%3Ff");
    // Unreserved characters must survive untouched, or every request grows.
    assert_eq!(urlencoding("Az09-._~"), "Az09-._~");
    // Encoding is by byte, so multi-byte UTF-8 stays recoverable.
    assert_eq!(urlencoding("é"), "%C3%A9");
}

#[test]
fn a_scope_path_deeper_than_the_engine_accepts_is_refused_here() {
    // Measured against a running engine: 32 segments are accepted, 33 come back
    // as `422 INVALID_BODY`. Refusing locally keeps the error specific instead
    // of surfacing as a generic body rejection from the wire.
    let deep = (0..32)
        .map(|i| format!("s{i}"))
        .collect::<Vec<_>>()
        .join("/");
    assert!(
        CortexDialect::scope_of(&deep).is_ok(),
        "32 segments are within the grammar and must not be refused"
    );

    let deeper = (0..33)
        .map(|i| format!("s{i}"))
        .collect::<Vec<_>>()
        .join("/");
    assert!(
        CortexDialect::scope_of(&deeper).is_err(),
        "33 segments exceed what the engine accepts and must be refused here"
    );
}

#[test]
fn two_writers_in_one_process_never_mint_the_same_key() {
    // The counter covers this much; the per-process salt covers the case no
    // unit test can reach, which is a second process minting concurrently.
    let keys: std::collections::HashSet<String> =
        (0..1000).map(|_| fresh_idempotency_key()).collect();
    assert_eq!(keys.len(), 1000, "an idempotency key was reused");

    // The salt is stable within a process, so a key is greppable back to the
    // run that wrote it.
    let salt_of = |k: &str| k.split('-').nth(1).map(str::to_string);
    assert_eq!(
        salt_of(&fresh_idempotency_key()),
        salt_of(&fresh_idempotency_key()),
        "the salt identifies the process and must not change between writes"
    );
}
