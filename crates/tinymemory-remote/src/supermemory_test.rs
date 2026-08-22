//! Supermemory adapter contract tests over its native HTTP shapes.

#![allow(clippy::expect_used)]

use std::sync::{Arc, Mutex};

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use tinymemory_api::{
    error::MemoryError,
    provider::{MemoryCore, MemoryProvider, MemoryRecall},
    recall::OwnedRecallOpts,
    traits::Memory,
    types::{MemoryCategory, MemoryTaint},
};

#[derive(Default)]
struct Fixture {
    records: Vec<Value>,
    container_tags: Vec<String>,
    last_search_tag: Option<String>,
    /// Every /v4/memories/list request body, for the issue #69 scoping
    /// assertions: a namespace-scoped read must ask for ONE tag.
    list_bodies: Vec<Value>,
}

#[derive(Clone, Default)]
struct AppState(Arc<Mutex<Fixture>>);

async fn tags(State(state): State<AppState>) -> Json<Value> {
    let fixture = state.0.lock().expect("state lock");
    Json(Value::Array(
        fixture
            .container_tags
            .iter()
            .map(|tag| json!({"containerTag": tag}))
            .collect(),
    ))
}

async fn list(State(state): State<AppState>, Json(body): Json<Value>) -> Json<Value> {
    let mut fixture = state.0.lock().expect("state lock");
    fixture.list_bodies.push(body);
    Json(json!({"memoryEntries": fixture.records, "pagination": {"totalPages": 1}}))
}
async fn add(State(state): State<AppState>, Json(body): Json<Value>) -> Json<Value> {
    let mut fixture = state.0.lock().expect("state lock");
    let id = format!("doc-{}", fixture.records.len() + 1);
    let container_tag = body["containerTag"].as_str().expect("container tag");
    if !fixture
        .container_tags
        .iter()
        .any(|tag| tag == container_tag)
    {
        fixture.container_tags.push(container_tag.to_owned());
    }
    fixture.records.push(json!({
        "id": id,
        "memory": body["memories"][0]["content"],
        "metadata": body["memories"][0]["metadata"],
        "containerTag": container_tag,
        "createdAt": "2026-08-12T00:00:00Z",
        "isLatest": true,
        "isForgotten": false
    }));
    Json(json!({"memories": [{"id": id}]}))
}
async fn update(State(state): State<AppState>, Json(body): Json<Value>) -> StatusCode {
    // The real PATCH /v4/memories requires `containerTag` ("Required to scope
    // the operation") and 400s without it — a double that accepted a tagless
    // PATCH hid exactly the adapter regression issue #75 found. And the VALUE
    // matters as much as the presence: the tag scopes the operation, so a
    // PATCH carrying another container's tag is a lost update or a
    // cross-container write — refuse a mismatch instead of filing it.
    let Some(sent_tag) = body["containerTag"].as_str().filter(|tag| !tag.is_empty()) else {
        return StatusCode::BAD_REQUEST;
    };
    let id = body["id"].as_str().unwrap_or_default();
    if let Some(record) = state
        .0
        .lock()
        .expect("state lock")
        .records
        .iter_mut()
        .find(|r| r["id"] == id)
    {
        if record["containerTag"] != sent_tag {
            return StatusCode::BAD_REQUEST;
        }
        record["memory"] = body["newContent"].clone();
        record["metadata"] = body["metadata"].clone();
    }
    StatusCode::OK
}
async fn remove(State(state): State<AppState>, Json(body): Json<Value>) -> StatusCode {
    let id = body["id"].as_str().unwrap_or_default();
    state
        .0
        .lock()
        .expect("state lock")
        .records
        .retain(|r| r["id"] != id);
    StatusCode::OK
}
async fn search(State(state): State<AppState>, Json(body): Json<Value>) -> Json<Value> {
    let mut fixture = state.0.lock().expect("state lock");
    fixture.last_search_tag = body["containerTag"].as_str().map(str::to_owned);
    let results = fixture.records.iter().map(|r| json!({"id": r["id"], "memory": r["memory"], "metadata": r["metadata"], "similarity": 0.95})).collect::<Vec<_>>();
    Json(json!({"results": results}))
}

async fn capture_auth(State(state): State<Arc<Mutex<Value>>>, headers: HeaderMap) -> StatusCode {
    *state.lock().expect("state lock") = json!({
        "authorization": headers
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
    });
    StatusCode::OK
}

#[tokio::test]
async fn supermemory_supports_provided_and_self_hosted_apis() {
    let captured = Arc::new(Mutex::new(Value::Null));
    // The health probe now proves auth + data plane via the container-tags
    // list (§U4), so that is where the capture sits — a bare root `/` no
    // longer receives the probe.
    let app = Router::new()
        .route("/v3/container-tags/list", get(capture_auth))
        .with_state(captured.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let endpoint = format!("http://{}", listener.local_addr().expect("address"));
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });

    for client in [
        super::SupermemoryMemory::api(&endpoint, "provided-secret").expect("api client"),
        super::SupermemoryMemory::self_hosted(&endpoint, "provided-secret")
            .expect("self-hosted client"),
    ] {
        assert!(client.health_check().await);
        let headers = captured.lock().expect("state lock").clone();
        assert_eq!(headers["authorization"], "Bearer provided-secret");
        assert!(!format!("{client:?}").contains("provided-secret"));
    }
    assert!(super::SupermemoryMemory::api(&endpoint, "").is_err());
}

#[test]
fn supermemory_container_tags_cover_arbitrary_contract_namespaces() {
    let unusual = format!("tenant / 🧠 / {}", "x".repeat(500));
    let tag = super::SupermemoryDialect::container_tag(&unusual);

    assert!(tag.starts_with("tinymemory:tm_"));
    assert!(tag.len() <= 100);
    assert!(tag
        .bytes()
        .all(|byte| { byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b':' | b'-') }));
    assert_eq!(tag, super::SupermemoryDialect::container_tag(&unusual));
}

#[tokio::test]
async fn native_supermemory_round_trips_the_tinymemory_contract() {
    let state = AppState::default();
    let app = Router::new()
        .route("/v3/container-tags/list", get(tags))
        .route("/v4/memories/list", post(list))
        .route("/v4/memories", post(add).patch(update).delete(remove))
        .route("/v4/search", post(search))
        .route("/", get(|| async { StatusCode::OK }))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let endpoint = format!("http://{}", listener.local_addr().expect("address"));
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });

    let driver = crate::supermemory_provider(
        super::SupermemoryMemory::self_hosted(&endpoint, "secret").expect("client"),
    );
    tinymemory_api::provider::audit_provider(&driver).expect("honest capabilities");
    driver
        .store(
            "project",
            "decision",
            "use Rust",
            MemoryCategory::Core,
            None,
            MemoryTaint::ExternalSync,
        )
        .await
        .expect("store");
    driver
        .store(
            "project",
            "decision",
            "use Rust 2024",
            MemoryCategory::Core,
            None,
            MemoryTaint::ExternalSync,
        )
        .await
        .expect("upsert");
    let entry = driver
        .get("project", "decision")
        .await
        .expect("get")
        .expect("entry");
    assert_eq!(entry.content, "use Rust 2024");
    assert_eq!(entry.taint, MemoryTaint::ExternalSync);
    let expected_tag = super::SupermemoryDialect::container_tag("project");
    assert_eq!(
        state.0.lock().expect("state lock").container_tags,
        vec![expected_tag.clone()]
    );
    assert_eq!(
        driver
            .recall(
                "Rust",
                1,
                &OwnedRecallOpts {
                    namespace: Some("project".into()),
                    ..OwnedRecallOpts::default()
                },
                None,
            )
            .await
            .expect("recall")
            .len(),
        1
    );
    assert_eq!(
        state.0.lock().expect("state lock").last_search_tag,
        Some(expected_tag)
    );
    // #68 review Major 2: min_score connected to the double's OWN response
    // shape — the first cut's strictness was only ever tested against
    // synthetic Option values, which is how a decode/emit field mismatch
    // dropped every hit. Below the double's similarity (0.95): survives.
    // Above it: drops. Semantics AND the decode, in one pair.
    let scored = |min: f64| {
        let driver = &driver;
        async move {
            driver
                .recall(
                    "Rust",
                    1,
                    &OwnedRecallOpts {
                        namespace: Some("project".into()),
                        min_score: Some(min),
                        ..OwnedRecallOpts::default()
                    },
                    None,
                )
                .await
                .expect("recall")
                .len()
        }
    };
    assert_eq!(
        scored(0.1).await,
        1,
        "a scored hit above the threshold survives"
    );
    assert_eq!(
        scored(0.99).await,
        0,
        "a scored hit below the threshold drops"
    );
    assert!(driver.forget("project", "decision").await.expect("forget"));
    assert!(driver.health().await.is_usable());
}

/// Issue #69: a keyed read asks the backend for ONE namespace's tag — never
/// the whole-account tag walk the pre-seam reads ran.
#[tokio::test]
async fn keyed_reads_scope_to_one_container_tag() {
    let state = AppState::default();
    let app = Router::new()
        .route("/v3/container-tags/list", get(tags))
        .route("/v4/memories/list", post(list))
        .route("/v4/memories", post(add).patch(update).delete(remove))
        .route("/", get(|| async { StatusCode::OK }))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let endpoint = format!("http://{}", listener.local_addr().expect("address"));
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    let driver = crate::supermemory_provider(
        super::SupermemoryMemory::self_hosted(&endpoint, "secret").expect("client"),
    );
    driver
        .store(
            "project",
            "decision",
            "use Rust 2024",
            MemoryCategory::Core,
            None,
            MemoryTaint::Internal,
        )
        .await
        .expect("store");
    state.0.lock().expect("state lock").list_bodies.clear();

    driver.get("project", "decision").await.expect("get");
    let bodies = state.0.lock().expect("state lock").list_bodies.clone();
    assert_eq!(bodies.len(), 1, "one scoped list request, not a tag walk");
    let expected = super::SupermemoryDialect::container_tag("project");
    assert_eq!(
        bodies[0]["containerTags"],
        serde_json::json!([expected]),
        "the request names exactly the namespace's tag"
    );
}

/// Spawns the Supermemory double and returns its fixture beside a driver.
///
/// The refusal tests below assert on what the double *did not* receive, so
/// they need the fixture as much as the driver.
async fn spawn_double() -> (AppState, impl MemoryProvider) {
    let state = AppState::default();
    let app = Router::new()
        .route("/v3/container-tags/list", get(tags))
        .route("/v4/memories/list", post(list))
        .route("/v4/memories", post(add).patch(update).delete(remove))
        .route("/v4/search", post(search))
        .route("/", get(|| async { StatusCode::OK }))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let endpoint = format!("http://{}", listener.local_addr().expect("address"));
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    let driver = crate::supermemory_provider(
        super::SupermemoryMemory::self_hosted(&endpoint, "secret").expect("client"),
    );
    (state, driver)
}

/// Issue #80: Supermemory removes NUL and U+FFFD from `content` server-side.
///
/// `MemoryCore::store` promises the content read back equals the content
/// stored, so the adapter must refuse rather than store a value the service
/// will quietly rewrite. `Invalid` is the documented refusal class — what is
/// being rejected is the caller's input.
#[tokio::test]
async fn content_supermemory_would_alter_is_refused() {
    let (state, driver) = spawn_double().await;
    for (label, ch) in [("NUL", '\u{0}'), ("replacement char", '\u{FFFD}')] {
        let content = format!("a{ch}b");
        let result = driver
            .store(
                "project",
                "decision",
                &content,
                MemoryCategory::Core,
                None,
                MemoryTaint::Internal,
            )
            .await;
        assert!(
            matches!(result, Err(MemoryError::Invalid(_))),
            "{label}: content the service would alter must be refused as Invalid, \
             got {result:?}"
        );
    }
    // Refused before the request, not after a round trip: the service accepts
    // this content with a 201 and alters it, so an adapter that asked first
    // would have already lost the character by the time it could object.
    assert!(
        state.0.lock().expect("state lock").records.is_empty(),
        "the refusal must short-circuit before any write reaches the service"
    );
}

/// The refusal has to be actionable without re-emitting the character: a raw
/// NUL in an error string propagates into logs, terminals, and shells that
/// hide it, turning a clear refusal into a confusing one.
#[tokio::test]
async fn the_refusal_names_the_character_without_emitting_it() {
    let (_state, driver) = spawn_double().await;
    let result = driver
        .store(
            "project",
            "decision",
            "a\u{0}b",
            MemoryCategory::Core,
            None,
            MemoryTaint::Internal,
        )
        .await;
    assert!(result.is_err(), "content carrying a NUL must be refused");
    let message = result
        .err()
        .map(|error| error.to_string())
        .unwrap_or_default();
    assert!(
        message.contains("U+0000"),
        "the message names the character: {message}"
    );
    assert!(
        !message.contains('\u{0}'),
        "the message must not carry the character itself: {message:?}"
    );
}

/// A refusal must stay clean even when the identity is not.
///
/// Metadata is not sanitised, so a namespace or key may itself carry a NUL and
/// be stored perfectly happily. That makes the two halves meet here: content
/// the service would alter, under an identity that holds a control character.
/// The message interpolates the identity, so this is where a refusal would
/// leak the very character the naming exists to keep out of logs.
#[tokio::test]
async fn a_refusal_emits_no_control_character_from_the_identity_either() {
    let (_state, driver) = spawn_double().await;
    let result = driver
        .store(
            "project\u{0}alpha",
            "decision\u{0}beta",
            "a\u{0}b",
            MemoryCategory::Core,
            None,
            MemoryTaint::Internal,
        )
        .await;
    assert!(result.is_err(), "the content must still be refused");
    let message = result
        .err()
        .map(|error| error.to_string())
        .unwrap_or_default();
    assert!(
        !message.chars().any(|character| character.is_control()),
        "the refusal must carry no control character at all: {message:?}"
    );
    assert!(
        message.contains("project") && message.contains("decision"),
        "the identity is still named, just escaped: {message}"
    );
}

/// The predicate stays as narrow as the defect.
///
/// Only these two characters were measured as dropped; every other C0 control
/// — plus DEL, NEL, ZWSP, BOM and U+2028 — survives the live service intact.
/// A refusal that widened to "control characters" would reject content
/// Supermemory stores perfectly well, and `assert_awkward_content_round_trips`
/// would still pass while the driver quietly became less useful.
#[tokio::test]
async fn content_supermemory_preserves_is_still_stored() {
    let (state, driver) = spawn_double().await;
    for (label, content) in [
        ("C0 control", "a\u{1}b".to_string()),
        ("tab and newlines", "a\tb\nc\r\nd".to_string()),
        ("unicode", "héllo — 👋 まいど".to_string()),
        (
            "zero-width space and BOM",
            "a\u{200B}b\u{FEFF}c".to_string(),
        ),
    ] {
        let result = driver
            .store(
                "project",
                label,
                &content,
                MemoryCategory::Core,
                None,
                MemoryTaint::Internal,
            )
            .await;
        assert!(result.is_ok(), "{label} must still store: {result:?}");
    }
    assert_eq!(
        state.0.lock().expect("state lock").records.len(),
        4,
        "every preserved shape reached the service"
    );
}

/// The refusal is scoped to `content`, because the defect is.
///
/// Identity rides in `metadata`, which the live service does not sanitise:
/// `tinymemory_key` and `tinymemory_namespace` round-trip both characters
/// unchanged, so a key is never silently rewritten into another key's. Were
/// that untrue the failure would be worse than mangled content — a re-store
/// would stop matching its own record and duplicate it instead. This pins the
/// scoping to the measurement, so widening it later has to re-measure first.
#[tokio::test]
async fn identity_carrying_the_same_characters_is_not_refused() {
    let (_state, driver) = spawn_double().await;
    for (label, character) in [("nul", '\u{0}'), ("replacement", '\u{FFFD}')] {
        let namespace = format!("project{character}alpha");
        let key = format!("decision{character}beta");
        let content = format!("ordinary content for {label}");
        driver
            .store(
                &namespace,
                &key,
                &content,
                MemoryCategory::Core,
                None,
                MemoryTaint::Internal,
            )
            .await
            .expect("metadata is not sanitised, so identity is not refused");
        let stored = driver.get(&namespace, &key).await.expect("get");
        assert_eq!(
            stored.map(|entry| entry.content),
            Some(content),
            "{label}: the record is reachable under the identity it was stored with"
        );
    }
}
