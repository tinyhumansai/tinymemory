//! Cognee adapter contract tests over dataset, raw-file, and recall APIs.

#![allow(clippy::expect_used)]

use std::sync::{Arc, Mutex};

use axum::{
    extract::{Multipart, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, post},
    Json, Router,
};
use serde_json::{json, Value};
use tinymemory_api::{
    provider::{MemoryCore, MemoryProvider, MemoryRecall},
    recall::OwnedRecallOpts,
    types::{MemoryCategory, MemoryTaint},
};

#[derive(Clone, Default)]
struct AppState(Arc<Mutex<Option<Vec<u8>>>>);

async fn datasets(State(state): State<AppState>) -> Json<Value> {
    let values = if state.0.lock().expect("state lock").is_some() {
        vec![json!({"id": "dataset-1", "name": "tinymemory__70726f6a656374"})]
    } else {
        vec![]
    };
    Json(Value::Array(values))
}
async fn data(State(state): State<AppState>) -> Json<Value> {
    let values = if state.0.lock().expect("state lock").is_some() {
        vec![
            json!({"id": "data-1", "name": "6b6579.tinymemory.json", "created_at": "2026-08-12T00:00:00Z"}),
        ]
    } else {
        vec![]
    };
    Json(Value::Array(values))
}
async fn raw(State(state): State<AppState>) -> impl IntoResponse {
    state.0.lock().expect("state lock").clone().map_or_else(
        || (StatusCode::NOT_FOUND, Vec::new()),
        |body| (StatusCode::OK, body),
    )
}
async fn remember(State(state): State<AppState>, mut multipart: Multipart) -> StatusCode {
    while let Some(field) = multipart.next_field().await.expect("multipart") {
        if field.name() == Some("data") {
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

#[tokio::test]
async fn native_cognee_round_trips_the_tinymemory_contract() {
    let state = AppState::default();
    let app = Router::new()
        .route("/api/v1/datasets", get(datasets))
        .route("/api/v1/datasets/{dataset}/data", get(data))
        .route("/api/v1/datasets/{dataset}/data/{data}/raw", get(raw))
        .route("/api/v1/datasets/{dataset}/data/{data}", delete(remove))
        .route("/api/v1/remember", post(remember))
        .route("/api/v1/recall", post(recall))
        .route("/health", get(|| async { StatusCode::OK }))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let endpoint = format!("http://{}", listener.local_addr().expect("address"));
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });

    let driver = crate::cognee_provider(super::CogneeMemory::new(&endpoint, None).expect("client"));
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
    let entry = driver
        .get("project", "key")
        .await
        .expect("get")
        .expect("entry");
    assert_eq!(entry.content, "knowledge graph");
    assert_eq!(entry.taint, MemoryTaint::ExternalSync);
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
    assert!(driver.forget("project", "key").await.expect("forget"));
    assert!(!driver.forget("project", "key").await.expect("forget again"));
    assert!(driver.health().await.is_usable());
}
