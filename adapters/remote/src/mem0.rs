//! Self-hosted Mem0 REST adapter.

use anyhow::Context;
use async_trait::async_trait;
use reqwest::Method;
use serde_json::{json, Value};
use tinymemory_api::recall::RecallOpts;
use tinymemory_api::traits::Memory;
use tinymemory_api::types::MemoryTaint;

use crate::common::{category, Dialect, HttpClient, RemoteMemory, StoredEntry};

/// Stable driver id used by configuration and status output.
pub use tinymemory::registry::MEM0_DRIVER_ID;

/// A self-hosted Mem0 server exposed through TinyMemory's storage contract.
#[derive(Debug)]
pub struct Mem0Memory {
    inner: RemoteMemory<Mem0Dialect>,
}

impl Mem0Memory {
    /// Connect to a Mem0 REST server.
    ///
    /// `api_key` is sent as `X-API-Key`. Pass `None` only when the server is
    /// explicitly running with `AUTH_DISABLED=true` for local development.
    ///
    /// # Errors
    ///
    /// Returns an error when `endpoint` is not an HTTP(S) URL.
    pub fn new(endpoint: &str, api_key: Option<&str>) -> anyhow::Result<Self> {
        Ok(Self {
            inner: RemoteMemory::new(Mem0Dialect {
                client: HttpClient::api_key(endpoint, api_key)?,
            }),
        })
    }
}

#[async_trait]
impl Memory for Mem0Memory {
    /// Returns the Mem0 driver identifier.
    fn name(&self) -> &str {
        self.inner.name()
    }
    /// Stores an internally sourced record through the shared contract.
    async fn store(
        &self,
        n: &str,
        k: &str,
        c: &str,
        cat: tinymemory_api::types::MemoryCategory,
        s: Option<&str>,
    ) -> anyhow::Result<()> {
        self.inner.store(n, k, c, cat, s).await
    }
    /// Stores a record while preserving its provenance taint.
    async fn store_with_taint(
        &self,
        n: &str,
        k: &str,
        c: &str,
        cat: tinymemory_api::types::MemoryCategory,
        s: Option<&str>,
        t: MemoryTaint,
    ) -> anyhow::Result<()> {
        self.inner.store_with_taint(n, k, c, cat, s, t).await
    }
    /// Runs native Mem0 semantic search and applies TinyMemory filters.
    async fn recall(
        &self,
        q: &str,
        l: usize,
        o: RecallOpts<'_>,
    ) -> anyhow::Result<Vec<tinymemory_api::types::MemoryEntry>> {
        self.inner.recall(q, l, o).await
    }
    /// Fetches one exact namespace/key record.
    async fn get(
        &self,
        n: &str,
        k: &str,
    ) -> anyhow::Result<Option<tinymemory_api::types::MemoryEntry>> {
        self.inner.get(n, k).await
    }
    /// Lists records matching the supplied TinyMemory filters.
    async fn list(
        &self,
        n: Option<&str>,
        c: Option<&tinymemory_api::types::MemoryCategory>,
        s: Option<&str>,
    ) -> anyhow::Result<Vec<tinymemory_api::types::MemoryEntry>> {
        self.inner.list(n, c, s).await
    }
    /// Deletes one exact namespace/key record.
    async fn forget(&self, n: &str, k: &str) -> anyhow::Result<bool> {
        self.inner.forget(n, k).await
    }
    /// Summarizes every namespace visible through this adapter.
    async fn namespace_summaries(
        &self,
    ) -> anyhow::Result<Vec<tinymemory_api::types::NamespaceSummary>> {
        self.inner.namespace_summaries().await
    }
    /// Counts every record visible through this adapter.
    async fn count(&self) -> anyhow::Result<usize> {
        self.inner.count().await
    }
    /// Checks whether the configured Mem0 service is reachable.
    async fn health_check(&self) -> bool {
        self.inner.health_check().await
    }
}

#[derive(Debug)]
/// Mem0-specific REST operations and wire-format conversion.
struct Mem0Dialect {
    client: HttpClient,
}

impl Mem0Dialect {
    /// Fetches Mem0's administrative memory listing.
    async fn values(&self) -> anyhow::Result<Vec<Value>> {
        let response: Value = self
            .client
            .json(Method::GET, "memories?top_k=1000", None)
            .await?;
        Ok(response
            .get("results")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    /// Decodes a Mem0 result containing TinyMemory-owned metadata.
    fn decode(value: &Value) -> Option<StoredEntry> {
        let metadata = value.get("metadata")?.as_object()?;
        let namespace = metadata.get("tinymemory_namespace")?.as_str()?.to_owned();
        let key = metadata.get("tinymemory_key")?.as_str()?.to_owned();
        Some(StoredEntry {
            remote_id: value.get("id")?.as_str()?.to_owned(),
            namespace,
            key,
            content: value
                .get("memory")
                .or_else(|| value.get("data"))?
                .as_str()?
                .to_owned(),
            category: category(metadata.get("tinymemory_category").and_then(Value::as_str)),
            timestamp: value
                .get("updated_at")
                .or_else(|| value.get("created_at"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            session_id: metadata
                .get("tinymemory_session_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            score: value.get("score").and_then(Value::as_f64),
            taint: metadata
                .get("tinymemory_taint")
                .and_then(Value::as_str)
                .map(MemoryTaint::from_db_str)
                .unwrap_or_default(),
        })
    }

    /// Encodes TinyMemory identity, classification, session, and provenance.
    fn metadata(entry: &StoredEntry) -> Value {
        let mut value = json!({
            "tinymemory_namespace": entry.namespace,
            "tinymemory_key": entry.key,
            "tinymemory_category": entry.category.to_string(),
            "tinymemory_taint": entry.taint.as_db_str(),
        });
        if let (Some(object), Some(session_id)) = (value.as_object_mut(), &entry.session_id) {
            object.insert("tinymemory_session_id".into(), json!(session_id));
        }
        value
    }
}

#[async_trait]
impl Dialect for Mem0Dialect {
    /// Returns the stable Mem0 driver identifier.
    fn name(&self) -> &'static str {
        MEM0_DRIVER_ID
    }

    /// Replaces an existing exact record or creates it with inference disabled.
    async fn upsert(&self, entry: StoredEntry) -> anyhow::Result<()> {
        let existing = self
            .entries()
            .await?
            .into_iter()
            .find(|item| item.namespace == entry.namespace && item.key == entry.key);
        let metadata = Self::metadata(&entry);
        if let Some(existing) = existing {
            self.client
                .empty(
                    Method::PUT,
                    &format!("memories/{}", existing.remote_id),
                    Some(&json!({"text": entry.content, "metadata": metadata})),
                )
                .await?;
        } else {
            self.client
                .empty(
                    Method::POST,
                    "memories",
                    Some(&json!({
                        "messages": [{"role": "user", "content": entry.content}],
                        "user_id": entry.namespace,
                        "run_id": entry.session_id,
                        "metadata": metadata,
                        "infer": false
                    })),
                )
                .await?;
        }
        Ok(())
    }

    /// Enumerates and decodes TinyMemory-owned Mem0 records.
    async fn entries(&self) -> anyhow::Result<Vec<StoredEntry>> {
        Ok(self
            .values()
            .await?
            .iter()
            .filter_map(Self::decode)
            .collect())
    }

    /// Executes Mem0's native vector search.
    async fn search(
        &self,
        query: &str,
        limit: usize,
        opts: RecallOpts<'_>,
    ) -> anyhow::Result<Vec<StoredEntry>> {
        let mut filters = serde_json::Map::new();
        if let Some(namespace) = opts.namespace {
            filters.insert("user_id".into(), json!(namespace));
        }
        let response: Value = self
            .client
            .json(
                Method::POST,
                "search",
                Some(&json!({
                    "query": query, "filters": filters, "top_k": limit, "threshold": opts.min_score
                })),
            )
            .await?;
        let values = response
            .get("results")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(values.iter().filter_map(Self::decode).collect())
    }

    /// Finds and deletes an exact TinyMemory logical record.
    async fn delete(&self, namespace: &str, key: &str) -> anyhow::Result<bool> {
        let Some(entry) = self
            .entries()
            .await?
            .into_iter()
            .find(|item| item.namespace == namespace && item.key == key)
        else {
            return Ok(false);
        };
        self.client
            .empty(
                Method::DELETE,
                &format!("memories/{}", entry.remote_id),
                None,
            )
            .await
            .context("failed to delete Mem0 memory")?;
        Ok(true)
    }

    /// Accepts either Mem0's health endpoint or its redirected root page.
    async fn health(&self) -> bool {
        self.client.healthy("api/health").await || self.client.healthy("").await
    }
}

#[cfg(test)]
#[path = "mem0_test.rs"]
mod test;
