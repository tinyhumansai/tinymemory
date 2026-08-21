//! Mem0 adapter contract tests over its native HTTP shapes.

#![allow(clippy::expect_used)]

use std::sync::{Arc, Mutex};

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post, put},
    Json, Router,
};
use serde_json::{json, Value};
use tinymemory_api::{
    capabilities::Capability,
    provider::{MemoryCore, MemoryProvider, MemoryRecall},
    recall::OwnedRecallOpts,
    types::{MemoryCategory, MemoryTaint},
};

#[derive(Clone, Default)]
struct AppState(Arc<Mutex<Vec<Value>>>, Arc<Mutex<Vec<String>>>);

async fn list(
    State(state): State<AppState>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> Json<Value> {
    let query = query.unwrap_or_default();
    state.1.lock().expect("query lock").push(query.clone());
    // Honour the user_id filter the way the OSS server does (issue #69): a
    // scoped request must not receive the whole store back.
    let user = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("user_id="))
        .map(str::to_owned);
    let rows = state.0.lock().expect("state lock").clone();
    let rows = match user {
        Some(ref encoded) => rows
            .into_iter()
            .filter(|row| {
                row["metadata"]["tinymemory_namespace"]
                    .as_str()
                    .map(|ns| super::percent_encode_query(ns) == *encoded)
                    == Some(true)
            })
            .collect(),
        None => rows,
    };
    Json(json!({"results": rows}))
}

async fn add(State(state): State<AppState>, Json(body): Json<Value>) -> Json<Value> {
    let mut records = state.0.lock().expect("state lock");
    let id = format!("mem-{}", records.len() + 1);
    records.push(json!({
        "id": id,
        "memory": body.pointer("/messages/0/content"),
        "metadata": body.get("metadata"),
        "created_at": "2026-08-12T00:00:00Z"
    }));
    Json(json!({"results": [{"id": id}]}))
}

async fn update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> StatusCode {
    if let Some(record) = state
        .0
        .lock()
        .expect("state lock")
        .iter_mut()
        .find(|record| record["id"] == id)
    {
        record["memory"] = body["text"].clone();
        record["metadata"] = body["metadata"].clone();
    }
    StatusCode::OK
}

async fn remove(State(state): State<AppState>, Path(id): Path<String>) -> StatusCode {
    state
        .0
        .lock()
        .expect("state lock")
        .retain(|record| record["id"] != id);
    StatusCode::OK
}

async fn search(State(state): State<AppState>) -> Json<Value> {
    let mut records = state.0.lock().expect("state lock").clone();
    for record in &mut records {
        record["score"] = json!(0.9);
    }
    Json(json!({"results": records}))
}

#[tokio::test]
async fn native_mem0_round_trips_the_tinymemory_contract() {
    let state = AppState::default();
    let app = Router::new()
        .route("/memories", get(list).post(add))
        .route("/memories/{id}", put(update).delete(remove))
        .route("/search", post(search))
        .route("/api/health", get(|| async { StatusCode::OK }))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let endpoint = format!("http://{}", listener.local_addr().expect("address"));
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });

    let memory = super::Mem0Memory::new(&endpoint, None).expect("client");
    let driver = crate::mem0_provider(memory);
    tinymemory_api::provider::audit_provider(&driver).expect("honest capabilities");
    driver
        .store(
            "people",
            "alice",
            "likes tea",
            MemoryCategory::Core,
            Some("s1"),
            MemoryTaint::ExternalSync,
        )
        .await
        .expect("store");
    driver
        .store(
            "people",
            "alice",
            "likes coffee",
            MemoryCategory::Daily,
            Some("s2"),
            MemoryTaint::Internal,
        )
        .await
        .expect("upsert");
    let entry = driver
        .get("people", "alice")
        .await
        .expect("get")
        .expect("entry");
    assert_eq!(entry.content, "likes coffee");
    assert_eq!(entry.category, MemoryCategory::Daily);
    let hits = driver
        .recall(
            "coffee",
            2,
            &OwnedRecallOpts {
                namespace: Some("people".into()),
                ..OwnedRecallOpts::default()
            },
            None,
        )
        .await
        .expect("recall");
    assert_eq!(hits.len(), 1);
    assert!(driver.forget("people", "alice").await.expect("forget"));
    assert!(!driver
        .forget("people", "alice")
        .await
        .expect("forget again"));
    assert!(driver.health().await.is_usable());
}

#[tokio::test]
async fn mem0_graph_filters_limits_and_exposes_only_supported_operations() {
    let state = AppState::default();
    let app = Router::new()
        .route("/memories", get(list).post(add))
        .route("/memories/{id}", put(update).delete(remove))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let endpoint = format!("http://{}", listener.local_addr().expect("address"));
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });

    let provider = crate::mem0_graph_provider(
        super::Mem0Memory::self_hosted(&endpoint, None).expect("client"),
    );
    tinymemory_api::provider::audit_provider(&provider).expect("honest graph capability");
    assert_eq!(provider.driver_id(), crate::MEM0_DRIVER_ID);
    assert!(provider.capabilities().contains(Capability::Graph));

    provider
        .store(
            "team",
            "first",
            "Alice met Bob. Alice introduced Carol.",
            MemoryCategory::Core,
            None,
            MemoryTaint::Internal,
        )
        .await
        .expect("delegated store");
    provider
        .store(
            "other",
            "second",
            "Alice met Mallory.",
            MemoryCategory::Core,
            None,
            MemoryTaint::Internal,
        )
        .await
        .expect("other namespace store");

    let graph = provider.as_graph().expect("advertised graph");
    let relations = graph
        .relations(Some("team"), Some("Alice"), Some("co_occurs_with"), 1)
        .await
        .expect("relations");
    assert_eq!(relations.len(), 1, "the caller's limit is a hard cap");
    assert_eq!(relations[0].subject, "Alice");
    assert_eq!(relations[0].object, "Bob");
    assert_eq!(relations[0].namespace.as_deref(), Some("team"));
    assert_eq!(relations[0].document_ids, ["mem-1"]);
    assert_eq!(relations[0].attrs["source"], "heuristic");
    assert!(graph
        .relations(Some("team"), None, Some("does_not_exist"), 10)
        .await
        .expect("predicate filter")
        .is_empty());
    assert!(graph
        .relations(Some("team"), None, None, 0)
        .await
        .expect("zero limit")
        .is_empty());

    let relation = relations[0].clone();
    let errors = [
        graph.kv_get(Some("team"), "key").await.err(),
        graph
            .kv_put(Some("team"), "key", json!({"value": 1}))
            .await
            .err(),
        graph.kv_delete(Some("team"), "key").await.err(),
        graph.kv_list(Some("team"), None, 10).await.err(),
        graph.put_relation(relation).await.err(),
    ];
    assert!(errors.iter().all(Option::is_some));
    assert!(format!("{:#}", errors[0].as_ref().expect("kv error"))
        .contains("no generic key/value store"));
    assert!(format!("{:#}", errors[4].as_ref().expect("relation error"))
        .contains("cannot be edited directly"));
}

#[tokio::test]
async fn mem0_graph_propagates_listing_failures() {
    let app = Router::new().route(
        "/memories",
        get(|| async { (StatusCode::BAD_REQUEST, "invalid namespace") }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let endpoint = format!("http://{}", listener.local_addr().expect("address"));
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });

    let provider = crate::mem0_graph_provider(
        super::Mem0Memory::self_hosted(&endpoint, None).expect("client"),
    );
    let error = provider
        .as_graph()
        .expect("graph")
        .relations(Some("team"), None, None, 10)
        .await
        .expect_err("listing failure must propagate");
    let rendered = format!("{error:#}");
    assert!(rendered.contains("HTTP 400"), "{rendered}");
    assert!(rendered.contains("invalid namespace"), "{rendered}");
}

/// Issue #69: a self-hosted keyed read scopes the listing to the namespace's
/// `user_id` — percent-encoded, since namespaces carry slashes — instead of
/// walking the whole store.
#[tokio::test]
async fn self_hosted_keyed_reads_scope_by_user_id() {
    let state = AppState::default();
    let app = Router::new()
        .route("/memories", get(list).post(add))
        .route("/memories/{id}", put(update).delete(remove))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let endpoint = format!("http://{}", listener.local_addr().expect("address"));
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    let driver = crate::mem0_provider(
        super::Mem0Memory::self_hosted(&endpoint, Some("token")).expect("client"),
    );
    driver
        .store(
            "oc/team a",
            "decision",
            "content",
            MemoryCategory::Core,
            None,
            MemoryTaint::Internal,
        )
        .await
        .expect("store");
    state.1.lock().expect("query lock").clear();

    let got = driver.get("oc/team a", "decision").await.expect("get");
    assert!(
        got.is_some(),
        "the scoped listing must still find the record"
    );
    let queries = state.1.lock().expect("query lock").clone();
    assert_eq!(queries.len(), 1, "one scoped request: {queries:?}");
    assert!(
        queries[0].contains("user_id=oc%2Fteam%20a"),
        "the namespace rides percent-encoded: {queries:?}"
    );
}

/// Issue #69: the hosted platform's keyed lookup is ONE filtered request
/// carrying the metadata key — and verify-after-resolve refuses a server
/// that answers with someone else's record instead of honoring the filter.
#[tokio::test]
async fn cloud_keyed_lookup_filters_by_metadata_and_verifies() {
    use axum::routing::post;
    let bodies: Arc<Mutex<Vec<Value>>> = Arc::default();
    let answer: Arc<Mutex<Value>> = Arc::default();
    let next_cursor: Arc<Mutex<Value>> = Arc::new(Mutex::new(Value::Null));
    let captured = bodies.clone();
    let served = answer.clone();
    let cursor = next_cursor.clone();
    let deletes: Arc<Mutex<Vec<String>>> = Arc::default();
    let removed = deletes.clone();
    let app = Router::new()
        .route(
            "/v3/memories/",
            post(move |Json(body): Json<Value>| {
                let captured = captured.clone();
                let served = served.clone();
                async move {
                    captured.lock().expect("bodies").push(body);
                    Json(json!({
                        "results": served.lock().expect("answer").clone(),
                        "next": cursor.lock().expect("cursor").clone(),
                    }))
                }
            }),
        )
        .route(
            "/v1/memories/{id}/",
            axum::routing::delete(move |Path(id): Path<String>| {
                let removed = removed.clone();
                async move {
                    removed.lock().expect("deletes").push(id);
                    StatusCode::NO_CONTENT
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let endpoint = format!("http://{}", listener.local_addr().expect("address"));
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    let record = |ns: &str, key: &str| {
        json!({"id": "mem-1", "memory": "content", "metadata": {
            "tinymemory_namespace": ns,
            "tinymemory_key": key,
            "tinymemory_category": "core",
            "tinymemory_taint": "internal",
        }})
    };

    let driver = crate::mem0_provider(super::Mem0Memory::api(&endpoint, "key").expect("client"));
    *answer.lock().expect("answer") = json!([record("project", "decision")]);
    let got = driver.get("project", "decision").await.expect("get");
    assert!(got.is_some());
    let sent = bodies.lock().expect("bodies").clone();
    assert_eq!(sent.len(), 1, "one filtered request, no account walk");
    let clauses = sent[0]["filters"]["AND"].as_array().expect("AND clauses");
    assert!(
        clauses
            .iter()
            .any(|c| c["metadata"]["tinymemory_key"] == json!("decision")),
        "the filter carries the key: {sent:?}"
    );

    // A server that ignores the metadata clause answers with the whole
    // namespace: the asked-for record must still resolve even when a sibling
    // rides ahead of it in the page.
    *answer.lock().expect("answer") = json!([
        record("project", "someone-elses"),
        record("project", "decision"),
    ]);
    let got = driver
        .get("project", "decision")
        .await
        .expect("degraded-filter get")
        .expect("record present in the degraded page");
    assert_eq!(got.key, "decision", "the exact match wins, not the sibling");

    // A server answering ONLY foreign records: refuse loudly.
    *answer.lock().expect("answer") = json!([record("project", "someone-elses")]);
    let err = driver.get("project", "decision").await;
    assert!(
        err.is_err(),
        "a mismatched filtered answer must refuse, not serve another record"
    );

    // Issue #75 (re-landing the #71 review's M2, dropped by the crates-layout
    // merge): a FULL page of records none of which even decode is
    // inconclusive, not absent — the record may sit past page 1 of a
    // filter-dropping server's account.
    let junk: Vec<Value> = (0..200)
        .map(|n| json!({"id": format!("foreign-{n}"), "memory": "not ours", "metadata": {}}))
        .collect();
    *answer.lock().expect("answer") = json!(junk);
    let err = driver.get("project", "decision").await;
    assert!(
        err.is_err(),
        "a full undecodable page must refuse, not report absent"
    );

    // A SHORT page of undecodable records IS a trustworthy absent — but only
    // under a terminal cursor: the server returned everything it had and
    // ours was not among it.
    *answer.lock().expect("answer") =
        json!([{"id": "foreign-1", "memory": "not ours", "metadata": {}}]);
    let got = driver
        .get("project", "decision")
        .await
        .expect("short undecodable page");
    assert!(got.is_none(), "a short terminal page proves absence");

    // The same short undecodable page with a NON-NULL `next` admits more
    // pages exist (a server paginating below the requested size): absence is
    // not proven, refuse.
    *next_cursor.lock().expect("cursor") = json!("https://api.mem0.ai/v3/memories/?page=2");
    let err = driver.get("project", "decision").await;
    assert!(
        err.is_err(),
        "a short undecodable page with a live cursor must refuse, not report absent"
    );
    *next_cursor.lock().expect("cursor") = Value::Null;

    // An EMPTY page is exhaustion regardless of the cursor — `cloud_walk`'s
    // own rule, mirrored so get/forget agree with list about one response
    // shape: a filter-honoring server with the record would have put it on
    // this first filtered page, so zero results is the trustworthy absent.
    *answer.lock().expect("answer") = json!([]);
    *next_cursor.lock().expect("cursor") = json!("https://api.mem0.ai/v3/memories/?page=2");
    let got = driver
        .get("project", "decision")
        .await
        .expect("empty page with a live cursor is still absence, not an error");
    assert!(got.is_none());
    assert!(
        !driver
            .forget("project", "decision")
            .await
            .expect("forget of an absent key answers false, not an error"),
        "forget must report false for an absent key"
    );
    *next_cursor.lock().expect("cursor") = Value::Null;

    // Issue #75: delete rides the SAME keyed seam — one filtered resolve
    // carrying the metadata key, then one DELETE by id. No account walk.
    *answer.lock().expect("answer") = json!([record("project", "decision")]);
    let before = bodies.lock().expect("bodies").len();
    let removed_ok = driver.forget("project", "decision").await.expect("forget");
    assert!(removed_ok);
    let sent = bodies.lock().expect("bodies").clone();
    assert_eq!(
        sent.len(),
        before + 1,
        "delete resolves with exactly one filtered request"
    );
    let clauses = sent[before]["filters"]["AND"].as_array().expect("AND");
    assert!(
        clauses
            .iter()
            .any(|c| c["metadata"]["tinymemory_key"] == json!("decision")),
        "the delete's resolve carries the key filter: {sent:?}"
    );
    assert_eq!(
        deletes.lock().expect("deletes").as_slice(),
        ["mem-1".to_owned()],
        "one DELETE by resolved id"
    );
}
