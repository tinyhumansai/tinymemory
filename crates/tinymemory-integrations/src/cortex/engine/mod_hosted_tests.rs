//! TinyHumans wire behaviour: `memory/*` routes, the per-request bearer,
//! envelopes and codes, `Idempotency-Key` claims, the outcome-unknown
//! recovery, and riding out rate limits.

use super::*;
use crate::cortex::StaticBearer;
use crate::cortex::error::error_code;
use crate::cortex::testing::{hosted_double, hosted_engine, sample_items, serve};
use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use tinymemory_api::{FetchMode, MetaFilter};

#[tokio::test]
async fn every_operation_maps_to_a_memory_path_and_never_a_v1_one() {
    let (endpoint, state) = hosted_double().await;
    let engine = hosted_engine(&endpoint);
    for item in sample_items() {
        engine.store(item).await.unwrap();
    }
    engine
        .list(ListRequest::new(MetaFilter::default(), 10))
        .await
        .unwrap();
    engine
        .fetch(FetchRequest::new("helix", FetchMode::Hybrid, 5))
        .await
        .unwrap();
    engine
        .recall(RecallRequest::new("which editor", 3))
        .await
        .unwrap();
    assert_eq!(engine.health().await, EngineHealth::Ok);
    engine
        .forget(ForgetTarget::Ids(
            sample_items()
                .iter()
                .map(|i| i.fingerprint().into())
                .collect(),
        ))
        .await
        .unwrap();

    let shapes: std::collections::BTreeSet<String> = state
        .requests()
        .iter()
        .map(|request| {
            let (method, target) = request.split_once(' ').unwrap();
            let (path, query) = target.split_once('?').unwrap_or((target, ""));
            let mut keys: Vec<&str> = query
                .split('&')
                .filter(|p| !p.is_empty())
                .map(|p| p.split_once('=').map_or(p, |(k, _)| k))
                .collect();
            keys.sort_unstable();
            format!("{method} {path}?{}", keys.join(","))
        })
        .collect();
    let expected: std::collections::BTreeSet<String> = [
        "POST /memory/experience?",
        "GET /memory/events?labels,limit,scope",
        "GET /memory/events?limit,scope",
        "POST /memory/recall?",
        "POST /memory/answer?",
        "POST /memory/forget?",
        "GET /memory/scopes?limit,prefix",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    assert_eq!(shapes, expected);
    assert!(
        state
            .requests()
            .iter()
            .all(|r| !r.contains("/v1/") && !r.contains("wait=indexed"))
    );
}

#[tokio::test]
async fn the_health_probe_lists_one_scope_under_an_accepted_prefix() {
    let (endpoint, state) = hosted_double().await;
    assert_eq!(hosted_engine(&endpoint).health().await, EngineHealth::Ok);
    let requests = state.requests();
    assert_eq!(
        requests,
        vec!["GET /memory/scopes?prefix=tmh%3Aprobe&limit=1"]
    );
}

#[tokio::test]
async fn the_bearer_is_resolved_on_every_request() {
    struct Rotating(AtomicUsize);
    #[async_trait]
    impl BearerSource for Rotating {
        async fn bearer(&self) -> Result<String> {
            Ok(format!("jwt-{}", self.0.fetch_add(1, Ordering::SeqCst)))
        }
    }
    let (endpoint, state) = hosted_double().await;
    let engine =
        CortexEngine::tinyhumans(&endpoint, Arc::new(Rotating(AtomicUsize::new(0)))).unwrap();
    // An exact reach names its one scope, so each listing is one request.
    let filter = MetaFilter {
        reach: Some(tinymemory_api::Reach::exact(
            tinymemory_api::Namespace::ROOT,
        )),
        ..MetaFilter::kinds([tinymemory_api::ItemKind::Learning])
    };
    for _ in 0..3 {
        engine
            .list(ListRequest::new(filter.clone(), 1))
            .await
            .unwrap();
    }
    let auth = state.seen.lock().unwrap().auth.clone();
    assert_eq!(auth, vec!["Bearer jwt-0", "Bearer jwt-1", "Bearer jwt-2"]);
}

#[tokio::test]
async fn a_failed_blank_or_unsafe_bearer_is_unauthorized_without_a_request() {
    struct Broken;
    #[async_trait]
    impl BearerSource for Broken {
        async fn bearer(&self) -> Result<String> {
            Err(Error::Unauthorized("signed out".into()))
        }
    }
    let (endpoint, state) = hosted_double().await;
    let sources: [Arc<dyn BearerSource>; 3] = [
        Arc::new(Broken),
        Arc::new(StaticBearer::new("   ")),
        Arc::new(StaticBearer::new("abc\r\nX-Injected: 1")),
    ];
    for source in sources {
        let engine = CortexEngine::tinyhumans(&endpoint, source).unwrap();
        let error = engine
            .list(ListRequest::new(MetaFilter::default(), 1))
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Unauthorized(_)), "{error:?}");
        assert!(!error.to_string().contains("X-Injected"));
    }
    assert!(state.requests().is_empty());
}

#[tokio::test]
async fn credentialed_cleartext_is_refused_off_loopback() {
    let source = || Arc::new(StaticBearer::new("t")) as Arc<dyn BearerSource>;
    assert!(matches!(
        CortexEngine::tinyhumans("http://api.example.com", source()),
        Err(Error::Config(_))
    ));
    assert!(CortexEngine::tinyhumans("https://api.example.com", source()).is_ok());
    assert!(CortexEngine::tinyhumans("http://127.0.0.1:1", source()).is_ok());
}

#[tokio::test]
async fn a_402_is_insufficient_credits_and_codes_survive() {
    let (endpoint, state) = hosted_double().await;
    let engine = hosted_engine(&endpoint);
    *state.fail_all.lock().unwrap() = Some((402, "USER_INSUFFICIENT_CREDITS"));
    let error = engine.store(sample_items().remove(0)).await.unwrap_err();
    assert!(matches!(error, Error::InsufficientCredits(_)), "{error:?}");
    assert!(
        !error
            .to_string()
            .contains(crate::cortex::testing::TEST_TOKEN)
    );

    *state.fail_all.lock().unwrap() = Some((400, "VALIDATION_ERROR"));
    let error = engine
        .list(ListRequest::new(MetaFilter::default(), 1))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::InvalidRequest(_)));
    assert_eq!(error_code(&error), Some("VALIDATION_ERROR"));

    *state.fail_all.lock().unwrap() = None;
    *state.accept_token.lock().unwrap() = Some("another".into());
    let error = engine
        .list(ListRequest::new(MetaFilter::default(), 1))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Unauthorized(_)));
    assert_eq!(error_code(&error), Some("UNAUTHORIZED"));
}

#[tokio::test]
async fn a_429_on_a_read_is_retried_and_a_persistent_one_is_unavailable() {
    let (endpoint, state) = hosted_double().await;
    let engine = hosted_engine(&endpoint);
    state.rate_limit_events.store(2, Ordering::SeqCst);
    engine
        .list(ListRequest::new(
            MetaFilter::kinds([tinymemory_api::ItemKind::Document]),
            1,
        ))
        .await
        .unwrap();

    *state.fail_all.lock().unwrap() = Some((500, "INTERNAL"));
    let before = state.requests().len();
    let error = engine
        .list(ListRequest::new(MetaFilter::default(), 1))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Unavailable(_)));
    assert_eq!(error_code(&error), Some("INTERNAL"));
    assert_eq!(
        state.requests().len() - before,
        3,
        "a read is retried on 500"
    );
}

#[tokio::test]
async fn a_body_without_the_envelope_or_data_is_an_engine_error() {
    use axum::routing::get;
    use axum::{Json, Router};
    for body in [
        serde_json::json!({ "items": [] }),
        serde_json::json!({ "success": true }),
    ] {
        let app = Router::new().route(
            "/memory/events",
            get(move || {
                let body = body.clone();
                async move { Json(body) }
            }),
        );
        let error = hosted_engine(&serve(app).await)
            .list(ListRequest::new(MetaFilter::default(), 1))
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Engine(_)), "{error:?}");
    }
}

#[tokio::test]
async fn write_claims_are_random_per_call_and_never_the_content_key() {
    let (endpoint, state) = hosted_double().await;
    let engine = hosted_engine(&endpoint);
    for item in sample_items() {
        engine.store(item).await.unwrap();
    }
    let seen = state.seen.lock().unwrap();
    assert_eq!(seen.idempotency.len(), 5, "one write per event");
    let mut claims = HashSet::new();
    for (body_key, claim) in &seen.idempotency {
        let claim = claim.as_deref().unwrap();
        assert_ne!(Some(claim), body_key.as_deref());
        assert!(claim.starts_with("tm-") && claim.len() <= 128);
        assert!(claims.insert(claim.to_string()), "claim reused: {claim}");
    }
    assert_eq!(seen.answers.len(), 0);
}

#[tokio::test]
async fn a_write_applied_before_its_response_was_lost_is_recovered() {
    recovered_after_a_lost_response(false).await;
}

/// An older backend relays the refused retry as `400` + `CONFLICT`.
#[tokio::test]
async fn a_lost_response_is_recovered_through_a_legacy_400_conflict() {
    recovered_after_a_lost_response(true).await;
}

#[tokio::test]
async fn a_conflict_reaches_the_client_as_a_409_or_a_legacy_400() {
    for (status, code) in [(409, "CONFLICT"), (400, "CONFLICT")] {
        let (endpoint, state) = hosted_double().await;
        *state.fail_all.lock().unwrap() = Some((status, code));
        let error = hosted_engine(&endpoint)
            .list(tinymemory_api::ListRequest::new(MetaFilter::default(), 1))
            .await
            .unwrap_err();
        assert!(
            matches!(error, tinymemory_api::Error::Conflict(_)),
            "{status}: {error:?}"
        );
    }
}

async fn recovered_after_a_lost_response(legacy: bool) {
    let (endpoint, state) = hosted_double().await;
    state.legacy_conflict_400.store(legacy, Ordering::SeqCst);
    state.apply_then_fail.store(1, Ordering::SeqCst);
    let receipt = hosted_engine(&endpoint)
        .store(sample_items().remove(0))
        .await
        .unwrap();
    assert!(!receipt.replayed);
    assert_eq!(state.event_count(), 1, "the retry was never forwarded");
    let seen = state.seen.lock().unwrap();
    let claims: Vec<_> = seen.idempotency.iter().map(|(_, c)| c.clone()).collect();
    assert_eq!(claims.len(), 2);
    assert_eq!(
        claims[0], claims[1],
        "one write reuses its claim across retries"
    );
}

#[tokio::test]
async fn a_claimed_write_that_never_landed_is_outcome_unknown() {
    let (endpoint, state) = hosted_double().await;
    state.claim_then_fail.store(1, Ordering::SeqCst);
    let error = hosted_engine(&endpoint)
        .with_test_timing(std::time::Duration::from_millis(200))
        .store(sample_items().remove(2))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Unavailable(_)), "{error:?}");
    assert!(error.to_string().contains("unknown"), "{error}");
    assert_eq!(state.event_count(), 0);
}

#[tokio::test]
async fn recovery_waits_out_a_rate_limit_and_a_slow_listing() {
    let (endpoint, state) = hosted_double().await;
    state.apply_then_fail.store(1, Ordering::SeqCst);
    // The recovery's first listing is rate limited past the transport's three
    // attempts, and the next two polls do not show the event yet.
    *state.arm_after_write.lock().unwrap() = Some((3, 2));
    hosted_engine(&endpoint)
        .store(sample_items().remove(0))
        .await
        .unwrap();
    assert_eq!(state.event_count(), 1);
}

#[tokio::test]
async fn writes_and_forgets_ride_out_rate_limits() {
    let (endpoint, state) = hosted_double().await;
    let engine = hosted_engine(&endpoint);
    state.rate_limit_experience.store(2, Ordering::SeqCst);
    let item = sample_items().remove(2);
    engine.store(item.clone()).await.unwrap();
    assert_eq!(
        state.count("POST /memory/experience"),
        3,
        "two refusals, then accepted"
    );
    assert_eq!(state.event_count(), 1);

    state.rate_limit_forget.store(1, Ordering::SeqCst);
    let report = engine
        .forget(ForgetTarget::Ids(vec![item.fingerprint().into()]))
        .await
        .unwrap();
    assert_eq!(report.forgotten, 1);
    assert_eq!(
        state.count("POST /memory/forget"),
        2,
        "the limited removal was resent"
    );
    assert_eq!(state.event_count(), 0);
}

#[tokio::test]
async fn a_429_while_waiting_for_visibility_does_not_fail_the_write() {
    let (endpoint, state) = hosted_double().await;
    // Four 429s exhaust the first visibility poll's three attempts and leak
    // into the second: the write was accepted, so the wait keeps going.
    *state.arm_after_write.lock().unwrap() = Some((4, 0));
    hosted_engine(&endpoint)
        .store(sample_items().remove(0))
        .await
        .unwrap();
    assert_eq!(state.count("POST /memory/experience"), 1);
}

#[tokio::test]
async fn a_write_polls_until_its_event_is_listed() {
    let (endpoint, state) = hosted_double().await;
    *state.arm_after_write.lock().unwrap() = Some((0, 3));
    hosted_engine(&endpoint)
        .store(sample_items().remove(0))
        .await
        .unwrap();
    assert_eq!(
        state.count("GET /memory/events"),
        5,
        "the lookup, three hidden polls, the hit"
    );
}

#[tokio::test]
async fn a_conversation_is_ordered_single_writes_and_one_wait() {
    let (endpoint, state) = hosted_double().await;
    let engine = hosted_engine(&endpoint);
    let item = sample_items().remove(1);
    let current_id = item.fingerprint();
    engine.store(item.clone()).await.unwrap();
    let requests = state.requests();
    assert_eq!(state.count("POST /memory/experience"), 3, "{requests:?}");
    assert!(!requests.iter().any(|r| r.contains("bulk")));
    assert_eq!(
        state.count("GET /memory/events"),
        3,
        "current id lookup, changed-channel thread fallback, and one visibility wait"
    );
    let reads: Vec<_> = requests
        .iter()
        .enumerate()
        .filter(|(_, request)| request.starts_with("GET /memory/events"))
        .collect();
    let writes: Vec<_> = requests
        .iter()
        .enumerate()
        .filter(|(_, request)| request.starts_with("POST /memory/experience"))
        .collect();
    let first_write = writes.first().unwrap().0;
    let last_write = writes.last().unwrap().0;
    assert_eq!(reads.len(), 3);
    assert!(reads[0].0 < first_write && reads[1].0 < first_write);
    assert!(
        reads[0]
            .1
            .contains(&crate::cortex::envelope::labels::digest(&current_id))
    );
    assert!(
        reads[1]
            .1
            .contains(&crate::cortex::envelope::labels::digest("t-chat"))
    );
    assert!(
        reads[2].0 > last_write,
        "the final read confirms visibility"
    );
    assert!(
        reads.iter().all(|(_, request)| {
            request.contains("scope=app%3Atinymemory%2Fapp%3Aconversations")
        })
    );
}

#[tokio::test]
async fn a_partially_applied_conversation_completes_on_retry() {
    let (endpoint, state) = hosted_double().await;
    let engine = hosted_engine(&endpoint);
    let item = sample_items().remove(1);
    state.fail_nth_experience.store(3, Ordering::SeqCst);
    engine.store(item.clone()).await.unwrap_err();
    assert_eq!(state.event_count(), 2);
    state.fail_nth_experience.store(0, Ordering::SeqCst);
    engine.store(item).await.unwrap();
    assert_eq!(state.event_count(), 3, "only the missing turn is new");
}

#[tokio::test]
async fn the_answer_body_holds_only_keys_the_strict_schema_allows() {
    let (endpoint, _state) = hosted_double().await;
    let engine = hosted_engine(&endpoint);
    engine.recall(RecallRequest::new("q1", 2)).await.unwrap();
    let mut with = RecallRequest::new("q2", 2);
    with.instructions = Some("be brief".into());
    engine.recall(with).await.unwrap();
}

#[tokio::test]
async fn recovery_does_not_take_another_turn_with_the_same_words_for_the_lost_one() {
    use tinymemory_api::{MemoryMeta, Role, Turn};
    let (endpoint, state) = hosted_double().await;
    let engine = hosted_engine(&endpoint).with_test_timing(std::time::Duration::from_millis(200));
    let item = StoreItem::Conversation {
        turns: vec![
            Turn::new(Role::User, "ok"),
            Turn::new(Role::Assistant, "ok"),
        ],
        meta: MemoryMeta::default(),
    };
    engine.store(item.clone()).await.unwrap();
    // The second turn's event is lost.
    state
        .log
        .lock()
        .unwrap()
        .lose_last("app:tinymemory/app:conversations");
    // Its re-write is claimed but never applied, so recovery must look for
    // it, and must not take the first turn, which says the same words.
    state.claim_then_fail.store(1, Ordering::SeqCst);
    let error = engine.store(item).await.unwrap_err();
    assert!(matches!(error, Error::Unavailable(_)), "{error:?}");
    assert_eq!(state.event_count(), 1, "the lost turn is still missing");
}

/// A visibility timeout says what the polls saw, so a log tells a listing
/// that never showed the event from one that could not be read.
#[tokio::test]
async fn a_visibility_timeout_says_whether_the_listing_was_rate_limited() {
    let (endpoint, state) = hosted_double().await;
    // Every listing after the write answers 429.
    *state.arm_after_write.lock().unwrap() = Some((usize::MAX, 0));
    let limited = hosted_engine(&endpoint)
        .store(sample_items().remove(0))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        limited.contains("(0 polls answered without it"),
        "{limited}"
    );
    assert!(
        limited.contains("failed transiently, the last: "),
        "{limited}"
    );
    assert!(!limited.contains(" 0 failed transiently"), "{limited}");

    let (endpoint, state) = hosted_double().await;
    // Every listing after the write answers, without the event.
    *state.arm_after_write.lock().unwrap() = Some((0, usize::MAX));
    let missing = hosted_engine(&endpoint)
        .store(sample_items().remove(0))
        .await
        .unwrap_err()
        .to_string();
    assert!(missing.contains(" 0 failed transiently)"), "{missing}");
    assert!(
        !missing.contains("(0 polls answered without it"),
        "{missing}"
    );
}

#[tokio::test]
async fn a_forget_is_sent_in_batches_and_a_retry_that_finds_them_gone_succeeds() {
    use axum::http::StatusCode;
    use axum::routing::post;
    use axum::{Json, Router};
    use std::sync::Mutex;
    let bodies: Arc<Mutex<Vec<serde_json::Value>>> = Arc::default();
    let seen = bodies.clone();
    // Each batch's first send fails transiently; its retry finds the
    // events already gone (404), which is success for an idempotent removal.
    let app = Router::new().route(
        "/memory/forget",
        post(move |Json(body): Json<serde_json::Value>| {
            let seen = seen.clone();
            async move {
                let mut seen = seen.lock().unwrap();
                seen.push(body);
                if seen.len() % 2 == 1 {
                    (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({})))
                } else {
                    (StatusCode::NOT_FOUND, Json(serde_json::json!({})))
                }
            }
        }),
    );
    let engine = hosted_engine(&serve(app).await);
    let ids: Vec<String> = (0..250).map(|i| format!("e{i}")).collect();
    engine
        .log
        .forget_events("user:alice/app:learnings", &ids)
        .await
        .unwrap();
    let sizes: Vec<usize> = bodies
        .lock()
        .unwrap()
        .iter()
        .map(|body| body["selector"]["memory_ids"].as_array().unwrap().len())
        .collect();
    assert_eq!(sizes, vec![100, 100, 100, 100, 50, 50]);
}
