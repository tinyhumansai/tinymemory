//! Shared transport and exact-record behavior for remote engine dialects.

use std::collections::BTreeMap;

use anyhow::{bail, Context};
use async_trait::async_trait;
use reqwest::header::{HeaderValue, AUTHORIZATION};
use reqwest::{Method, RequestBuilder, StatusCode, Url};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tinymemory_api::error::MemoryError;
use tinymemory_api::traits::Memory;
use tinymemory_api::types::{
    MemoryCategory, MemoryEntry, MemoryTaint, NamespaceSummary, RecallOpts,
};

#[derive(Clone)]
/// HTTP transport shared by every remote-engine dialect.
///
/// Authentication material is deliberately omitted from its `Debug` output.
pub(crate) struct HttpClient {
    inner: reqwest::Client,
    endpoint: Url,
    auth: Auth,
}

#[derive(Clone)]
/// Authentication scheme applied to every request for one backend.
enum Auth {
    None,
    Bearer(String),
    ApiKey(String),
    /// `Authorization: Token <key>` — Mem0's hosted platform.
    ///
    /// Distinct from [`Auth::Bearer`] on the wire *and* in behaviour:
    /// api.mem0.ai routes a `Bearer` credential into its JWT verifier and
    /// answers `token_not_valid`, so sending the wrong one of the two reports
    /// a failure in the wrong subsystem.
    Token(String),
}

impl std::fmt::Debug for HttpClient {
    /// Renders endpoint origin and authentication presence without credentials.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpClient")
            .field("endpoint", &self.endpoint.origin().ascii_serialization())
            .field("authenticated", &!matches!(self.auth, Auth::None))
            .finish()
    }
}

/// Largest response body any hosted engine may return.
///
/// The endpoint is operator-supplied (`SupermemoryMemory::api`,
/// `Mem0Memory::new`, `CogneeMemory::self_hosted` all take an arbitrary URL),
/// so a broken or hostile server must not be able to exhaust the host's
/// memory. 64 MiB is far above any real memory payload -- the largest thing
/// these APIs return is a page of records -- and far below a size that
/// threatens a process.
const MAX_RESPONSE_BYTES: u64 = 64 * 1024 * 1024;

/// Read a response body, failing once it exceeds [`MAX_RESPONSE_BYTES`].
///
/// `Response::json()`/`text()` buffer the whole body before any size check, so
/// a server that omits or understates `Content-Length` (a chunked response,
/// say) could OOM the process despite a declared limit. Reading incrementally
/// enforces the cap while the bytes arrive. Same argument, and same shape, as
/// `tinymemory-sources`' `read_body_capped` -- that guard was written for the
/// web-page reader and simply had not been applied on this path.
/// Error bodies get a far smaller cap: [`status_error`] surfaces ~300 chars,
/// so 64 KiB preserves every message any API writes while denying a hostile
/// endpoint the unbounded buffer `Response::text()` would hand it — the exact
/// threat [`MAX_RESPONSE_BYTES`] names, which the error paths had skipped
/// (issue #75). Truncation is silent by design: an error body is diagnostic
/// text, not data.
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

/// Reads at most [`MAX_ERROR_BODY_BYTES`] of a non-success body, never
/// failing: the caller is already about to return the status error, and a
/// body-read fault must not mask it.
async fn read_error_body(response: reqwest::Response) -> String {
    use futures::StreamExt;
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(Ok(chunk)) = stream.next().await {
        let room = MAX_ERROR_BODY_BYTES.saturating_sub(body.len());
        body.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if body.len() >= MAX_ERROR_BODY_BYTES {
            break;
        }
    }
    String::from_utf8_lossy(&body).into_owned()
}

async fn read_capped(response: reqwest::Response, path: &str) -> anyhow::Result<Vec<u8>> {
    use futures::StreamExt;
    if let Some(len) = response.content_length() {
        if len > MAX_RESPONSE_BYTES {
            anyhow::bail!(
                "memory API {path} response exceeds {MAX_RESPONSE_BYTES}-byte limit \
                 (Content-Length={len})"
            );
        }
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("memory API {path} body read failed"))?;
        // Check BEFORE appending: one oversized chunk would otherwise be
        // allocated in full before the limit is noticed, which is the
        // allocation this cap exists to prevent.
        let next_len = body
            .len()
            .checked_add(chunk.len())
            .context("memory API response length overflowed")?;
        if next_len as u64 > MAX_RESPONSE_BYTES {
            anyhow::bail!(
                "memory API {path} response exceeds {MAX_RESPONSE_BYTES}-byte limit \
                 (would reach {next_len} bytes)"
            );
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Wraps a credential in a header value that will not be printed back out.
///
/// `RequestBuilder::bearer_auth` marks its `Authorization` value sensitive on
/// the caller's behalf; `RequestBuilder::header` handed a plain string does
/// not. So the two schemes that have no such helper -- `X-API-Key` and
/// `Authorization: Token` -- would otherwise carry a live API key through
/// every `Debug` rendering of the request and through any middleware that
/// formats headers. The flag is set here instead.
///
/// Parsing up front is the second half of the same fix: a credential holding a
/// newline or another byte no header may carry becomes an error at the call
/// site, naming the credential, rather than a deferred failure inside `send`
/// that reads as a transport fault. The parse error carries no value, so the
/// credential does not reach the message either.
fn credential_header(value: &str) -> anyhow::Result<HeaderValue> {
    let mut header =
        HeaderValue::from_str(value).context("credential is not a valid HTTP header value")?;
    header.set_sensitive(true);
    Ok(header)
}

/// The caller's statement of a request's idempotence — every `json`/`text`
/// call site must choose, which is what makes the read/write retry split
/// CHECKABLE instead of conventional (#68 review, Major 4: the first cut's
/// split lived only in a comment, and wrapping the write helper in the retry
/// path failed nothing).
///
/// `RetryTransient` is only sound when repeating the request cannot double-
/// apply anything: reads, searches, list walks — POST included when the POST
/// is a query. `Once` is for everything whose repetition has a cost.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Attempts {
    /// Retry up to three times on the typed transient classes.
    RetryTransient,
    /// One attempt, whatever the failure.
    //
    // No current caller: the audit behind #68 found every existing
    // `json`/`text` call is a read, which is exactly why the marker exists —
    // the FIRST write-shaped caller must pick this variant instead of
    // silently inheriting retry. Deliberately present before its first use.
    #[allow(dead_code)]
    Once,
}

impl HttpClient {
    /// Builds a client that optionally authenticates with a bearer token.
    pub(crate) fn bearer(endpoint: &str, credential: Option<&str>) -> anyhow::Result<Self> {
        Self::new(
            endpoint,
            credential.map_or(Auth::None, |value| Auth::Bearer(value.into())),
        )
    }

    /// A client authenticating with `Authorization: Token <key>`.
    pub(crate) fn token(endpoint: &str, credential: Option<&str>) -> anyhow::Result<Self> {
        Self::new(
            endpoint,
            credential.map_or(Auth::None, |value| Auth::Token(value.into())),
        )
    }

    /// Builds a client that optionally authenticates with `X-API-Key`.
    pub(crate) fn api_key(endpoint: &str, credential: Option<&str>) -> anyhow::Result<Self> {
        Self::new(
            endpoint,
            credential.map_or(Auth::None, |value| Auth::ApiKey(value.into())),
        )
    }

    /// Validates and normalizes an endpoint before constructing the transport.
    fn new(endpoint: &str, auth: Auth) -> anyhow::Result<Self> {
        let mut endpoint = Url::parse(endpoint).context("memory endpoint is not a valid URL")?;
        if !matches!(endpoint.scheme(), "http" | "https") {
            bail!("memory endpoint must use http or https");
        }
        if !endpoint.path().ends_with('/') {
            let path = format!("{}/", endpoint.path());
            endpoint.set_path(&path);
        }
        Ok(Self {
            inner: Self::build_inner(std::time::Duration::from_secs(60))?,
            endpoint,
            auth,
        })
    }

    /// One place builds the reqwest client, so the two timeouts stay paired:
    /// the per-request deadline, and a connect deadline that keeps a
    /// black-holed endpoint from consuming the whole request budget before
    /// the first byte.
    fn build_inner(timeout: std::time::Duration) -> anyhow::Result<reqwest::Client> {
        Ok(reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(std::time::Duration::from_secs(10).min(timeout))
            .build()?)
    }

    /// Rebuilds this client with a different per-request deadline (issue #18
    /// follow-up U5). The 60s default suits interactive calls; a bulk
    /// migration or a health probe may want its own budget.
    ///
    /// Note the effective worst-case wall time of a retrying READ is ~3x
    /// this value plus 750ms of backoff — the transient-retry policy runs up
    /// to three attempts, each with its own deadline.
    pub(crate) fn with_timeout(mut self, timeout: std::time::Duration) -> anyhow::Result<Self> {
        self.inner = Self::build_inner(timeout)?;
        Ok(self)
    }

    /// Resolves a relative API path and attaches the configured authentication.
    fn request(&self, method: Method, path: &str) -> anyhow::Result<RequestBuilder> {
        let url = self
            .endpoint
            .join(path.trim_start_matches('/'))
            .context("memory API path is invalid")?;
        let request = self.inner.request(method, url);
        Ok(match &self.auth {
            Auth::None => request,
            Auth::Bearer(token) => request.bearer_auth(token),
            Auth::ApiKey(key) => request.header("X-API-Key", credential_header(key)?),
            Auth::Token(key) => {
                request.header(AUTHORIZATION, credential_header(&format!("Token {key}"))?)
            }
        })
    }

    /// Sends a JSON request and decodes a successful JSON response.
    /// The error for a request that never produced a response.
    ///
    /// `reqwest`'s own Display is one clause — "error sending request" — and
    /// the cause that matters (DNS, TLS, timeout, refused) is one or more
    /// `source()` hops down, which a host that logs only the top line never
    /// sees. Real case this was written for: a hosted endpoint that accepted
    /// TCP and then aborted the TLS handshake, reported to the operator as
    /// "request failed" with nothing to act on.
    ///
    /// So the class is named up front and the underlying chain is appended.
    /// Naming the class is a judgement, not a parse: `reqwest` exposes
    /// `is_timeout`/`is_connect` directly, and TLS is recognised from the
    /// chain's text because rustls' error types are not in this crate's
    /// public dependencies.
    fn transport_error(&self, error: reqwest::Error) -> anyhow::Error {
        let host = self.endpoint.host_str().unwrap_or("<endpoint>");
        let chain = {
            let mut parts: Vec<String> = Vec::new();
            let mut source: Option<&(dyn std::error::Error + 'static)> =
                std::error::Error::source(&error);
            while let Some(cause) = source {
                parts.push(cause.to_string());
                source = cause.source();
            }
            parts.join(": ")
        };
        let class = classify_transport(error.is_timeout(), error.is_connect(), &chain);
        let described = class.describe();
        let message = if chain.is_empty() {
            format!("memory API request to {host}: {described}")
        } else {
            format!("memory API request to {host}: {described} ({chain})")
        };
        // §A4: the typed error rides as the anyhow payload, so
        // `tinymemory_api::mandatory::engine_error` can downcast it back out
        // at the contract boundary instead of flattening it into `Other`.
        anyhow::Error::new(match class {
            TransportClass::Timeout => MemoryError::Timeout(message),
            TransportClass::Dns | TransportClass::Tls | TransportClass::Connect => {
                MemoryError::Unreachable(message)
            }
            TransportClass::Other => return anyhow::anyhow!("{message}"),
        })
    }

    /// The error for a non-success status, written for the operator reading a
    /// log: it names the endpoint host (never the credential) and calls out a
    /// rejected credential specifically, because "HTTP 401" three layers deep
    /// in an anyhow chain reads as "the engine is down" and sends the operator
    /// to the wrong runbook.
    fn status_error(&self, path: &str, status: reqwest::StatusCode, body: &str) -> anyhow::Error {
        let host = self.endpoint.host_str().unwrap_or("<endpoint>");
        // Hosted engines explain a rejection in the response body — mem0
        // answers `{"detail": "..."}`, cognee likewise — and discarding it
        // turned "this one field is invalid" into a bare status code that
        // said only that something, somewhere, was wrong. Truncated because
        // an error body is not a payload budget, and only ever an error
        // body: success responses never reach here.
        let detail = body.trim();
        let detail = if detail.is_empty() {
            String::new()
        } else {
            let mut shown: String = detail.chars().take(300).collect();
            if detail.chars().count() > 300 {
                shown.push('…');
            }
            format!(" — {shown}")
        };
        // §A4: every bucket mints a typed [`MemoryError`] carried as the
        // anyhow payload — same prose as before, now matchable downstream.
        anyhow::Error::new(match status.as_u16() {
            401 | 403 => {
                let hint = match &self.auth {
                    Auth::ApiKey(_) | Auth::Token(_) => "check the API key",
                    Auth::Bearer(_) => "check the bearer token",
                    Auth::None => {
                        "the endpoint requires credentials this client was not configured with"
                    }
                };
                MemoryError::Unauthorized(format!(
                    "memory API {path} on {host}: the configured credential was rejected \
                     (HTTP {status}) — {hint}{detail}"
                ))
            }
            404 => MemoryError::NotFound(format!(
                "memory API {path} on {host} returned HTTP 404{detail}"
            )),
            // A validation refusal: the backend understood the request and
            // rejected its CONTENT. Without this arm a real validating
            // backend (all three vendors validate; only the in-tree doubles
            // accept everything) could never produce the `Invalid` the
            // tightened conformance refusal-assertion demands (#68 review,
            // Major 5).
            400 | 422 => MemoryError::Invalid(format!(
                "memory API {path} on {host} returned HTTP {status}{detail}"
            )),
            // The answered-but-cannot-serve class: rate limiting and the
            // gateway trio. Distinct from `Backend` so a retry policy can key
            // on it without parsing prose.
            429 | 502 | 503 | 504 => MemoryError::Unavailable(format!(
                "memory API {path} on {host} returned HTTP {status}{detail}"
            )),
            _ => MemoryError::Backend(format!(
                "memory API {path} on {host} returned HTTP {status}{detail}"
            )),
        })
    }

    /// Whether an error is worth one more attempt on a READ path: the typed
    /// transient classes only (issue #18 follow-up U5). Keyed on the §A4
    /// variants rather than message substrings — the fragility the
    /// composio sync client's needle-matching retry shows the cost of.
    /// `Unauthorized`, `Invalid`, `NotFound`, `Backend` never retry: the
    /// answer will not change.
    fn retryable(error: &anyhow::Error) -> bool {
        matches!(
            error.downcast_ref::<MemoryError>(),
            Some(
                MemoryError::Timeout(_) | MemoryError::Unreachable(_) | MemoryError::Unavailable(_)
            )
        )
    }

    /// Runs a read-path attempt up to three times with 250ms·2ⁿ backoff.
    ///
    /// READ paths only — `json` and `text` below, whose calls are all list,
    /// search and raw-fetch operations across the three adapters. The write
    /// paths (`empty`, `multipart`) are deliberately not routed through
    /// here: a `Timeout` on a write leaves whether the backend applied it
    /// unknown, and Cognee's multipart upsert and Mem0's add are not
    /// idempotent.
    async fn with_read_retry<T, F, Fut>(&self, attempt: F) -> anyhow::Result<T>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = anyhow::Result<T>>,
    {
        const MAX_ATTEMPTS: u32 = 3;
        let mut tried = 0;
        loop {
            tried += 1;
            match attempt().await {
                Ok(value) => return Ok(value),
                Err(error) if tried < MAX_ATTEMPTS && Self::retryable(&error) => {
                    let backoff = std::time::Duration::from_millis(250) * 2_u32.pow(tried - 1);
                    tokio::time::sleep(backoff).await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    pub(crate) async fn json<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&serde_json::Value>,
        attempts: Attempts,
    ) -> anyhow::Result<T> {
        if matches!(attempts, Attempts::Once) {
            return self.json_attempt(method, path, body).await;
        }
        self.with_read_retry(|| self.json_attempt(method.clone(), path, body))
            .await
    }

    /// One send of a JSON request — the body `json` retries (or not, per its
    /// `Attempts` marker).
    async fn json_attempt<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> anyhow::Result<T> {
        let mut request = self.request(method, path)?;
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request
            .send()
            .await
            .map_err(|error| self.transport_error(error))?;
        let status = response.status();
        if !status.is_success() {
            let body = read_error_body(response).await;
            return Err(self.status_error(path, status, &body));
        }
        let body = read_capped(response, path).await?;
        serde_json::from_slice(&body)
            .with_context(|| format!("memory API {path} returned invalid JSON"))
    }

    /// Sends a request and returns a successful response body as text.
    pub(crate) async fn text(
        &self,
        method: Method,
        path: &str,
        attempts: Attempts,
    ) -> anyhow::Result<String> {
        let attempt = || async {
            let response = self
                .request(method.clone(), path)?
                .send()
                .await
                .map_err(|error| self.transport_error(error))?;
            let status = response.status();
            if !status.is_success() {
                let body = read_error_body(response).await;
                return Err(self.status_error(path, status, &body));
            }
            let body = read_capped(response, path).await?;
            String::from_utf8(body).context("memory API response was not valid UTF-8")
        };
        if matches!(attempts, Attempts::Once) {
            return attempt().await;
        }
        self.with_read_retry(attempt).await
    }

    /// Sends a request whose successful response body is not needed.
    pub(crate) async fn empty(
        &self,
        method: Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> anyhow::Result<StatusCode> {
        let mut request = self.request(method, path)?;
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request
            .send()
            .await
            .map_err(|error| self.transport_error(error))?;
        let status = response.status();
        if !status.is_success() {
            let body = read_error_body(response).await;
            return Err(self.status_error(path, status, &body));
        }
        Ok(status)
    }

    /// Starts an authenticated multipart request.
    pub(crate) fn multipart(&self, method: Method, path: &str) -> anyhow::Result<RequestBuilder> {
        self.request(method, path)
    }

    /// Sends a prepared multipart form and types its failures like every
    /// other path: transport faults through [`Self::transport_error`],
    /// non-success statuses through [`Self::status_error`]. The upload leg
    /// previously spoke raw `anyhow!` strings, so a 400 refusal reached
    /// callers as `Other` instead of `Invalid`, a 401 was not `Unauthorized`,
    /// and a 429/503 was never retried-or-classified `Unavailable` — the one
    /// write path outside the §A4 taxonomy (issue #75).
    pub(crate) async fn send_multipart(
        &self,
        method: Method,
        path: &str,
        form: reqwest::multipart::Form,
    ) -> anyhow::Result<()> {
        let response = self
            .multipart(method, path)?
            .multipart(form)
            .send()
            .await
            .map_err(|error| self.transport_error(error))?;
        let status = response.status();
        if !status.is_success() {
            let body = read_error_body(response).await;
            return Err(self.status_error(path, status, &body));
        }
        Ok(())
    }

    /// Probes a GET endpoint and reports WHY it failed, typed (issue #18
    /// follow-up U4). The boolean `healthy` below discards status, body and
    /// transport class; this keeps them, so a health surface can distinguish
    /// "credential rejected" from "unreachable" from "answered 500".
    pub(crate) async fn probe(&self, path: &str) -> anyhow::Result<()> {
        let response = self
            .request(Method::GET, path)?
            .send()
            .await
            .map_err(|error| self.transport_error(error))?;
        let status = response.status();
        if !status.is_success() {
            let body = read_error_body(response).await;
            return Err(self.status_error(path, status, &body));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// Lossless TinyMemory record stored in backend-native metadata or content.
pub(crate) struct StoredEntry {
    #[serde(default)]
    pub(crate) remote_id: String,
    pub(crate) namespace: String,
    pub(crate) key: String,
    pub(crate) content: String,
    pub(crate) category: MemoryCategory,
    #[serde(default)]
    pub(crate) timestamp: String,
    #[serde(default)]
    pub(crate) session_id: Option<String>,
    #[serde(default)]
    pub(crate) score: Option<f64>,
    #[serde(default)]
    pub(crate) taint: MemoryTaint,
}

impl StoredEntry {
    /// Creates an unstored record; the dialect fills in the remote identifier.
    pub(crate) fn new(
        namespace: &str,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        taint: MemoryTaint,
    ) -> Self {
        Self {
            remote_id: String::new(),
            namespace: namespace.to_owned(),
            key: key.to_owned(),
            content: content.to_owned(),
            category,
            timestamp: String::new(),
            session_id: session_id.map(str::to_owned),
            score: None,
            taint,
        }
    }

    /// Converts the transport envelope into the public TinyMemory record type.
    pub(crate) fn into_memory_entry(self) -> MemoryEntry {
        MemoryEntry {
            id: if self.remote_id.is_empty() {
                stable_id(&self.namespace, &self.key)
            } else {
                self.remote_id
            },
            key: self.key,
            content: self.content,
            namespace: Some(self.namespace),
            category: self.category,
            timestamp: self.timestamp,
            session_id: self.session_id,
            score: self.score,
            taint: self.taint,
        }
    }
}

/// Derives a deterministic fallback identifier from a logical record key.
pub(crate) fn stable_id(namespace: &str, key: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(namespace.as_bytes());
    digest.update([0]);
    digest.update(key.as_bytes());
    format!("tm_{}", encode(&digest.finalize()[..20]))
}

/// Encodes arbitrary bytes as lowercase hexadecimal text safe for remote names.
pub(crate) fn encode(value: impl AsRef<[u8]>) -> String {
    let value = value.as_ref();
    value.iter().fold(
        String::with_capacity(value.len() * 2),
        |mut output, byte| {
            use std::fmt::Write as _;
            let _ = write!(output, "{byte:02x}");
            output
        },
    )
}

/// Parses a stored category, preserving unknown or absent values as remote data.
pub(crate) fn category(raw: Option<&str>) -> MemoryCategory {
    raw.and_then(|value| value.parse().ok())
        .unwrap_or_else(|| MemoryCategory::Custom("remote".into()))
}

#[async_trait]
/// Backend-specific operations needed by the shared TinyMemory implementation.
pub(crate) trait Dialect: Send + Sync + std::fmt::Debug {
    /// Returns the stable driver identifier.
    fn name(&self) -> &'static str;
    /// Creates or replaces one exact logical record.
    async fn upsert(&self, entry: StoredEntry) -> anyhow::Result<()>;
    /// Enumerates every record owned by this adapter.
    async fn entries(&self) -> anyhow::Result<Vec<StoredEntry>>;
    /// One namespace's records (issue #69, the keyed-CRUD seam).
    ///
    /// The default enumerates and filters — exactly what every caller did
    /// before the seam existed — so a dialect overrides only when its
    /// backend can scope the fetch server-side (Supermemory's container
    /// tags; Mem0's entity filters). Callers that genuinely need EVERY
    /// record (`count`, `namespace_summaries`, export) stay on `entries`;
    /// that full walk is the documented floor, not an accident.
    async fn namespace_entries(&self, namespace: &str) -> anyhow::Result<Vec<StoredEntry>> {
        Ok(self
            .entries()
            .await?
            .into_iter()
            .filter(|entry| entry.namespace == namespace)
            .collect())
    }
    /// One record by its exact logical key (issue #69).
    ///
    /// Default: the namespace's records, filtered — which itself defaults to
    /// the full walk. A dialect with a true server-side keyed lookup (Mem0
    /// cloud metadata filters) overrides this directly.
    async fn entry(&self, namespace: &str, key: &str) -> anyhow::Result<Option<StoredEntry>> {
        Ok(self
            .namespace_entries(namespace)
            .await?
            .into_iter()
            .find(|entry| entry.key == key))
    }
    /// Runs the backend's native recall operation.
    async fn search(
        &self,
        query: &str,
        limit: usize,
        opts: RecallOpts<'_>,
    ) -> anyhow::Result<Vec<StoredEntry>>;
    /// Deletes one exact logical record and reports whether it existed.
    async fn delete(&self, namespace: &str, key: &str) -> anyhow::Result<bool>;
    /// Probes whether the backend is available, reporting WHY not, typed
    /// (`Ok(())` = serving; the error carries a §A4 `MemoryError` payload).
    async fn health(&self) -> anyhow::Result<()>;
    /// Whether this backend's recall responses carry a similarity score.
    ///
    /// Decides the `min_score` tier in [`RemoteMemory::recall`]: a scoring
    /// backend gets the strict filter (an unscored hit cannot clear a
    /// threshold), while a backend that STRUCTURALLY cannot score — Cognee's
    /// context-only recall has no score field at all — keeps its hits, because
    /// dropping 100% of every result is not honesty, it is a different lie
    /// (the #68 review's Major 2). The inertness on scoreless backends is
    /// deliberate and documented rather than silent: this flag is where.
    fn scores_recall(&self) -> bool {
        true
    }
}

#[derive(Debug)]
/// TinyMemory's exact-record contract composed over a native backend dialect.
pub(crate) struct RemoteMemory<D> {
    dialect: D,
}

impl<D> RemoteMemory<D> {
    /// Mutable access for the adapters' builder-style configuration
    /// (`with_request_timeout` on each public type).
    pub(crate) fn dialect_mut(&mut self) -> &mut D {
        &mut self.dialect
    }

    /// Wraps a backend dialect with shared filtering and conversion behavior.
    pub(crate) fn new(dialect: D) -> Self {
        Self { dialect }
    }
}

#[async_trait]
impl<D: Dialect + 'static> Memory for RemoteMemory<D> {
    /// Returns the wrapped dialect's stable driver identifier.
    fn name(&self) -> &str {
        self.dialect.name()
    }

    /// Stores a record with the default internal provenance.
    async fn store(
        &self,
        namespace: &str,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        self.store_with_taint(
            namespace,
            key,
            content,
            category,
            session_id,
            MemoryTaint::Internal,
        )
        .await
    }

    /// Validates identity fields and delegates a provenance-preserving upsert.
    async fn store_with_taint(
        &self,
        namespace: &str,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        taint: MemoryTaint,
    ) -> anyhow::Result<()> {
        if namespace.is_empty() || key.is_empty() {
            bail!("namespace and key must not be empty");
        }
        self.dialect
            .upsert(StoredEntry::new(
                namespace, key, content, category, session_id, taint,
            ))
            .await
    }

    /// Runs native search, enforces remaining filters, and caps the result set.
    async fn recall(
        &self,
        query: &str,
        limit: usize,
        opts: RecallOpts<'_>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        if limit == 0 || query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let min_score = opts.min_score;
        let mut entries = self.dialect.search(query, limit, opts.clone()).await?;
        entries.retain(|entry| matches_filters(entry, &opts));
        if let Some(minimum) = min_score {
            if self.dialect.scores_recall() {
                entries.retain(|entry| clears_min_score(entry.score, minimum));
            }
            // else: the backend cannot score (see `Dialect::scores_recall`) —
            // the threshold is documented-inert rather than silently
            // everything-dropping.
        }
        entries.truncate(limit);
        Ok(entries
            .into_iter()
            .map(StoredEntry::into_memory_entry)
            .collect())
    }

    /// Locates one record by its exact logical namespace and key — through
    /// the dialect's keyed seam, so a backend that can resolve a key
    /// server-side does (issue #69); the default is the old full walk.
    async fn get(&self, namespace: &str, key: &str) -> anyhow::Result<Option<MemoryEntry>> {
        Ok(self
            .dialect
            .entry(namespace, key)
            .await?
            .map(StoredEntry::into_memory_entry))
    }

    /// Enumerates records and applies exact category and session filters.
    ///
    /// A namespace-scoped list goes through the dialect's namespace seam
    /// (issue #69): on a scoping backend that is one tag/entity fetch
    /// instead of the whole account. The all-namespaces list has no scope to
    /// exploit and stays on the full walk.
    async fn list(
        &self,
        namespace: Option<&str>,
        category: Option<&MemoryCategory>,
        session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        let mut entries = match namespace {
            Some(value) => self.dialect.namespace_entries(value).await?,
            None => self.dialect.entries().await?,
        };
        entries.retain(|entry| {
            namespace.is_none_or(|value| entry.namespace == value)
                && category.is_none_or(|value| &entry.category == value)
                && session_id.is_none_or(|value| entry.session_id.as_deref() == Some(value))
        });
        Ok(entries
            .into_iter()
            .map(StoredEntry::into_memory_entry)
            .collect())
    }

    /// Delegates exact logical deletion to the backend dialect.
    async fn forget(&self, namespace: &str, key: &str) -> anyhow::Result<bool> {
        self.dialect.delete(namespace, key).await
    }

    /// Aggregates record counts and latest timestamps by namespace.
    async fn namespace_summaries(&self) -> anyhow::Result<Vec<NamespaceSummary>> {
        let mut summaries: BTreeMap<String, NamespaceSummary> = BTreeMap::new();
        for entry in self.dialect.entries().await? {
            let summary =
                summaries
                    .entry(entry.namespace.clone())
                    .or_insert_with(|| NamespaceSummary {
                        namespace: entry.namespace,
                        count: 0,
                        last_updated: None,
                    });
            summary.count += 1;
            if !entry.timestamp.is_empty()
                && summary
                    .last_updated
                    .as_ref()
                    .is_none_or(|current| current < &entry.timestamp)
            {
                summary.last_updated = Some(entry.timestamp);
            }
        }
        Ok(summaries.into_values().collect())
    }

    /// Counts all records owned by the adapter.
    async fn count(&self) -> anyhow::Result<usize> {
        Ok(self.dialect.entries().await?.len())
    }

    /// Delegates availability checking to the backend dialect.
    async fn health_check(&self) -> bool {
        self.dialect.health().await.is_ok()
    }

    /// The typed answer behind `health_check` (issue #18 §U4): the probe's
    /// §A4 class decides the health state. `Unavailable` — the backend
    /// answered that it cannot serve right now (429 / gateway trio) — maps to
    /// `Degraded`: alive, impaired, worth saying so instead of "down".
    /// Everything else that fails maps to `Down` with the probe's own reason
    /// (which names host and class, never a credential).
    async fn health_probe(&self) -> Option<tinymemory_api::health::MemoryHealth> {
        use tinymemory_api::health::MemoryHealth;
        Some(match self.dialect.health().await {
            Ok(()) => MemoryHealth::Ready,
            Err(error) => {
                let reason = health_reason(&error);
                match error.downcast_ref::<MemoryError>() {
                    Some(MemoryError::Unavailable(_)) => MemoryHealth::degraded(reason),
                    _ => MemoryHealth::down(reason),
                }
            }
        })
    }
}

/// A health `reason` from a probe failure, REDACTED for the status surface.
///
/// `MemoryHealth`'s contract: the reason is logged and rendered in operator
/// status and must never carry credentials or content. `status_error`
/// interpolates up to 300 chars of the backend's OWN error body — which a
/// vendor is free to fill with the rejected key (#68 review). Every message
/// this crate builds puts that detail after a spaced em-dash, so the reason
/// keeps each chain segment's head and drops the tails. The full untruncated
/// error still flows to the CALLER of the failing operation; only the
/// standing status string is trimmed. Walking `chain()` (not just the top)
/// keeps Mem0's both-probes-failed context instead of losing it to a
/// consuming downcast.
fn health_reason(error: &anyhow::Error) -> String {
    error
        .chain()
        .map(|cause| {
            let text = cause.to_string();
            match text.split_once(" — ") {
                Some((head, _)) => format!("{head} — detail withheld from status; see logs"),
                None => text,
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Honesty over leniency (issue #18 §U6): an entry with NO score cannot be
/// shown to clear a threshold the caller asked for, so it drops. The old
/// `is_none_or` let unscored hits pass, which made `min_score` silently inert
/// against any backend that omits score numbers — a caller asking for ≥0.8
/// got unranked everything and never learned the filter did nothing.
fn clears_min_score(score: Option<f64>, minimum: f64) -> bool {
    score.is_some_and(|value| value >= minimum)
}

/// Applies TinyMemory recall filters that a backend may not support natively.
fn matches_filters(entry: &StoredEntry, opts: &RecallOpts<'_>) -> bool {
    opts.namespace.is_none_or(|value| entry.namespace == value)
        && opts
            .category
            .as_ref()
            .is_none_or(|value| &entry.category == value)
        && opts
            .session_id
            .is_none_or(|value| entry.session_id.as_deref() == Some(value))
}

/// The class of a transport failure — a real enum rather than a prose string
/// (issue #18 §A4), so [`HttpClient::transport_error`] can mint a typed
/// [`MemoryError`] and a retry policy can key on the class instead of
/// substring-matching a rendered message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransportClass {
    Timeout,
    Dns,
    Tls,
    Connect,
    /// The request did not complete for a reason the chain does not name —
    /// deliberately NOT mapped onto a typed variant, because claiming
    /// "unreachable" for (say) a mid-body disconnect would be a guess.
    Other,
}

impl TransportClass {
    /// The operator-facing prose for this class — exactly the strings the
    /// pre-§A4 version returned, so log lines do not change spelling.
    fn describe(self) -> &'static str {
        match self {
            Self::Timeout => "timed out",
            Self::Dns => "the host could not be resolved — check the URL",
            Self::Tls => {
                "TLS failed — the endpoint answered on the port but could not establish a \
                 secure connection; check that the URL is the engine's real API host"
            }
            Self::Connect => "could not connect — check the URL and that the service is reachable",
            Self::Other => "the request did not complete",
        }
    }
}

/// Name the class of a transport failure from what the error chain says.
///
/// Pure so the ORDER is testable, which is the whole reason it exists as its
/// own function: `is_connect()` is also true for DNS and TLS failures, so a
/// naive `if is_connect()` first collapses every class into "could not
/// connect". That is exactly what the first version of this did, and it took
/// a live run against a real broken endpoint to notice.
fn classify_transport(is_timeout: bool, is_connect: bool, chain: &str) -> TransportClass {
    let lower = chain.to_ascii_lowercase();
    if is_timeout {
        TransportClass::Timeout
    } else if lower.contains("dns")
        || lower.contains("name or service")
        || lower.contains("failed to lookup")
    {
        TransportClass::Dns
    } else if lower.contains("tls")
        || lower.contains("handshake")
        || lower.contains("certificate")
        || lower.contains("fatal alert")
        || lower.contains("invalid peer")
        || lower.contains("unknown issuer")
    {
        TransportClass::Tls
    } else if is_connect {
        TransportClass::Connect
    } else {
        TransportClass::Other
    }
}

#[cfg(test)]
#[path = "common_credential_header_tests.rs"]
mod credential_header_tests;

#[cfg(test)]
#[path = "common_transport_tests.rs"]
mod transport_tests;
