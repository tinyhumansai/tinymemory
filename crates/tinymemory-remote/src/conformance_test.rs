//! The conformance suite, run against the hosted adapters.
//!
//! Issue #18's acceptance criterion 5: "the conformance suite passes for
//! TinyCortex and every remote adapter". Until now it ran against the
//! in-memory reference driver and the null driver — both written alongside the
//! suite, so passing proved the assertions were self-consistent and little
//! else.
//!
//! These run the same `assert_provider` against the real adapters, over a real
//! TCP socket, against a double that speaks each vendor's own HTTP shapes and
//! **actually retains what it is sent**. That is the difference from
//! `failure_test`, whose doubles only need to misbehave: here the double has to
//! be a working backend, because the suite writes and reads back.
//!
//! What this proves is narrow and worth stating precisely. It is not that
//! Supermemory, Mem0 or Cognee uphold the contract — nobody here can prove that
//! about someone else's service. It is that **the adapter** does, given a
//! backend that answers its own documented shapes. A contract violation on the
//! adapter's side of the wire — a dropped taint, an export cursor that never
//! terminates, an upsert that duplicates — is caught here.

#![allow(clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, Query, State};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde_json::{json, Value};
use tinymemory_api::capabilities::Capability;
use tinymemory_api::provider::{MemoryCore, MemoryProvider};

use tinymemory_api::traits::Memory;
use tinymemory_api::types::MemoryCategory;

use crate::{cortex_provider, mem0_provider, CortexMemory, Mem0Memory};

/// A record as one of the vendor doubles holds it.
#[derive(Clone, Debug)]
struct Row {
    id: String,
    content: String,
    metadata: Value,
    /// The `containerTag` the adapter sent at create time. The real service
    /// files the row under exactly this tag and answers tag-filtered lists
    /// with it; the double must do the same, or a lookup scoped to the tag
    /// the adapter derives (as `upsert`/`delete` now do) misses rows this
    /// double filed under an invented tag — which is a bug in the double, not
    /// in the adapter.
    tag: String,
}

/// The doubles' shared store: `id -> Row`, plus a counter for fresh ids.
#[derive(Default, Debug)]
struct Backend {
    rows: BTreeMap<String, Row>,
    next: usize,
}

impl Backend {
    fn fresh_id(&mut self) -> String {
        self.next += 1;
        format!("rec-{}", self.next)
    }
}

type Store = Arc<Mutex<Backend>>;

/// Serves `app` on an ephemeral port and returns its base URL.
async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let endpoint = format!("http://{}", listener.local_addr().expect("address"));
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    endpoint
}

// ── Mem0's native shapes ─────────────────────────────────────────────────────
//
// Five routes, matching what `Mem0Dialect` issues: list, create, update,
// delete, search. The response envelopes (`results`, `memory`, `metadata`) are
// the ones its `decode` reads, so a shape drift on either side fails here
// rather than silently returning nothing.

async fn mem0_list(State(store): State<Store>) -> Json<Value> {
    let store = store.lock().expect("store lock");
    let results: Vec<Value> = store
        .rows
        .values()
        .map(|r| {
            json!({
                "id": r.id,
                "memory": r.content,
                "metadata": r.metadata,
                "created_at": "1970-01-01T00:00:00Z",
            })
        })
        .collect();
    Json(json!({ "results": results }))
}

async fn mem0_create(State(store): State<Store>, Json(body): Json<Value>) -> Json<Value> {
    let mut store = store.lock().expect("store lock");
    let id = store.fresh_id();
    let content = body["messages"][0]["content"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let metadata = body["metadata"].clone();
    store.rows.insert(
        id.clone(),
        Row {
            id: id.clone(),
            content,
            metadata,
            // Mem0 has no container tags; rows carry an empty one and the
            // supermemory-only tag routes never see them.
            tag: String::new(),
        },
    );
    Json(json!({ "results": [{ "id": id }] }))
}

async fn mem0_update(
    State(store): State<Store>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let mut store = store.lock().expect("store lock");
    if let Some(row) = store.rows.get_mut(&id) {
        if let Some(text) = body["text"].as_str() {
            row.content = text.to_owned();
        }
        if !body["metadata"].is_null() {
            row.metadata = body["metadata"].clone();
        }
    }
    Json(json!({ "id": id }))
}

async fn mem0_delete(State(store): State<Store>, Path(id): Path<String>) -> Json<Value> {
    store.lock().expect("store lock").rows.remove(&id);
    Json(json!({ "deleted": true }))
}

async fn mem0_search(State(store): State<Store>, Json(body): Json<Value>) -> Json<Value> {
    // Substring matching is enough: the suite asserts that recall *narrows*,
    // not that the backend ranks well.
    let needle = body["query"].as_str().unwrap_or_default().to_lowercase();
    let limit = body["top_k"].as_u64().unwrap_or(100) as usize;
    let store = store.lock().expect("store lock");
    let results: Vec<Value> = store
        .rows
        .values()
        .filter(|r| r.content.to_lowercase().contains(&needle))
        .take(limit)
        .map(|r| {
            json!({
                "id": r.id,
                "memory": r.content,
                "metadata": r.metadata,
                "score": 0.9,
            })
        })
        .collect();
    Json(json!({ "results": results }))
}

/// A Mem0 double that retains what it is sent.
async fn mem0_backend() -> String {
    let store: Store = Arc::new(Mutex::new(Backend::default()));
    let app = Router::new()
        .route("/memories", get(mem0_list).post(mem0_create))
        .route("/memories/{id}", put(mem0_update).delete(mem0_delete))
        .route("/search", post(mem0_search))
        .with_state(store);
    serve(app).await
}

#[tokio::test]
async fn mem0_upholds_the_contract() {
    let endpoint = mem0_backend().await;
    let provider = mem0_provider(Mem0Memory::new(&endpoint, None).expect("client"));
    tinymemory_conformance::assert_provider(Arc::new(provider)).await;
}

#[tokio::test]
async fn mem0_routes_conversation_ingestion_without_claiming_other_ingest_kinds() {
    let endpoint = mem0_backend().await;
    let provider = mem0_provider(Mem0Memory::new(&endpoint, None).expect("client"));
    assert!(provider
        .capabilities()
        .contains(Capability::ConversationIngest));
    assert!(!provider.capabilities().contains(Capability::DocumentIngest));

    let messages = vec![serde_json::from_value(json!({
        "source": "conversation",
        "source_id": "thread-1",
        "author": "user",
        "content": "I prefer terse answers"
    }))
    .expect("conversation item")];
    let outcome = provider
        .as_conversation_ingest()
        .expect("conversation route")
        .ingest_conversation(messages)
        .await
        .expect("ingest conversation");
    assert_eq!(outcome.written, 1);
    assert_eq!(
        provider
            .list(Some("conversation:thread-1"), None, None)
            .await
            .expect("list")
            .len(),
        1
    );
}

/// The suite's write-path assertions only run when the driver retains, so a
/// double that silently dropped writes would let the whole run pass vacuously.
/// This pins that the Mem0 double is genuinely retaining.
#[tokio::test]
async fn the_mem0_double_actually_retains() {
    let endpoint = mem0_backend().await;
    let provider = mem0_provider(Mem0Memory::new(&endpoint, None).expect("client"));
    assert!(
        tinymemory_conformance::retains_writes(&provider).await,
        "the Mem0 double must retain writes, or `assert_provider` skips every \
         assertion that matters and still reports success"
    );
}

// ── Supermemory's native shapes ──────────────────────────────────────────────
//
// Container tags are Supermemory's namespace equivalent, and the adapter
// derives one per TinyMemory namespace. The double keeps a tag per row so the
// tag listing — which drives `entries()` — reflects what has actually been
// written, rather than a fixed set the adapter would then filter to nothing.

/// The tag the adapter derives, as sent on create.
fn tag_of(row: &Row) -> String {
    row.tag.clone()
}

async fn sm_tags(State(store): State<Store>) -> Json<Value> {
    let store = store.lock().expect("store lock");
    // Mem0 rows carry an empty tag (that dialect has no containers); they must
    // not surface as a Supermemory container.
    let mut tags: Vec<String> = store
        .rows
        .values()
        .map(tag_of)
        .filter(|tag| !tag.is_empty())
        .collect();
    tags.sort();
    tags.dedup();
    Json(Value::Array(
        tags.into_iter()
            .map(|t| json!({ "containerTag": t }))
            .collect(),
    ))
}

async fn sm_list(State(store): State<Store>, Json(body): Json<Value>) -> Json<Value> {
    // The adapter pages until a short page comes back, so a double that always
    // returned a full page would spin. One page, then empty.
    let page = body["page"].as_u64().unwrap_or(1);
    let wanted = body["containerTags"][0].as_str().unwrap_or_default();
    let store = store.lock().expect("store lock");
    let entries: Vec<Value> = if page > 1 {
        Vec::new()
    } else {
        store
            .rows
            .values()
            .filter(|r| tag_of(r) == wanted)
            .map(|r| {
                json!({
                    "id": r.id,
                    "content": r.content,
                    "metadata": r.metadata,
                    "createdAt": "1970-01-01T00:00:00Z",
                    "isLatest": true,
                    "isForgotten": false,
                })
            })
            .collect()
    };
    Json(json!({ "memoryEntries": entries }))
}

async fn sm_create(
    State(store): State<Store>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, axum::http::StatusCode> {
    // The real v4 API requires `containerTag`; a double that silently filed a
    // malformed create under "" would hide an adapter regression.
    let Some(tag) = body["containerTag"].as_str().filter(|tag| !tag.is_empty()) else {
        return Err(axum::http::StatusCode::BAD_REQUEST);
    };
    let mut store = store.lock().expect("store lock");
    let id = store.fresh_id();
    let first = &body["memories"][0];
    store.rows.insert(
        id.clone(),
        Row {
            id: id.clone(),
            content: first["content"].as_str().unwrap_or_default().to_owned(),
            metadata: first["metadata"].clone(),
            tag: tag.to_owned(),
        },
    );
    Ok(Json(json!({ "memories": [{ "id": id }] })))
}

async fn sm_update(
    State(store): State<Store>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, axum::http::StatusCode> {
    // Mirrors `sm_create`: the real PATCH requires `containerTag` and 400s
    // without it (issue #75) — and the value must match the row it scopes: a
    // foreign tag on a PATCH is a cross-container write, not a detail.
    let Some(sent_tag) = body["containerTag"].as_str().filter(|tag| !tag.is_empty()) else {
        return Err(axum::http::StatusCode::BAD_REQUEST);
    };
    let sent_tag = sent_tag.to_owned();
    let mut store = store.lock().expect("store lock");
    let id = body["id"].as_str().unwrap_or_default().to_owned();
    if let Some(row) = store.rows.get_mut(&id) {
        if row.tag != sent_tag {
            return Err(axum::http::StatusCode::BAD_REQUEST);
        }
        if let Some(text) = body["newContent"].as_str() {
            row.content = text.to_owned();
        }
        if !body["metadata"].is_null() {
            row.metadata = body["metadata"].clone();
        }
    }
    Ok(Json(json!({ "id": id })))
}

async fn sm_delete(State(store): State<Store>, Json(body): Json<Value>) -> Json<Value> {
    let id = body["id"].as_str().unwrap_or_default();
    store.lock().expect("store lock").rows.remove(id);
    Json(json!({ "deleted": true }))
}

async fn sm_search(State(store): State<Store>, Json(body): Json<Value>) -> Json<Value> {
    let needle = body["q"]
        .as_str()
        .or_else(|| body["query"].as_str())
        .unwrap_or_default()
        .to_lowercase();
    let limit = body["limit"].as_u64().unwrap_or(100) as usize;
    let tag = body["containerTag"].as_str();
    let store = store.lock().expect("store lock");
    let results: Vec<Value> = store
        .rows
        .values()
        .filter(|r| tag.is_none_or(|t| tag_of(r) == t))
        .filter(|r| r.content.to_lowercase().contains(&needle))
        .take(limit)
        .map(|r| {
            json!({
                "id": r.id,
                "content": r.content,
                "metadata": r.metadata,
                "score": 0.9,
            })
        })
        .collect();
    Json(json!({ "results": results }))
}

async fn supermemory_backend() -> String {
    let store: Store = Arc::new(Mutex::new(Backend::default()));
    let app = Router::new()
        .route("/v3/container-tags/list", get(sm_tags))
        .route("/v4/memories/list", post(sm_list))
        .route(
            "/v4/memories",
            post(sm_create).patch(sm_update).delete(sm_delete),
        )
        .route("/v4/search", post(sm_search))
        .with_state(store);
    serve(app).await
}

#[tokio::test]
async fn supermemory_upholds_the_contract() {
    let endpoint = supermemory_backend().await;
    let provider = crate::supermemory_provider(
        crate::SupermemoryMemory::new(&endpoint, None).expect("client"),
    );
    tinymemory_conformance::assert_provider(Arc::new(provider)).await;
}

#[tokio::test]
async fn the_supermemory_double_actually_retains() {
    let endpoint = supermemory_backend().await;
    let provider = crate::supermemory_provider(
        crate::SupermemoryMemory::new(&endpoint, None).expect("client"),
    );
    assert!(
        tinymemory_conformance::retains_writes(&provider).await,
        "the Supermemory double must retain writes, or the suite passes vacuously"
    );
}

// ── Cognee's native shapes ───────────────────────────────────────────────────
//
// The odd one out. Cognee has no per-record API: the adapter uploads each
// record as a JSON *file* into a per-namespace dataset, and reads it back
// through `/raw` — so the double stores the uploaded bytes verbatim and serves
// them unchanged. That is also why this double is the strictest of the three:
// the envelope it hands back is deserialised straight into `StoredEntry`, so a
// field the adapter fails to write is a parse failure here rather than a
// silently empty value.

/// A dataset, keyed by the name the adapter derives from a namespace.
type Datasets = Arc<Mutex<BTreeMap<String, BTreeMap<String, String>>>>;

async fn cg_datasets(State(sets): State<Datasets>) -> Json<Value> {
    let sets = sets.lock().expect("store lock");
    Json(Value::Array(
        sets.keys()
            .map(|name| json!({ "id": name, "name": name }))
            .collect(),
    ))
}

async fn cg_data(State(sets): State<Datasets>, Path(dataset): Path<String>) -> Json<Value> {
    let sets = sets.lock().expect("store lock");
    let ids: Vec<Value> = sets
        .get(&dataset)
        .map(|d| {
            d.keys()
                // `name` is required, and the adapter skips anything not
                // ending `.tinymemory[.json]` — Cognee's own loader strips the
                // extension, so both spellings are accepted. The data id here
                // *is* the uploaded filename, which already carries it.
                .map(|id| json!({ "id": id, "name": id }))
                .collect()
        })
        .unwrap_or_default();
    Json(Value::Array(ids))
}

async fn cg_raw(
    State(sets): State<Datasets>,
    Path((dataset, data_id)): Path<(String, String)>,
) -> String {
    sets.lock()
        .expect("store lock")
        .get(&dataset)
        .and_then(|d| d.get(&data_id))
        .cloned()
        .unwrap_or_default()
}

async fn cg_delete(
    State(sets): State<Datasets>,
    Path((dataset, data_id)): Path<(String, String)>,
) -> Json<Value> {
    if let Some(d) = sets.lock().expect("store lock").get_mut(&dataset) {
        d.remove(&data_id);
    }
    Json(json!({ "deleted": true }))
}

/// Pulls the uploaded envelope and the dataset name out of a multipart body.
async fn multipart_parts(mut form: axum::extract::Multipart) -> (String, String, String) {
    let (mut body, mut dataset, mut filename) = (String::new(), String::new(), String::new());
    while let Ok(Some(field)) = form.next_field().await {
        match field.name().unwrap_or_default().to_owned().as_str() {
            "datasetName" => dataset = field.text().await.unwrap_or_default(),
            "data" | "file" | "files" => {
                filename = field.file_name().unwrap_or_default().to_owned();
                body = field.text().await.unwrap_or_default();
            }
            _ => {
                let _ = field.bytes().await;
            }
        }
    }
    (body, dataset, filename)
}

async fn cg_remember(State(sets): State<Datasets>, form: axum::extract::Multipart) -> Json<Value> {
    let (body, dataset, filename) = multipart_parts(form).await;
    let mut sets = sets.lock().expect("store lock");
    sets.entry(dataset).or_default().insert(filename, body);
    Json(json!({ "status": "ok" }))
}

async fn cg_update(
    State(sets): State<Datasets>,
    Query(q): Query<BTreeMap<String, String>>,
    form: axum::extract::Multipart,
) -> Json<Value> {
    let (body, _, _) = multipart_parts(form).await;
    let dataset = q.get("dataset_id").cloned().unwrap_or_default();
    let data_id = q.get("data_id").cloned().unwrap_or_default();
    if let Some(d) = sets.lock().expect("store lock").get_mut(&dataset) {
        d.insert(data_id, body);
    }
    Json(json!({ "status": "ok" }))
}

async fn cg_recall(State(sets): State<Datasets>, Json(body): Json<Value>) -> Json<Value> {
    let needle = body["query"].as_str().unwrap_or_default().to_lowercase();
    let limit = body["top_k"].as_u64().unwrap_or(100) as usize;
    let wanted: Option<Vec<String>> = body["datasets"].as_array().map(|a| {
        a.iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect()
    });
    let sets = sets.lock().expect("store lock");
    let hits: Vec<Value> = sets
        .iter()
        .filter(|(name, _)| wanted.as_ref().is_none_or(|w| w.contains(name)))
        .flat_map(|(_, d)| d.values())
        .filter(|raw| raw.to_lowercase().contains(&needle))
        .take(limit)
        .map(|raw| json!({ "text": raw }))
        .collect();
    // Cognee's `only_context` recall response is the result array itself. The
    // adapter deliberately decodes that native shape (the focused Cognee
    // contract double does too); wrapping it in `{ "results": ... }` makes a
    // healthy adapter appear to return no rows and lets this conformance test
    // fail for a bug in its own fake backend.
    Json(Value::Array(hits))
}

async fn cognee_backend() -> String {
    let sets: Datasets = Arc::new(Mutex::new(BTreeMap::new()));
    let app = Router::new()
        // The real API serves the collection at the slashed form and 307s the
        // bare one; the adapter now asks for `/api/v1/datasets/` directly, so
        // the double must answer there or it stops mirroring the service.
        .route("/api/v1/datasets/", get(cg_datasets))
        .route("/api/v1/datasets/{dataset}/data", get(cg_data))
        .route("/api/v1/datasets/{dataset}/data/{data_id}/raw", get(cg_raw))
        .route(
            "/api/v1/datasets/{dataset}/data/{data_id}",
            delete(cg_delete),
        )
        .route("/api/v1/remember", post(cg_remember))
        .route("/api/v1/update", axum::routing::patch(cg_update))
        .route("/api/v1/recall", post(cg_recall))
        .with_state(sets);
    serve(app).await
}

#[tokio::test]
async fn cognee_upholds_the_contract() {
    let endpoint = cognee_backend().await;
    let provider =
        crate::cognee_provider(crate::CogneeMemory::self_hosted(&endpoint, None).expect("client"));
    tinymemory_conformance::assert_provider(Arc::new(provider)).await;
}

#[tokio::test]
async fn the_cognee_double_actually_retains() {
    let endpoint = cognee_backend().await;
    let provider =
        crate::cognee_provider(crate::CogneeMemory::self_hosted(&endpoint, None).expect("client"));
    assert!(
        tinymemory_conformance::retains_writes(&provider).await,
        "the Cognee double must retain writes, or the suite passes vacuously"
    );
}

// ── CortexDB's native shapes ────────────────────────────────────────────────
//
// This double is deliberately the least accommodating of the three. The others
// model keyed stores, so an adapter bug around replacement would still look
// like success. CortexDB is an append-only event log, and the whole reason its
// adapter exists in its current shape is that a key cannot be rewritten — so
// the double reproduces that constraint exactly, refusing a reused idempotency
// key carrying a different body with the same `409 IDEMPOTENCY_CONFLICT` the
// real engine returns.
//
// A permissive double here would prove nothing: the suite's upsert assertion
// would pass because the backend allowed an overwrite, not because the adapter
// folded the log correctly.

#[derive(Default)]
struct CortexLog {
    /// Every event ever appended, in order. Never mutated — that is the point.
    events: Vec<Value>,
    /// `idempotency_key` -> the body it was first seen with, and the id of the
    /// event that body produced. The id is stored rather than looked up by
    /// content, because two keys may legitimately carry identical text and a
    /// replay must answer with its *own* event.
    idempotency: BTreeMap<String, (String, String)>,
    next_offset: u64,
    next_id: u64,
}

type CortexStore = Arc<Mutex<CortexLog>>;

async fn cortex_experience(
    State(store): State<CortexStore>,
    Json(body): Json<Value>,
) -> (axum::http::StatusCode, Json<Value>) {
    let mut log = store.lock().expect("cortex log");
    let key = body
        .get("idempotency_key")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let payload = body
        .pointer("/content/text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    if let Some((seen, event_id)) = log.idempotency.get(&key) {
        if seen != &payload {
            // The refusal the whole adapter is designed around.
            return (
                axum::http::StatusCode::CONFLICT,
                Json(json!({ "error_code": "IDEMPOTENCY_CONFLICT" })),
            );
        }
        return (
            axum::http::StatusCode::ACCEPTED,
            Json(json!({ "event_id": event_id, "replayed_from_idempotency": true })),
        );
    }

    log.next_offset += 2;
    log.next_id += 1;
    let offset = log.next_offset;
    let id = format!("evt_{}", log.next_id);
    log.idempotency.insert(key, (payload.clone(), id.clone()));
    let scope = body
        .get("scope")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let modality = body
        .get("modality")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let content = body.get("content").cloned().unwrap_or_default();
    log.events.push(json!({
        "id": id,
        "scope": scope,
        "modality": modality,
        "wal_offset": offset,
        "content": content,
        "context": { "recorded_at": "2026-09-02T00:00:00Z" },
    }));
    // The real id, not a placeholder: `/v1/experience` answers with the id the
    // event was actually stored under, and the adapter waits on that id
    // becoming readable before it reports the write as done.
    (
        axum::http::StatusCode::ACCEPTED,
        Json(json!({
            "event_id": id,
            "status": "captured",
            "replayed_from_idempotency": false
        })),
    )
}

async fn cortex_experience_bulk(
    State(store): State<CortexStore>,
    Json(body): Json<Value>,
) -> (axum::http::StatusCode, Json<Value>) {
    let items = body
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut results = Vec::with_capacity(items.len());
    for (index, item) in items.into_iter().enumerate() {
        let (status, Json(receipt)) = cortex_experience(State(store.clone()), Json(item)).await;
        if !status.is_success() {
            return (status, Json(receipt));
        }
        results.push(json!({
            "index": index,
            "event_id": receipt.get("event_id").cloned().unwrap_or_default(),
            "replayed_from_idempotency": receipt
                .get("replayed_from_idempotency")
                .cloned()
                .unwrap_or(json!(false)),
        }));
    }
    (
        axum::http::StatusCode::OK,
        Json(json!({ "accepted": results.len(), "results": results })),
    )
}

async fn cortex_events(
    State(store): State<CortexStore>,
    Query(params): Query<BTreeMap<String, String>>,
) -> Json<Value> {
    let log = store.lock().expect("cortex log");
    let scope = params.get("scope").cloned().unwrap_or_default();
    let cursor: usize = params
        .get("cursor")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let limit: usize = params
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(50);
    // Newest first, and every record emitted twice, because that is what the
    // engine does. Both details are load-bearing: an adapter that trusted array
    // order instead of `wal_offset`, or that assumed `items` held distinct
    // events, would pass against a tidier double and fail in production. Note
    // `limit` counts the duplicates, so a page holds half as many records as
    // its size suggests.
    let mut stream: Vec<Value> = Vec::new();
    for event in log
        .events
        .iter()
        .rev()
        .filter(|e| e.get("scope").and_then(Value::as_str) == Some(scope.as_str()))
    {
        stream.push(event.clone());
        stream.push(event.clone());
    }
    let page: Vec<Value> = stream.iter().skip(cursor).take(limit).cloned().collect();
    let next = cursor + page.len();
    let has_more = next < stream.len();
    Json(json!({
        "items": page,
        "has_more": has_more,
        "next_cursor": next.to_string(),
    }))
}

/// The destructive endpoint, with the interlocks the real one has.
///
/// Three behaviours here are not decoration; each one has caught something:
///
/// - the selector's id field is `memory_ids`. An unrecognised field is **not**
///   rejected — it deserialises to an empty selector, which means "the whole
///   scope";
/// - an empty selector without `confirm_all` is refused, which is what keeps
///   that mistake from being destructive on its own;
/// - a non-empty selector *with* `confirm_all` is refused as ambiguous, rather
///   than silently widened to the scope.
async fn cortex_forget(
    State(store): State<CortexStore>,
    Json(body): Json<Value>,
) -> (axum::http::StatusCode, Json<Value>) {
    let mut log = store.lock().expect("cortex log");
    let scope = body
        .get("scope")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let confirm_all = body
        .get("confirm_all")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let ids: Vec<String> = body
        .pointer("/selector/memory_ids")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let narrowed = ["about_subject", "about_entity", "predicate"]
        .iter()
        .any(|f| body.pointer(&format!("/selector/{f}")).is_some());
    let selective = !ids.is_empty() || narrowed;

    if selective && confirm_all {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({ "error_code": "AMBIGUOUS_SELECTOR_CONFIRM_ALL" })),
        );
    }
    if !selective && !confirm_all {
        return (
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "error_code": "EMPTY_SELECTOR_WITHOUT_CONFIRMATION" })),
        );
    }

    let before = log.events.len();
    if selective {
        log.events.retain(|e| {
            !ids.contains(
                &e.get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            )
        });
    } else {
        log.events
            .retain(|e| e.get("scope").and_then(Value::as_str) != Some(scope.as_str()));
    }
    // Faithful to the engine: forgetting an event does NOT release its
    // idempotency key. An adapter that tried delete-then-rewrite would be
    // refused here, exactly as it is in production.
    let deleted = before - log.events.len();
    (
        axum::http::StatusCode::OK,
        Json(json!({
            "deleted": { "events": deleted },
            "requested": ids.len(),
            "matched": deleted,
        })),
    )
}

async fn cortex_recall(State(store): State<CortexStore>, Json(body): Json<Value>) -> Json<Value> {
    let log = store.lock().expect("cortex log");
    let scope = body
        .get("scope")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let query = body
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let hits: Vec<Value> = log
        .events
        .iter()
        .filter(|e| e.get("scope").and_then(Value::as_str) == Some(scope))
        .filter(|e| {
            query.is_empty()
                || e.pointer("/content/text")
                    .and_then(Value::as_str)
                    .is_some_and(|t| t.to_lowercase().contains(&query.to_lowercase()))
        })
        .map(|e| {
            // Recall renders content for a reader rather than returning it as
            // stored: the speaker is prefixed. The listing does not do this,
            // so the two read paths hand back different bytes for the same
            // event — which is why the adapter parses both forms.
            let mut hit = e.clone();
            if let Some(text) = e.pointer("/content/text").and_then(Value::as_str) {
                hit["content"]["text"] = json!(format!("[user] {text}"));
            }
            hit
        })
        .collect();
    Json(json!({ "pack_id": "pack_test", "layers": { "events": hits } }))
}

async fn cortex_answer(Json(body): Json<Value>) -> (axum::http::StatusCode, Json<Value>) {
    if body.get("use_pack_id").and_then(Value::as_str) != Some("pack_test") {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({ "error_code": "MISSING_PACK" })),
        );
    }
    let query = body
        .get("question")
        .and_then(Value::as_str)
        .unwrap_or_default();
    (
        axum::http::StatusCode::OK,
        Json(json!({
            "answer": format!("grounded answer for {query}"),
            "citations": [{
                "id": "evt_answer_source",
                "key": "answer-source",
                "content": "grounding evidence",
                "score": 0.9
            }],
            "context_block": "grounding evidence",
            "diagnostics": { "answer_model": "reasoning" }
        })),
    )
}

/// The real engine caps this listing at fifty unless `limit` says otherwise,
/// and documents neither the cap nor the parameter — the response carries no
/// cursor and no total, so a caller that does not ask cannot tell it was cut
/// short. The double reproduces the cap, because a double that returns
/// everything cannot catch the adapter forgetting to ask.
async fn cortex_scopes(
    State(store): State<CortexStore>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Json<Value> {
    let limit = params
        .get("limit")
        .and_then(|l| l.parse::<usize>().ok())
        .unwrap_or(50);
    let log = store.lock().expect("cortex log");
    let mut paths: Vec<String> = log
        .events
        .iter()
        .filter_map(|e| e.get("scope").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    paths.sort();
    paths.dedup();
    paths.truncate(limit);
    Json(json!({
        "items": paths.into_iter().map(|p| json!({ "path": p })).collect::<Vec<_>>()
    }))
}

async fn cortex_backend() -> String {
    let store: CortexStore = Arc::new(Mutex::new(CortexLog::default()));
    let app = Router::new()
        .route("/v1/experience", post(cortex_experience))
        .route("/v1/experience/bulk", post(cortex_experience_bulk))
        .route("/v1/events", get(cortex_events))
        .route("/v1/forget", post(cortex_forget))
        .route("/v1/recall", post(cortex_recall))
        .route("/v1/answer", post(cortex_answer))
        .route("/v1/scopes/list", get(cortex_scopes))
        .route(
            "/v1/admin/health",
            get(|| async { Json(json!({ "status": "healthy" })) }),
        )
        .with_state(store);
    serve(app).await
}

/// The same backend, but with ranked recall broken.
///
/// Used to prove that a store still succeeds when the search index cannot be
/// reached — the write is durable and readable by key, and the settle probe is
/// explicitly best-effort.
async fn cortex_backend_with_recall_down() -> String {
    let store: CortexStore = Arc::new(Mutex::new(CortexLog::default()));
    let app = Router::new()
        .route("/v1/experience", post(cortex_experience))
        .route("/v1/events", get(cortex_events))
        .route("/v1/forget", post(cortex_forget))
        .route(
            "/v1/recall",
            post(|| async {
                (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error_code": "INTERNAL" })),
                )
            }),
        )
        .route("/v1/scopes/list", get(cortex_scopes))
        .route(
            "/v1/admin/health",
            get(|| async { Json(json!({ "status": "healthy" })) }),
        )
        .with_state(store);
    serve(app).await
}

/// A durable, readable write must not be reported as a failure because the
/// search index is down.
///
/// The write path waits twice: once for the keyed read path, which is required,
/// and once for ranked recall, which is not. The second wait exists to make
/// read-after-write hold for `search` in the common case, and it must degrade
/// to "the index will catch up" rather than turning a successful store into an
/// error.
#[tokio::test]
async fn a_store_succeeds_when_ranked_recall_is_unreachable() {
    let endpoint = cortex_backend_with_recall_down().await;
    let memory = CortexMemory::api(&endpoint, "test-key").expect("client");

    memory
        .store("tenant", "k", "content", MemoryCategory::Core, None)
        .await
        .expect("a store must not fail because the settle probe could not be answered");

    assert_eq!(
        memory
            .get("tenant", "k")
            .await
            .expect("get")
            .map(|e| e.content)
            .as_deref(),
        Some("content"),
        "the record the store reported as written must be readable by key"
    );
}

#[tokio::test]
async fn cortex_upholds_the_contract() {
    let endpoint = cortex_backend().await;
    let provider = cortex_provider(CortexMemory::api(&endpoint, "test-key").expect("client"));
    tinymemory_conformance::assert_provider(Arc::new(provider)).await;
}

fn cortex_ingest_item(
    namespace: &str,
    source_id: &str,
    content: &str,
    author: Option<&str>,
) -> tinymemory_api::provider::IngestItem {
    tinymemory_api::provider::IngestItem {
        namespace: Some(namespace.to_string()),
        source: tinymemory_api::chunks::DataSource::Conversation,
        source_id: source_id.to_string(),
        owner: "simulation-user".to_string(),
        source_ref: None,
        content: content.to_string(),
        mime: Some("text/plain".to_string()),
        timestamp: None,
        tags: vec!["simulation".to_string()],
        author: author.map(str::to_owned),
        channel_label: None,
        platform: Some("tinymemory-test".to_string()),
        to: Vec::new(),
        cc: Vec::new(),
        subject: None,
        list_unsubscribe: None,
        taint: tinymemory_api::types::MemoryTaint::ExternalSync,
        path_scope: None,
    }
}

/// CortexDB exposes every product-facing ingestion shape and grounded answers.
#[tokio::test]
async fn cortex_full_provider_ingests_every_product_shape() {
    use tinymemory_api::evidence::EvidenceRef;
    use tinymemory_api::learning::{CueFamily, FacetClass, LearningCandidate};
    use tinymemory_api::provider::{AnswerRequest, MemoryProvider, MemoryRecall, RawMemoryEvent};
    use tinymemory_api::recall::OwnedRecallOpts;
    use tinymemory_api::types::MemoryTaint;

    let endpoint = cortex_backend().await;
    let provider = cortex_provider(CortexMemory::api(&endpoint, "test-key").expect("client"));
    tinymemory_api::provider::audit_provider(&provider).expect("capability audit");
    for capability in [
        Capability::DocumentIngest,
        Capability::ConversationIngest,
        Capability::LearningIngest,
        Capability::EventIngest,
        Capability::Answer,
    ] {
        assert!(provider.capabilities().contains(capability));
        assert!(provider.provides(capability));
    }

    let document_namespace = "simulation/documents";
    let document = provider
        .as_document_ingest()
        .expect("document ingest")
        .ingest_document(cortex_ingest_item(
            document_namespace,
            "architecture",
            "The launch architecture uses CortexDB.",
            None,
        ))
        .await
        .expect("document");
    assert_eq!(document.written, 1);

    let conversation_namespace = "simulation/conversation";
    let conversation = provider
        .as_conversation_ingest()
        .expect("conversation ingest")
        .ingest_conversation(vec![
            cortex_ingest_item(
                conversation_namespace,
                "thread-1",
                "Please remember the launch date.",
                Some("user"),
            ),
            cortex_ingest_item(
                conversation_namespace,
                "thread-1",
                "The launch date is Thursday.",
                Some("assistant"),
            ),
        ])
        .await
        .expect("conversation");
    assert_eq!(conversation.written, 2);

    let learning = provider
        .as_learning_ingest()
        .expect("learning ingest")
        .ingest_learning(LearningCandidate {
            class: FacetClass::Tooling,
            key: "package_manager".to_string(),
            value: "pnpm".to_string(),
            cue_family: CueFamily::Explicit,
            evidence: EvidenceRef::ToolCall {
                tool_name: "shell".to_string(),
                episodic_id: 7,
            },
            initial_confidence: 0.95,
            observed_at: 1_700_000_000.0,
        })
        .await
        .expect("learning");
    assert_eq!(learning.written, 1);

    let event_namespace = "simulation/events";
    let tool_call = RawMemoryEvent {
        id: "tool-call-1".to_string(),
        namespace: event_namespace.to_string(),
        event_type: "tool_call".to_string(),
        content: "The shell tool ran cargo test successfully.".to_string(),
        occurred_at: None,
        session_id: Some("thread-1".to_string()),
        metadata: json!({
            "tool_name": "shell",
            "arguments": {"command": "cargo test"},
            "outcome": "success"
        }),
        taint: MemoryTaint::Internal,
    };
    let event_ingest = provider.as_event_ingest().expect("event ingest");
    let event = event_ingest
        .ingest_event(tool_call.clone())
        .await
        .expect("tool-call event");
    assert_eq!(event.written, 1);
    let replay = event_ingest
        .ingest_event(tool_call)
        .await
        .expect("idempotent replay");
    assert_eq!(replay.written, 0);
    assert!(replay.already_ingested);
    assert!(replay.ids.is_empty());

    for (namespace, query) in [
        (document_namespace, "launch architecture"),
        (conversation_namespace, "launch date"),
        ("learning:tooling", "package_manager"),
        (event_namespace, "cargo test"),
    ] {
        let hits = provider
            .recall(
                query,
                10,
                &OwnedRecallOpts {
                    namespace: Some(namespace.to_string()),
                    ..OwnedRecallOpts::default()
                },
                None,
            )
            .await
            .expect("recall");
        assert!(!hits.is_empty(), "{namespace} was not recallable");
    }

    let answer = provider
        .as_answer()
        .expect("answer")
        .answer(AnswerRequest {
            query: "When is launch?".to_string(),
            limit: 5,
            recall: OwnedRecallOpts {
                namespace: Some(conversation_namespace.to_string()),
                ..OwnedRecallOpts::default()
            },
            scope: None,
            instructions: None,
        })
        .await
        .expect("grounded answer");
    assert!(answer.answer.contains("When is launch?"));
    assert_eq!(answer.citations.len(), 1);
    assert_eq!(answer.model.as_deref(), Some("reasoning"));

    let global_answer = provider
        .as_answer()
        .expect("answer")
        .answer(AnswerRequest::new("What is globally relevant?"))
        .await
        .expect("default global answer request");
    assert!(!global_answer.answer.is_empty());

    let filtered_answer = provider
        .as_answer()
        .expect("answer")
        .answer(AnswerRequest {
            query: "When is launch?".to_string(),
            limit: 5,
            recall: OwnedRecallOpts {
                namespace: Some(conversation_namespace.to_string()),
                session_id: Some("thread-1".to_string()),
                ..OwnedRecallOpts::default()
            },
            scope: None,
            instructions: None,
        })
        .await;
    assert!(matches!(
        filtered_answer,
        Err(tinymemory_api::error::MemoryError::Invalid(_))
    ));

    let invalid_document = provider
        .as_document_ingest()
        .expect("document ingest")
        .ingest_document(cortex_ingest_item(
            document_namespace,
            "",
            "not addressable",
            None,
        ))
        .await;
    assert!(matches!(
        invalid_document,
        Err(tinymemory_api::error::MemoryError::Invalid(_))
    ));

    let events: Value = reqwest::Client::new()
        .get(format!(
            "{endpoint}/v1/events?scope=tm%3Asimulation%2Ftm%3Aevents&limit=20"
        ))
        .bearer_auth("test-key")
        .send()
        .await
        .expect("event listing")
        .json()
        .await
        .expect("event JSON");
    let tool_event = events["items"]
        .as_array()
        .and_then(|items| items.iter().find(|item| item["modality"] == "tool_call"))
        .expect("tool-call event retained its modality");
    let envelope: Value = serde_json::from_str(
        tool_event["content"]["text"]
            .as_str()
            .expect("tool-call envelope text"),
    )
    .expect("tool-call envelope JSON");
    assert_eq!(envelope["x"]["metadata"]["tool_name"], "shell");
    assert_eq!(envelope["x"]["metadata"]["outcome"], "success");
}

/// The suite's write-path assertions only run when the driver retains.
#[tokio::test]
async fn the_cortex_double_actually_retains() {
    let endpoint = cortex_backend().await;
    let provider = cortex_provider(CortexMemory::api(&endpoint, "test-key").expect("client"));
    assert!(
        tinymemory_conformance::retains_writes(&provider).await,
        "the CortexDB double must retain writes, or `assert_provider` skips every \
         assertion that matters and still reports success"
    );
}

/// The double must refuse a reused key with a changed body, or the suite's
/// upsert assertion passes for the wrong reason.
///
/// This is the constraint the adapter is built around. If the double ever
/// becomes permissive, `cortex_upholds_the_contract` would prove that a keyed
/// backend upholds the contract — which is true and irrelevant.
#[tokio::test]
async fn the_cortex_double_refuses_a_reused_key_with_a_changed_body() {
    let endpoint = cortex_backend().await;
    let client = reqwest::Client::new();
    let send = |text: &str| {
        let body = json!({
            "scope": "tm:probe",
            "idempotency_key": "fixed",
            "content": { "kind": "message", "role": "user", "text": text },
            "context": {},
        });
        client
            .post(format!("{endpoint}/v1/experience"))
            .json(&body)
            .send()
    };
    assert_eq!(send("first").await.expect("send").status(), 202);
    assert_eq!(
        send("second").await.expect("send").status(),
        409,
        "a permissive double would make the adapter's whole reason for existing untested"
    );
}

/// A scope larger than one page must fold completely.
///
/// The listing pages with `cursor`/`next_cursor`, and the engine ignores query
/// parameters it does not recognise rather than refusing them — so a wrong
/// parameter name does not surface as an error, it silently re-serves page one
/// until the adapter's page ceiling trips. Because the engine also emits every
/// record twice and counts the duplicates against `limit`, the boundary arrives
/// at roughly half the page size. This writes past it.
#[tokio::test]
async fn a_scope_past_one_page_folds_completely() {
    let endpoint = cortex_backend().await;
    let memory = CortexMemory::api(&endpoint, "test-key").expect("client");
    for i in 0..140 {
        memory
            .store(
                "paged",
                &format!("key-{i:03}"),
                &format!("value {i}"),
                MemoryCategory::Core,
                None,
            )
            .await
            .expect("store");
    }
    let entries = memory.list(Some("paged"), None, None).await.expect("list");
    assert_eq!(
        entries.len(),
        140,
        "the fold walked a truncated listing; every distinct record must survive paging"
    );
    let found = memory.get("paged", "key-139").await.expect("get");
    assert_eq!(found.map(|e| e.content).as_deref(), Some("value 139"));
}

/// Deleting one key must not take the scope with it.
///
/// The destructive endpoint has two failure shapes that both end in an empty
/// selector: an unrecognised selector field, and `confirm_all` sent alongside a
/// real one. The first is silent. This asserts the adapter lands in neither.
#[tokio::test]
async fn deleting_one_key_leaves_its_neighbours_alone() {
    let endpoint = cortex_backend().await;
    let memory = CortexMemory::api(&endpoint, "test-key").expect("client");
    for key in ["alpha", "beta", "gamma"] {
        memory
            .store(
                "tenant",
                key,
                &format!("{key} value"),
                MemoryCategory::Core,
                None,
            )
            .await
            .expect("store");
    }
    // A second version of the doomed key, so the delete has to reach both.
    memory
        .store(
            "tenant",
            "beta",
            "beta rewritten",
            MemoryCategory::Core,
            None,
        )
        .await
        .expect("store");

    assert!(memory.forget("tenant", "beta").await.expect("forget"));

    assert!(memory.get("tenant", "beta").await.expect("get").is_none());
    assert_eq!(
        memory
            .get("tenant", "alpha")
            .await
            .expect("get")
            .map(|e| e.content)
            .as_deref(),
        Some("alpha value"),
        "a neighbour disappeared: the delete widened to the whole scope"
    );
    assert_eq!(
        memory
            .get("tenant", "gamma")
            .await
            .expect("get")
            .map(|e| e.content)
            .as_deref(),
        Some("gamma value")
    );
    let left = memory.list(Some("tenant"), None, None).await.expect("list");
    assert_eq!(
        left.len(),
        2,
        "expected alpha and gamma to remain, got {left:?}"
    );
}

/// A deleted key stays deleted even if the removal half fails.
///
/// The tombstone is what makes that true, and it is why `delete` writes one
/// before touching the destructive endpoint at all.
#[tokio::test]
async fn a_tombstone_alone_is_enough_to_hide_a_key() {
    let endpoint = cortex_backend().await;
    let memory = CortexMemory::api(&endpoint, "test-key").expect("client");
    memory
        .store("tenant", "doomed", "still here", MemoryCategory::Core, None)
        .await
        .expect("store");

    // Append the tombstone by hand and never call forget, standing in for a
    // removal whose second half was lost.
    let client = reqwest::Client::new();
    let tombstone = json!({ "k": "doomed", "c": "", "d": true });
    let sent = client
        .post(format!("{endpoint}/v1/experience"))
        .json(&json!({
            "scope": "tm:tenant",
            "idempotency_key": "hand-written-tombstone",
            "content": { "kind": "message", "role": "user", "text": tombstone.to_string() },
            "context": {},
        }))
        .send()
        .await
        .expect("send");
    assert_eq!(sent.status(), 202);

    assert!(
        memory.get("tenant", "doomed").await.expect("get").is_none(),
        "the fold ignored a tombstone, so a delete that lost its second half \
         would resurrect the record"
    );
    assert!(memory
        .list(Some("tenant"), None, None)
        .await
        .expect("list")
        .is_empty());
}

/// Recall must return what the engine ranked, not what sorts first.
///
/// The fold orders by key, which is right for a listing and wrong for a ranked
/// answer: truncating an alphabetical order to `limit` discards the engine's
/// best hits and keeps whichever keys happen to sort early. The conformance
/// suite cannot catch this on its own — its recall fixture stores identical
/// content under `r1`/`r2`/`r3`, where ranked and alphabetical order coincide.
#[tokio::test]
async fn recall_keeps_the_engine_ranking_when_it_truncates() {
    let endpoint = cortex_backend().await;
    let memory = CortexMemory::api(&endpoint, "test-key").expect("client");
    // Stored — and so ranked by the double — in the opposite order to the one
    // the keys sort in.
    for key in ["zulu", "alpha"] {
        memory
            .store(
                "ranked",
                key,
                "shared needle text",
                MemoryCategory::Core,
                None,
            )
            .await
            .expect("store");
    }

    let opts = tinymemory_api::recall::RecallOpts {
        namespace: Some("ranked"),
        ..Default::default()
    };
    let hits = memory.recall("needle", 1, opts).await.expect("recall");

    assert_eq!(hits.len(), 1, "the limit must still be honoured");
    assert_eq!(
        hits[0].key, "zulu",
        "recall returned the alphabetically first key, not the highest ranked \
         one — the engine's ordering was thrown away before the truncation"
    );
}

/// A replay must answer with its own event, not one that happens to match.
///
/// Two idempotency keys may legitimately carry identical text — the adapter
/// mints a fresh key per write, so a re-store of unchanged content is exactly
/// this shape. A double that resolved a replay by searching content would hand
/// back the first matching event for both, and the write path waits on the id
/// it is given: it would be waiting on the wrong record.
#[tokio::test]
async fn a_replay_returns_the_event_its_own_key_created() {
    let endpoint = cortex_backend().await;
    let client = reqwest::Client::new();
    let send = |key: &'static str| {
        let body = json!({
            "scope": "tm:probe",
            "idempotency_key": key,
            "content": { "kind": "message", "role": "user", "text": "identical text" },
            "context": {},
        });
        client
            .post(format!("{endpoint}/v1/experience"))
            .json(&body)
            .send()
    };
    let id_of = |v: &Value| {
        v.get("event_id")
            .and_then(Value::as_str)
            .map(str::to_string)
    };

    let first: Value = send("key-a")
        .await
        .expect("send")
        .json()
        .await
        .expect("json");
    let second: Value = send("key-b")
        .await
        .expect("send")
        .json()
        .await
        .expect("json");
    let (a, b) = (id_of(&first), id_of(&second));
    assert_ne!(a, b, "two keys with the same text must create two events");

    let replay_a: Value = send("key-a")
        .await
        .expect("send")
        .json()
        .await
        .expect("json");
    let replay_b: Value = send("key-b")
        .await
        .expect("send")
        .json()
        .await
        .expect("json");
    assert_eq!(
        replay_a.get("replayed_from_idempotency"),
        Some(&json!(true))
    );
    assert_eq!(
        id_of(&replay_a),
        a,
        "key-a replayed with another key\'s event"
    );
    assert_eq!(
        id_of(&replay_b),
        b,
        "key-b replayed with another key\'s event"
    );
}

/// A namespace count above the engine's undocumented default must not be
/// silently truncated.
///
/// `scopes` feeds `entries`, which feeds `namespace_summaries`, which feeds
/// `export_page` and so `opencompany memory migrate`. Before this was fixed the
/// adapter sent no `limit`, so a company with more than fifty namespaces
/// migrated a subset of itself and the migration reported success. Per-namespace
/// reads never notice, which is why it stayed invisible.
#[tokio::test]
async fn every_namespace_is_listed_past_the_engines_undocumented_default() {
    let store: CortexStore = Arc::new(Mutex::new(CortexLog::default()));
    let app = Router::new()
        .route("/v1/experience", post(cortex_experience))
        .route("/v1/events", get(cortex_events))
        .route("/v1/forget", post(cortex_forget))
        .route("/v1/recall", post(cortex_recall))
        .route("/v1/scopes/list", get(cortex_scopes))
        .route(
            "/v1/admin/health",
            get(|| async { Json(json!({ "status": "healthy" })) }),
        )
        .with_state(store);
    let endpoint = serve(app).await;

    let memory = CortexMemory::api(&endpoint, "test-key").expect("client");
    // Comfortably past fifty, and past it by enough that an off-by-one in the
    // cap would not pass by luck.
    const NAMESPACES: usize = 64;
    for n in 0..NAMESPACES {
        memory
            .store(
                &format!("oc/acme-{n:032x}"),
                "k",
                "v",
                MemoryCategory::Core,
                None,
            )
            .await
            .expect("store");
    }

    let summaries = memory.namespace_summaries().await.expect("summaries");
    assert_eq!(
        summaries.len(),
        NAMESPACES,
        "a migration reading this would have left {} namespaces behind without saying so",
        NAMESPACES.saturating_sub(summaries.len())
    );
}

/// A listing that exactly fills the limit is refused, not returned.
///
/// The engine sends no cursor, no `has_more` and no total, so a response
/// holding as many entries as were asked for is indistinguishable from one that
/// was cut short. Returning it would hand a caller a subset labelled as the
/// whole, which is the failure this guard exists to prevent — better a loud
/// error than a migration that quietly leaves namespaces behind.
#[tokio::test]
async fn a_scope_listing_that_fills_the_limit_is_refused_rather_than_trusted() {
    // The engine's ceiling as the adapter asks for it. Kept as a literal on
    // purpose: this test should fail loudly if `SCOPE_LIST_LIMIT` moves without
    // someone reconsidering the guard.
    const ASKED_FOR: usize = 10_000;
    let app = Router::new().route(
        "/v1/scopes/list",
        get(|| async {
            let items: Vec<Value> = (0..ASKED_FOR)
                .map(|n| json!({ "path": format!("tenant:acme-{n}") }))
                .collect();
            Json(json!({ "items": items }))
        }),
    );
    let endpoint = serve(app).await;

    let memory = CortexMemory::api(&endpoint, "test-key").expect("client");
    let error = memory
        .namespace_summaries()
        .await
        .expect_err("a full page must not pass for a complete listing");
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains("truncated"),
        "the error must say why the listing cannot be trusted, got: {rendered}"
    );
}
