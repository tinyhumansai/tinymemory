//! The conformance suite, run against the hosted adapters.
//!
//! Issue #18's acceptance criterion 5: "the conformance suite passes for
//! TinyCortex and all three remote adapters". Until now it ran against the
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

use crate::{mem0_provider, Mem0Memory};

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
