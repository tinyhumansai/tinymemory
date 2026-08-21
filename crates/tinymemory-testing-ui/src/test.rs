//! HTTP and static UI contract tests for the local testing harness.

use std::process::Command;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use tinymemory_api::capabilities::{Capabilities, Capability};
use tinymemory_api::error::MemoryError;
use tinymemory_api::health::MemoryHealth;
use tinymemory_api::provider::types::{ExportPage, ExportRecord, ImportOutcome, SourceScope};
use tinymemory_api::provider::{
    MemoryCore, MemoryDocuments, MemoryGraph, MemoryPortability, MemoryProvider, MemoryRecall,
};
use tinymemory_api::recall::OwnedRecallOpts;
use tinymemory_api::types::{
    GraphRelationRecord, MemoryCategory, MemoryEntry, MemoryKvRecord, MemoryTaint,
    NamespaceDocumentInput, NamespaceRetrievalContext, NamespaceSummary, StoredMemoryDocument,
};

use super::*;

fn empty_state() -> SharedState {
    Arc::new(AppState {
        active: RwLock::new(None),
        url_fetcher: guarded_url_fetcher(),
    })
}

fn test_app(state: SharedState) -> Router {
    app(state, concat!(env!("CARGO_MANIFEST_DIR"), "/web"))
}

fn json_request(method: Method, uri: &str, value: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(value.to_string()))
        .unwrap()
}

async fn json_body(response: Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    }
}

async fn connect_local(router: &Router) {
    let response = router
        .clone()
        .oneshot(json_request(
            Method::POST,
            "/api/connect",
            json!({ "engine": "local" }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn operations_require_a_connected_engine() {
    let response = test_app(empty_state())
        .oneshot(json_request(
            Method::POST,
            "/api/store",
            json!({ "namespace": "notes", "key": "one", "content": "body" }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        json_body(response).await,
        json!({ "error": "no engine connected yet" })
    );
}

#[tokio::test]
async fn capability_routes_check_connection_before_processing_input() {
    let requests = [
        Request::get("/api/graph/relations")
            .body(Body::empty())
            .unwrap(),
        json_request(Method::POST, "/api/graph/view", json!({ "seeds": ["ada"] })),
        multipart_request(&[("file", Some("note.txt"), Some("text/plain"), b"body")]),
        json_request(
            Method::POST,
            "/api/ingest/url",
            json!({
                "url": "https://user:secret@example.com/private",
                "namespace": "documents"
            }),
        ),
    ];

    for request in requests {
        let response = test_app(empty_state()).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(
            json_body(response).await,
            json!({ "error": "no engine connected yet" })
        );
    }
}

#[tokio::test]
async fn local_connect_status_and_disconnect_are_consistent() {
    let router = test_app(empty_state());
    let initial = router
        .clone()
        .oneshot(Request::get("/api/status").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(json_body(initial).await["connected"], false);
    connect_local(&router).await;

    let status = router
        .clone()
        .oneshot(Request::get("/api/status").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let body = json_body(status).await;
    assert_eq!(body["connected"], true);
    assert_eq!(body["driver_id"], "tinycortex");

    let disconnected = router
        .clone()
        .oneshot(
            Request::post("/api/disconnect")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        json_body(disconnected).await,
        json!({
            "connected": false,
            "driver_id": null,
            "engine": null,
            "has_graph": false
        })
    );
}

#[tokio::test]
async fn defaulted_core_queries_and_static_fallback_are_callable() {
    let router = test_app(empty_state());
    connect_local(&router).await;

    let stored = router
        .clone()
        .oneshot(json_request(
            Method::POST,
            "/api/store",
            json!({ "namespace": "defaults", "key": "one", "content": "plain body" }),
        ))
        .await
        .unwrap();
    assert_eq!(stored.status(), StatusCode::NO_CONTENT);

    for uri in ["/api/list", "/api/namespaces", "/api/export"] {
        let response = router
            .clone()
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
    }
    let recalled = router
        .clone()
        .oneshot(json_request(
            Method::POST,
            "/api/recall",
            json!({ "query": "plain" }),
        ))
        .await
        .unwrap();
    assert_eq!(recalled.status(), StatusCode::OK);

    let index = router
        .oneshot(Request::get("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(index.status(), StatusCode::OK);
    assert!(String::from_utf8(
        index
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec()
    )
    .unwrap()
    .contains("TinyMemory"));
}

#[tokio::test]
async fn local_engine_supports_the_complete_core_http_workflow() {
    let router = test_app(empty_state());
    connect_local(&router).await;

    let stored = router
        .clone()
        .oneshot(json_request(
            Method::POST,
            "/api/store",
            json!({
                "namespace": "notes",
                "key": "theme",
                "content": "prefers dark mode",
                "category": "daily",
                "session_id": "session-1",
                "taint": "external_sync"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(stored.status(), StatusCode::NO_CONTENT);

    let entry = router
        .clone()
        .oneshot(
            Request::get("/api/get?namespace=notes&key=theme")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let entry = json_body(entry).await;
    assert_eq!(entry["content"], "prefers dark mode");
    assert_eq!(entry["category"], "daily");
    assert_eq!(entry["session_id"], "session-1");
    assert_eq!(entry["taint"], "external_sync");

    let listed = router
        .clone()
        .oneshot(
            Request::get("/api/list?namespace=notes&category=daily&session_id=session-1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(json_body(listed).await.as_array().unwrap().len(), 1);

    let recalled = router
        .clone()
        .oneshot(json_request(
            Method::POST,
            "/api/recall",
            json!({ "query": "dark mode", "namespace": "notes", "limit": 10 }),
        ))
        .await
        .unwrap();
    assert_eq!(json_body(recalled).await[0]["key"], "theme");

    let exported = router
        .clone()
        .oneshot(
            Request::get("/api/export?limit=10")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        json_body(exported).await["records"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let forgotten = router
        .clone()
        .oneshot(json_request(
            Method::POST,
            "/api/forget",
            json!({ "namespace": "notes", "key": "theme" }),
        ))
        .await
        .unwrap();
    assert_eq!(json_body(forgotten).await, json!(true));
}

#[tokio::test]
async fn invalid_engine_deployment_and_cloud_credentials_are_rejected() {
    let cases = [
        (json!({ "engine": "unknown" }), "unknown engine: unknown"),
        (
            json!({ "engine": "supermemory" }),
            "supermemory requires an endpoint URL",
        ),
        (
            json!({ "engine": "mem0", "endpoint": "http://localhost", "deployment": "other" }),
            "unknown Mem0 deployment: other",
        ),
        (
            json!({ "engine": "mem0", "endpoint": "https://api.mem0.ai", "deployment": "cloud" }),
            "Mem0 Cloud requires an API key",
        ),
        (
            json!({ "engine": "cognee", "endpoint": "https://example.invalid", "deployment": "cloud" }),
            "Cognee Cloud requires an API key",
        ),
        (
            json!({ "engine": "cognee", "endpoint": "https://example.invalid", "deployment": "other" }),
            "unknown Cognee deployment: other",
        ),
    ];

    for (request, message) in cases {
        let response = test_app(empty_state())
            .oneshot(json_request(Method::POST, "/api/connect", request))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(json_body(response).await["error"], message);
    }
}

#[tokio::test]
async fn malformed_remote_endpoints_are_rejected_at_connection_time() {
    for request in [
        json!({ "engine": "supermemory", "endpoint": "://bad" }),
        json!({ "engine": "mem0", "endpoint": "://bad", "deployment": "self_hosted" }),
        json!({ "engine": "cognee", "endpoint": "://bad", "deployment": "self_hosted" }),
    ] {
        let response = test_app(empty_state())
            .oneshot(json_request(Method::POST, "/api/connect", request))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(!json_body(response).await["error"]
            .as_str()
            .unwrap()
            .is_empty());
    }
}

#[tokio::test]
async fn empty_remote_connection_fields_are_treated_as_missing() {
    let cases = [
        (
            json!({ "engine": "supermemory", "endpoint": "", "api_key": "" }),
            "supermemory requires an endpoint URL",
        ),
        (
            json!({ "engine": "mem0", "endpoint": "", "api_key": "" }),
            "mem0 requires an endpoint URL",
        ),
        (
            json!({ "engine": "cognee", "endpoint": "", "api_key": "" }),
            "cognee requires an endpoint URL",
        ),
    ];

    for (request, message) in cases {
        let response = test_app(empty_state())
            .oneshot(json_request(Method::POST, "/api/connect", request))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(json_body(response).await["error"], message);
    }
}

#[tokio::test]
async fn omitted_deployment_uses_each_remote_engines_documented_default() {
    let cases = [
        (
            json!({
                "engine": "mem0",
                "endpoint": tinymemory_remote::MEM0_API_ENDPOINT,
                "api_key": "test-key"
            }),
            "mem0",
        ),
        (
            json!({ "engine": "mem0", "endpoint": "http://127.0.0.1:9" }),
            "mem0",
        ),
        (
            json!({ "engine": "cognee", "endpoint": "http://127.0.0.1:9" }),
            "cognee",
        ),
    ];

    for (request, driver_id) in cases {
        let response = test_app(empty_state())
            .oneshot(json_request(Method::POST, "/api/connect", request))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(json_body(response).await["driver_id"], driver_id);
    }
}

#[tokio::test]
async fn empty_remote_api_keys_follow_each_deployments_credential_policy() {
    let supermemory = test_app(empty_state())
        .oneshot(json_request(
            Method::POST,
            "/api/connect",
            json!({
                "engine": "supermemory",
                "endpoint": "http://127.0.0.1:9",
                "api_key": ""
            }),
        ))
        .await
        .unwrap();
    assert_eq!(supermemory.status(), StatusCode::OK);

    for request in [
        json!({
            "engine": "mem0",
            "endpoint": tinymemory_remote::MEM0_API_ENDPOINT,
            "deployment": "cloud",
            "api_key": ""
        }),
        json!({
            "engine": "cognee",
            "endpoint": "https://example.invalid",
            "deployment": "cloud",
            "api_key": ""
        }),
    ] {
        let response = test_app(empty_state())
            .oneshot(json_request(Method::POST, "/api/connect", request))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(json_body(response).await["error"]
            .as_str()
            .unwrap()
            .contains("requires an API key"));
    }

    let implicit_cloud = test_app(empty_state())
        .oneshot(json_request(
            Method::POST,
            "/api/connect",
            json!({ "engine": "mem0", "endpoint": tinymemory_remote::MEM0_API_ENDPOINT }),
        ))
        .await
        .unwrap();
    assert_eq!(implicit_cloud.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        json_body(implicit_cloud).await["error"],
        "Mem0 Cloud requires an API key"
    );
}

#[tokio::test]
async fn every_remote_connection_mode_builds_without_contacting_its_endpoint() {
    let cases = [
        (
            json!({ "engine": "supermemory", "endpoint": "http://127.0.0.1:9" }),
            "supermemory",
            false,
        ),
        (
            json!({ "engine": "mem0", "endpoint": "http://127.0.0.1:9", "deployment": "self_hosted" }),
            "mem0",
            true,
        ),
        (
            json!({ "engine": "mem0", "endpoint": "https://api.mem0.ai", "deployment": "cloud", "api_key": "test-key" }),
            "mem0",
            true,
        ),
        (
            json!({ "engine": "cognee", "endpoint": "http://127.0.0.1:9", "deployment": "self_hosted" }),
            "cognee",
            true,
        ),
        (
            json!({ "engine": "cognee", "endpoint": "https://example.invalid", "deployment": "cloud", "api_key": "test-key" }),
            "cognee",
            true,
        ),
    ];

    for (request, driver_id, has_graph) in cases {
        let response = test_app(empty_state())
            .oneshot(json_request(Method::POST, "/api/connect", request))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["driver_id"], driver_id);
        assert_eq!(body["has_graph"], has_graph);
    }
}

#[tokio::test]
async fn memory_errors_have_stable_http_statuses_and_json_bodies() {
    let cases = [
        (MemoryError::Invalid("bad".into()), StatusCode::BAD_REQUEST),
        (
            MemoryError::PathEscape("bad".into()),
            StatusCode::BAD_REQUEST,
        ),
        (MemoryError::NotFound("gone".into()), StatusCode::NOT_FOUND),
        (
            MemoryError::BudgetExceeded("large".into()),
            StatusCode::PAYLOAD_TOO_LARGE,
        ),
        (
            MemoryError::Unauthorized("key".into()),
            StatusCode::UNAUTHORIZED,
        ),
        (
            MemoryError::Timeout("slow".into()),
            StatusCode::GATEWAY_TIMEOUT,
        ),
        (
            MemoryError::Unavailable("busy".into()),
            StatusCode::SERVICE_UNAVAILABLE,
        ),
        (MemoryError::Backend("bad".into()), StatusCode::BAD_GATEWAY),
    ];

    for (error, expected) in cases {
        let expected_message = error.to_string();
        let response = ApiError::from(error).into_response();
        assert_eq!(response.status(), expected);
        assert_eq!(json_body(response).await["error"], expected_message);
    }
}

#[tokio::test]
async fn document_formats_report_conversion_and_connection_route() {
    let router = test_app(empty_state());
    let disconnected = router
        .clone()
        .oneshot(
            Request::get("/api/documents/formats")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let disconnected = json_body(disconnected).await;
    assert_eq!(disconnected["route"], Value::Null);
    assert_eq!(
        disconnected["formats"],
        json!(["markdown", "plain_text", "html"])
    );

    connect_local(&router).await;
    let connected = router
        .oneshot(
            Request::get("/api/documents/formats")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(json_body(connected).await["route"].is_string());
}

#[tokio::test]
async fn graph_provider_status_advertises_graph_without_claiming_an_engine_name() {
    let state = state_with_provider(RecordingProvider::default());
    let response = test_app(state)
        .oneshot(Request::get("/api/status").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        json_body(response).await,
        json!({
            "connected": true,
            "driver_id": "recording",
            "engine": null,
            "has_graph": true
        })
    );
}

type MultipartPart<'a> = (&'a str, Option<&'a str>, Option<&'a str>, &'a [u8]);

fn multipart_request(parts: &[MultipartPart<'_>]) -> Request<Body> {
    let boundary = "tinymemory-test-boundary";
    let mut body = Vec::new();
    for (name, filename, content_type, value) in parts {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"").as_bytes(),
        );
        if let Some(filename) = filename {
            body.extend_from_slice(format!("; filename=\"{filename}\"").as_bytes());
        }
        body.extend_from_slice(b"\r\n");
        if let Some(content_type) = content_type {
            body.extend_from_slice(format!("Content-Type: {content_type}\r\n").as_bytes());
        }
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(value);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    Request::builder()
        .method(Method::POST)
        .uri("/api/documents/upload")
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))
        .unwrap()
}

#[tokio::test]
async fn document_upload_validates_required_parts_and_supported_formats() {
    let router = test_app(empty_state());
    connect_local(&router).await;

    let no_file = router
        .clone()
        .oneshot(multipart_request(&[(
            "namespace",
            None,
            None,
            b"documents",
        )]))
        .await
        .unwrap();
    assert_eq!(no_file.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        json_body(no_file).await["error"],
        "no `file` part in the upload"
    );

    let no_namespace = router
        .clone()
        .oneshot(multipart_request(&[(
            "file",
            Some("note.txt"),
            Some("text/plain"),
            b"hello",
        )]))
        .await
        .unwrap();
    assert_eq!(no_namespace.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        json_body(no_namespace).await["error"],
        "no `namespace` part in the upload"
    );

    let unsupported = router
        .oneshot(multipart_request(&[
            ("namespace", None, None, b"documents"),
            ("file", Some("note.pdf"), Some("application/pdf"), b"%PDF"),
        ]))
        .await
        .unwrap();
    assert_eq!(unsupported.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn document_upload_rejects_invalid_text_and_malformed_multipart() {
    let router = test_app(empty_state());
    connect_local(&router).await;

    let invalid_text = router
        .clone()
        .oneshot(multipart_request(&[
            ("namespace", None, None, b"documents"),
            (
                "file",
                Some("broken.txt"),
                Some("text/plain"),
                &[0xff, 0xfe, 0xfd],
            ),
        ]))
        .await
        .unwrap();
    assert_eq!(invalid_text.status(), StatusCode::BAD_REQUEST);
    assert!(!json_body(invalid_text).await["error"]
        .as_str()
        .unwrap()
        .is_empty());

    let malformed = Request::builder()
        .method(Method::POST)
        .uri("/api/documents/upload")
        .header(
            header::CONTENT_TYPE,
            "multipart/form-data; boundary=broken-boundary",
        )
        .body(Body::from(
            b"--broken-boundary\r\nContent-Disposition: form-data; name=\"namespace\"\r\ninvalid header\r\n\r\ndocuments\r\n--broken-boundary--\r\n"
                .as_slice(),
        ))
        .unwrap();
    let malformed = router.oneshot(malformed).await.unwrap();
    assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
    assert!(!json_body(malformed).await["error"]
        .as_str()
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn minimal_upload_uses_safe_external_defaults_and_ignores_unknown_parts() {
    let provider = Arc::new(RecordingProvider::default());
    let state = empty_state();
    *state.active.write().await = Some(provider.clone());

    let response = test_app(state)
        .oneshot(multipart_request(&[
            ("namespace", None, None, b"documents"),
            ("key", None, None, b""),
            ("tags", None, None, b" first, , second "),
            ("unknown", None, None, b"ignored"),
            ("file", None, None, b"minimal text"),
        ]))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let recorded = provider.document.lock().unwrap();
    let document = recorded.as_ref().unwrap();
    assert_eq!(document.namespace, "documents");
    assert_eq!(document.tags, ["first", "second"]);
    assert_eq!(document.taint, MemoryTaint::ExternalSync);
    assert_eq!(document.category, MemoryCategory::Core.to_string());
    assert_eq!(document.content, "minimal text");
}

#[derive(Default)]
struct RecordingProvider {
    document: Mutex<Option<NamespaceDocumentInput>>,
    relations: Vec<GraphRelationRecord>,
    failure: Mutex<Option<MemoryError>>,
    store_call: Mutex<Option<StoreCall>>,
    recall_call: Mutex<Option<RecallCall>>,
    export_call: Mutex<Option<(Option<String>, usize)>>,
    relations_call: Mutex<Option<RelationsCall>>,
}

struct StoreCall {
    namespace: String,
    key: String,
    content: String,
    category: MemoryCategory,
    session_id: Option<String>,
    taint: MemoryTaint,
}

struct RecallCall {
    query: String,
    limit: usize,
    opts: OwnedRecallOpts,
}

type RelationsCall = (Option<String>, Option<String>, Option<String>, usize);

impl RecordingProvider {
    fn failing(error: MemoryError) -> Self {
        Self {
            failure: Mutex::new(Some(error)),
            ..Self::default()
        }
    }

    fn take_failure(&self) -> Result<(), MemoryError> {
        match self.failure.lock().unwrap().take() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

#[async_trait]
impl MemoryCore for RecordingProvider {
    async fn store(
        &self,
        namespace: &str,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        taint: MemoryTaint,
    ) -> Result<(), MemoryError> {
        self.take_failure()?;
        *self.store_call.lock().unwrap() = Some(StoreCall {
            namespace: namespace.to_string(),
            key: key.to_string(),
            content: content.to_string(),
            category,
            session_id: session_id.map(str::to_string),
            taint,
        });
        Ok(())
    }

    async fn get(&self, _namespace: &str, _key: &str) -> Result<Option<MemoryEntry>, MemoryError> {
        self.take_failure()?;
        Ok(None)
    }

    async fn forget(&self, _namespace: &str, _key: &str) -> Result<bool, MemoryError> {
        self.take_failure()?;
        Ok(false)
    }

    async fn list(
        &self,
        _namespace: Option<&str>,
        _category: Option<&MemoryCategory>,
        _session_id: Option<&str>,
    ) -> Result<Vec<MemoryEntry>, MemoryError> {
        self.take_failure()?;
        Ok(Vec::new())
    }

    async fn namespaces(&self) -> Result<Vec<NamespaceSummary>, MemoryError> {
        self.take_failure()?;
        Ok(Vec::new())
    }
}

#[async_trait]
impl MemoryRecall for RecordingProvider {
    async fn recall(
        &self,
        query: &str,
        limit: usize,
        opts: &OwnedRecallOpts,
        _scope: Option<&SourceScope>,
    ) -> Result<Vec<MemoryEntry>, MemoryError> {
        self.take_failure()?;
        *self.recall_call.lock().unwrap() = Some(RecallCall {
            query: query.to_string(),
            limit,
            opts: opts.clone(),
        });
        Ok(Vec::new())
    }
}

#[async_trait]
impl MemoryPortability for RecordingProvider {
    async fn export_page(
        &self,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<ExportPage, MemoryError> {
        self.take_failure()?;
        *self.export_call.lock().unwrap() = Some((cursor.map(str::to_string), limit));
        Ok(ExportPage::default())
    }

    async fn import_records(
        &self,
        _records: Vec<ExportRecord>,
    ) -> Result<ImportOutcome, MemoryError> {
        Ok(ImportOutcome::default())
    }
}

#[async_trait]
impl MemoryDocuments for RecordingProvider {
    async fn put_document(&self, input: NamespaceDocumentInput) -> Result<String, MemoryError> {
        self.take_failure()?;
        *self.document.lock().unwrap() = Some(input);
        Ok("document-1".to_string())
    }

    async fn get_document(
        &self,
        _namespace: &str,
        _key: &str,
    ) -> Result<Option<StoredMemoryDocument>, MemoryError> {
        Ok(None)
    }

    async fn list_documents(&self, _namespace: Option<&str>) -> Result<Value, MemoryError> {
        Ok(Value::Null)
    }

    async fn list_namespaces(&self) -> Result<Vec<String>, MemoryError> {
        Ok(Vec::new())
    }

    async fn delete_document(
        &self,
        _namespace: &str,
        _document_id: &str,
    ) -> Result<Value, MemoryError> {
        Ok(Value::Null)
    }

    async fn clear_namespace(&self, _namespace: &str) -> Result<(), MemoryError> {
        Ok(())
    }

    async fn query_documents(
        &self,
        namespace: &str,
        _query: &str,
        _limit: usize,
    ) -> Result<NamespaceRetrievalContext, MemoryError> {
        Ok(NamespaceRetrievalContext {
            namespace: namespace.to_string(),
            query: None,
            context_text: String::new(),
            hits: Vec::new(),
        })
    }
}

#[async_trait]
impl MemoryGraph for RecordingProvider {
    async fn kv_get(
        &self,
        _namespace: Option<&str>,
        _key: &str,
    ) -> Result<Option<MemoryKvRecord>, MemoryError> {
        Ok(None)
    }

    async fn kv_put(
        &self,
        _namespace: Option<&str>,
        _key: &str,
        _value: Value,
    ) -> Result<(), MemoryError> {
        Ok(())
    }

    async fn kv_delete(&self, _namespace: Option<&str>, _key: &str) -> Result<bool, MemoryError> {
        Ok(false)
    }

    async fn kv_list(
        &self,
        _namespace: Option<&str>,
        _prefix: Option<&str>,
        _limit: usize,
    ) -> Result<Vec<MemoryKvRecord>, MemoryError> {
        Ok(Vec::new())
    }

    async fn relations(
        &self,
        namespace: Option<&str>,
        subject: Option<&str>,
        predicate: Option<&str>,
        limit: usize,
    ) -> Result<Vec<GraphRelationRecord>, MemoryError> {
        self.take_failure()?;
        *self.relations_call.lock().unwrap() = Some((
            namespace.map(str::to_string),
            subject.map(str::to_string),
            predicate.map(str::to_string),
            limit,
        ));
        Ok(self
            .relations
            .iter()
            .filter(|edge| namespace.is_none_or(|value| edge.namespace.as_deref() == Some(value)))
            .filter(|edge| subject.is_none_or(|value| edge.subject == value))
            .filter(|edge| predicate.is_none_or(|value| edge.predicate == value))
            .take(limit)
            .cloned()
            .collect())
    }

    async fn put_relation(&self, _relation: GraphRelationRecord) -> Result<(), MemoryError> {
        Ok(())
    }
}

#[async_trait]
impl MemoryProvider for RecordingProvider {
    fn driver_id(&self) -> &str {
        "recording"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::mandatory()
            .with(Capability::Documents)
            .with(Capability::Graph)
    }

    async fn health(&self) -> MemoryHealth {
        MemoryHealth::Ready
    }

    fn as_documents(&self) -> Option<&dyn MemoryDocuments> {
        Some(self)
    }

    fn as_graph(&self) -> Option<&dyn MemoryGraph> {
        Some(self)
    }
}

#[tokio::test]
async fn text_upload_preserves_filename_tags_category_and_taint() {
    let provider = Arc::new(RecordingProvider::default());
    let state = empty_state();
    *state.active.write().await = Some(provider.clone());

    let response = test_app(state)
        .oneshot(multipart_request(&[
            ("namespace", None, None, b"document:manual"),
            ("key", None, None, b"readme"),
            ("tags", None, None, b"guide, important"),
            ("category", None, None, b"custom:manual"),
            ("taint", None, None, b"external_sync"),
            (
                "file",
                Some("README.txt"),
                Some("text/plain"),
                b"TinyMemory manual",
            ),
        ]))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await["route"], "documents");

    let recorded = provider.document.lock().unwrap();
    let document = recorded.as_ref().unwrap();
    assert_eq!(document.namespace, "document:manual");
    assert_eq!(document.key, "readme");
    assert_eq!(document.content, "TinyMemory manual");
    assert_eq!(document.tags, ["guide", "important"]);
    assert_eq!(document.category, "custom:manual");
    assert_eq!(document.taint, MemoryTaint::ExternalSync);
    assert_eq!(document.metadata["filename"], "README.txt");
    assert_eq!(document.metadata["source_format"], "plain_text");
}

#[tokio::test]
async fn graph_view_http_route_returns_a_bounded_renderable_view() {
    let provider = Arc::new(RecordingProvider {
        relations: vec![GraphRelationRecord {
            namespace: Some("people".to_string()),
            subject: "ada".to_string(),
            predicate: "wrote".to_string(),
            object: "notes".to_string(),
            attrs: Value::Null,
            updated_at: 0.0,
            evidence_count: 1,
            order_index: None,
            document_ids: Vec::new(),
            chunk_ids: Vec::new(),
        }],
        ..RecordingProvider::default()
    });
    let state = empty_state();
    *state.active.write().await = Some(provider);

    let response = test_app(state)
        .oneshot(json_request(
            Method::POST,
            "/api/graph/view",
            json!({
                "namespace": "people",
                "seeds": ["ada"],
                "depth": 1,
                "max_nodes": 8,
                "max_edges": 8
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["namespace"], "people");
    assert_eq!(body["seeds"], json!(["ada"]));
    assert_eq!(body["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(body["edges"][0]["predicate"], "wrote");
}

#[tokio::test]
async fn graph_view_reports_an_unadvertised_graph_family() {
    let router = test_app(empty_state());
    connect_local(&router).await;
    let response = router
        .oneshot(json_request(
            Method::POST,
            "/api/graph/view",
            json!({ "seeds": ["missing"] }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    assert_eq!(
        json_body(response).await["error"],
        "the connected engine does not advertise a graph"
    );
}

#[tokio::test]
async fn graph_relations_filters_and_non_graph_engines_are_exposed_over_http() {
    let local = test_app(empty_state());
    connect_local(&local).await;
    let unsupported = local
        .oneshot(
            Request::get("/api/graph/relations")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unsupported.status(), StatusCode::NOT_IMPLEMENTED);

    let provider = Arc::new(RecordingProvider {
        relations: vec![
            GraphRelationRecord {
                namespace: Some("people".into()),
                subject: "ada".into(),
                predicate: "wrote".into(),
                object: "notes".into(),
                attrs: Value::Null,
                updated_at: 0.0,
                evidence_count: 1,
                order_index: None,
                document_ids: Vec::new(),
                chunk_ids: Vec::new(),
            },
            GraphRelationRecord {
                namespace: Some("other".into()),
                subject: "ada".into(),
                predicate: "read".into(),
                object: "book".into(),
                attrs: Value::Null,
                updated_at: 0.0,
                evidence_count: 1,
                order_index: None,
                document_ids: Vec::new(),
                chunk_ids: Vec::new(),
            },
        ],
        ..RecordingProvider::default()
    });
    let state = empty_state();
    *state.active.write().await = Some(provider);
    let response = test_app(state)
        .oneshot(
            Request::get(
                "/api/graph/relations?namespace=people&subject=ada&predicate=wrote&limit=1",
            )
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["object"], "notes");
}

fn state_with_fetch_error(error: MemoryError) -> SharedState {
    let error = Arc::new(Mutex::new(Some(error)));
    Arc::new(AppState {
        active: RwLock::new(Some(Arc::new(RecordingProvider::default()))),
        url_fetcher: Arc::new(move |_| {
            let error = error
                .lock()
                .unwrap()
                .take()
                .expect("test fetcher is called exactly once");
            Box::pin(async move { Err(error) })
        }),
    })
}

fn state_with_document(provider: Arc<RecordingProvider>) -> SharedState {
    Arc::new(AppState {
        active: RwLock::new(Some(provider)),
        url_fetcher: Arc::new(|url| {
            Box::pin(async move {
                Ok(RawDocument::new("fetched text")
                    .with_origin(url)
                    .with_mime("text/plain"))
            })
        }),
    })
}

fn state_with_local_document(content: &'static str, mime: &'static str) -> SharedState {
    let memory: Arc<dyn tinymemory_tinycortex::tinycortex::memory::Memory> =
        Arc::new(tinymemory_tinycortex::InMemoryMemoryStore::new());
    let provider: Arc<dyn MemoryProvider> = Arc::new(tinymemory_tinycortex::provider(memory));
    Arc::new(AppState {
        active: RwLock::new(Some(provider)),
        url_fetcher: Arc::new(move |url| {
            Box::pin(async move { Ok(RawDocument::new(content).with_origin(url).with_mime(mime)) })
        }),
    })
}

#[tokio::test]
async fn text_upload_falls_back_to_core_storage_for_the_local_engine() {
    let router = test_app(empty_state());
    connect_local(&router).await;

    let uploaded = router
        .clone()
        .oneshot(multipart_request(&[
            ("namespace", None, None, b"manuals"),
            ("key", None, None, b"quickstart"),
            ("tags", None, None, b"guide, local"),
            (
                "file",
                Some("guide.html"),
                Some("text/html"),
                b"<h1>Start</h1><p>Use TinyMemory locally.</p>",
            ),
        ]))
        .await
        .unwrap();
    assert_eq!(uploaded.status(), StatusCode::OK);
    let receipt = json_body(uploaded).await;
    assert_eq!(receipt["route"], "core");
    assert_eq!(receipt["namespace"], "manuals");
    assert_eq!(receipt["key"], "quickstart");

    let stored = router
        .oneshot(
            Request::get("/api/get?namespace=manuals&key=quickstart")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(stored.status(), StatusCode::OK);
    let entry = json_body(stored).await;
    assert_eq!(entry["key"], "quickstart");
    assert!(entry["content"].as_str().unwrap().contains("Start"));
    assert!(entry["content"]
        .as_str()
        .unwrap()
        .contains("Use TinyMemory locally."));
    assert_eq!(entry["taint"], "external_sync");
}

#[tokio::test]
async fn url_ingest_falls_back_to_core_storage_without_network() {
    let router = test_app(state_with_local_document(
        "# Remote guide\n\nFetched deterministically.",
        "text/markdown",
    ));
    let ingested = router
        .clone()
        .oneshot(json_request(
            Method::POST,
            "/api/ingest/url",
            json!({
                "url": "https://example.com/guides/remote.md",
                "namespace": "web-guides",
                "key": "remote",
                "tags": ["guide"]
            }),
        ))
        .await
        .unwrap();
    assert_eq!(ingested.status(), StatusCode::OK);
    let receipt = json_body(ingested).await;
    assert_eq!(receipt["route"], "core");
    assert_eq!(receipt["key"], "remote");

    let recalled = router
        .oneshot(json_request(
            Method::POST,
            "/api/recall",
            json!({ "query": "Fetched deterministically", "namespace": "web-guides" }),
        ))
        .await
        .unwrap();
    assert_eq!(recalled.status(), StatusCode::OK);
    let hits = json_body(recalled).await;
    assert_eq!(hits[0]["key"], "remote");
    assert_eq!(hits[0]["taint"], "external_sync");
}

#[tokio::test]
async fn successful_url_ingest_preserves_origin_and_optional_intake_fields() {
    let provider = Arc::new(RecordingProvider::default());
    let response = test_app(state_with_document(provider.clone()))
        .oneshot(json_request(
            Method::POST,
            "/api/ingest/url",
            json!({
                "url": "https://example.com/note.txt",
                "namespace": "web",
                "key": "note",
                "tags": ["remote", "text"],
                "taint": "internal",
                "category": "daily"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let recorded = provider.document.lock().unwrap();
    let document = recorded.as_ref().unwrap();
    assert_eq!(document.namespace, "web");
    assert_eq!(document.key, "note");
    assert_eq!(document.content, "fetched text");
    assert_eq!(document.tags, ["remote", "text"]);
    assert_eq!(document.taint, MemoryTaint::Internal);
    assert_eq!(document.category, MemoryCategory::Daily.to_string());
    assert_eq!(document.metadata["origin"], "https://example.com/note.txt");
}

#[tokio::test]
async fn minimal_url_ingest_uses_external_core_defaults_and_a_generated_key() {
    let provider = Arc::new(RecordingProvider::default());
    let response = test_app(state_with_document(provider.clone()))
        .oneshot(json_request(
            Method::POST,
            "/api/ingest/url",
            json!({
                "url": "http://example.com:8080/path/../note.txt",
                "namespace": "web"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let recorded = provider.document.lock().unwrap();
    let document = recorded.as_ref().unwrap();
    assert_eq!(document.namespace, "web");
    assert!(!document.key.is_empty());
    assert!(document.tags.is_empty());
    assert_eq!(document.taint, MemoryTaint::ExternalSync);
    assert_eq!(document.category, MemoryCategory::Core.to_string());
    assert_eq!(
        document.metadata["origin"],
        "http://example.com:8080/note.txt"
    );
}

#[tokio::test]
async fn explicit_empty_upload_options_preserve_closed_intake_defaults() {
    let provider = Arc::new(RecordingProvider::default());
    let state = empty_state();
    *state.active.write().await = Some(provider.clone());

    let response = test_app(state)
        .oneshot(multipart_request(&[
            ("namespace", None, None, b"documents"),
            ("key", None, None, b""),
            ("tags", None, None, b", ,"),
            ("taint", None, None, b""),
            ("category", None, None, b""),
            ("file", Some("note.md"), Some("text/markdown"), b"# Note"),
        ]))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let recorded = provider.document.lock().unwrap();
    let document = recorded.as_ref().unwrap();
    assert!(!document.key.is_empty());
    assert!(document.tags.is_empty());
    assert_eq!(document.taint, MemoryTaint::ExternalSync);
    assert_eq!(document.category, MemoryCategory::Core.to_string());
    assert_eq!(document.metadata["source_format"], "markdown");
}

#[tokio::test]
async fn url_ingest_rejects_malformed_schemes_private_targets_and_credentials() {
    let router = test_app(empty_state());
    connect_local(&router).await;
    let cases = [
        ("not a URL", "invalid URL"),
        ("file:///etc/passwd", "URL scheme must be http or https"),
        (
            "http://127.0.0.1/private",
            "URL is not an allowed fetch target",
        ),
        (
            "https://user:top-secret@example.com/private",
            "URL credentials are not allowed",
        ),
    ];

    for (url, message) in cases {
        let response = router
            .clone()
            .oneshot(json_request(
                Method::POST,
                "/api/ingest/url",
                json!({ "url": url, "namespace": "documents" }),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{url}");
        let body = json_body(response).await;
        assert_eq!(body["error"], message, "{url}");
        assert!(!body.to_string().contains("top-secret"));
    }
}

#[tokio::test]
async fn url_ingest_maps_size_and_blocked_redirect_failures_without_network() {
    let cases = [
        (
            MemoryError::BudgetExceeded("response body exceeds 33554432-byte limit".to_string()),
            StatusCode::PAYLOAD_TOO_LARGE,
            "URL response exceeds document size limit",
        ),
        (
            MemoryError::Invalid(
                "redirect from https://example.com/?api_key=top-secret is not allowed".to_string(),
            ),
            StatusCode::BAD_REQUEST,
            "URL is not an allowed fetch target",
        ),
    ];

    for (error, status, message) in cases {
        let response = test_app(state_with_fetch_error(error))
            .oneshot(json_request(
                Method::POST,
                "/api/ingest/url",
                json!({ "url": "https://example.com/document.txt", "namespace": "documents" }),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), status);
        let body = json_body(response).await;
        assert_eq!(body["error"], message);
        assert!(!body.to_string().contains("top-secret"));
    }
}

fn state_with_provider(provider: RecordingProvider) -> SharedState {
    Arc::new(AppState {
        active: RwLock::new(Some(Arc::new(provider))),
        url_fetcher: guarded_url_fetcher(),
    })
}

#[tokio::test]
async fn empty_categories_use_defaults_in_every_core_request_shape() {
    let cases = [
        (
            Method::POST,
            "/api/store",
            json!({
                "namespace": "notes",
                "key": "one",
                "content": "body",
                "category": ""
            }),
        ),
        (
            Method::POST,
            "/api/recall",
            json!({ "query": "body", "category": "" }),
        ),
    ];

    for (method, uri, request) in cases {
        let response = test_app(state_with_provider(RecordingProvider::default()))
            .oneshot(json_request(method, uri, request))
            .await
            .unwrap();
        assert!(response.status().is_success(), "{uri}");
    }

    let listed = test_app(state_with_provider(RecordingProvider::default()))
        .oneshot(
            Request::get("/api/list?category=")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(listed.status(), StatusCode::OK);
}

#[tokio::test]
async fn arbitrary_document_categories_are_preserved_as_custom_values() {
    let upload_provider = Arc::new(RecordingProvider::default());
    let upload_state = empty_state();
    *upload_state.active.write().await = Some(upload_provider.clone());
    let uploaded = test_app(upload_state)
        .oneshot(multipart_request(&[
            ("namespace", None, None, b"documents"),
            ("category", None, None, b"research-notes"),
            ("file", Some("note.txt"), Some("text/plain"), b"body"),
        ]))
        .await
        .unwrap();
    assert_eq!(uploaded.status(), StatusCode::OK);
    assert_eq!(
        upload_provider
            .document
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .category,
        "custom:research-notes"
    );

    let ingest_provider = Arc::new(RecordingProvider::default());
    let ingested = test_app(state_with_document(ingest_provider.clone()))
        .oneshot(json_request(
            Method::POST,
            "/api/ingest/url",
            json!({
                "url": "https://example.com/note.txt",
                "namespace": "documents",
                "category": "web-clipping"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(ingested.status(), StatusCode::OK);
    assert_eq!(
        ingest_provider
            .document
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .category,
        "custom:web-clipping"
    );
}

#[tokio::test]
async fn core_filter_cursor_and_graph_queries_reach_the_provider_unchanged() {
    let provider = Arc::new(RecordingProvider::default());
    let state = empty_state();
    *state.active.write().await = Some(provider.clone());
    let router = test_app(state);

    let stored = router
        .clone()
        .oneshot(json_request(
            Method::POST,
            "/api/store",
            json!({
                "namespace": "projects",
                "key": "alpha",
                "content": "project context",
                "category": "project-memory",
                "session_id": "session-9",
                "taint": "unrecognised-client-value"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(stored.status(), StatusCode::NO_CONTENT);
    {
        let store = provider.store_call.lock().unwrap();
        let store = store.as_ref().unwrap();
        assert_eq!(store.namespace, "projects");
        assert_eq!(store.key, "alpha");
        assert_eq!(store.content, "project context");
        assert_eq!(
            store.category,
            MemoryCategory::Custom("project-memory".into())
        );
        assert_eq!(store.session_id.as_deref(), Some("session-9"));
        assert_eq!(store.taint, MemoryTaint::Internal);
    }

    let recalled = router
        .clone()
        .oneshot(json_request(
            Method::POST,
            "/api/recall",
            json!({
                "query": "project",
                "limit": 37,
                "namespace": "projects",
                "category": "conversation",
                "session_id": "session-9",
                "min_score": 0.625,
                "cross_session": true
            }),
        ))
        .await
        .unwrap();
    assert_eq!(recalled.status(), StatusCode::OK);
    {
        let recall = provider.recall_call.lock().unwrap();
        let recall = recall.as_ref().unwrap();
        assert_eq!(recall.query, "project");
        assert_eq!(recall.limit, 37);
        assert_eq!(recall.opts.namespace.as_deref(), Some("projects"));
        assert_eq!(recall.opts.category, Some(MemoryCategory::Conversation));
        assert_eq!(recall.opts.session_id.as_deref(), Some("session-9"));
        assert_eq!(recall.opts.min_score, Some(0.625));
        assert!(recall.opts.cross_session);
    }

    let exported = router
        .clone()
        .oneshot(
            Request::get("/api/export?cursor=page%3A2&limit=73")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(exported.status(), StatusCode::OK);
    assert_eq!(
        provider.export_call.lock().unwrap().as_ref(),
        Some(&(Some("page:2".to_string()), 73))
    );

    let relations = router
        .clone()
        .oneshot(
            Request::get(
                "/api/graph/relations?namespace=projects&subject=alpha&predicate=depends_on&limit=29",
            )
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(relations.status(), StatusCode::OK);
    assert_eq!(
        provider.relations_call.lock().unwrap().as_ref(),
        Some(&(
            Some("projects".to_string()),
            Some("alpha".to_string()),
            Some("depends_on".to_string()),
            29,
        ))
    );

    let defaults = [
        (Method::POST, "/api/recall", Some(json!({ "query": "all" }))),
        (Method::GET, "/api/export", None),
        (Method::GET, "/api/graph/relations", None),
    ];
    for (method, uri, body) in defaults {
        let request = match body {
            Some(body) => json_request(method, uri, body),
            None => Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        };
        assert_eq!(
            router.clone().oneshot(request).await.unwrap().status(),
            StatusCode::OK
        );
    }
    assert_eq!(
        provider.recall_call.lock().unwrap().as_ref().unwrap().limit,
        10
    );
    assert_eq!(
        provider.export_call.lock().unwrap().as_ref(),
        Some(&(None, 50))
    );
    assert_eq!(
        provider.relations_call.lock().unwrap().as_ref(),
        Some(&(None, None, None, 100))
    );
}

#[tokio::test]
async fn provider_failures_are_propagated_by_every_core_handler() {
    let requests = [
        json_request(
            Method::POST,
            "/api/store",
            json!({ "namespace": "n", "key": "k", "content": "body" }),
        ),
        Request::get("/api/get?namespace=n&key=k")
            .body(Body::empty())
            .unwrap(),
        json_request(
            Method::POST,
            "/api/forget",
            json!({ "namespace": "n", "key": "k" }),
        ),
        Request::get("/api/list").body(Body::empty()).unwrap(),
        Request::get("/api/namespaces").body(Body::empty()).unwrap(),
        json_request(Method::POST, "/api/recall", json!({ "query": "body" })),
        Request::get("/api/export").body(Body::empty()).unwrap(),
    ];

    for request in requests {
        let response = test_app(state_with_provider(RecordingProvider::failing(
            MemoryError::Backend("driver rejected request".into()),
        )))
        .oneshot(request)
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            json_body(response).await["error"],
            "backend failed: driver rejected request"
        );
    }
}

#[tokio::test]
async fn absent_core_records_have_stable_success_response_shapes() {
    let router = test_app(state_with_provider(RecordingProvider::default()));

    let missing = router
        .clone()
        .oneshot(
            Request::get("/api/get?namespace=notes&key=missing")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::OK);
    assert_eq!(json_body(missing).await, Value::Null);

    let forgotten = router
        .clone()
        .oneshot(json_request(
            Method::POST,
            "/api/forget",
            json!({ "namespace": "notes", "key": "missing" }),
        ))
        .await
        .unwrap();
    assert_eq!(forgotten.status(), StatusCode::OK);
    assert_eq!(json_body(forgotten).await, json!(false));

    let namespaces = router
        .oneshot(Request::get("/api/namespaces").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(namespaces.status(), StatusCode::OK);
    assert_eq!(json_body(namespaces).await, json!([]));
}

#[tokio::test]
async fn provider_failures_are_propagated_by_graph_and_document_handlers() {
    let graph_requests = [
        Request::get("/api/graph/relations")
            .body(Body::empty())
            .unwrap(),
        json_request(Method::POST, "/api/graph/view", json!({ "seeds": ["ada"] })),
    ];
    for request in graph_requests {
        let response = test_app(state_with_provider(RecordingProvider::failing(
            MemoryError::Unavailable("graph offline".into()),
        )))
        .oneshot(request)
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            json_body(response).await["error"],
            "unavailable: graph offline"
        );
    }

    let upload = test_app(state_with_provider(RecordingProvider::failing(
        MemoryError::BudgetExceeded("document quota reached".into()),
    )))
    .oneshot(multipart_request(&[
        ("namespace", None, None, b"documents"),
        ("file", Some("note.txt"), Some("text/plain"), b"body"),
    ]))
    .await
    .unwrap();
    assert_eq!(upload.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        json_body(upload).await["error"],
        "budget exceeded: document quota reached"
    );
}

#[tokio::test]
async fn url_fetch_backend_details_and_query_secrets_are_never_reflected() {
    let response = test_app(state_with_fetch_error(MemoryError::Backend(
        "GET https://example.com/note?token=top-secret failed".into(),
    )))
    .oneshot(json_request(
        Method::POST,
        "/api/ingest/url",
        json!({
            "url": "https://example.com/note?token=top-secret",
            "namespace": "documents"
        }),
    ))
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = json_body(response).await;
    assert_eq!(body["error"], "URL fetch failed");
    assert!(!body.to_string().contains("top-secret"));
}

#[tokio::test]
async fn url_fetch_timeout_keeps_its_status_but_hides_upstream_details() {
    let response = test_app(state_with_fetch_error(MemoryError::Timeout(
        "https://example.com/?signature=top-secret timed out".into(),
    )))
    .oneshot(json_request(
        Method::POST,
        "/api/ingest/url",
        json!({
            "url": "https://example.com/document.txt",
            "namespace": "documents"
        }),
    ))
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    let body = json_body(response).await;
    assert_eq!(body["error"], "URL fetch failed");
    assert!(!body.to_string().contains("top-secret"));
}

#[tokio::test]
async fn url_validation_rejects_password_only_credentials_and_accepts_https_ports() {
    let Err(ApiError(status, message)) = validate_ingest_url("https://:secret@example.com/private")
    else {
        panic!("password-only URL credentials must be rejected");
    };
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(message, "URL credentials are not allowed");

    let Ok(url) = validate_ingest_url("HTTPS://example.com:8443/a/../note?q=one#section") else {
        panic!("a credential-free HTTPS URL must be accepted");
    };
    assert_eq!(url, "https://example.com:8443/note?q=one#section");
}

#[test]
fn browser_upload_workflow_contract_executes() {
    let output = Command::new("node")
        .args(["--test", "web/workflows.test.js"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("Node.js is required to test the browser workflow contract");
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
