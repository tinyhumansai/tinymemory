//! Direct wire behaviour: `?wait=indexed`, the bulk route, visibility
//! waits, forget selectors, the retry split and status mapping.

use super::*;
use crate::cortex::testing::{direct_double, direct_engine, sample_items, thread_meta};
use std::sync::atomic::Ordering;
use tinymemory_api::{MetaFilter, Role, Turn};

#[tokio::test]
async fn a_document_waits_indexed_and_a_conversation_is_one_strict_bulk() {
    let (endpoint, state) = direct_double().await;
    let engine = direct_engine(&endpoint);
    for item in sample_items() {
        engine.store(item).await.unwrap();
    }
    let requests = state.requests();
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.starts_with("POST /v1/experience?wait=indexed"))
            .count(),
        2,
        "the document and the learning: {requests:?}"
    );
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.starts_with("POST /v1/experience/bulk?wait=indexed"))
            .count(),
        1,
        "the conversation is one ordered batch"
    );
    assert!(requests.iter().all(|r| !r.contains("/memory/")));
    assert_eq!(state.event_count(), 5, "one event per turn");
    let events = state.log.lock().unwrap().events.clone();
    let turns: Vec<_> = events
        .iter()
        .filter(|e| e["scope"] == "app:tinymemory/app:conversations")
        .map(|e| e["content"]["role"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(turns, vec!["user", "assistant", "user"], "in order");
}

#[tokio::test]
async fn a_write_polls_until_its_event_is_listed() {
    let (endpoint, state) = direct_double().await;
    state.hide_listing_for.store(4, Ordering::SeqCst);
    direct_engine(&endpoint)
        .store(sample_items().remove(0))
        .await
        .unwrap();
    assert!(state.count("GET /v1/events") >= 5);
}

#[tokio::test]
async fn a_write_never_listed_is_an_error_not_a_success() {
    let (endpoint, state) = direct_double().await;
    state.hide_listing_for.store(usize::MAX, Ordering::SeqCst);
    let error = direct_engine(&endpoint)
        .with_test_timing(std::time::Duration::from_millis(100))
        .store(sample_items().remove(2))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Unavailable(_)), "{error:?}");
    assert!(error.to_string().contains("readable"), "{error}");
}

#[tokio::test]
async fn forget_names_events_in_memory_ids_never_an_empty_selector() {
    let (endpoint, state) = direct_double().await;
    let engine = direct_engine(&endpoint);
    for item in sample_items() {
        engine.store(item).await.unwrap();
    }
    let missing = engine
        .forget(ForgetTarget::Ids(vec!["0".repeat(40).into()]))
        .await
        .unwrap();
    assert_eq!(missing.forgotten, 0);
    assert!(
        state.seen.lock().unwrap().forgets.is_empty(),
        "nothing to remove sends nothing"
    );
    engine
        .forget(ForgetTarget::Filter(MetaFilter::kinds([
            tinymemory_api::ItemKind::Conversation,
        ])))
        .await
        .unwrap();
    let seen = state.seen.lock().unwrap();
    assert_eq!(seen.forgets.len(), 1);
    let body = &seen.forgets[0];
    assert_eq!(body["selector"]["memory_ids"].as_array().unwrap().len(), 3);
    assert!(body.get("confirm_all").is_none());
    assert_eq!(body["layers"], serde_json::json!(["events"]));
}

#[tokio::test]
async fn forget_batches_event_ids_at_one_hundred() {
    let (endpoint, state) = direct_double().await;
    let engine = direct_engine(&endpoint);
    let turns: Vec<Turn> = (0..230)
        .map(|i| Turn::new(Role::User, format!("t{i}")))
        .collect();
    let item = StoreItem::Conversation {
        turns,
        meta: thread_meta("big"),
    };
    engine.store(item.clone()).await.unwrap();
    let report = engine
        .forget(ForgetTarget::Ids(vec![item.fingerprint().into()]))
        .await
        .unwrap();
    assert_eq!(report.forgotten, 1);
    let sizes: Vec<usize> = state
        .seen
        .lock()
        .unwrap()
        .forgets
        .iter()
        .map(|b| b["selector"]["memory_ids"].as_array().unwrap().len())
        .collect();
    assert_eq!(sizes, vec![100, 100, 30]);
    assert_eq!(state.event_count(), 0);
}

#[tokio::test]
async fn every_forget_names_the_cascade_that_removes_the_events() {
    // CortexDB's default cascade (`derived_only`) keeps the events, so a
    // forget that left it to the default would delete nothing written.
    let (endpoint, state) = direct_double().await;
    let engine = direct_engine(&endpoint);
    let item = sample_items().remove(0);
    engine.store(item.clone()).await.unwrap();
    engine
        .forget(ForgetTarget::Ids(vec![item.fingerprint().into()]))
        .await
        .unwrap();
    let forgets = state.seen.lock().unwrap().forgets.clone();
    assert!(!forgets.is_empty());
    for body in &forgets {
        assert_eq!(body["cascade"], "redact_events", "{body}");
    }
    assert_eq!(state.event_count(), 0);
}

#[tokio::test]
async fn a_partially_applied_conversation_completes_on_retry() {
    let (endpoint, state) = direct_double().await;
    let engine = direct_engine(&endpoint);
    let item = sample_items().remove(1);
    state.fail_nth_experience.store(3, Ordering::SeqCst);
    let error = engine.store(item.clone()).await.unwrap_err();
    assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");
    assert_eq!(state.event_count(), 2);

    state.fail_nth_experience.store(0, Ordering::SeqCst);
    let receipt = engine.store(item.clone()).await.unwrap();
    assert!(!receipt.replayed);
    assert_eq!(state.event_count(), 3, "only the missing turn was written");
    let listed = engine
        .list(ListRequest::new(MetaFilter::default(), 5))
        .await
        .unwrap();
    assert_eq!(listed.items[0].text, item.render_text());
}

#[tokio::test]
async fn a_failed_write_is_sent_once_and_reads_retry() {
    let (endpoint, state) = direct_double().await;
    let engine = direct_engine(&endpoint);
    state.claim_then_fail.store(1, Ordering::SeqCst);
    let error = engine.store(sample_items().remove(0)).await.unwrap_err();
    assert!(matches!(error, Error::Unavailable(_)), "{error:?}");
    assert_eq!(
        state.count("POST /v1/experience"),
        1,
        "a write is never retried"
    );

    state.rate_limit_events.store(2, Ordering::SeqCst);
    let listed = engine
        .list(ListRequest::new(MetaFilter::default(), 5))
        .await
        .unwrap();
    assert!(listed.items.is_empty());
}

/// Whether an error is the expected variant.
type ErrorCheck = fn(&Error) -> bool;

#[tokio::test]
async fn statuses_map_onto_the_contract() {
    let (endpoint, state) = direct_double().await;
    let engine = direct_engine(&endpoint);
    let cases: [(u16, ErrorCheck); 6] = [
        (401, |e| matches!(e, Error::Unauthorized(_))),
        (404, |e| matches!(e, Error::NotFound(_))),
        (422, |e| matches!(e, Error::InvalidRequest(_))),
        (409, |e| matches!(e, Error::Conflict(_))),
        (500, |e| matches!(e, Error::Unavailable(_))),
        (402, |e| matches!(e, Error::Engine(_))),
    ];
    for (code, check) in cases {
        *state.fail_all.lock().unwrap() = Some((code, "X"));
        let error = engine
            .list(ListRequest::new(MetaFilter::default(), 1))
            .await
            .unwrap_err();
        assert!(check(&error), "{code}: {error:?}");
        assert!(
            !error
                .to_string()
                .contains(crate::cortex::testing::TEST_TOKEN)
        );
    }
}

#[tokio::test]
async fn a_rejected_key_is_unauthorized() {
    let (endpoint, state) = direct_double().await;
    *state.accept_token.lock().unwrap() = Some("another-key".into());
    let error = direct_engine(&endpoint)
        .store(sample_items().remove(0))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Unauthorized(_)), "{error:?}");
    assert!(error.to_string().contains("API key"), "{error}");
}

#[tokio::test]
async fn an_accepted_write_neither_asks_for_indexing_nor_waits_to_be_listed() {
    let (endpoint, state) = direct_double().await;
    let engine = direct_engine(&endpoint);
    let before = state.count("GET /v1/events");
    // Listings never show the write: a visible store would time out here.
    state.hide_listing_for.store(usize::MAX, Ordering::SeqCst);
    let mut items = sample_items();
    let conversation = items.remove(1);
    let document = items.remove(0);
    for item in [document, conversation] {
        let receipt = engine
            .clone()
            .with_test_timing(std::time::Duration::from_millis(50))
            .store_with(item.clone(), tinymemory_api::WriteOptions::accepted())
            .await
            .unwrap();
        assert_eq!(receipt.id.as_str(), item.fingerprint());
    }
    let requests = state.requests();
    assert!(
        requests.iter().any(|r| r == "POST /v1/experience"),
        "{requests:?}"
    );
    assert!(
        requests.iter().any(|r| r == "POST /v1/experience/bulk"),
        "{requests:?}"
    );
    assert!(requests.iter().all(|r| !r.contains("wait=indexed")));
    assert_eq!(
        state.count("GET /v1/events") - before,
        3,
        "one document lookup and a conversation lookup plus thread fallback; no visibility polling"
    );
}

/// The race a concurrent scope registration causes, through the engine's
/// own read paths: a store's replay lookup and a list wait out up to five
/// `503 AUTHORIZATION_STATE_CHANGED` answers, and a sixth gives up with the
/// code leading the error.
#[tokio::test]
async fn reads_wait_out_a_scope_authorization_change() {
    let (endpoint, state) = direct_double().await;
    let engine = direct_engine(&endpoint);
    state.state_change_events.store(5, Ordering::SeqCst);
    engine.store(sample_items().remove(0)).await.unwrap();
    assert_eq!(state.state_change_events.load(Ordering::SeqCst), 0);

    state.state_change_events.store(5, Ordering::SeqCst);
    let listed = engine
        .list(ListRequest::new(MetaFilter::default(), 5))
        .await
        .unwrap();
    assert_eq!(listed.items.len(), 1);

    state.state_change_events.store(6, Ordering::SeqCst);
    let before = state.count("GET /v1/events");
    let error = engine
        .list(ListRequest::new(MetaFilter::default(), 5))
        .await
        .unwrap_err();
    assert!(
        matches!(&error, Error::Unavailable(m) if m.starts_with("[AUTHORIZATION_STATE_CHANGED]")),
        "{error:?}"
    );
    assert_eq!(
        state.count("GET /v1/events") - before,
        6,
        "six attempts, then it gives up"
    );
}

#[test]
fn the_double_refuses_a_cascade_that_is_not_a_known_string() {
    let mut log = crate::cortex::testing::CortexLog::default();
    for cascade in [
        serde_json::json!(null),
        serde_json::json!(123),
        serde_json::json!("nope"),
    ] {
        let (code, answer) = log.forget(&serde_json::json!({
            "scope": "app:x", "confirm_all": true, "cascade": cascade
        }));
        assert_eq!(code, 400, "{cascade}: {answer}");
    }
}
