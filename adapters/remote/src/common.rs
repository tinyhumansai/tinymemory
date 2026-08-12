//! Shared transport and exact-record behavior for remote engine dialects.

use std::collections::BTreeMap;

use anyhow::{bail, Context};
use async_trait::async_trait;
use reqwest::{Method, RequestBuilder, StatusCode, Url};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
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

impl HttpClient {
    /// Builds a client that optionally authenticates with a bearer token.
    pub(crate) fn bearer(endpoint: &str, credential: Option<&str>) -> anyhow::Result<Self> {
        Self::new(
            endpoint,
            credential.map_or(Auth::None, |value| Auth::Bearer(value.into())),
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
            inner: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()?,
            endpoint,
            auth,
        })
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
            Auth::ApiKey(key) => request.header("X-API-Key", key),
        })
    }

    /// Sends a JSON request and decodes a successful JSON response.
    pub(crate) async fn json<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> anyhow::Result<T> {
        let mut request = self.request(method, path)?;
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await.context("memory API request failed")?;
        let status = response.status();
        if !status.is_success() {
            bail!("memory API {path} returned HTTP {status}");
        }
        response
            .json()
            .await
            .with_context(|| format!("memory API {path} returned invalid JSON"))
    }

    /// Sends a request and returns a successful response body as text.
    pub(crate) async fn text(&self, method: Method, path: &str) -> anyhow::Result<String> {
        let response = self.request(method, path)?.send().await?;
        let status = response.status();
        if !status.is_success() {
            bail!("memory API {path} returned HTTP {status}");
        }
        response
            .text()
            .await
            .context("memory API response was unreadable")
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
        let response = request.send().await?;
        let status = response.status();
        if !status.is_success() {
            bail!("memory API {path} returned HTTP {status}");
        }
        Ok(status)
    }

    /// Starts an authenticated multipart POST request.
    pub(crate) fn multipart(&self, path: &str) -> anyhow::Result<RequestBuilder> {
        self.request(Method::POST, path)
    }

    /// Reports whether a GET endpoint responds successfully.
    pub(crate) async fn healthy(&self, path: &str) -> bool {
        let Ok(request) = self.request(Method::GET, path) else {
            return false;
        };
        request
            .send()
            .await
            .is_ok_and(|response| response.status().is_success())
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
    /// Runs the backend's native recall operation.
    async fn search(
        &self,
        query: &str,
        limit: usize,
        opts: RecallOpts<'_>,
    ) -> anyhow::Result<Vec<StoredEntry>>;
    /// Deletes one exact logical record and reports whether it existed.
    async fn delete(&self, namespace: &str, key: &str) -> anyhow::Result<bool>;
    /// Checks whether the backend is available for requests.
    async fn health(&self) -> bool;
}

#[derive(Debug)]
/// TinyMemory's exact-record contract composed over a native backend dialect.
pub(crate) struct RemoteMemory<D> {
    dialect: D,
}

impl<D> RemoteMemory<D> {
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
            entries.retain(|entry| entry.score.is_none_or(|score| score >= minimum));
        }
        entries.truncate(limit);
        Ok(entries
            .into_iter()
            .map(StoredEntry::into_memory_entry)
            .collect())
    }

    /// Locates one record by its exact logical namespace and key.
    async fn get(&self, namespace: &str, key: &str) -> anyhow::Result<Option<MemoryEntry>> {
        Ok(self
            .dialect
            .entries()
            .await?
            .into_iter()
            .find(|entry| entry.namespace == namespace && entry.key == key)
            .map(StoredEntry::into_memory_entry))
    }

    /// Enumerates records and applies exact category and session filters.
    async fn list(
        &self,
        namespace: Option<&str>,
        category: Option<&MemoryCategory>,
        session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        let mut entries = self.dialect.entries().await?;
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
        self.dialect.health().await
    }
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
