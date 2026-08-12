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
    provider::{MemoryCore, MemoryProvider, MemoryRecall},
    recall::OwnedRecallOpts,
    types::{MemoryCategory, MemoryTaint},
};

#[derive(Clone, Default)]
struct AppState(Arc<Mutex<Vec<Value>>>);

async fn list(State(state): State<AppState>) -> Json<Value> {
    Json(json!({"results": state.0.lock().expect("state lock").clone()}))
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
