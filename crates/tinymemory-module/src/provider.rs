//! Complete TinyMemory provider backed by the module-owned engine.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use tinymemory::mandatory::MemoryTraitProvider;
use tinymemory_api::capabilities::Capabilities;
use tinymemory_api::chunks::Chunk;
use tinymemory_api::error::MemoryError;
use tinymemory_api::goals::GoalsDoc;
use tinymemory_api::health::MemoryHealth;
use tinymemory_api::host::{
    CloudProviderCreds, ComposioMode, LocalAiConfig, MemoryConfig, MemoryHostConfig,
    MemoryTreeConfig, SchedulerGateConfig,
};
use tinymemory_api::provider::types::{
    ChangeKind, DiffReport, EntityHit, EntityRef, ExportPage, ExportRecord, ImportOutcome,
    IngestItem, IngestOutcome, MaintenanceReport, SnapshotRef, SourceChange, SourceItem,
    SourceScope,
};
use tinymemory_api::provider::{
    MemoryCore, MemoryDiff, MemoryDocuments, MemoryEntities, MemoryGoals, MemoryGraph,
    MemoryIngest, MemoryMaintenance, MemoryPortability, MemoryProvider, MemoryRecall,
    MemorySourceSink, MemoryToolMemory, MemoryTree,
};
use tinymemory_api::recall::OwnedRecallOpts;
use tinymemory_api::tool_memory::ToolMemoryRule;
use tinymemory_api::tree::{IngestRequest, QueryResult, TreeStatus};
use tinymemory_api::types::{
    GraphRelationRecord, MemoryCategory, MemoryEntry, MemoryKvRecord, MemoryTaint,
    NamespaceDocumentInput, NamespaceRetrievalContext, NamespaceSummary, StoredMemoryDocument,
};
use tinymemory_core::store::{MemoryClient, MemoryClientRef};
use tinymemory_tinycortex::TinycortexMemory;

use crate::ModuleConfig;

/// The concrete, credential-free host configuration available inside a module.
#[derive(Debug, Clone)]
struct ModuleRuntimeConfig {
    workspace_dir: PathBuf,
    config_path: PathBuf,
    memory: MemoryConfig,
    memory_tree: MemoryTreeConfig,
    scheduler_gate: SchedulerGateConfig,
    local_ai: LocalAiConfig,
    embeddings_provider: Option<String>,
    memory_provider: Option<String>,
    default_model: Option<String>,
    default_temperature: f64,
    output_language: Option<String>,
    memory_sources: serde_json::Value,
}

impl From<&ModuleConfig> for ModuleRuntimeConfig {
    fn from(config: &ModuleConfig) -> Self {
        Self {
            workspace_dir: config.workspace_dir.clone(),
            config_path: config.workspace_dir.join("config.toml"),
            memory: config.memory.clone(),
            memory_tree: config.memory_tree.clone(),
            scheduler_gate: config.scheduler_gate.clone(),
            local_ai: config.local_ai.clone(),
            embeddings_provider: config.embeddings_provider.clone(),
            memory_provider: config.memory_provider.clone(),
            default_model: config.default_model.clone(),
            default_temperature: config.default_temperature,
            output_language: config.output_language.clone(),
            memory_sources: config.memory_sources.clone(),
        }
    }
}

#[async_trait]
impl MemoryHostConfig for ModuleRuntimeConfig {
    fn workspace_dir(&self) -> &PathBuf {
        &self.workspace_dir
    }
    fn config_path(&self) -> &PathBuf {
        &self.config_path
    }
    fn memory_tree_content_root(&self) -> PathBuf {
        self.memory_tree
            .content_dir
            .clone()
            .unwrap_or_else(|| self.workspace_dir.join("memory_tree/content"))
    }
    fn memory(&self) -> &MemoryConfig {
        &self.memory
    }
    fn memory_tree(&self) -> &MemoryTreeConfig {
        &self.memory_tree
    }
    fn scheduler_gate(&self) -> &SchedulerGateConfig {
        &self.scheduler_gate
    }
    fn local_ai(&self) -> &LocalAiConfig {
        &self.local_ai
    }
    fn cloud_providers(&self) -> &Vec<CloudProviderCreds> {
        static NONE: Vec<CloudProviderCreds> = Vec::new();
        &NONE
    }
    fn embeddings_provider(&self) -> Option<&str> {
        self.embeddings_provider.as_deref()
    }
    fn memory_provider(&self) -> Option<&str> {
        self.memory_provider.as_deref()
    }
    fn workload_local_model(&self, workload: &str) -> Option<String> {
        let route = match workload {
            "memory" => self.memory_provider.as_deref(),
            "embeddings" => self.embeddings_provider.as_deref(),
            _ => None,
        }?;
        route
            .strip_prefix("ollama:")
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn to_arc(&self) -> Arc<dyn MemoryHostConfig> {
        Arc::new(self.clone())
    }
    fn api_url(&self) -> Option<&str> {
        None
    }
    fn effective_backend_api_url(&self) -> String {
        String::new()
    }
    fn session_token(&self) -> Result<Option<String>, String> {
        Ok(None)
    }
    fn default_model(&self) -> Option<&str> {
        self.default_model.as_deref()
    }
    fn default_temperature(&self) -> f64 {
        self.default_temperature
    }
    fn output_language(&self) -> Option<&str> {
        self.output_language.as_deref()
    }
    fn memory_sync_interval_secs(&self) -> Option<u64> {
        Some(0)
    }
    fn onboarding_completed(&self) -> bool {
        true
    }
    fn secrets_encrypt(&self) -> bool {
        false
    }
    fn composio(&self) -> ComposioMode {
        ComposioMode::default()
    }
    fn memory_sources_json(&self) -> anyhow::Result<serde_json::Value> {
        Ok(self.memory_sources.clone())
    }
    fn set_memory_sources_json(&mut self, value: serde_json::Value) -> anyhow::Result<()> {
        self.memory_sources = value;
        Ok(())
    }
    fn composio_source_caps_migration_version(&self) -> u32 {
        0
    }
    fn set_composio_source_caps_migration_version(&mut self, _version: u32) {}
    fn apply_env_overrides(&mut self) {}
    async fn save(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// The module-owned implementation of every TinyMemory capability family.
pub(crate) struct ModuleMemoryProvider {
    driver_id: String,
    mandatory: MemoryTraitProvider,
    client: MemoryClientRef,
    config: ModuleRuntimeConfig,
}

impl ModuleMemoryProvider {
    pub(crate) fn new(config: &ModuleConfig, client: Arc<MemoryClient>) -> Self {
        let memory = client.memory_handle();
        let mandatory = MemoryTraitProvider::new(
            Arc::new(TinycortexMemory::new(memory)),
            config.driver_id.clone(),
        );
        Self {
            driver_id: config.driver_id.clone(),
            mandatory,
            client,
            config: ModuleRuntimeConfig::from(config),
        }
    }

    fn other(context: &'static str, error: impl std::fmt::Display) -> MemoryError {
        MemoryError::Other(anyhow::anyhow!("{context}: {error}"))
    }

    fn cross<A: serde::Serialize, B: serde::de::DeserializeOwned>(
        value: &A,
        context: &'static str,
    ) -> Result<B, MemoryError> {
        let value = serde_json::to_value(value).map_err(|error| Self::other(context, error))?;
        serde_json::from_value(value).map_err(|error| Self::other(context, error))
    }
}

fn validate_ingest_item(item: &IngestItem) -> Result<(), MemoryError> {
    if item.taint != MemoryTaint::default() {
        return Err(MemoryError::Invalid(
            "ingest cannot preserve a non-default taint in the chunk tier".to_string(),
        ));
    }
    if item.content.trim().is_empty() {
        return Err(MemoryError::Invalid(
            "ingest content must not be empty".to_string(),
        ));
    }
    if let Some(mime) = item.mime.as_deref() {
        let mime = mime.trim().to_ascii_lowercase();
        let base = mime.split(';').next().unwrap_or("").trim();
        if !(base.starts_with("text/")
            || base.ends_with("+json")
            || base.ends_with("+xml")
            || matches!(
                base,
                "application/json" | "application/xml" | "application/x-ndjson"
            ))
        {
            return Err(MemoryError::Invalid(format!(
                "unsupported MIME '{mime}': ingest accepts decoded text only"
            )));
        }
    }
    Ok(())
}

async fn blocking<T, F>(
    config: ModuleRuntimeConfig,
    context: &'static str,
    run: F,
) -> Result<T, MemoryError>
where
    T: Send + 'static,
    F: FnOnce(&ModuleRuntimeConfig) -> anyhow::Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(move || run(&config))
        .await
        .map_err(|error| ModuleMemoryProvider::other(context, error))?
        .map_err(|error| ModuleMemoryProvider::other(context, error))
}

#[async_trait]
impl MemoryCore for ModuleMemoryProvider {
    async fn store(
        &self,
        namespace: &str,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        taint: MemoryTaint,
    ) -> Result<(), MemoryError> {
        self.mandatory
            .store(namespace, key, content, category, session_id, taint)
            .await
    }
    async fn get(&self, namespace: &str, key: &str) -> Result<Option<MemoryEntry>, MemoryError> {
        self.mandatory.get(namespace, key).await
    }
    async fn forget(&self, namespace: &str, key: &str) -> Result<bool, MemoryError> {
        self.mandatory.forget(namespace, key).await
    }
    async fn list(
        &self,
        namespace: Option<&str>,
        category: Option<&MemoryCategory>,
        session_id: Option<&str>,
    ) -> Result<Vec<MemoryEntry>, MemoryError> {
        self.mandatory.list(namespace, category, session_id).await
    }
    async fn namespaces(&self) -> Result<Vec<NamespaceSummary>, MemoryError> {
        self.mandatory.namespaces().await
    }
}

#[async_trait]
impl MemoryRecall for ModuleMemoryProvider {
    async fn recall(
        &self,
        query: &str,
        limit: usize,
        opts: &OwnedRecallOpts,
        scope: Option<&SourceScope>,
    ) -> Result<Vec<MemoryEntry>, MemoryError> {
        self.mandatory.recall(query, limit, opts, scope).await
    }
}

#[async_trait]
impl MemoryPortability for ModuleMemoryProvider {
    async fn export_page(
        &self,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<ExportPage, MemoryError> {
        self.mandatory.export_page(cursor, limit).await
    }
    async fn import_records(
        &self,
        records: Vec<ExportRecord>,
    ) -> Result<ImportOutcome, MemoryError> {
        self.mandatory.import_records(records).await
    }
}

#[async_trait]
impl MemoryDocuments for ModuleMemoryProvider {
    async fn put_document(&self, input: NamespaceDocumentInput) -> Result<String, MemoryError> {
        let input = Self::cross(&input, "convert document input")?;
        self.client
            .put_doc(input)
            .await
            .map_err(|error| Self::other("put_document", error))
    }
    async fn get_document(
        &self,
        namespace: &str,
        key: &str,
    ) -> Result<Option<StoredMemoryDocument>, MemoryError> {
        let document = self
            .client
            .get_document(namespace, key)
            .await
            .map_err(|error| Self::other("get_document", error))?;
        document
            .map(|document| Self::cross(&document, "convert stored document"))
            .transpose()
    }

    async fn list_documents(
        &self,
        namespace: Option<&str>,
    ) -> Result<serde_json::Value, MemoryError> {
        self.client
            .list_documents(namespace)
            .await
            .map_err(|error| Self::other("list_documents", error))
    }

    async fn list_namespaces(&self) -> Result<Vec<String>, MemoryError> {
        self.client
            .list_namespaces()
            .await
            .map_err(|error| Self::other("list_namespaces", error))
    }

    async fn delete_document(
        &self,
        namespace: &str,
        document_id: &str,
    ) -> Result<serde_json::Value, MemoryError> {
        self.client
            .delete_document(namespace, document_id)
            .await
            .map_err(|error| Self::other("delete_document", error))
    }

    async fn clear_namespace(&self, namespace: &str) -> Result<(), MemoryError> {
        self.client
            .clear_namespace(namespace)
            .await
            .map_err(|error| Self::other("clear_namespace", error))
    }
    async fn query_documents(
        &self,
        namespace: &str,
        query: &str,
        limit: usize,
    ) -> Result<NamespaceRetrievalContext, MemoryError> {
        let limit = u32::try_from(limit).unwrap_or(u32::MAX);
        let context = self
            .client
            .query_namespace_context_data(namespace, query, limit)
            .await
            .map_err(|error| Self::other("query_documents", error))?;
        Self::cross(&context, "convert document query result")
    }
}

#[async_trait]
impl MemoryIngest for ModuleMemoryProvider {
    async fn ingest_document(&self, item: IngestItem) -> Result<IngestOutcome, MemoryError> {
        validate_ingest_item(&item)?;
        let document = tinycortex::memory::ingest::canonicalize::document::DocumentInput {
            provider: item.source.as_str().to_string(),
            title: String::new(),
            body: item.content,
            modified_at: item.timestamp.unwrap_or_else(Utc::now),
            source_ref: item.source_ref.map(|source_ref| source_ref.value),
        };
        let result = tinymemory_core::ingest_pipeline::ingest_document_with_scope(
            &self.config,
            &item.source_id,
            &item.owner,
            item.tags,
            document,
            item.path_scope,
        )
        .await
        .map_err(|error| Self::other("ingest document", error))?;
        Ok(IngestOutcome {
            written: u32::try_from(result.chunks_written).unwrap_or(u32::MAX),
            skipped: if result.already_ingested {
                1
            } else {
                u32::try_from(result.chunks_dropped).unwrap_or(u32::MAX)
            },
            ids: result.chunk_ids,
        })
    }

    async fn ingest_chat(&self, messages: Vec<IngestItem>) -> Result<IngestOutcome, MemoryError> {
        let Some(first) = messages.first() else {
            return Ok(IngestOutcome::default());
        };
        let source_id = first.source_id.clone();
        let owner = first.owner.clone();
        let tags = first.tags.clone();
        let platform = first.source.as_str().to_string();
        for item in &messages {
            validate_ingest_item(item)?;
            if item.source_id != source_id {
                return Err(MemoryError::Invalid(
                    "ingest_chat batches must contain one conversation".to_string(),
                ));
            }
        }
        let batch = tinycortex::memory::ingest::canonicalize::chat::ChatBatch {
            platform,
            channel_label: source_id.clone(),
            messages: messages
                .into_iter()
                .map(
                    |item| tinycortex::memory::ingest::canonicalize::chat::ChatMessage {
                        author: item.owner,
                        timestamp: item.timestamp.unwrap_or_else(Utc::now),
                        text: item.content,
                        source_ref: item.source_ref.map(|source_ref| source_ref.value),
                    },
                )
                .collect(),
        };
        let result = tinymemory_core::ingest_pipeline::ingest_chat(
            &self.config,
            &source_id,
            &owner,
            tags,
            batch,
        )
        .await
        .map_err(|error| Self::other("ingest chat", error))?;
        Ok(IngestOutcome {
            written: u32::try_from(result.chunks_written).unwrap_or(u32::MAX),
            skipped: if result.already_ingested {
                1
            } else {
                u32::try_from(result.chunks_dropped).unwrap_or(u32::MAX)
            },
            ids: result.chunk_ids,
        })
    }
}

#[async_trait]
impl MemoryGraph for ModuleMemoryProvider {
    async fn kv_get(
        &self,
        namespace: Option<&str>,
        key: &str,
    ) -> Result<Option<MemoryKvRecord>, MemoryError> {
        let record = self
            .client
            .kv_records(namespace)
            .await
            .map_err(|error| Self::other("kv_get", error))?
            .into_iter()
            .find(|record| record.key == key);
        record
            .map(|record| Self::cross(&record, "convert key/value record"))
            .transpose()
    }
    async fn kv_put(
        &self,
        namespace: Option<&str>,
        key: &str,
        value: serde_json::Value,
    ) -> Result<(), MemoryError> {
        self.client
            .kv_set(namespace, key, &value)
            .await
            .map_err(|error| Self::other("kv_put", error))
    }

    async fn kv_delete(&self, namespace: Option<&str>, key: &str) -> Result<bool, MemoryError> {
        self.client
            .kv_delete(namespace, key)
            .await
            .map_err(|error| Self::other("kv_delete", error))
    }
    async fn kv_list(
        &self,
        namespace: Option<&str>,
        prefix: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MemoryKvRecord>, MemoryError> {
        let mut records = self
            .client
            .kv_records(namespace)
            .await
            .map_err(|error| Self::other("kv_list", error))?;
        if let Some(prefix) = prefix {
            records.retain(|record| record.key.starts_with(prefix));
        }
        records.truncate(limit);
        Self::cross(&records, "convert key/value records")
    }
    async fn relations(
        &self,
        namespace: Option<&str>,
        subject: Option<&str>,
        predicate: Option<&str>,
        limit: usize,
    ) -> Result<Vec<GraphRelationRecord>, MemoryError> {
        let mut records = self
            .client
            .graph_relations(namespace, subject, predicate)
            .await
            .map_err(|error| Self::other("relations", error))?;
        records.truncate(limit);
        Self::cross(&records, "convert graph relations")
    }
    async fn put_relation(&self, relation: GraphRelationRecord) -> Result<(), MemoryError> {
        self.client
            .graph_upsert(
                relation.namespace.as_deref(),
                &relation.subject,
                &relation.predicate,
                &relation.object,
                &relation.attrs,
            )
            .await
            .map_err(|error| Self::other("put_relation", error))
    }
}

#[async_trait]
impl MemoryGoals for ModuleMemoryProvider {
    async fn goals(&self) -> Result<GoalsDoc, MemoryError> {
        let workspace = self.config.workspace_dir.clone();
        let document =
            tokio::task::spawn_blocking(move || tinycortex::memory::goals::store::load(&workspace))
                .await
                .map_err(|error| Self::other("join goals read", error))?
                .map_err(|error| Self::other("read goals", error))?;
        Self::cross(&document, "convert goals")
    }

    async fn set_goals(&self, goals: GoalsDoc) -> Result<(), MemoryError> {
        let workspace = self.config.workspace_dir.clone();
        let mut goals = Self::cross(&goals, "convert goals")?;
        tokio::task::spawn_blocking(move || {
            tinycortex::memory::goals::store::save(&workspace, &mut goals)
        })
        .await
        .map_err(|error| Self::other("join goals write", error))?
        .map_err(|error| Self::other("write goals", error))
    }
}

#[async_trait]
impl MemoryToolMemory for ModuleMemoryProvider {
    async fn tool_rules(&self, tool_name: &str) -> Result<Vec<ToolMemoryRule>, MemoryError> {
        let rules = tinymemory_core::tool_memory::tool_memory_store(self.client.memory_handle())
            .list_rules(tool_name)
            .await
            .map_err(|error| Self::other("list tool rules", error))?;
        Self::cross(&rules, "convert tool rules")
    }

    async fn put_tool_rule(&self, rule: ToolMemoryRule) -> Result<(), MemoryError> {
        let rule = Self::cross(&rule, "convert tool rule")?;
        tinymemory_core::tool_memory::tool_memory_store(self.client.memory_handle())
            .put_rule(rule)
            .await
            .map(|_| ())
            .map_err(|error| Self::other("put tool rule", error))
    }

    async fn delete_tool_rule(&self, tool_name: &str, rule_id: &str) -> Result<bool, MemoryError> {
        tinymemory_core::tool_memory::tool_memory_store(self.client.memory_handle())
            .delete_rule(tool_name, rule_id)
            .await
            .map_err(|error| Self::other("delete tool rule", error))
    }
}

#[async_trait]
impl MemoryTree for ModuleMemoryProvider {
    async fn append(&self, request: IngestRequest) -> Result<(), MemoryError> {
        tinycortex::memory::tree::runtime::store::validate_namespace(&request.namespace)
            .map_err(MemoryError::Invalid)?;
        if request.content.trim().is_empty() {
            return Err(MemoryError::Invalid(
                "content must not be empty".to_string(),
            ));
        }
        let namespace = request.namespace.trim().to_string();
        let content = request.content;
        let timestamp = request.timestamp.unwrap_or_else(Utc::now);
        let metadata = request.metadata;
        blocking(self.config.clone(), "append tree content", move |config| {
            tinymemory_core::tree::tree_runtime::store::buffer_write(
                config,
                &namespace,
                &content,
                &timestamp,
                metadata.as_ref(),
            )
            .map(|_| ())
        })
        .await
    }

    async fn query_source(
        &self,
        namespace: &str,
        source_id: &str,
        limit: usize,
        scope: Option<&SourceScope>,
    ) -> Result<Vec<Chunk>, MemoryError> {
        tinycortex::memory::tree::runtime::store::validate_namespace(namespace)
            .map_err(MemoryError::Invalid)?;
        let query = tinymemory_core::store::chunks::ListChunksQuery {
            source_id: Some(source_id.to_string()),
            source_scope: scope.map(|scope| scope.allow.iter().cloned().collect::<HashSet<_>>()),
            limit: Some(limit),
            exclude_dropped: true,
            ..Default::default()
        };
        let chunks = blocking(self.config.clone(), "query source", move |config| {
            tinymemory_core::store::chunks::list_chunks(config, &query)
        })
        .await?;
        Self::cross(&chunks, "convert source chunks")
    }

    async fn drill_down(&self, namespace: &str, node_id: &str) -> Result<QueryResult, MemoryError> {
        tinycortex::memory::tree::runtime::store::validate_namespace(namespace)
            .map_err(MemoryError::Invalid)?;
        tinycortex::memory::tree::runtime::store::validate_node_id(node_id)
            .map_err(MemoryError::Invalid)?;
        let namespace = namespace.trim().to_string();
        let node_id = node_id.to_string();
        let lookup_namespace = namespace.clone();
        let lookup_node = node_id.clone();
        let result = blocking(self.config.clone(), "drill down", move |config| {
            let Some(node) = tinymemory_core::tree::tree_runtime::store::read_node(
                config,
                &lookup_namespace,
                &lookup_node,
            )?
            else {
                return Ok(None);
            };
            let children = tinymemory_core::tree::tree_runtime::store::read_children(
                config,
                &lookup_namespace,
                &lookup_node,
            )?;
            Ok(Some((node, children)))
        })
        .await?
        .ok_or_else(|| {
            MemoryError::NotFound(format!("tree node '{node_id}' not found in '{namespace}'"))
        })?;
        Self::cross(&result, "convert tree drill-down")
            .map(|(node, children)| QueryResult { node, children })
    }

    async fn seal(&self, namespace: &str) -> Result<TreeStatus, MemoryError> {
        tinycortex::memory::tree::runtime::store::validate_namespace(namespace)
            .map_err(MemoryError::Invalid)?;
        let namespace = namespace.trim().to_string();
        let read_namespace = namespace.clone();
        let buffered = blocking(self.config.clone(), "read tree buffer", move |config| {
            tinymemory_core::tree::tree_runtime::store::buffer_read(config, &read_namespace)
        })
        .await?;
        if !buffered.is_empty() {
            let (model, _) = tinymemory_core::chat_host::create_chat_model_with_model_id(
                "summarization",
                &self.config,
                self.config.default_temperature,
            )
            .map_err(|error| Self::other("create summarizer", error))?;
            tinymemory_core::tree::tree_runtime::engine::run_summarization(
                &self.config,
                model.as_ref(),
                &namespace,
                Utc::now(),
            )
            .await
            .map_err(|error| Self::other("seal tree", error))?;
        }
        let status = blocking(self.config.clone(), "read tree status", move |config| {
            tinymemory_core::tree::tree_runtime::store::get_tree_status(config, &namespace)
        })
        .await?;
        Self::cross(&status, "convert tree status")
    }

    async fn cascade(&self, namespace: &str) -> Result<TreeStatus, MemoryError> {
        tinycortex::memory::tree::runtime::store::validate_namespace(namespace)
            .map_err(MemoryError::Invalid)?;
        let namespace = namespace.trim().to_string();
        let read_namespace = namespace.clone();
        let status = blocking(self.config.clone(), "read tree status", move |config| {
            tinymemory_core::tree::tree_runtime::store::get_tree_status(config, &read_namespace)
        })
        .await?;
        if status.total_nodes == 0 {
            return Self::cross(&status, "convert tree status");
        }
        let (model, _) = tinymemory_core::chat_host::create_chat_model_with_model_id(
            "summarization",
            &self.config,
            self.config.default_temperature,
        )
        .map_err(|error| Self::other("create summarizer", error))?;
        let status = tinymemory_core::tree::tree_runtime::engine::rebuild_tree(
            &self.config,
            model.as_ref(),
            &namespace,
        )
        .await
        .map_err(|error| Self::other("cascade tree", error))?;
        Self::cross(&status, "convert tree status")
    }
}

#[async_trait]
impl MemoryEntities for ModuleMemoryProvider {
    async fn entities(
        &self,
        namespace: &str,
        query: Option<&str>,
        limit: usize,
    ) -> Result<Vec<EntityHit>, MemoryError> {
        let namespace = namespace.to_string();
        let query_namespace = namespace.clone();
        let query = query.map(str::to_string);
        let rows = blocking(
            self.config.clone(),
            "list namespace entities",
            move |config| {
                tinymemory_core::store::entities::namespace_entities(
                    config,
                    &query_namespace,
                    query.as_deref(),
                    limit,
                )
            },
        )
        .await?
        .into_iter()
        .map(|hit| (hit.id, hit.kind, hit.name, hit.mentions))
        .collect::<Vec<_>>();

        let config = self.config.clone();
        blocking(config, "attach entity hotness", move |config| {
            Ok(rows
                .into_iter()
                .map(|(id, kind, name, mentions)| {
                    let hotness_key = format!("{namespace}:{id}");
                    let hotness = tinymemory_core::store::trees::hotness::get(config, &hotness_key)
                        .ok()
                        .flatten()
                        .map_or(0.0, |counters| {
                            f64::from(
                                tinymemory_core::tree_policy::TreePolicy::topic().topic_hotness(
                                    &id,
                                    &counters.stats(),
                                    Utc::now().timestamp_millis(),
                                ),
                            )
                        });
                    EntityHit {
                        entity: EntityRef { id, kind, name },
                        hotness,
                        mentions,
                    }
                })
                .collect())
        })
        .await
    }

    async fn entity_edges(
        &self,
        namespace: &str,
        entity_id: &str,
        limit: usize,
    ) -> Result<Vec<GraphRelationRecord>, MemoryError> {
        let subject = entity_id.to_string();
        let lookup = subject.clone();
        let namespace = namespace.to_string();
        let query_namespace = namespace.clone();
        let neighbours = blocking(self.config.clone(), "read entity edges", move |config| {
            tinymemory_core::store::entities::namespace_entity_edges(
                config,
                &query_namespace,
                &lookup,
                limit,
            )
        })
        .await?;
        Ok(neighbours
            .into_iter()
            .map(|(object, weight)| GraphRelationRecord {
                namespace: Some(namespace.clone()),
                subject: subject.clone(),
                predicate: "co_occurs_with".to_string(),
                object,
                attrs: serde_json::Value::Null,
                updated_at: 0.0,
                evidence_count: weight,
                order_index: None,
                document_ids: Vec::new(),
                chunk_ids: Vec::new(),
            })
            .collect())
    }

    async fn touch_entities(
        &self,
        namespace: &str,
        entity_ids: &[String],
    ) -> Result<(), MemoryError> {
        let entity_ids = entity_ids.to_vec();
        let namespace = namespace.to_string();
        blocking(self.config.clone(), "touch entities", move |config| {
            let now = Utc::now().timestamp_millis();
            for entity_id in entity_ids {
                let entity_id = format!("{namespace}:{entity_id}");
                let mut counters =
                    tinymemory_core::store::trees::hotness::get_or_fresh(config, &entity_id)?;
                counters.mention_count_30d = counters.mention_count_30d.saturating_add(1);
                counters.last_seen_ms = Some(now);
                counters.last_updated_ms = now;
                tinymemory_core::store::trees::hotness::upsert(config, &counters)?;
            }
            Ok(())
        })
        .await
    }
}

#[async_trait]
impl MemoryDiff for ModuleMemoryProvider {
    async fn capture_snapshot(&self, source_id: &str) -> Result<SnapshotRef, MemoryError> {
        let source = tinymemory_core::sources::registry::decode_memory_sources(&self.config)
            .into_iter()
            .find(|source| source.id == source_id)
            .ok_or_else(|| MemoryError::NotFound(source_id.to_string()))?;
        let snapshot = tinymemory_core::diff::ops::take_snapshot(
            &source,
            &self.config,
            tinymemory_core::diff::SnapshotTrigger::Manual,
        )
        .await
        .map_err(|error| Self::other("capture snapshot", error))?;
        Ok(SnapshotRef {
            id: snapshot.id,
            source_id: snapshot.source_id,
            label: snapshot.label,
            item_count: snapshot.item_count,
            taken_at_ms: snapshot.taken_at_ms,
        })
    }

    async fn snapshots(
        &self,
        source_id: &str,
        limit: usize,
    ) -> Result<Vec<SnapshotRef>, MemoryError> {
        let snapshots = tinymemory_core::diff::ops::list_snapshots(
            &self.config,
            Some(source_id),
            u32::try_from(limit).unwrap_or(u32::MAX),
        )
        .await
        .map_err(|error| Self::other("list snapshots", error))?;
        Ok(snapshots
            .into_iter()
            .map(|snapshot| SnapshotRef {
                id: snapshot.id,
                source_id: snapshot.source_id,
                label: snapshot.label,
                item_count: snapshot.item_count,
                taken_at_ms: snapshot.taken_at_ms,
            })
            .collect())
    }

    async fn diff(
        &self,
        source_id: &str,
        from: Option<&str>,
        to: &str,
    ) -> Result<DiffReport, MemoryError> {
        let result = tinymemory_core::diff::ops::compute_diff(&self.config, from, to, false)
            .await
            .map_err(|error| Self::other("compute diff", error))?;
        if result.source_id != source_id {
            return Err(MemoryError::Invalid(format!(
                "snapshot '{to}' belongs to a different source"
            )));
        }
        let changes = result
            .changes
            .into_iter()
            .map(|change| SourceChange {
                item_id: change.item_id,
                title: change.title,
                kind: match change.kind {
                    tinymemory_core::diff::ChangeKind::Added => ChangeKind::Added,
                    tinymemory_core::diff::ChangeKind::Removed => ChangeKind::Removed,
                    tinymemory_core::diff::ChangeKind::Modified => ChangeKind::Modified,
                },
                old_content_hash: change.old_content_hash,
                new_content_hash: change.new_content_hash,
            })
            .collect();
        Ok(DiffReport {
            source_id: result.source_id,
            from_snapshot_id: result.from_snapshot_id,
            to_snapshot_id: result.to_snapshot_id,
            added: result.summary.added,
            removed: result.summary.removed,
            modified: result.summary.modified,
            unchanged: result.summary.unchanged,
            changes,
        })
    }
}

#[async_trait]
impl MemorySourceSink for ModuleMemoryProvider {
    async fn accept_source_items(
        &self,
        source_id: &str,
        source_kind: &str,
        items: Vec<SourceItem>,
        taint: MemoryTaint,
    ) -> Result<IngestOutcome, MemoryError> {
        let namespace = format!("source:{source_id}");
        let mut outcome = IngestOutcome::default();
        for item in items {
            if item.item_id.trim().is_empty() {
                return Err(MemoryError::Invalid(
                    "source item_id must not be empty".to_string(),
                ));
            }
            let title = if item.title.trim().is_empty() {
                item.item_id.clone()
            } else {
                item.title.clone()
            };
            let input = NamespaceDocumentInput {
                namespace: namespace.clone(),
                key: item.item_id,
                title,
                content: item.content,
                source_type: source_kind.to_string(),
                priority: "medium".to_string(),
                tags: item.tags,
                metadata: serde_json::json!({
                    "sourceId": source_id,
                    "sourceKind": source_kind,
                    "url": item.url,
                    "mime": item.mime,
                    "updatedAtMs": item.updated_at_ms,
                }),
                category: "core".to_string(),
                session_id: None,
                document_id: None,
                taint,
            };
            let input = Self::cross(&input, "convert source document")?;
            match self.client.put_doc(input).await {
                Ok(id) => {
                    outcome.written = outcome.written.saturating_add(1);
                    outcome.ids.push(id);
                }
                Err(_) => {
                    outcome.skipped = outcome.skipped.saturating_add(1);
                }
            }
        }
        Ok(outcome)
    }

    async fn forget_source(&self, source_id: &str) -> Result<u64, MemoryError> {
        let namespace = format!("source:{source_id}");
        let listed = self
            .client
            .list_documents(Some(&namespace))
            .await
            .map_err(|error| Self::other("list source documents", error))?;
        let documents = listed
            .get("documents")
            .and_then(serde_json::Value::as_array)
            .map_or(0, Vec::len);
        if documents > 0 {
            self.client
                .clear_namespace(&namespace)
                .await
                .map_err(|error| Self::other("clear source documents", error))?;
        }
        let source_id = source_id.to_string();
        let chunks = blocking(self.config.clone(), "clear source chunks", move |config| {
            use tinymemory_core::store::chunks::{
                delete_chunks_by_source, delete_orphaned_source_tree, SourceKind,
            };
            let removed = delete_chunks_by_source(config, SourceKind::Document, &source_id)?;
            delete_orphaned_source_tree(config, SourceKind::Document, &source_id)?;
            Ok(removed)
        })
        .await?;
        Ok(u64::try_from(documents.saturating_add(chunks)).unwrap_or(u64::MAX))
    }
}

#[async_trait]
impl MemoryMaintenance for ModuleMemoryProvider {
    async fn reembed(&self) -> Result<MaintenanceReport, MemoryError> {
        let (examined, changed) =
            blocking(self.config.clone(), "enqueue re-embedding", move |config| {
                let total = tinymemory_core::queue::count_total(config).unwrap_or(0);
                let before = tinymemory_core::queue::count_by_status(
                    config,
                    tinymemory_core::queue::JobStatus::Ready,
                )
                .unwrap_or(0);
                tinymemory_core::queue::ensure_reembed_backfill(config);
                let after = tinymemory_core::queue::count_by_status(
                    config,
                    tinymemory_core::queue::JobStatus::Ready,
                )
                .unwrap_or(0);
                Ok((total, after.saturating_sub(before)))
            })
            .await?;
        Ok(MaintenanceReport {
            operation: "reembed".to_string(),
            examined,
            changed,
            findings: vec![format!("enqueued {changed} re-embedding job(s)")],
        })
    }

    async fn compact(&self) -> Result<MaintenanceReport, MemoryError> {
        let (examined, changed) =
            blocking(self.config.clone(), "compact memory queue", move |config| {
                Ok((
                    tinymemory_core::queue::count_total(config).unwrap_or(0),
                    u64::try_from(tinymemory_core::queue::recover_stale_locks(config).unwrap_or(0))
                        .unwrap_or(u64::MAX),
                ))
            })
            .await?;
        Ok(MaintenanceReport {
            operation: "compact".to_string(),
            examined,
            changed,
            findings: vec![format!("released {changed} stale queue lock(s)")],
        })
    }

    async fn consolidate(&self) -> Result<MaintenanceReport, MemoryError> {
        let (examined, enqueued) = blocking(
            self.config.clone(),
            "enqueue consolidation",
            move |config| {
                Ok((
                    tinymemory_core::queue::count_total(config).unwrap_or(0),
                    tinymemory_core::queue::scheduler::enqueue_flush_stale_job(config)
                        .map_err(anyhow::Error::msg)?,
                ))
            },
        )
        .await?;
        Ok(MaintenanceReport {
            operation: "consolidate".to_string(),
            examined,
            changed: u64::from(enqueued),
            findings: vec![if enqueued {
                "enqueued a stale-buffer flush".to_string()
            } else {
                "a stale-buffer flush is already queued".to_string()
            }],
        })
    }

    async fn doctor(&self) -> Result<MaintenanceReport, MemoryError> {
        let report = tinymemory_core::tree::health::async_run_doctor(&self.config).await;
        Ok(MaintenanceReport {
            operation: "doctor".to_string(),
            examined: report.counters.total_chunks,
            changed: 0,
            findings: report
                .stages
                .into_iter()
                .filter(|stage| !stage.ok)
                .map(|stage| format!("{}: {}", stage.stage, stage.note))
                .collect(),
        })
    }
}

#[async_trait]
impl MemoryProvider for ModuleMemoryProvider {
    fn driver_id(&self) -> &str {
        &self.driver_id
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities::all()
    }
    async fn health(&self) -> MemoryHealth {
        if self.client.memory_handle().health_check().await {
            MemoryHealth::Ready
        } else {
            MemoryHealth::down("memory store is unavailable")
        }
    }
    fn as_documents(&self) -> Option<&dyn MemoryDocuments> {
        Some(self)
    }
    fn as_ingest(&self) -> Option<&dyn MemoryIngest> {
        Some(self)
    }
    fn as_graph(&self) -> Option<&dyn MemoryGraph> {
        Some(self)
    }
    fn as_goals(&self) -> Option<&dyn MemoryGoals> {
        Some(self)
    }
    fn as_tool_memory(&self) -> Option<&dyn MemoryToolMemory> {
        Some(self)
    }
    fn as_tree(&self) -> Option<&dyn MemoryTree> {
        Some(self)
    }
    fn as_entities(&self) -> Option<&dyn MemoryEntities> {
        Some(self)
    }
    fn as_diff(&self) -> Option<&dyn MemoryDiff> {
        Some(self)
    }
    fn as_sources(&self) -> Option<&dyn MemorySourceSink> {
        Some(self)
    }
    fn as_maintenance(&self) -> Option<&dyn MemoryMaintenance> {
        Some(self)
    }
}
