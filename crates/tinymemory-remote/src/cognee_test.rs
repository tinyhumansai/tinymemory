//! Cognee adapter contract tests over dataset, raw-file, and recall APIs.

#![allow(clippy::expect_used)]

use std::sync::{Arc, Mutex};

use axum::{
    extract::{Multipart, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{delete, get, patch, post},
    Json, Router,
};
use serde_json::{json, Value};
use tinymemory_api::{
    capabilities::Capability,
    provider::{MemoryCore, MemoryGraph, MemoryProvider, MemoryRecall},
    recall::OwnedRecallOpts,
    traits::Memory,
    types::{MemoryCategory, MemoryTaint},
};

#[derive(Clone, Default)]
struct AppState(
    Arc<Mutex<Option<Vec<u8>>>>,
    Arc<Mutex<CallCounts>>,
    /// The filename the adapter actually uploaded — served back in the data
    /// listing, because the issue #69 keyed path resolves BY that name. The
    /// old double hardcoded a name nothing ever read.
    Arc<Mutex<Option<String>>>,
);

/// Per-route request counters for the issue #69 fan-out assertions.
#[derive(Default, Clone, Copy)]
struct CallCounts {
    datasets: usize,
    listings: usize,
    raws: usize,
}

async fn datasets(State(state): State<AppState>) -> Json<Value> {
    state.1.lock().expect("counts").datasets += 1;
    let values = if state.0.lock().expect("state lock").is_some() {
        vec![json!({
            "id": "dataset-1",
            "name": super::CogneeDialect::dataset_name("project")
        })]
    } else {
        vec![]
    };
    Json(Value::Array(values))
}
async fn data(State(state): State<AppState>) -> Json<Value> {
    state.1.lock().expect("counts").listings += 1;
    let name = state.2.lock().expect("name lock").clone();
    let values = if state.0.lock().expect("state lock").is_some() {
        let name = name.unwrap_or_else(|| "6b6579.tinymemory".to_owned());
        // `updatedAt` present-but-null is what real Cognee serializes for a
        // never-updated record: the backfill must fall through to
        // `created_at` instead of committing to the null (issue #75).
        vec![json!({
            "id": "data-1",
            "name": name,
            "updatedAt": null,
            "created_at": "2026-08-12T00:00:00Z"
        })]
    } else {
        vec![]
    };
    Json(Value::Array(values))
}
async fn raw(State(state): State<AppState>) -> impl IntoResponse {
    state.1.lock().expect("counts").raws += 1;
    state.0.lock().expect("state lock").clone().map_or_else(
        || (StatusCode::NOT_FOUND, Vec::new()),
        |body| (StatusCode::OK, body),
    )
}
async fn remember(State(state): State<AppState>, mut multipart: Multipart) -> StatusCode {
    while let Some(field) = multipart.next_field().await.expect("multipart") {
        if field.name() == Some("data") {
            if let Some(name) = field.file_name() {
                // Cognee's loader strips the final `.json`; mirror it.
                *state.2.lock().expect("name lock") =
                    Some(name.trim_end_matches(".json").to_owned());
            }
            *state.0.lock().expect("state lock") =
                Some(field.bytes().await.expect("body").to_vec());
        }
    }
    StatusCode::OK
}
async fn remove(State(state): State<AppState>) -> StatusCode {
    *state.0.lock().expect("state lock") = None;
    StatusCode::NO_CONTENT
}
async fn recall(State(state): State<AppState>) -> Json<Value> {
    let records = state
        .0
        .lock()
        .expect("state lock")
        .as_ref()
        .and_then(|bytes| String::from_utf8(bytes.clone()).ok())
        .map(|text| vec![json!({"text": text, "score": 0.8})])
        .unwrap_or_default();
    Json(Value::Array(records))
}

async fn capture_auth(State(state): State<Arc<Mutex<Value>>>, headers: HeaderMap) -> StatusCode {
    *state.lock().expect("state lock") = json!({
        "authorization": headers
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
        "api_key": headers
            .get("x-api-key")
            .and_then(|value| value.to_str().ok()),
    });
    StatusCode::OK
}

async fn capture_graph_auth(
    State(state): State<Arc<Mutex<Value>>>,
    headers: HeaderMap,
) -> Json<Value> {
    *state.lock().expect("state lock") = json!({
        "authorization": headers
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
        "api_key": headers
            .get("x-api-key")
            .and_then(|value| value.to_str().ok()),
    });
    Json(Value::Array(Vec::new()))
}

#[tokio::test]
async fn cognee_supports_cloud_api_keys_and_self_hosted_bearer_tokens() {
    let captured = Arc::new(Mutex::new(Value::Null));
    let app = Router::new()
        .route("/health", get(capture_auth))
        .with_state(captured.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let endpoint = format!("http://{}", listener.local_addr().expect("address"));
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });

    let api = super::CogneeMemory::api(&endpoint, "cloud-secret").expect("api client");
    assert!(api.health_check().await);
    let api_headers = captured.lock().expect("state lock").clone();
    assert_eq!(api_headers["api_key"], "cloud-secret");
    assert!(api_headers["authorization"].is_null());

    let hosted = super::CogneeMemory::self_hosted(&endpoint, Some("local-secret"))
        .expect("self-hosted client");
    assert!(hosted.health_check().await);
    let hosted_headers = captured.lock().expect("state lock").clone();
    assert_eq!(hosted_headers["authorization"], "Bearer local-secret");
    assert!(hosted_headers["api_key"].is_null());

    let debug = format!("{api:?}");
    assert!(!debug.contains("cloud-secret"));
    assert!(super::CogneeMemory::api(&endpoint, "  ").is_err());
}

#[tokio::test]
async fn cognee_graph_supports_cloud_api_keys_and_self_hosted_bearer_tokens() {
    let captured = Arc::new(Mutex::new(Value::Null));
    let app = Router::new()
        .route("/api/v1/datasets/", get(capture_graph_auth))
        .with_state(captured.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let endpoint = format!("http://{}", listener.local_addr().expect("address"));
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });

    let api = crate::CogneeGraph::api(&endpoint, "cloud-secret").expect("api graph client");
    assert!(api
        .relations(Some("project"), None, None, 10)
        .await
        .expect("cloud relations")
        .is_empty());
    let api_headers = captured.lock().expect("state lock").clone();
    assert_eq!(api_headers["api_key"], "cloud-secret");
    assert!(api_headers["authorization"].is_null());

    let hosted =
        crate::CogneeGraph::new(&endpoint, Some("local-secret")).expect("self-hosted graph client");
    assert!(hosted
        .relations(Some("project"), None, None, 10)
        .await
        .expect("self-hosted relations")
        .is_empty());
    let hosted_headers = captured.lock().expect("state lock").clone();
    assert_eq!(hosted_headers["authorization"], "Bearer local-secret");
    assert!(hosted_headers["api_key"].is_null());
    assert!(crate::CogneeGraph::api(&endpoint, "  ").is_err());
}

#[tokio::test]
async fn cognee_graph_maps_filters_and_limits_native_edges() {
    let graph_calls = Arc::new(Mutex::new(0_usize));
    let calls = graph_calls.clone();
    let app = Router::new()
        .route(
            "/api/v1/datasets/",
            get(|| async {
                Json(json!([{
                    "id": "dataset-1",
                    "name": super::CogneeDialect::dataset_name("project")
                }]))
            }),
        )
        .route(
            "/api/v1/datasets/dataset-1/graph",
            get(move || {
                let calls = calls.clone();
                async move {
                    *calls.lock().expect("calls") += 1;
                    Json(json!({
                        "nodes": [
                            {"id": "alice", "label": "Alice"},
                            {"id": "bob", "label": "Bob"},
                            {"id": "carol", "label": "Carol"}
                        ],
                        "edges": [
                            {"source": "alice", "target": "bob", "label": "knows"},
                            {"source": "alice", "target": "carol", "label": "manages"},
                            {"source": "unknown", "target": "bob", "label": null},
                            {"source": 12, "target": "bob", "label": "malformed"}
                        ]
                    }))
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

    let graph = crate::CogneeGraph::new(&endpoint, None).expect("client");
    let relations = graph
        .relations(Some("project"), Some("Alice"), None, 1)
        .await
        .expect("filtered relations");
    assert_eq!(relations.len(), 1);
    assert_eq!(relations[0].subject, "Alice");
    assert_eq!(relations[0].predicate, "knows");
    assert_eq!(relations[0].object, "Bob");
    assert_eq!(relations[0].namespace.as_deref(), Some("project"));

    let fallback = graph
        .relations(Some("project"), Some("unknown"), Some(""), 10)
        .await
        .expect("id fallback");
    assert_eq!(fallback.len(), 1);
    assert_eq!(fallback[0].object, "Bob");
    assert_eq!(*graph_calls.lock().expect("calls"), 2);
}

#[tokio::test]
async fn cognee_graph_rejects_unscoped_queries_and_unsupported_mutations() {
    let app = Router::new().route("/api/v1/datasets/", get(|| async { Json(json!([])) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let endpoint = format!("http://{}", listener.local_addr().expect("address"));
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    let graph = crate::CogneeGraph::new(&endpoint, None).expect("client");

    let error = graph
        .relations(None, None, None, 10)
        .await
        .expect_err("namespace is required");
    assert!(matches!(
        error,
        tinymemory_api::error::MemoryError::Invalid(_)
    ));
    assert!(graph
        .relations(Some("missing"), None, None, 10)
        .await
        .expect("missing dataset")
        .is_empty());

    let relation = tinymemory_api::types::GraphRelationRecord {
        namespace: Some("project".into()),
        subject: "Alice".into(),
        predicate: "knows".into(),
        object: "Bob".into(),
        attrs: Value::Null,
        updated_at: 0.0,
        evidence_count: 1,
        order_index: None,
        document_ids: Vec::new(),
        chunk_ids: Vec::new(),
    };
    let errors = [
        graph.kv_get(Some("project"), "key").await.err(),
        graph.kv_put(Some("project"), "key", json!(1)).await.err(),
        graph.kv_delete(Some("project"), "key").await.err(),
        graph.kv_list(Some("project"), None, 10).await.err(),
        graph.put_relation(relation).await.err(),
    ];
    assert!(errors.iter().all(Option::is_some));
    assert!(format!("{:#}", errors[0].as_ref().expect("kv error"))
        .contains("no generic key/value store"));
    assert!(format!("{:#}", errors[4].as_ref().expect("relation error"))
        .contains("cannot be edited directly"));
}

#[tokio::test]
async fn cognee_graph_provider_advertises_an_auditable_graph() {
    let app = Router::new().route("/api/v1/datasets/", get(|| async { Json(json!([])) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let endpoint = format!("http://{}", listener.local_addr().expect("address"));
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });

    let memory = super::CogneeMemory::self_hosted(&endpoint, None).expect("memory client");
    let provider = crate::cognee_graph_provider(memory, &endpoint, None).expect("graph provider");
    tinymemory_api::provider::audit_provider(&provider).expect("honest graph capability");
    assert_eq!(provider.driver_id(), crate::COGNEE_DRIVER_ID);
    assert!(provider.capabilities().contains(Capability::Graph));
    assert!(provider.as_graph().is_some());
}

#[tokio::test]
async fn cognee_graph_surfaces_http_failures_without_parsing_them_as_empty() {
    let app = Router::new()
        .route(
            "/api/v1/datasets/",
            get(|| async {
                Json(json!([{
                    "id": "dataset-1",
                    "name": super::CogneeDialect::dataset_name("project")
                }]))
            }),
        )
        .route(
            "/api/v1/datasets/dataset-1/graph",
            get(|| async { (StatusCode::BAD_REQUEST, "graph unavailable") }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let endpoint = format!("http://{}", listener.local_addr().expect("address"));
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });

    let error = crate::CogneeGraph::new(&endpoint, None)
        .expect("client")
        .relations(Some("project"), None, None, 10)
        .await
        .expect_err("HTTP failure must propagate");
    let rendered = format!("{error:#}");
    assert!(rendered.contains("HTTP 400"), "{rendered}");
    assert!(rendered.contains("graph unavailable"), "{rendered}");
}

#[test]
fn cognee_remote_names_are_bounded_and_safe_for_arbitrary_contract_keys() {
    let unusual = format!("tenant / 🧠 / {}", "x".repeat(500));
    let dataset = super::CogneeDialect::dataset_name(&unusual);
    let filename = super::CogneeDialect::filename(&unusual);

    assert!(dataset.starts_with("tinymemory__tm_"));
    assert!(dataset.len() < 100);
    assert!(dataset
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'));
    assert!(filename.starts_with("tm_"));
    assert!(filename.ends_with(".tinymemory.json"));
    assert!(filename.len() < 100);
    assert_eq!(dataset, super::CogneeDialect::dataset_name(&unusual));
}

#[tokio::test]
async fn native_cognee_round_trips_the_tinymemory_contract() {
    let state = AppState::default();
    let app = Router::new()
        // The real API serves the collection at the slashed form and 307s the
        // bare one; the adapter now asks for `/api/v1/datasets/` directly, so
        // the double must answer there or it stops mirroring the service.
        .route("/api/v1/datasets/", get(datasets))
        .route("/api/v1/datasets/{dataset}/data", get(data))
        .route("/api/v1/datasets/{dataset}/data/{data}/raw", get(raw))
        .route("/api/v1/datasets/{dataset}/data/{data}", delete(remove))
        .route("/api/v1/remember", post(remember))
        .route("/api/v1/update", patch(remember))
        .route("/api/v1/recall", post(recall))
        .route("/health", get(|| async { StatusCode::OK }))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let endpoint = format!("http://{}", listener.local_addr().expect("address"));
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });

    let driver =
        crate::cognee_provider(super::CogneeMemory::self_hosted(&endpoint, None).expect("client"));
    tinymemory_api::provider::audit_provider(&driver).expect("honest capabilities");
    driver
        .store(
            "project",
            "key",
            "knowledge graph",
            MemoryCategory::Conversation,
            Some("session"),
            MemoryTaint::ExternalSync,
        )
        .await
        .expect("store");
    driver
        .store(
            "project",
            "key",
            "updated knowledge graph",
            MemoryCategory::Conversation,
            Some("session"),
            MemoryTaint::ExternalSync,
        )
        .await
        .expect("upsert");
    let entry = driver
        .get("project", "key")
        .await
        .expect("get")
        .expect("entry");
    assert_eq!(entry.content, "updated knowledge graph");
    assert_eq!(entry.taint, MemoryTaint::ExternalSync);
    // The uploaded envelope's timestamp is empty by construction, so the
    // keyed fetch must backfill from the listing the way the enumeration
    // path always did — get and list answering different timestamps for the
    // same record was the #71-review regression, and a null `updatedAt`
    // halting the fallback chain was issue #75's.
    assert_eq!(
        entry.timestamp, "2026-08-12T00:00:00Z",
        "keyed get backfills the listing timestamp past the null updatedAt"
    );
    let listed = driver
        .list(Some("project"), None, None)
        .await
        .expect("list");
    assert_eq!(
        listed[0].timestamp, entry.timestamp,
        "keyed get and namespace list agree on the timestamp"
    );
    assert_eq!(
        driver
            .recall(
                "graph",
                3,
                &OwnedRecallOpts {
                    namespace: Some("project".into()),
                    ..OwnedRecallOpts::default()
                },
                None
            )
            .await
            .expect("recall")
            .len(),
        1
    );
    // #68 review Major 2: Cognee's context-only recall is scoreless, so the
    // strict filter would have dropped 100% of every thresholded result.
    // The dialect declares scores_recall() = false and min_score is
    // documented-inert: the hit survives.
    assert_eq!(
        driver
            .recall(
                "graph",
                3,
                &OwnedRecallOpts {
                    namespace: Some("project".into()),
                    min_score: Some(0.5),
                    ..OwnedRecallOpts::default()
                },
                None
            )
            .await
            .expect("recall with a threshold the backend cannot score")
            .len(),
        1
    );
    assert!(driver.forget("project", "key").await.expect("forget"));
    assert!(!driver.forget("project", "key").await.expect("forget again"));
    assert!(driver.health().await.is_usable());
}

/// Issue #69: the keyed get is three requests — dataset resolve, one
/// listing, ONE raw — however many records the store holds. The pre-seam
/// path raw-fetched every record in every dataset (1 + D + N), which is what
/// made a 10k-record hosted store cost ~10,002 serial requests per get. And
/// a keyed delete needs no envelope at all: zero raws.
#[tokio::test]
async fn keyed_ops_never_fan_out_over_raw_fetches() {
    let state = AppState::default();
    let app = Router::new()
        .route("/api/v1/datasets/", get(datasets))
        .route("/api/v1/datasets/{dataset}/data", get(data))
        .route("/api/v1/datasets/{dataset}/data/{data}/raw", get(raw))
        .route("/api/v1/datasets/{dataset}/data/{data}", delete(remove))
        .route("/api/v1/remember", post(remember))
        .route("/api/v1/update", patch(remember))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let endpoint = format!("http://{}", listener.local_addr().expect("address"));
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    let driver =
        crate::cognee_provider(super::CogneeMemory::self_hosted(&endpoint, None).expect("client"));

    driver
        .store(
            "project",
            "key",
            "knowledge graph",
            MemoryCategory::Core,
            None,
            MemoryTaint::Internal,
        )
        .await
        .expect("store");
    *state.1.lock().expect("counts") = CallCounts::default();

    driver
        .get("project", "key")
        .await
        .expect("get")
        .expect("entry");
    let counts = *state.1.lock().expect("counts");
    assert_eq!(counts.raws, 1, "exactly one raw fetch per keyed get");
    assert_eq!(counts.listings, 1, "exactly one data listing per keyed get");
    assert!(
        counts.datasets <= 1,
        "one dataset resolve per keyed get, got {}",
        counts.datasets
    );

    *state.1.lock().expect("counts") = CallCounts::default();
    assert!(driver.forget("project", "key").await.expect("forget"));
    let counts = *state.1.lock().expect("counts");
    assert_eq!(counts.raws, 0, "a keyed delete reads no envelopes");
}

/// The envelope-over-filename trust boundary, pinned the way its mem0 twin
/// is. A file whose deterministic name matches the asked key but whose
/// envelope names a DIFFERENT record (hash collision, foreign file wearing
/// our extension) must refuse — never serve someone else's memory.
#[tokio::test]
async fn a_filename_match_with_a_foreign_envelope_is_refused() {
    use crate::common::StoredEntry;

    let foreign = serde_json::to_vec(&StoredEntry::new(
        "someone-elses-ns",
        "decision",
        "not yours",
        MemoryCategory::Conversation,
        None,
        MemoryTaint::Internal,
    ))
    .expect("envelope");
    let name = super::CogneeDialect::filename("decision");
    let app = Router::new()
        .route(
            "/api/v1/datasets/",
            get(|| async {
                Json(json!([{
                    "id": "dataset-1",
                    "name": super::CogneeDialect::dataset_name("project")
                }]))
            }),
        )
        .route(
            "/api/v1/datasets/{dataset}/data",
            get(move || {
                let name = name.clone();
                async move {
                    Json(json!([{
                        "id": "data-1",
                        "name": name,
                        "created_at": "2026-08-12T00:00:00Z"
                    }]))
                }
            }),
        )
        .route(
            "/api/v1/datasets/{dataset}/data/{data}/raw",
            get(move || {
                let body = foreign.clone();
                async move { (StatusCode::OK, body) }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let endpoint = format!("http://{}", listener.local_addr().expect("address"));
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });

    let driver =
        crate::cognee_provider(super::CogneeMemory::self_hosted(&endpoint, None).expect("client"));
    let err = driver.get("project", "decision").await;
    let message = format!("{:?}", err.expect_err("mismatched envelope must refuse"));
    assert!(
        message.contains("envelope names"),
        "the refusal names the mismatch: {message}"
    );
}

/// Issue #75: recall in a namespace that has never stored anything answers
/// EMPTY, like every sibling op — real Cognee 404s a recall naming a dataset
/// that resolves to nothing. The double serves no /api/v1/recall route at
/// all, so this also proves the guard short-circuits before asking.
#[tokio::test]
async fn recall_in_a_fresh_namespace_is_empty_not_an_error() {
    use tinymemory_api::recall::OwnedRecallOpts;

    let app = Router::new().route("/api/v1/datasets/", get(|| async { Json(json!([])) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let endpoint = format!("http://{}", listener.local_addr().expect("address"));
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });

    let driver =
        crate::cognee_provider(super::CogneeMemory::self_hosted(&endpoint, None).expect("client"));
    let hits = driver
        .recall(
            "anything",
            3,
            &OwnedRecallOpts {
                namespace: Some("never-stored".into()),
                ..OwnedRecallOpts::default()
            },
            None,
        )
        .await
        .expect("a fresh namespace recalls empty, not an error");
    assert!(hits.is_empty());
}
