//! Supermemory adapter contract tests over its native HTTP shapes.

#![allow(clippy::expect_used)]

use std::sync::{Arc, Mutex};

use axum::{
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use tinymemory_api::{
    provider::{MemoryCore, MemoryProvider, MemoryRecall},
    recall::OwnedRecallOpts,
    types::{MemoryCategory, MemoryTaint},
};

#[derive(Clone, Default)]
struct AppState(Arc<Mutex<Vec<Value>>>);

async fn tags() -> Json<Value> {
    Json(json!([{"containerTag": "project"}]))
}

async fn list(State(state): State<AppState>) -> Json<Value> {
    let records = state.0.lock().expect("state lock");
    Json(json!({"memoryEntries": records.clone(), "pagination": {"totalPages": 1}}))
}
async fn add(State(state): State<AppState>, Json(body): Json<Value>) -> Json<Value> {
    let mut records = state.0.lock().expect("state lock");
    let id = format!("doc-{}", records.len() + 1);
    records.push(json!({
        "id": id,
        "memory": body["memories"][0]["content"],
        "metadata": body["memories"][0]["metadata"],
        "createdAt": "2026-08-12T00:00:00Z",
        "isLatest": true,
        "isForgotten": false
    }));
    Json(json!({"memories": [{"id": id}]}))
}
async fn update(State(state): State<AppState>, Json(body): Json<Value>) -> StatusCode {
    let id = body["id"].as_str().unwrap_or_default();
    if let Some(record) = state
        .0
        .lock()
        .expect("state lock")
        .iter_mut()
        .find(|r| r["id"] == id)
    {
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
        .retain(|r| r["id"] != id);
    StatusCode::OK
}
async fn search(State(state): State<AppState>) -> Json<Value> {
    let results = state.0.lock().expect("state lock").iter().map(|r| json!({"id": r["id"], "memory": r["memory"], "metadata": r["metadata"], "similarity": 0.95})).collect::<Vec<_>>();
    Json(json!({"results": results}))
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
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let endpoint = format!("http://{}", listener.local_addr().expect("address"));
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });

    let driver = crate::supermemory_provider(
        super::SupermemoryMemory::new(&endpoint, Some("secret")).expect("client"),
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
    assert_eq!(
        driver
            .recall("Rust", 1, &OwnedRecallOpts::default(), None)
            .await
            .expect("recall")
            .len(),
        1
    );
    assert!(driver.forget("project", "decision").await.expect("forget"));
    assert!(driver.health().await.is_usable());
}
