//! Self-hosted Cognee REST adapter.

use anyhow::{anyhow, Context};
use async_trait::async_trait;
use reqwest::{multipart, Method};
use serde_json::{json, Value};
use tinymemory_api::recall::RecallOpts;
use tinymemory_api::traits::Memory;
use tinymemory_api::types::MemoryTaint;

use crate::common::{encode, Dialect, HttpClient, RemoteMemory, StoredEntry};

/// Stable driver id used by configuration and status output.
pub use tinymemory::registry::COGNEE_DRIVER_ID;

/// A self-hosted Cognee server exposed through TinyMemory's storage contract.
#[derive(Debug)]
pub struct CogneeMemory {
    inner: RemoteMemory<CogneeDialect>,
}

impl CogneeMemory {
    /// Connect to a Cognee server.
    ///
    /// `access_token` is sent as a bearer token. Local deployments with
    /// backend access control disabled may pass `None`.
    ///
    /// # Errors
    ///
    /// Returns an error when `endpoint` is not an HTTP(S) URL.
    pub fn new(endpoint: &str, access_token: Option<&str>) -> anyhow::Result<Self> {
        Ok(Self {
            inner: RemoteMemory::new(CogneeDialect {
                client: HttpClient::bearer(endpoint, access_token)?,
            }),
        })
    }
}

#[async_trait]
impl Memory for CogneeMemory {
    /// Returns the Cognee driver identifier.
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
    /// Runs native Cognee recall and applies TinyMemory filters.
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
    /// Checks whether the configured Cognee service is reachable.
    async fn health_check(&self) -> bool {
        self.inner.health_check().await
    }
}

#[derive(Debug)]
/// Cognee-specific REST operations and wire-format conversion.
struct CogneeDialect {
    client: HttpClient,
}

#[derive(Debug, Clone)]
/// Identity of a Cognee dataset used for one TinyMemory namespace.
struct Dataset {
    id: String,
    name: String,
}

impl CogneeDialect {
    /// Encodes a TinyMemory namespace as a collision-free Cognee dataset name.
    fn dataset_name(namespace: &str) -> String {
        format!("tinymemory__{}", encode(namespace))
    }
    /// Encodes a TinyMemory key as the uploaded envelope's filename.
    fn filename(key: &str) -> String {
        format!("{}.tinymemory.json", encode(key))
    }

    /// Discovers only datasets owned by the TinyMemory adapter.
    async fn datasets(&self) -> anyhow::Result<Vec<Dataset>> {
        let response: Value = self
            .client
            .json(Method::GET, "api/v1/datasets", None)
            .await?;
        Ok(response
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|value| {
                Some(Dataset {
                    id: value.get("id")?.as_str()?.to_owned(),
                    name: value.get("name")?.as_str()?.to_owned(),
                })
            })
            .filter(|dataset| dataset.name.starts_with("tinymemory__"))
            .collect())
    }

    /// Downloads and decodes every TinyMemory envelope in one dataset.
    async fn dataset_entries(&self, dataset: &Dataset) -> anyhow::Result<Vec<StoredEntry>> {
        let response: Value = self
            .client
            .json(
                Method::GET,
                &format!("api/v1/datasets/{}/data", dataset.id),
                None,
            )
            .await?;
        let mut entries = Vec::new();
        for data in response.as_array().into_iter().flatten() {
            let Some(id) = data.get("id").and_then(Value::as_str) else {
                continue;
            };
            let Some(name) = data.get("name").and_then(Value::as_str) else {
                continue;
            };
            // Cognee's text loader strips the final `.json` extension from
            // uploaded filenames; API-shaped test doubles may preserve it.
            if !name.ends_with(".tinymemory") && !name.ends_with(".tinymemory.json") {
                continue;
            }
            let raw = self
                .client
                .text(
                    Method::GET,
                    &format!("api/v1/datasets/{}/data/{id}/raw", dataset.id),
                )
                .await?;
            let mut entry: StoredEntry =
                serde_json::from_str(&raw).context("Cognee record envelope is invalid")?;
            entry.remote_id = format!("{}:{id}", dataset.id);
            if entry.timestamp.is_empty() {
                entry.timestamp = data
                    .get("updatedAt")
                    .or_else(|| data.get("updated_at"))
                    .or_else(|| data.get("createdAt"))
                    .or_else(|| data.get("created_at"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
            }
            entries.push(entry);
        }
        Ok(entries)
    }

    /// Resolves the dataset assigned to a namespace.
    async fn find_dataset(&self, namespace: &str) -> anyhow::Result<Option<Dataset>> {
        let name = Self::dataset_name(namespace);
        Ok(self
            .datasets()
            .await?
            .into_iter()
            .find(|dataset| dataset.name == name))
    }

    /// Deletes a stored envelope using its composite remote identifier.
    async fn delete_entry(&self, entry: &StoredEntry) -> anyhow::Result<()> {
        let (dataset_id, data_id) = entry
            .remote_id
            .split_once(':')
            .ok_or_else(|| anyhow!("Cognee record has no dataset id"))?;
        self.client
            .empty(
                Method::DELETE,
                &format!("api/v1/datasets/{dataset_id}/data/{data_id}"),
                None,
            )
            .await?;
        Ok(())
    }
}

#[async_trait]
impl Dialect for CogneeDialect {
    /// Returns the stable Cognee driver identifier.
    fn name(&self) -> &'static str {
        COGNEE_DRIVER_ID
    }

    /// Replaces an existing envelope and uploads the new exact record.
    async fn upsert(&self, entry: StoredEntry) -> anyhow::Result<()> {
        if let Some(existing) = self
            .entries()
            .await?
            .into_iter()
            .find(|item| item.namespace == entry.namespace && item.key == entry.key)
        {
            self.delete_entry(&existing).await?;
        }
        let body = serde_json::to_vec(&entry)?;
        let form = multipart::Form::new()
            .text("datasetName", Self::dataset_name(&entry.namespace))
            .text("run_in_background", "false")
            .part(
                "data",
                multipart::Part::bytes(body)
                    .file_name(Self::filename(&entry.key))
                    .mime_str("application/json")?,
            );
        let response = self
            .client
            .multipart("api/v1/remember")?
            .multipart(form)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(anyhow!(
                "memory API api/v1/remember returned HTTP {}",
                response.status()
            ));
        }
        Ok(())
    }

    /// Enumerates records across all TinyMemory-owned Cognee datasets.
    async fn entries(&self) -> anyhow::Result<Vec<StoredEntry>> {
        let mut entries = Vec::new();
        for dataset in self.datasets().await? {
            entries.extend(self.dataset_entries(&dataset).await?);
        }
        Ok(entries)
    }

    /// Executes Cognee's native chunk recall and decodes returned envelopes.
    async fn search(
        &self,
        query: &str,
        limit: usize,
        opts: RecallOpts<'_>,
    ) -> anyhow::Result<Vec<StoredEntry>> {
        let datasets = opts.namespace.map(Self::dataset_name);
        let response: Value = self
            .client
            .json(
                Method::POST,
                "api/v1/recall",
                Some(&json!({
                    "query": query,
                    "search_type": "CHUNKS",
                    "datasets": datasets.map(|name| vec![name]),
                    "top_k": limit,
                    "only_context": true,
                    "session_id": opts.session_id
                })),
            )
            .await?;
        let mut entries = Vec::new();
        for value in response.as_array().into_iter().flatten() {
            let text = value
                .get("text")
                .or_else(|| value.get("content"))
                .or_else(|| value.get("result_object"))
                .and_then(Value::as_str);
            if let Some(text) = text {
                if let Ok(mut entry) = serde_json::from_str::<StoredEntry>(text) {
                    entry.score = value.get("score").and_then(Value::as_f64);
                    entries.push(entry);
                } else {
                    // CHUNKS recall may coalesce adjacent source documents into
                    // newline-delimited text. Each source remains a complete
                    // TinyMemory envelope, so decode them independently.
                    for line in text.lines() {
                        if let Ok(mut entry) = serde_json::from_str::<StoredEntry>(line) {
                            entry.score = value.get("score").and_then(Value::as_f64);
                            entries.push(entry);
                        }
                    }
                }
            }
        }
        Ok(entries)
    }

    /// Finds and deletes an exact TinyMemory logical record.
    async fn delete(&self, namespace: &str, key: &str) -> anyhow::Result<bool> {
        let Some(dataset) = self.find_dataset(namespace).await? else {
            return Ok(false);
        };
        let Some(entry) = self
            .dataset_entries(&dataset)
            .await?
            .into_iter()
            .find(|item| item.key == key)
        else {
            return Ok(false);
        };
        self.delete_entry(&entry).await?;
        Ok(true)
    }

    /// Checks Cognee's aggregate health endpoint.
    async fn health(&self) -> bool {
        self.client.healthy("health").await
    }
}

#[cfg(test)]
#[path = "cognee_test.rs"]
mod test;
