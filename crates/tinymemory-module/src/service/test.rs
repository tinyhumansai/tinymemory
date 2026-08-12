//! Tests for the service's error mapping.
//!
//! This is the module's half of the wire contract: every `MemoryError` the
//! engine can raise has to leave as a named bus error the host's client can map
//! back. `tinymemory_api::wire_tests` pins the table itself; what is tested here
//! is that the service actually goes through it.
//!
//! Covered here rather than in the loader E2E deliberately. An E2E can only
//! provoke the errors the engine happens to raise for a given input, which makes
//! it a test of engine internals this port does not own — an earlier revision
//! tried `ExportPage` with a zero limit, and since a driver accepting a zero
//! limit is equally legitimate, the test asserted nothing when it passed. Here
//! every variant is reachable by construction.

use tinybus::Error as BusError;
use tinymemory_api::error::MemoryError;
use tinymemory_api::wire;

use super::into_bus_error;

/// The name and message a mapped error carries on the wire.
fn mapped(error: &MemoryError) -> (String, String) {
    match into_bus_error(error) {
        BusError::MethodFailed { name, message } => (name, message),
        other => panic!("expected MethodFailed, got {other:?}"),
    }
}

#[test]
fn every_variant_leaves_under_its_contract_name() {
    // Exhaustive by construction: `wire::wire_name` is a total match over
    // `MemoryError`, so a new variant fails to compile there before it can
    // silently leave this list.
    let cases = [
        (MemoryError::NotFound("k".into()), wire::NOT_FOUND),
        (MemoryError::Invalid("bad".into()), wire::INVALID),
        (
            MemoryError::BudgetExceeded("too big".into()),
            wire::BUDGET_EXCEEDED,
        ),
        (
            MemoryError::PathEscape("../outside".into()),
            wire::PATH_ESCAPE,
        ),
        (MemoryError::unsupported_raw("tree"), wire::UNSUPPORTED),
        (
            MemoryError::Other(anyhow::anyhow!("engine fell over")),
            wire::OTHER,
        ),
    ];

    for (error, expected) in &cases {
        let (name, _) = mapped(error);
        assert_eq!(&name, expected, "{error:?} left under the wrong name");
    }
}

#[test]
fn a_path_escape_never_leaves_as_an_invalid() {
    // The security-relevant collapse. `Invalid` tells a caller its input was
    // malformed and invites a retry; a sandbox escape is not that, and the host
    // re-raises whatever it receives to its own callers.
    let (name, _) = mapped(&MemoryError::PathEscape("../../etc".into()));
    assert_eq!(name, wire::PATH_ESCAPE);
    assert_ne!(name, wire::INVALID);
}

#[test]
fn a_miss_never_leaves_as_an_invalid() {
    // `get`'s contract makes a miss `Ok(None)`, so a `NotFound` that arrived as
    // `Invalid` would turn an ordinary absence into a caller-visible failure.
    let (name, _) = mapped(&MemoryError::NotFound("absent".into()));
    assert_eq!(name, wire::NOT_FOUND);
    assert_ne!(name, wire::INVALID);
}

#[test]
fn the_names_the_service_emits_are_the_ones_the_host_decodes() {
    // The drift that matters is silent, so this closes the loop rather than
    // trusting the two tables to agree: map out through the service, back
    // through the client's decoder, and require the variant to survive.
    let originals = [
        MemoryError::NotFound("k".into()),
        MemoryError::Invalid("bad".into()),
        MemoryError::BudgetExceeded("too big".into()),
        MemoryError::PathEscape("../outside".into()),
        MemoryError::unsupported_raw("tree"),
    ];

    for original in &originals {
        let (name, message) = mapped(original);
        let decoded = wire::from_wire(&name, &message);
        assert_eq!(
            std::mem::discriminant(&decoded),
            std::mem::discriminant(original),
            "{original:?} did not survive the round trip, arrived as {decoded:?}"
        );
    }
}

#[test]
fn a_message_carries_no_user_content_beyond_what_the_engine_put_there() {
    // Not a redaction test — the engine owns its message. This pins that the
    // service adds nothing of its own, so the only thing that can leak is what
    // the engine already chose to say.
    let error = MemoryError::NotFound("some-key".into());
    let (_, message) = mapped(&error);
    assert_eq!(message, wire::wire_message(&error));
}

/// An entry whose content is `bytes` long.
fn entry_of(bytes: usize) -> tinymemory_api::types::MemoryEntry {
    tinymemory_api::types::MemoryEntry {
        id: "id".into(),
        key: "key".into(),
        content: "x".repeat(bytes),
        namespace: Some("ns".into()),
        category: tinymemory_api::types::MemoryCategory::Core,
        timestamp: "2026-01-01T00:00:00Z".into(),
        session_id: None,
        score: None,
        taint: tinymemory_api::types::MemoryTaint::Internal,
    }
}

#[test]
fn an_ordinary_list_response_is_not_refused() {
    // The ceiling must not be so tight that normal use trips it. A hundred
    // entries of a kilobyte each is an unremarkable namespace.
    let entries: Vec<_> = (0..100).map(|_| entry_of(1024)).collect();
    assert!(super::ensure_response_fits(&entries, "List").is_ok());
}

#[test]
fn an_empty_list_response_is_not_refused() {
    assert!(
        super::ensure_response_fits(&Vec::<tinymemory_api::types::MemoryEntry>::new(), "List")
            .is_ok()
    );
}

#[test]
fn a_response_over_the_ceiling_is_refused_as_a_budget_error() {
    // `List` takes no limit and no cursor, so entries accumulate across
    // individually valid `Store` calls until the response cannot cross a
    // 16 MiB frame. Without this check the caller gets a transport failure it
    // cannot act on; with it, a named error that says how to narrow the query.
    let entries: Vec<_> = (0..2)
        .map(|_| entry_of(super::MAX_RESPONSE_BYTES))
        .collect();

    let error = super::ensure_response_fits(&entries, "List")
        .expect_err("a response over the ceiling must be refused");
    match error {
        BusError::MethodFailed { name, message } => {
            assert_eq!(
                name,
                wire::BUDGET_EXCEEDED,
                "must use a name the host already decodes"
            );
            assert!(message.contains("List"), "{message}");
            assert!(
                message.contains("narrow"),
                "the message must tell the caller what to do: {message}"
            );
        }
        other => panic!("expected MethodFailed, got {other:?}"),
    }
}

#[test]
fn the_refusal_decodes_host_side_as_a_budget_error() {
    // The whole point of reusing an existing name: a new one would decode to
    // `Other` on any host older than the module, turning an actionable "narrow
    // your query" into an opaque backend failure.
    let entries: Vec<_> = (0..2)
        .map(|_| entry_of(super::MAX_RESPONSE_BYTES))
        .collect();

    let BusError::MethodFailed { name, message } =
        super::ensure_response_fits(&entries, "List").expect_err("refused")
    else {
        panic!("expected MethodFailed");
    };

    let decoded = wire::from_wire(&name, &message);
    assert!(
        matches!(decoded, MemoryError::BudgetExceeded(_)),
        "{decoded:?}"
    );
}

#[test]
fn the_refusal_message_carries_no_entry_content() {
    // Entry content is user memory. The message names sizes and the method, and
    // nothing that was stored.
    let secret = "correct-horse-battery-staple";
    let mut entries: Vec<_> = (0..2)
        .map(|_| entry_of(super::MAX_RESPONSE_BYTES))
        .collect();
    entries[0].content.push_str(secret);

    let BusError::MethodFailed { message, .. } =
        super::ensure_response_fits(&entries, "List").expect_err("refused")
    else {
        panic!("expected MethodFailed");
    };
    assert!(!message.contains(secret), "{message}");
}

#[test]
fn the_per_entry_overhead_is_counted_so_many_tiny_entries_still_trip_it() {
    // A million empty entries carry no content at all but still cannot cross a
    // frame — the JSON structure around each one is the payload. Counting only
    // `content.len()` would let this through.
    let encoded_entry = serde_json::to_vec(&entry_of(0))
        .expect("serializable")
        .len();
    let count = super::MAX_RESPONSE_BYTES / encoded_entry + 1;
    let entries: Vec<_> = (0..count).map(|_| entry_of(0)).collect();
    assert!(
        super::ensure_response_fits(&entries, "List").is_err(),
        "entries with no content must still be counted"
    );
}
