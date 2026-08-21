//! Local HTTP harness for exercising TinyMemory engines by hand.
//!
//! Not a host. It skips every policy layer a real host owns (tier
//! enforcement, taint stamping, redaction, egress checks) and exists purely so
//! a person can point a browser at a running server, pick an engine, and call
//! `store`/`recall`/`list`/`export` against it directly. See this crate's
//! `README.md` for how to run it.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;

use axum::extract::{Multipart, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tower_http::services::ServeDir;

use tinymemory_api::graph::GraphViewQuery;
use tinymemory_api::provider::types::SourceScope;
use tinymemory_api::provider::MemoryProvider;
use tinymemory_api::recall::OwnedRecallOpts;
use tinymemory_api::types::{MemoryCategory, MemoryTaint};
use tinymemory_documents::convert::{ConverterChain, RawDocument};
use tinymemory_documents::ingest::{DocumentIntake, IntakeRequest};

struct AppState {
    active: RwLock<Option<Arc<dyn MemoryProvider>>>,
    url_fetcher: UrlFetcher,
}

type SharedState = Arc<AppState>;
type FetchFuture =
    Pin<Box<dyn Future<Output = Result<RawDocument, tinymemory_api::error::MemoryError>> + Send>>;
type UrlFetcher = Arc<dyn Fn(String) -> FetchFuture + Send + Sync>;

fn guarded_url_fetcher() -> UrlFetcher {
    Arc::new(|url| Box::pin(async move { tinymemory_documents::fetch::fetch_url(&url).await }))
}

/// A JSON-friendly wrapper around [`tinymemory_api::error::MemoryError`] and
/// this harness's own connection-state errors.
struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}

impl From<tinymemory_api::error::MemoryError> for ApiError {
    fn from(err: tinymemory_api::error::MemoryError) -> Self {
        // The document intake routes are the first callers to send caller
        // input (not just driver responses) through this conversion, so
        // `Invalid`/`BudgetExceeded`/etc. need their own status rather than
        // the blanket 502 that was close enough when every error came from a
        // backend.
        use tinymemory_api::error::MemoryError as E;
        let status = match &err {
            E::Invalid(_) | E::PathEscape(_) => StatusCode::BAD_REQUEST,
            E::NotFound(_) => StatusCode::NOT_FOUND,
            E::BudgetExceeded(_) => StatusCode::PAYLOAD_TOO_LARGE,
            E::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            E::Timeout(_) => StatusCode::GATEWAY_TIMEOUT,
            E::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::BAD_GATEWAY,
        };
        ApiError(status, err.to_string())
    }
}

fn parse_category(value: &Option<String>) -> Result<Option<MemoryCategory>, ApiError> {
    value
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<MemoryCategory>()
                .map_err(|e| ApiError(StatusCode::BAD_REQUEST, e))
        })
        .transpose()
}

fn parse_taint(value: &Option<String>) -> MemoryTaint {
    match value.as_deref() {
        Some("external_sync") => MemoryTaint::ExternalSync,
        _ => MemoryTaint::Internal,
    }
}

async fn current(state: &SharedState) -> Result<Arc<dyn MemoryProvider>, ApiError> {
    state
        .active
        .read()
        .await
        .clone()
        .ok_or_else(|| ApiError(StatusCode::CONFLICT, "no engine connected yet".into()))
}

#[derive(Deserialize)]
struct ConnectRequest {
    engine: String,
    #[serde(default)]
    deployment: Option<String>,
    #[serde(default)]
    endpoint: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
}

#[derive(Serialize, Clone)]
struct EngineStatus {
    connected: bool,
    driver_id: Option<String>,
    engine: Option<String>,
    has_graph: bool,
}

async fn connect(
    State(state): State<SharedState>,
    Json(req): Json<ConnectRequest>,
) -> Result<Json<EngineStatus>, ApiError> {
    let bad_request = |msg: &str| ApiError(StatusCode::BAD_REQUEST, msg.to_string());

    let provider: Arc<dyn MemoryProvider> = match req.engine.as_str() {
        "local" => {
            let memory: Arc<dyn tinymemory_tinycortex::tinycortex::memory::Memory> =
                Arc::new(tinymemory_tinycortex::InMemoryMemoryStore::new());
            Arc::new(tinymemory_tinycortex::provider(memory))
        }
        "supermemory" => {
            let endpoint = req
                .endpoint
                .as_deref()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| bad_request("supermemory requires an endpoint URL"))?;
            let memory = tinymemory_remote::SupermemoryMemory::new(
                endpoint,
                req.api_key.as_deref().filter(|s| !s.is_empty()),
            )
            .map_err(|e| bad_request(&e.to_string()))?;
            Arc::new(tinymemory_remote::supermemory_provider(memory))
        }
        "mem0" => {
            let endpoint = req
                .endpoint
                .as_deref()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| bad_request("mem0 requires an endpoint URL"))?;
            let api_key = req.api_key.as_deref().filter(|s| !s.is_empty());
            let is_cloud = match req.deployment.as_deref() {
                Some("cloud") => true,
                Some("self_hosted") => false,
                None => endpoint == tinymemory_remote::MEM0_API_ENDPOINT,
                Some(other) => {
                    return Err(bad_request(&format!("unknown Mem0 deployment: {other}")));
                }
            };
            let memory = if is_cloud {
                tinymemory_remote::Mem0Memory::api(
                    endpoint,
                    api_key.ok_or_else(|| bad_request("Mem0 Cloud requires an API key"))?,
                )
            } else {
                tinymemory_remote::Mem0Memory::new(endpoint, api_key)
            }
            .map_err(|e| bad_request(&e.to_string()))?;
            // Also advertises Graph via `Mem0Graph` — a client-side heuristic
            // over the same stored entries, not Mem0's native Graph Memory
            // (dropped from the self-hosted OSS package's 2.x line; see the
            // module docs on `Mem0Graph`).
            Arc::new(tinymemory_remote::mem0_graph_provider(memory))
        }
        "cognee" => {
            let endpoint = req
                .endpoint
                .as_deref()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| bad_request("cognee requires an endpoint URL"))?;
            let api_key = req.api_key.as_deref().filter(|s| !s.is_empty());
            let is_cloud = match req.deployment.as_deref() {
                Some("cloud") => true,
                Some("self_hosted") | None => false,
                Some(other) => {
                    return Err(bad_request(&format!("unknown Cognee deployment: {other}")));
                }
            };
            let memory = if is_cloud {
                tinymemory_remote::CogneeMemory::api(
                    endpoint,
                    api_key.ok_or_else(|| bad_request("Cognee Cloud requires an API key"))?,
                )
            } else {
                tinymemory_remote::CogneeMemory::new(endpoint, api_key)
            }
            .map_err(|e| bad_request(&e.to_string()))?;
            // Cognee is graph-native, so its provider also advertises Graph
            // (relations only — see `CogneeGraph`'s docs for the exact split
            // between what's a real endpoint and what isn't).
            let provider = if is_cloud {
                tinymemory_remote::cognee_api_graph_provider(
                    memory,
                    endpoint,
                    api_key.unwrap_or_default(),
                )
            } else {
                tinymemory_remote::cognee_graph_provider(memory, endpoint, api_key)
            }
            .map_err(|e| bad_request(&e.to_string()))?;
            Arc::new(provider)
        }
        other => {
            return Err(bad_request(&format!("unknown engine: {other}")));
        }
    };

    let status = EngineStatus {
        connected: true,
        driver_id: Some(provider.driver_id().to_string()),
        engine: Some(req.engine),
        has_graph: provider.as_graph().is_some(),
    };
    *state.active.write().await = Some(provider);
    Ok(Json(status))
}

async fn disconnect(State(state): State<SharedState>) -> Json<EngineStatus> {
    *state.active.write().await = None;
    Json(EngineStatus {
        connected: false,
        driver_id: None,
        engine: None,
        has_graph: false,
    })
}

async fn status(State(state): State<SharedState>) -> Json<EngineStatus> {
    let guard = state.active.read().await;
    Json(EngineStatus {
        connected: guard.is_some(),
        driver_id: guard.as_ref().map(|p| p.driver_id().to_string()),
        engine: None,
        has_graph: guard.as_ref().is_some_and(|p| p.as_graph().is_some()),
    })
}

#[derive(Deserialize)]
struct StoreRequest {
    namespace: String,
    key: String,
    content: String,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    taint: Option<String>,
}

async fn store(
    State(state): State<SharedState>,
    Json(req): Json<StoreRequest>,
) -> Result<StatusCode, ApiError> {
    let provider = current(&state).await?;
    let category = parse_category(&req.category)?.unwrap_or(MemoryCategory::Core);
    let taint = parse_taint(&req.taint);
    provider
        .store(
            &req.namespace,
            &req.key,
            &req.content,
            category,
            req.session_id.as_deref(),
            taint,
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct GetQuery {
    namespace: String,
    key: String,
}

async fn get_entry(
    State(state): State<SharedState>,
    Query(q): Query<GetQuery>,
) -> Result<Response, ApiError> {
    let provider = current(&state).await?;
    let entry = provider.get(&q.namespace, &q.key).await?;
    Ok(Json(entry).into_response())
}

#[derive(Deserialize)]
struct ForgetRequest {
    namespace: String,
    key: String,
}

async fn forget(
    State(state): State<SharedState>,
    Json(req): Json<ForgetRequest>,
) -> Result<Json<bool>, ApiError> {
    let provider = current(&state).await?;
    let existed = provider.forget(&req.namespace, &req.key).await?;
    Ok(Json(existed))
}

#[derive(Deserialize, Default)]
struct ListQuery {
    #[serde(default)]
    namespace: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
}

async fn list(
    State(state): State<SharedState>,
    Query(q): Query<ListQuery>,
) -> Result<Response, ApiError> {
    let provider = current(&state).await?;
    let category = parse_category(&q.category)?;
    let entries = provider
        .list(
            q.namespace.as_deref(),
            category.as_ref(),
            q.session_id.as_deref(),
        )
        .await?;
    Ok(Json(entries).into_response())
}

async fn namespaces(State(state): State<SharedState>) -> Result<Response, ApiError> {
    let provider = current(&state).await?;
    let namespaces = provider.namespaces().await?;
    Ok(Json(namespaces).into_response())
}

#[derive(Deserialize)]
struct RecallRequest {
    query: String,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    namespace: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    min_score: Option<f64>,
    #[serde(default)]
    cross_session: bool,
}

fn default_limit() -> usize {
    10
}

async fn recall(
    State(state): State<SharedState>,
    Json(req): Json<RecallRequest>,
) -> Result<Response, ApiError> {
    let provider = current(&state).await?;
    let category = parse_category(&req.category)?;
    let opts = OwnedRecallOpts {
        namespace: req.namespace,
        category,
        session_id: req.session_id,
        min_score: req.min_score,
        cross_session: req.cross_session,
    };
    let hits = provider
        .recall(&req.query, req.limit, &opts, None::<&SourceScope>)
        .await?;
    Ok(Json(hits).into_response())
}

#[derive(Deserialize, Default)]
struct ExportQuery {
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default = "default_export_limit")]
    limit: usize,
}

fn default_export_limit() -> usize {
    50
}

async fn export(
    State(state): State<SharedState>,
    Query(q): Query<ExportQuery>,
) -> Result<Response, ApiError> {
    let provider = current(&state).await?;
    let page = provider.export_page(q.cursor.as_deref(), q.limit).await?;
    Ok(Json(page).into_response())
}

#[derive(Deserialize, Default)]
struct GraphRelationsQuery {
    #[serde(default)]
    namespace: Option<String>,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    predicate: Option<String>,
    #[serde(default = "default_relations_limit")]
    limit: usize,
}

fn default_relations_limit() -> usize {
    100
}

async fn graph_relations(
    State(state): State<SharedState>,
    Query(q): Query<GraphRelationsQuery>,
) -> Result<Response, ApiError> {
    let provider = current(&state).await?;
    let graph = provider.as_graph().ok_or_else(|| {
        ApiError(
            StatusCode::NOT_IMPLEMENTED,
            "the connected engine does not advertise a graph".to_string(),
        )
    })?;
    let relations = graph
        .relations(
            q.namespace.as_deref(),
            q.subject.as_deref(),
            q.predicate.as_deref(),
            q.limit,
        )
        .await?;
    Ok(Json(relations).into_response())
}

async fn graph_view(
    State(state): State<SharedState>,
    Json(query): Json<GraphViewQuery>,
) -> Result<Response, ApiError> {
    let provider = current(&state).await?;
    let graph = provider.as_graph().ok_or_else(|| {
        ApiError(
            StatusCode::NOT_IMPLEMENTED,
            "the connected engine does not advertise a graph".to_string(),
        )
    })?;
    let view = graph.graph_view(&query).await?;
    Ok(Json(view).into_response())
}

/// The converter chain this harness runs.
///
/// The default one: text, markdown and HTML natively, and a clear error for
/// PDF and DOCX. A real host prepends its own extractor; a harness has nothing
/// to prepend, and pretending otherwise would make it report capabilities the
/// engine behind it does not have.
fn converters() -> ConverterChain {
    ConverterChain::default()
}

/// What this build can convert, and where an accepted document would land.
///
/// Answerable without a write, which is the point: a person driving the
/// harness can see that a PDF will be refused *before* uploading one.
async fn document_formats(State(state): State<SharedState>) -> Result<Response, ApiError> {
    let chain = converters();
    let formats: Vec<String> = chain
        .supported_formats()
        .into_iter()
        .map(|format| format.to_string())
        .collect();
    let route = state.active.read().await.as_ref().map(|provider| {
        DocumentIntake::new(provider.as_ref(), &chain)
            .route()
            .as_str()
            .to_string()
    });
    Ok(Json(serde_json::json!({ "formats": formats, "route": route })).into_response())
}

/// Fields a document upload may carry alongside its file part.
#[derive(Default)]
struct UploadFields {
    namespace: Option<String>,
    key: Option<String>,
    tags: Vec<String>,
    taint: Option<String>,
    category: Option<String>,
    filename: Option<String>,
    content_type: Option<String>,
    bytes: Option<Vec<u8>>,
}

async fn upload_document(
    State(state): State<SharedState>,
    mut multipart: Multipart,
) -> Result<Response, ApiError> {
    let provider = current(&state).await?;
    let mut fields = UploadFields::default();

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|error| ApiError(StatusCode::BAD_REQUEST, error.to_string()))?
    {
        let name = field.name().unwrap_or_default().to_string();
        // The file part is read as bytes and every other part as text: a
        // `.docx` is not UTF-8, and reading it as a string would corrupt it
        // before conversion ever sees it.
        if name == "file" {
            fields.filename = field.file_name().map(str::to_string);
            fields.content_type = field.content_type().map(str::to_string);
            let bytes = field
                .bytes()
                .await
                .map_err(|error| ApiError(StatusCode::BAD_REQUEST, error.to_string()))?;
            fields.bytes = Some(bytes.to_vec());
            continue;
        }
        let value = field
            .text()
            .await
            .map_err(|error| ApiError(StatusCode::BAD_REQUEST, error.to_string()))?;
        match name.as_str() {
            "namespace" => fields.namespace = Some(value),
            "key" => fields.key = Some(value),
            "taint" => fields.taint = Some(value),
            "category" => fields.category = Some(value),
            "tags" => {
                fields.tags = value
                    .split(',')
                    .map(str::trim)
                    .filter(|tag| !tag.is_empty())
                    .map(str::to_string)
                    .collect();
            }
            _ => {}
        }
    }

    let bytes = fields.bytes.ok_or_else(|| {
        ApiError(
            StatusCode::BAD_REQUEST,
            "no `file` part in the upload".to_string(),
        )
    })?;
    let namespace = fields.namespace.ok_or_else(|| {
        ApiError(
            StatusCode::BAD_REQUEST,
            "no `namespace` part in the upload".to_string(),
        )
    })?;

    let mut document = RawDocument::new(bytes);
    if let Some(filename) = fields.filename {
        document = document.with_filename(filename);
    }
    if let Some(content_type) = fields.content_type {
        document = document.with_mime(content_type);
    }

    let request = intake_request(
        IntakeRequest::new(namespace),
        fields.key,
        fields.tags,
        &fields.taint,
        &fields.category,
    )?;

    let chain = converters();
    let receipt = DocumentIntake::new(provider.as_ref(), &chain)
        .accept(&document, &request)
        .await?;
    Ok(Json(receipt).into_response())
}

#[derive(Deserialize)]
struct UrlIngestRequest {
    url: String,
    namespace: String,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    taint: Option<String>,
    #[serde(default)]
    category: Option<String>,
}

async fn ingest_url(
    State(state): State<SharedState>,
    Json(req): Json<UrlIngestRequest>,
) -> Result<Response, ApiError> {
    let provider = current(&state).await?;
    let url = validate_ingest_url(&req.url)?;
    let document = (state.url_fetcher)(url)
        .await
        .map_err(safe_url_fetch_error)?;
    let request = intake_request(
        IntakeRequest::from_url(req.namespace),
        req.key,
        req.tags,
        &req.taint,
        &req.category,
    )?;
    let chain = converters();
    let receipt = DocumentIntake::new(provider.as_ref(), &chain)
        .accept(&document, &request)
        .await?;
    Ok(Json(receipt).into_response())
}

/// Preserve the fetch error's HTTP class without reflecting its URL. Query
/// strings often carry signed tokens, and the fetch layer includes its input
/// URL in diagnostic errors intended for trusted library callers.
fn safe_url_fetch_error(error: tinymemory_api::error::MemoryError) -> ApiError {
    use tinymemory_api::error::MemoryError;

    let message = match &error {
        MemoryError::Invalid(_) => "URL is not an allowed fetch target",
        MemoryError::BudgetExceeded(_) => "URL response exceeds document size limit",
        _ => "URL fetch failed",
    };
    let ApiError(status, _) = ApiError::from(error);
    ApiError(status, message.to_string())
}

/// Validate sensitive URL fields before the fetch layer can include them in
/// an error. The document fetcher remains responsible for SSRF, redirects,
/// DNS pinning, and response-size policy.
fn validate_ingest_url(raw: &str) -> Result<String, ApiError> {
    let url = url::Url::parse(raw)
        .map_err(|_| ApiError(StatusCode::BAD_REQUEST, "invalid URL".to_string()))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "URL scheme must be http or https".to_string(),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "URL credentials are not allowed".to_string(),
        ));
    }
    Ok(url.to_string())
}

/// Apply the optional intake fields both intake endpoints share.
fn intake_request(
    base: IntakeRequest,
    key: Option<String>,
    tags: Vec<String>,
    taint: &Option<String>,
    category: &Option<String>,
) -> Result<IntakeRequest, ApiError> {
    let mut request = base.with_tags(tags);
    // Only an explicit value overrides `IntakeRequest::new`'s closed default
    // (`ExternalSync`, since this content arrived from outside). Applying
    // `parse_taint` unconditionally would silently reverse that default to
    // `Internal` for every request that omits `taint`.
    if let Some(taint) = taint.as_deref().filter(|value| !value.is_empty()) {
        request = request.with_taint(parse_taint(&Some(taint.to_string())));
    }
    if let Some(key) = key.filter(|key| !key.is_empty()) {
        request = request.with_key(key);
    }
    if let Some(category) = parse_category(category)? {
        request = request.with_category(category);
    }
    Ok(request)
}

fn app(state: SharedState, web_dir: impl Into<String>) -> Router {
    let api = Router::new()
        .route("/connect", post(connect))
        .route("/disconnect", post(disconnect))
        .route("/status", get(status))
        .route("/store", post(store))
        .route("/get", get(get_entry))
        .route("/forget", post(forget))
        .route("/list", get(list))
        .route("/namespaces", get(namespaces))
        .route("/recall", post(recall))
        .route("/export", get(export))
        .route("/graph/relations", get(graph_relations))
        .route("/graph/view", post(graph_view))
        .route("/documents/formats", get(document_formats))
        .route("/documents/upload", post(upload_document))
        .route("/ingest/url", post(ingest_url))
        .with_state(state);

    Router::new()
        .nest("/api", api)
        .fallback_service(ServeDir::new(web_dir.into()))
}

#[tokio::main]
async fn main() {
    let state: SharedState = Arc::new(AppState {
        active: RwLock::new(None),
        url_fetcher: guarded_url_fetcher(),
    });

    let web_dir = std::env::var("TINYMEMORY_TESTING_UI_WEB")
        .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/web").to_string());

    let app = app(state, web_dir);

    let addr: SocketAddr = std::env::var("TINYMEMORY_TESTING_UI_ADDR")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], 4180)));

    println!("tinymemory testing UI listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("bind testing UI address");
    axum::serve(listener, app).await.expect("serve testing UI");
}

#[cfg(test)]
mod test;
