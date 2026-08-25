//! The full TinyCortex provider: every capability family the engine can serve.
//!
//! `MemoryTraitProvider` composes the three mandatory families over the
//! `Memory` storage trait and stops there, which is honest but is also why
//! anything wanting a summary tree, entities, or a diff ledger had to reach
//! past the contract to the engine directly. This module closes that gap: the
//! optional families are implemented here, against the contract, so a host
//! filtering its surface from a negotiated capability set gets the whole engine
//! rather than a third of it.
//!
//! Lifted wholesale from `crates/tinymemory-module` (issue #18 §C3), which had
//! grown these implementations because it needed them and nowhere else had
//! them. They were never module-specific — every one delegates to
//! `tinymemory-core` on a blocking thread — so the module crate keeps only its
//! bus transport and the conversion from its own config.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use crate::TinycortexMemory;
use async_trait::async_trait;
use chrono::Utc;
use tinymemory_api::capabilities::Capabilities;
use tinymemory_api::chunks::Chunk;
use tinymemory_api::error::MemoryError;
use tinymemory_api::goals::GoalsDoc;
use tinymemory_api::health::MemoryHealth;
use tinymemory_api::host::{
    CloudProviderCreds, ComposioMode, LocalAiConfig, MemoryConfig, MemoryHostConfig,
    MemoryTreeConfig, SchedulerGateConfig,
};
use tinymemory_api::mandatory::MemoryTraitProvider;
use tinymemory_api::provider::types::{
    ChunkEntityOccurrence, EntityHit, EntityOccurrence, EntityRef, ExportPage, ExportRecord,
    FlushOutcome, ForgetOutcome, ForgetSelector, ImportOutcome, IngestItem, IngestOutcome,
    MaintenanceReport, PurgeOutcome, QueueFailure, QueueStats, ResetOutcome, SourceItem,
    SourceScope, StoreStats,
};
// Diff-family value types, used only by the `MemoryDiff` impl below — which is
// compiled out without the git-backed snapshot store.
#[cfg(feature = "memory-git")]
use tinymemory_api::provider::types::{ChangeKind, DiffReport, SnapshotRef, SourceChange};
use tinymemory_api::provider::{
    AddressBookSeedOutcome, ChunkDetail, ChunkEmbedding, ChunkListRow, ChunkQuery,
    CodingSessionIngestReport, CodingSessionIngestRequest, CodingSessionSource,
    ConversationSegment, CoverWindowQuery, DegradedCapabilities, Diagnosis, DiagnosisCounters,
    DiagnosisFailure, DiagnosisStage, EntityMatch, EpisodicEvent, EpisodicTurn, EventKind,
    FacetType, FastRetrieveQuery, MemoryChunks, MemoryCodingSessions, MemoryCore, MemoryDiff,
    MemoryDocuments, MemoryEntities, MemoryEpisodic, MemoryGoals, MemoryGraph, MemoryIngest,
    MemoryMaintenance, MemoryPeople, MemoryPortability, MemoryProfile, MemoryProvider,
    MemoryRecall, MemoryRetrieval, MemorySourceSink, MemorySourceSync, MemoryToolMemory,
    MemoryTree, PersonHandle, PersonInteraction, PersonRecord, PersonScore, ProfileFacet,
    RankedPerson, RawArchiveCoverage, RawRebuildOutcome, ResolvedPerson, RetrievalHit,
    RetrievalResponse, SourceRetrievalQuery, SourceSyncState, SourceSyncStatus, SourceTotal,
    SyncAuditEntry, SyncFreshness, SyncRunOutcome, UserState,
};
use tinymemory_api::recall::OwnedRecallOpts;
use tinymemory_api::tool_memory::ToolMemoryRule;
use tinymemory_api::tree::{
    IngestRequest, QueryResult, SummaryForest, TreeLeaf, TreeStatus, TreeSummary,
};
use tinymemory_api::types::{
    GraphRelationRecord, MemoryCategory, MemoryEntry, MemoryKvRecord, MemoryTaint,
    NamespaceDocumentInput, NamespaceMemoryHit, NamespaceRetrievalContext, NamespaceSummary,
    StoredMemoryDocument,
};
// The KV read paths must address rows by the same canonical form the
// write-path shim stores them under — see `MemoryGraph::kv_get` below.
use tinymemory_core::store::safety::canonical_identifier;
use tinymemory_core::store::{MemoryClient, MemoryClientRef};

/// The concrete, credential-free host configuration available inside a module.
#[derive(Debug, Clone)]
pub struct EngineRuntimeConfig {
    /// Root of the memory workspace on disk.
    pub workspace_dir: PathBuf,
    /// The `config.toml` inside [`Self::workspace_dir`].
    pub config_path: PathBuf,
    /// Memory engine settings.
    pub memory: MemoryConfig,
    /// Summary-tree settings.
    pub memory_tree: MemoryTreeConfig,
    /// Whether background work may run, and under what budget.
    pub scheduler_gate: SchedulerGateConfig,
    /// Local inference settings.
    pub local_ai: LocalAiConfig,
    /// Embeddings provider id, when one is configured.
    pub embeddings_provider: Option<String>,
    /// Memory driver id, when the host names one.
    pub memory_provider: Option<String>,
    /// Default chat model id, when one is configured.
    pub default_model: Option<String>,
    /// Default sampling temperature.
    pub default_temperature: f64,
    /// Preferred output language, when the host sets one.
    pub output_language: Option<String>,
    /// Opaque source configuration, passed through verbatim.
    pub memory_sources: serde_json::Value,
    /// The user's global memory-sync cadence in seconds, as the host resolved
    /// it.
    ///
    /// `None` is "no explicit choice" and callers fall back to
    /// [`DEFAULT_MEMORY_SYNC_INTERVAL_SECS`](tinymemory_api::host::DEFAULT_MEMORY_SYNC_INTERVAL_SECS);
    /// `Some(0)` is manual-only.
    ///
    /// Carried rather than answered with a constant because both periodic sync
    /// loops read it as their gate, and the constant they used to get was
    /// `Some(0)` — manual-only, which skips every source on every tick with no
    /// error, no warning and nothing in the log. A cadence a host cannot state
    /// is the one field where a wrong constant is invisible.
    pub memory_sync_interval_secs: Option<u64>,
    /// Composio routing mode:
    /// [`COMPOSIO_MODE_BACKEND`](tinymemory_api::host::COMPOSIO_MODE_BACKEND) or
    /// [`COMPOSIO_MODE_DIRECT`](tinymemory_api::host::COMPOSIO_MODE_DIRECT).
    ///
    /// Empty means the host stated no mode, and reads as "not direct" — the same
    /// answer backend mode gets, which is what an unset Composio integration
    /// should look like.
    pub composio_mode: String,
    /// The Composio entity the host authenticates as.
    ///
    /// An identifier, not a credential: it selects whose connected accounts a
    /// direct-mode call addresses. Empty is sent as no entity at all rather than
    /// as an empty one — see `ComposioClient::execute_direct`.
    pub composio_entity_id: String,
}

/// Why a module-side engine configuration can never answer a backend session
/// token, said once so every path that hits it reports the same cause.
///
/// The bare "not configured" this used to produce is the wrong story. It reads
/// as "the user is signed out", which a reader then tries to fix by signing in;
/// the truth is structural and no sign-in changes it. This configuration is
/// built from a `ModuleConfig`, which has no field for a bearer and deliberately
/// never will — and even if it did, a load-time snapshot could not follow a
/// token the host refreshes mid-session, so the module would authenticate with
/// an expired bearer until the next module load.
const NO_BACKEND_SESSION: &str =
    "this engine configuration carries no backend session token, by design: a loaded memory \
     module holds no credentials, and a bearer the host refreshes mid-session could not be \
     answered from a load-time snapshot anyway. Backend-mode Composio memory sync therefore \
     cannot run inside the module — only direct mode resolves here — and closing that means \
     routing the sync client through `ComposioHost::execute`, not handing the module a token";

#[async_trait]
impl MemoryHostConfig for EngineRuntimeConfig {
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
    /// Always the named refusal, never `Ok(None)`.
    ///
    /// The contract distinguishes the two: `Ok(None)` is "read fine, not signed
    /// in", `Err` is "could not be read". This configuration is neither — there
    /// is no store to read, and there never will be. `Ok(None)` would send
    /// `sync::pipelines::host::composio_config` down its backend branch to fail
    /// with "OpenHuman backend bearer token is not configured", which points a
    /// reader at a sign-in that would not help. See `NO_BACKEND_SESSION`,
    /// which is where the whole reason is written down.
    fn session_token(&self) -> Result<Option<String>, String> {
        Err(NO_BACKEND_SESSION.to_string())
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
        self.memory_sync_interval_secs
    }
    fn onboarding_completed(&self) -> bool {
        true
    }
    fn secrets_encrypt(&self) -> bool {
        false
    }
    /// The routing mode and entity the host resolved, and nothing else.
    ///
    /// Rebuilt from two scalar fields rather than stored as a [`ComposioMode`]
    /// so the two remaining members can only ever hold what this type promises:
    ///
    /// - `api_key` stays `None` because the direct-mode key is not configuration
    ///   here. `composio_config` asks the `ComposioHost` seam for it first and
    ///   falls back to this field second, so leaving it empty routes the key
    ///   through the seam — fetched for the duration of one call, held in no
    ///   field, which is the property that lets this struct call itself
    ///   credential-free.
    /// - `triage_disabled` stays `false` because nothing in the memory layer
    ///   reads it; it gates the host's LLM triage of Composio *triggers*, a path
    ///   that never enters this crate. Carrying a value nobody reads would
    ///   invite a reader to believe it does something here.
    fn composio(&self) -> ComposioMode {
        ComposioMode {
            mode: self.composio_mode.clone(),
            entity_id: self.composio_entity_id.clone(),
            api_key: None,
            triage_disabled: false,
        }
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
pub struct TinycortexProvider {
    driver_id: String,
    mandatory: MemoryTraitProvider,
    client: MemoryClientRef,
    config: EngineRuntimeConfig,
}

impl TinycortexProvider {
    /// Binds the engine as a provider.
    ///
    /// `driver_id` is the id the host admitted this driver under, not something
    /// the adapter chooses — see the class note in `tinymemory::registry`.
    pub fn new(driver_id: String, config: EngineRuntimeConfig, client: Arc<MemoryClient>) -> Self {
        let memory = client.memory_handle();
        let mandatory =
            MemoryTraitProvider::new(Arc::new(TinycortexMemory::new(memory)), driver_id.clone());
        Self {
            driver_id,
            mandatory,
            client,
            config,
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

/// Maps the engine's ingest summary onto the contract's outcome.
///
/// Shared by all three ingest methods because everything that distinguishes
/// them happens on the way *in* — how the source is canonicalised — and none
/// of it on the way out.
///
/// The two counts stay apart. `chunks_dropped` is content the scorer declined;
/// `already_ingested` is a call the source gate refused before any content was
/// looked at. Folding the second into the first is what the caller then has to
/// undo by guessing, and it cannot: one dropped chunk and a refused call
/// produce the same number.
fn ingest_outcome(result: tinymemory_core::ingest_pipeline::IngestResult) -> IngestOutcome {
    IngestOutcome {
        written: u32::try_from(result.chunks_written).unwrap_or(u32::MAX),
        skipped: u32::try_from(result.chunks_dropped).unwrap_or(u32::MAX),
        ids: result.chunk_ids,
        already_ingested: result.already_ingested,
        extract_jobs_enqueued: u32::try_from(result.extract_jobs_enqueued).unwrap_or(u32::MAX),
    }
}

async fn blocking<T, F>(
    config: EngineRuntimeConfig,
    context: &'static str,
    run: F,
) -> Result<T, MemoryError>
where
    T: Send + 'static,
    F: FnOnce(&EngineRuntimeConfig) -> anyhow::Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(move || run(&config))
        .await
        .map_err(|error| TinycortexProvider::other(context, error))?
        .map_err(|error| TinycortexProvider::other(context, error))
}

/// The capability families this build can actually serve.
///
/// A free function rather than only a method because it is the rule that has to
/// stay true, and it is testable on its own — constructing a provider requires
/// a `MemoryClient`, which requires the host's process-global seams to be
/// installed, and a test that installs those is order-dependent.
///
/// The `Diff` family is compiled out without `memory-git`, so a build without
/// it must not advertise `Diff`: `audit_provider` compares this set against the
/// reachable `as_*` accessors, and a mismatch is the failure that audit exists
/// to catch. The `#[cfg]` here and the one on [`MemoryProvider::as_diff`] are
/// the same condition on purpose.
#[must_use]
pub fn advertised_capabilities() -> Capabilities {
    #[cfg(feature = "memory-git")]
    {
        Capabilities::all()
    }
    #[cfg(not(feature = "memory-git"))]
    {
        Capabilities::all().without(tinymemory_api::capabilities::Capability::Diff)
    }
}

#[async_trait]
impl MemoryCore for TinycortexProvider {
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
impl MemoryRecall for TinycortexProvider {
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
impl MemoryPortability for TinycortexProvider {
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
impl MemoryDocuments for TinycortexProvider {
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

    async fn recall_documents(
        &self,
        namespace: &str,
        limit: usize,
    ) -> Result<NamespaceRetrievalContext, MemoryError> {
        let limit = u32::try_from(limit).unwrap_or(u32::MAX);
        let context = self
            .client
            .recall_namespace_context_data(namespace, limit)
            .await
            .map_err(|error| Self::other("recall_documents", error))?;
        Self::cross(&context, "convert document recall result")
    }
}

#[async_trait]
impl MemoryIngest for TinycortexProvider {
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
        Ok(ingest_outcome(result))
    }

    async fn ingest_chat(&self, messages: Vec<IngestItem>) -> Result<IngestOutcome, MemoryError> {
        let Some(first) = messages.first() else {
            return Ok(IngestOutcome::default());
        };
        let source_id = first.source_id.clone();
        let owner = first.owner.clone();
        let tags = first.tags.clone();
        // The three widened fields default to what this mapping always did, so
        // a caller that never sets them stores byte-identical rows.
        let platform = first
            .platform
            .clone()
            .unwrap_or_else(|| first.source.as_str().to_string());
        let channel_label = first
            .channel_label
            .clone()
            .unwrap_or_else(|| source_id.clone());
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
            channel_label,
            messages: messages
                .into_iter()
                .map(
                    |item| tinycortex::memory::ingest::canonicalize::chat::ChatMessage {
                        // The speaking role when the caller distinguishes it;
                        // the owner otherwise. Attributing every message to
                        // the owner is what destroyed role attribution for
                        // multi-speaker batches.
                        author: item.author.unwrap_or(item.owner),
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
        Ok(ingest_outcome(result))
    }

    async fn ingest_email(&self, messages: Vec<IngestItem>) -> Result<IngestOutcome, MemoryError> {
        let Some(first) = messages.first() else {
            return Ok(IngestOutcome::default());
        };
        let source_id = first.source_id.clone();
        let owner = first.owner.clone();
        let tags = first.tags.clone();
        let provider = first
            .platform
            .clone()
            .unwrap_or_else(|| first.source.as_str().to_string());
        let thread_subject = first
            .channel_label
            .clone()
            .unwrap_or_else(|| source_id.clone());
        for item in &messages {
            validate_ingest_item(item)?;
            if item.source_id != source_id {
                return Err(MemoryError::Invalid(
                    "ingest_email batches must contain one thread".to_string(),
                ));
            }
        }
        let thread = tinycortex::memory::ingest::canonicalize::email::EmailThread {
            provider,
            thread_subject: thread_subject.clone(),
            messages: messages
                .into_iter()
                .map(
                    |item| tinycortex::memory::ingest::canonicalize::email::EmailMessage {
                        // Same rule as the chat mapping: the speaking role when
                        // the caller distinguishes it, the owner otherwise.
                        from: item.author.unwrap_or(item.owner),
                        to: item.to,
                        cc: item.cc,
                        // A reply carries the thread's subject; only a renamed
                        // thread differs, which is what the per-item field is
                        // for.
                        subject: item.subject.unwrap_or_else(|| thread_subject.clone()),
                        sent_at: item.timestamp.unwrap_or_else(Utc::now),
                        body: item.content,
                        source_ref: item.source_ref.map(|source_ref| source_ref.value),
                        // Carried verbatim: an unsubscribe flow reads this back
                        // out of stored mail, so dropping it makes that flow
                        // impossible rather than merely less complete.
                        list_unsubscribe: item.list_unsubscribe,
                    },
                )
                .collect(),
        };
        let result = tinymemory_core::ingest_pipeline::ingest_email(
            &self.config,
            &source_id,
            &owner,
            tags,
            thread,
        )
        .await
        .map_err(|error| Self::other("ingest email", error))?;
        Ok(ingest_outcome(result))
    }
}

#[async_trait]
impl MemoryGraph for TinycortexProvider {
    async fn kv_get(
        &self,
        namespace: Option<&str>,
        key: &str,
    ) -> Result<Option<MemoryKvRecord>, MemoryError> {
        // The write path stores `canonical_identifier(key)` (the KV shim in
        // `tinymemory-core` canonicalizes on the way in), so the stored keys
        // are canonical and a raw-key comparison misses every rewritten key.
        // `kv_delete` already goes through the shim; this lookup has to apply
        // the same transform or put→get misses while put→delete works.
        let key = canonical_identifier(key);
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
            // Stored keys are canonical (see `kv_get`), so prefix matching is
            // over canonical stored keys: the caller's prefix is canonicalized
            // before comparing. A prefix that truncates a PII pattern
            // mid-match stays raw (the transform is a no-op on it) and will
            // not reach a rewritten key — the placeholder is the stored form.
            let prefix = canonical_identifier(prefix);
            records.retain(|record| record.key.starts_with(&prefix));
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
impl MemoryGoals for TinycortexProvider {
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
impl MemoryToolMemory for TinycortexProvider {
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
impl MemoryTree for TinycortexProvider {
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

    async fn summary_forest(
        &self,
        limit: usize,
        scope: Option<&SourceScope>,
    ) -> Result<SummaryForest, MemoryError> {
        /// This driver's own ceiling on one forest walk.
        ///
        /// The contract says the driver clamps, and this is where. It is not a
        /// quota anyone should meet: it is the bound that stops a single read
        /// from naming every summary in a store that has been ingesting for a
        /// year, which the module would then have to refuse for overrunning a
        /// frame.
        const MAX_FOREST_SUMMARIES: usize = 20_000;

        /// Epoch milliseconds as the contract's timestamp.
        ///
        /// A row outside the representable range floors at the epoch rather
        /// than failing the whole walk: one nonsense timestamp is a bad node,
        /// not a bad read.
        fn at_ms(ms: i64) -> chrono::DateTime<Utc> {
            chrono::DateTime::from_timestamp_millis(ms)
                .unwrap_or(chrono::DateTime::<Utc>::UNIX_EPOCH)
        }

        let limit = limit.clamp(1, MAX_FOREST_SUMMARIES);
        let scope = scope.cloned();
        blocking(self.config.clone(), "walk summary forest", move |config| {
            tinymemory_core::store::chunks::store::with_connection(config, |conn| {
                // The scope predicate applies to the *tree*, not to each
                // summary: a summary's only source attribution is the tree it
                // sealed into. Resolving the allowed trees first and binding
                // their ids into the row query keeps the filter inside SQL and
                // ahead of the LIMIT — filtering afterwards would let a
                // withheld tree eat the caller's budget.
                let allowed_tree_ids = match &scope {
                    None => None,
                    Some(scope) => {
                        // `allows_source_id` is the contract's own published
                        // predicate. The retrieval ranker uses a looser
                        // tree-scope variant that additionally accepts a bare
                        // `mem_src:<id>` allow entry; this is deliberately the
                        // stricter of the two. Over-denying hides a node from a
                        // graph, which the user can see; under-denying leaks
                        // one, which the user cannot.
                        let mut stmt = conn.prepare("SELECT id, scope FROM mem_tree_trees")?;
                        let mut ids = Vec::new();
                        let rows = stmt.query_map([], |row| {
                            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                        })?;
                        for row in rows {
                            let (id, tree_scope) = row?;
                            if scope.allows_source_id(&tree_scope) {
                                ids.push(id);
                            }
                        }
                        Some(ids)
                    }
                };

                // An empty allowlist denies everything — `SourceScope`'s
                // fail-closed rule — and so does a scope that matched no tree.
                // Both are an empty forest that is *not* truncated: nothing was
                // withheld by a bound, so the caller must not be told to ask
                // again with a bigger one.
                if allowed_tree_ids.as_ref().is_some_and(Vec::is_empty) {
                    return Ok(SummaryForest::default());
                }

                // One row over the limit, so truncation is observed rather than
                // inferred. A store holding exactly `limit` nodes is complete,
                // and `rows.len() == limit` alone cannot tell that apart from a
                // store holding one more.
                let probe = i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX);
                let mut sql = String::from(
                    "SELECT s.id, s.tree_id, s.tree_kind, t.scope, s.level, s.parent_id,
                            s.child_ids_json, s.time_range_start_ms, s.time_range_end_ms
                       FROM mem_tree_summaries s
                       JOIN mem_tree_trees t ON t.id = s.tree_id
                      WHERE s.deleted = 0",
                );
                let mut bound: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
                if let Some(ids) = &allowed_tree_ids {
                    sql.push_str(" AND s.tree_id IN (");
                    for (index, id) in ids.iter().enumerate() {
                        if index > 0 {
                            sql.push(',');
                        }
                        sql.push('?');
                        bound.push(Box::new(id.clone()));
                    }
                    sql.push(')');
                }
                // Tree-major, so a truncated walk loses whole trees off the
                // tail rather than thinning every tree by a little. That is the
                // honest way to cut a forest — a caller can see a source is
                // missing, where it cannot see that every tree is short some
                // nodes — and it is what `SummaryForest::truncated` documents.
                sql.push_str(" ORDER BY s.tree_id, s.level, s.sealed_at_ms LIMIT ?");
                bound.push(Box::new(probe));
                let params = bound
                    .iter()
                    .map(|value| value.as_ref() as &dyn rusqlite::ToSql)
                    .collect::<Vec<_>>();

                let mut summaries = conn
                    .prepare(&sql)?
                    .query_map(params.as_slice(), |row| {
                        let child_ids_json: String = row.get(6)?;
                        Ok(TreeSummary {
                            id: row.get(0)?,
                            tree_id: row.get(1)?,
                            tree_kind: row.get(2)?,
                            tree_scope: row.get(3)?,
                            // The column is signed and the contract is not.
                            // The sealer never writes a negative level, so a
                            // corrupt row floors at zero rather than wrapping
                            // to four billion and sorting last.
                            level: u32::try_from(row.get::<_, i64>(4)?.max(0)).unwrap_or(u32::MAX),
                            // A node that is its tree's current root stores
                            // NULL; the empty string is the same state written
                            // by an older path. Both must read as "no parent"
                            // or the caller draws an edge to a node named "".
                            parent_id: row.get::<_, Option<String>>(5)?.filter(|id| !id.is_empty()),
                            // A malformed blob is a broken row, not a broken
                            // read: the node still belongs in the graph, it
                            // just contributes no child edges.
                            child_ids: serde_json::from_str(&child_ids_json).unwrap_or_default(),
                            time_range_start: at_ms(row.get(7)?),
                            time_range_end: at_ms(row.get(8)?),
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;

                let truncated = summaries.len() > limit;
                summaries.truncate(limit);
                Ok(SummaryForest {
                    summaries,
                    truncated,
                })
            })
            .map_err(|error| anyhow::anyhow!("walk summary forest: {error}"))
        })
        .await
    }

    async fn recent_leaves(
        &self,
        limit: usize,
        scope: Option<&SourceScope>,
    ) -> Result<Vec<TreeLeaf>, MemoryError> {
        // Row selection delegates to `list_chunks` rather than a second
        // hand-written `SELECT`, and that is the important part: it already
        // applies the source allowlist in SQL ahead of the LIMIT, including the
        // tags-aware rule that lets through content carrying no memory-source
        // provenance at all. A second copy of that predicate here would be free
        // to drift from the one every other chunk read goes through, and a
        // source gate that drifts fails open.
        let query = tinymemory_core::store::chunks::ListChunksQuery {
            source_scope: scope.map(|scope| scope.allow.iter().cloned().collect::<HashSet<_>>()),
            limit: Some(limit),
            exclude_dropped: true,
            ..Default::default()
        };
        let chunks = blocking(self.config.clone(), "list recent leaves", move |config| {
            tinymemory_core::store::chunks::list_chunks(config, &query)
        })
        .await?;
        if chunks.is_empty() {
            return Ok(Vec::new());
        }

        // The parent link lives on the chunk row but not in the chunk model, so
        // it is a second read keyed by the ids the first one returned. It
        // cannot widen the result: every id here already passed the scope
        // predicate above, and this query filters to exactly those ids. A leaf
        // sealed between the two reads comes back unattached — the answer the
        // first read was true for, and a state the caller already has to handle
        // for content the scheduler has not reached.
        let mut ids = Vec::with_capacity(chunks.len());
        for chunk in &chunks {
            ids.push(chunk.id.clone());
        }
        let parents = blocking(self.config.clone(), "read leaf parents", move |config| {
            tinymemory_core::store::chunks::store::with_connection(config, |conn| {
                // Built by hand rather than by `repeat_n(..).join(..)` so the
                // bind order and the placeholder count come from one loop:
                // a mismatch between them is a runtime SQL error, not a
                // compile-time one.
                let mut placeholders = String::with_capacity(ids.len() * 2);
                for index in 0..ids.len() {
                    if index > 0 {
                        placeholders.push(',');
                    }
                    placeholders.push('?');
                }
                let sql = format!(
                    "SELECT id, parent_summary_id FROM mem_tree_chunks WHERE id IN ({placeholders})"
                );
                let params = ids
                    .iter()
                    .map(|id| id as &dyn rusqlite::ToSql)
                    .collect::<Vec<_>>();
                let parents = conn
                    .prepare(&sql)?
                    .query_map(params.as_slice(), |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, Option<String>>(1)?.filter(|id| !id.is_empty()),
                        ))
                    })?
                    .collect::<rusqlite::Result<std::collections::HashMap<String, Option<String>>>>(
                    )?;
                Ok(parents)
            })
            .map_err(|error| anyhow::anyhow!("read leaf parents: {error}"))
        })
        .await?;

        Ok(chunks
            .into_iter()
            .map(|chunk| {
                let (time_range_start, time_range_end) = chunk.metadata.time_range;
                TreeLeaf {
                    parent_summary_id: parents.get(&chunk.id).cloned().flatten(),
                    source_id: chunk.metadata.source_id,
                    // The shared helper rather than a local truncation: two
                    // drivers disagreeing about what a preview is shows up as a
                    // label that changes length when the driver changes.
                    preview: tinymemory_api::tree::leaf_preview(&chunk.content),
                    chunk_id: chunk.id,
                    time_range_start,
                    time_range_end,
                }
            })
            .collect())
    }

    async fn flush_source_tree(&self, source_scope: &str) -> Result<u64, MemoryError> {
        let scope = source_scope.to_string();
        // The lookup is synchronous SQLite and it also (re)writes the source's
        // `_source.md` mirror, so it goes to a blocking thread like every other
        // synchronous read in this file. It creates the tree when the scope has
        // none, which is what makes the member idempotent rather than a probe
        // for which scopes exist.
        let tree = blocking(self.config.clone(), "open the source tree", move |config| {
            tinymemory_core::tree_source::get_or_create_source_tree(config, &scope)
        })
        .await?;

        // The labelling policy comes from the tree's own kind and scope, which
        // is the reason the contract passes a scope rather than a namespace:
        // the caller has no way to make this choice and should not be making
        // it. `from_tree` reads the kind off the row just fetched, so a topic
        // or global tree reached through this member still gets its own
        // policy rather than the source default.
        let strategy =
            tinymemory_core::tree::tree::TreeFactory::from_tree(&tree).label_strategy(&self.config);

        // Seal *and* cascade, in one call: `force_flush_tree` cascades from
        // level zero, so the leaves it seals are rolled up in the same pass.
        // Stopping after the seal would leave a tier of leaves under no
        // summary, which every structural query reads as an empty tree.
        let sealed = tinymemory_core::tree::tree::flush::force_flush_tree(
            &self.config,
            &tree.id,
            None,
            &strategy,
        )
        .await
        .map_err(|error| Self::other("flush the source tree", error))?;
        Ok(u64::try_from(sealed.len()).unwrap_or(u64::MAX))
    }
}

/// Validate entity-kind wire strings and re-emit them in the index's spelling.
///
/// The same parser and the same rule `MemoryEntities::top_entities` applies to
/// its single kind: an unrecognised one is an error rather than a filter that
/// matches nothing, because a misspelling and an empty index produce the same
/// empty answer and the caller acts on it either way. Re-emitting `as_str`
/// settles the spelling on the one the index writes, so a kind that reached
/// the caller through some other vocabulary still matches.
fn canonical_entity_kinds(kinds: &[String]) -> Result<Vec<String>, MemoryError> {
    kinds
        .iter()
        .map(|kind| {
            tinymemory_core::tree::score::extract::EntityKind::parse(kind)
                .map(|parsed| parsed.as_str().to_string())
                .map_err(|_| MemoryError::Invalid(format!("unknown entity kind: {kind}")))
        })
        .collect()
}

#[async_trait]
impl MemoryEntities for TinycortexProvider {
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

    async fn top_entities(
        &self,
        kind: Option<&str>,
        limit: usize,
    ) -> Result<Vec<EntityOccurrence>, MemoryError> {
        // Parsed rather than passed through, because the column stores whatever
        // string the writer used: an unrecognised filter would match no rows and
        // arrive as "this store knows about nothing". Same rule and same parser
        // as `MemoryRetrieval::search_entities`, and re-emitting `as_str` keeps
        // the comparison against the canonical spelling the index writes.
        let kind = match kind {
            Some(kind) => Some(
                tinymemory_core::tree::score::extract::EntityKind::parse(kind)
                    .map_err(|_| MemoryError::Invalid(format!("unknown entity kind: {kind}")))?
                    .as_str()
                    .to_string(),
            ),
            None => None,
        };
        let rows = blocking(self.config.clone(), "read top entities", move |config| {
            tinymemory_core::store::entities::top_entity_rows(config, kind.as_deref(), limit)
        })
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| EntityOccurrence {
                entity_id: row.entity_id,
                kind: row.entity_kind,
                surface: row.surface,
                mentions: row.mentions,
            })
            .collect())
    }

    async fn chunk_entities(
        &self,
        chunk_ids: &[String],
        kinds: Option<&[String]>,
    ) -> Result<Vec<ChunkEntityOccurrence>, MemoryError> {
        let kinds = match kinds {
            // No filter. The store spells that as an empty kind list, the same
            // way every plural `ChunkQuery` filter does.
            None => Vec::new(),
            // A filter admitting no kind, which the contract answers with an
            // empty vector. It has to be answered here rather than passed
            // down, because the store would read the same empty list as
            // *unfiltered* — the right reading there and the wrong one for an
            // `Option` whose `None` already says "no filter". The two readings
            // meet at this line and nowhere else.
            Some([]) => return Ok(Vec::new()),
            Some(kinds) => canonical_entity_kinds(kinds)?,
        };
        let node_ids = chunk_ids.to_vec();
        let rows = blocking(self.config.clone(), "read chunk entities", move |config| {
            tinymemory_core::store::entities::node_entity_rows(config, &node_ids, &kinds)
        })
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| ChunkEntityOccurrence {
                // The index calls this a node id because summaries live in the
                // same table; every row here came back under an id the caller
                // asked for, so tagging it as the chunk is not a widening.
                chunk_id: row.node_id,
                occurrence: EntityOccurrence {
                    entity_id: row.entity_id,
                    kind: row.entity_kind,
                    surface: row.surface,
                    mentions: row.mentions,
                },
            })
            .collect())
    }

    async fn entity_chunk_ids(
        &self,
        entity_id: &str,
        limit: usize,
    ) -> Result<Vec<String>, MemoryError> {
        let entity_id = entity_id.to_string();
        blocking(
            self.config.clone(),
            "read entity chunk ids",
            move |config| {
                tinymemory_core::store::entities::entity_leaf_node_ids(config, &entity_id, limit)
            },
        )
        .await
    }
}

#[cfg(feature = "memory-git")]
#[async_trait]
impl MemoryDiff for TinycortexProvider {
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

/// Read a contract source-kind wire string in the engine's vocabulary.
///
/// The contract carries the kind as text because the host's sync machinery
/// grows kinds without a contract change; this engine stores three of them and
/// has to say so rather than match nothing. A rejected kind is
/// [`MemoryError::Invalid`] and never an outcome of zero — on the delete paths
/// this serves, a zero would tell an operator their content was already gone
/// when nothing had been looked at.
fn parse_source_kind(
    kind: &str,
) -> Result<tinymemory_core::store::chunks::SourceKind, MemoryError> {
    tinymemory_core::store::chunks::SourceKind::parse(kind).map_err(MemoryError::Invalid)
}

#[async_trait]
impl MemorySourceSink for TinycortexProvider {
    async fn accept_source_items(
        &self,
        source_id: &str,
        source_kind: &str,
        items: Vec<SourceItem>,
        taint: MemoryTaint,
    ) -> Result<IngestOutcome, MemoryError> {
        let items_len = items.len();
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
                // A write failure is NOT `skipped`. The contract defines that
                // field as "units the driver recognised as already present"
                // (`IngestOutcome::skipped`), so counting a failed write there
                // reports a locked database, a full disk or a dead embedder as
                // a successful no-op: the sync caller marks the items done and
                // they are never written. Propagate instead — a partial batch
                // has no truthful representation in `IngestOutcome`, and a
                // caller that wants best-effort ingestion can catch this.
                Err(error) => {
                    return Err(MemoryError::Other(anyhow::anyhow!(
                        "source ingest failed after {} of {} item(s) were written: {error}",
                        outcome.written,
                        items_len
                    )));
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

    async fn forget_matching(
        &self,
        selector: &ForgetSelector,
    ) -> Result<ForgetOutcome, MemoryError> {
        // The kind arrives as a wire string and is parsed before any delete
        // runs, rather than inside the blocking closure: that closure's error
        // channel is `anyhow`, which would surface a kind this driver does not
        // recognise as a store failure. On a destructive call the difference
        // decides what an operator does next — retry, or fix the argument.
        //
        // `trees_cleaned` is only ever non-zero for the exact-source arm, and
        // that is a property of the engine rather than an omission here. Its
        // delete already cascades the trees whose scope it emptied; the extra
        // sweep is the *legacy* cleanup for a source whose chunks went in an
        // earlier, partial delete and left a tree behind. That question can
        // only be asked of one source id — a prefix or an owner names a set,
        // and there is no orphaned scope to name for a set.
        let removed = |count: usize| u64::try_from(count).unwrap_or(u64::MAX);
        let outcome = match selector {
            ForgetSelector::Chunk { chunk_id } => {
                let chunk_id = chunk_id.clone();
                let count = blocking(self.config.clone(), "forget one chunk", move |config| {
                    tinymemory_core::store::chunks::delete_chunk_by_id(config, &chunk_id)
                })
                .await?;
                ForgetOutcome {
                    chunks_removed: removed(count),
                    trees_cleaned: 0,
                }
            }
            ForgetSelector::Source {
                source_kind,
                source_id,
            } => {
                let kind = parse_source_kind(source_kind)?;
                let source_id = source_id.clone();
                blocking(self.config.clone(), "forget one source", move |config| {
                    let count = tinymemory_core::store::chunks::delete_chunks_by_source(
                        config, kind, &source_id,
                    )?;
                    let cleaned = tinymemory_core::store::chunks::delete_orphaned_source_tree(
                        config, kind, &source_id,
                    )?;
                    Ok(ForgetOutcome {
                        chunks_removed: u64::try_from(count).unwrap_or(u64::MAX),
                        trees_cleaned: u64::from(cleaned),
                    })
                })
                .await?
            }
            ForgetSelector::SourcePrefix {
                source_kind,
                source_id_prefix,
            } => {
                let kind = parse_source_kind(source_kind)?;
                let prefix = source_id_prefix.clone();
                let count = blocking(
                    self.config.clone(),
                    "forget a source prefix",
                    move |config| {
                        tinymemory_core::store::chunks::delete_chunks_by_source_prefix(
                            config, kind, &prefix,
                        )
                    },
                )
                .await?;
                ForgetOutcome {
                    chunks_removed: removed(count),
                    trees_cleaned: 0,
                }
            }
            ForgetSelector::Owner { source_kind, owner } => {
                let kind = parse_source_kind(source_kind)?;
                let owner = owner.clone();
                let count = blocking(self.config.clone(), "forget one owner", move |config| {
                    tinymemory_core::store::chunks::delete_chunks_by_owner(config, kind, &owner)
                })
                .await?;
                ForgetOutcome {
                    chunks_removed: removed(count),
                    trees_cleaned: 0,
                }
            }
        };
        Ok(outcome)
    }
}

#[async_trait]
impl MemoryMaintenance for TinycortexProvider {
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

    async fn retry_failed(&self) -> Result<MaintenanceReport, MemoryError> {
        let (examined, changed) = blocking(
            self.config.clone(),
            "retry failed queue work",
            move |config| {
                let examined = tinymemory_core::queue::count_total(config).unwrap_or(0);
                let changed = tinymemory_core::queue::store::requeue_failed(config)?;
                // Inside the same call, and only when something moved: rows
                // put back on `ready` sit until the next scheduled window
                // otherwise, which reads as a retry that did nothing.
                if changed > 0 {
                    tinymemory_core::queue::wake_workers();
                }
                Ok((examined, changed))
            },
        )
        .await?;
        Ok(MaintenanceReport {
            operation: "retry_failed".to_string(),
            examined,
            changed,
            findings: vec![format!("requeued {changed} failed job(s)")],
        })
    }

    async fn store_stats(&self) -> Result<StoreStats, MemoryError> {
        blocking(self.config.clone(), "read store stats", move |config| {
            tinymemory_core::store::chunks::store::with_connection(config, |conn| {
                // One statement for all three. The count and the extracted
                // count are a ratio the caller displays, and sampling them
                // either side of a write can put the numerator above the
                // denominator — a coverage over 100%.
                //
                // `MAX` over an empty table is SQL NULL, which is the same
                // answer as "no chunks" and must stay distinguishable from a
                // chunk stamped at the epoch — hence `Option`, not `0`.
                let (chunks, chunks_with_structure, most_recent_chunk_ms): (i64, i64, Option<i64>) =
                    conn.query_row(
                        "SELECT
                       COUNT(*),
                       COALESCE(SUM(EXISTS (
                         SELECT 1 FROM mem_tree_entity_index e
                          WHERE e.node_id = c.id
                       )), 0),
                       MAX(timestamp_ms)
                     FROM mem_tree_chunks c",
                        [],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )?;
                let count = |n: i64| u64::try_from(n).unwrap_or(0);
                Ok(StoreStats {
                    chunks: count(chunks),
                    chunks_with_structure: count(chunks_with_structure),
                    most_recent_chunk_ms,
                })
            })
            .map_err(|error| anyhow::anyhow!("store stats: {error}"))
        })
        .await
    }

    async fn queue_stats(&self, kind: Option<&str>) -> Result<QueueStats, MemoryError> {
        let kind = kind.map(str::to_string);
        blocking(self.config.clone(), "read queue stats", move |config| {
            let now_ms = chrono::Utc::now().timestamp_millis();
            tinymemory_core::store::chunks::store::with_connection(config, |conn| {
                // One statement rather than six round trips, and one `now`
                // rather than one per sub-query: counts taken at different
                // instants can disagree with each other, and an idle-time
                // calculation built on that reads as a stall that never
                // happened.
                let (
                    ready,
                    running,
                    done,
                    failed,
                    failed_unrecoverable,
                    eligible_now,
                    last_completed_ms,
                    oldest_eligible_ms,
                ): (i64, i64, i64, i64, i64, i64, Option<i64>, Option<i64>) = conn.query_row(
                    "SELECT
                       COALESCE(SUM(status = 'ready'), 0),
                       COALESCE(SUM(status = 'running'), 0),
                       COALESCE(SUM(status = 'done'), 0),
                       COALESCE(SUM(status = 'failed'), 0),
                       COALESCE(SUM(status = 'failed'
                                    AND failure_class = 'unrecoverable'), 0),
                       COALESCE(SUM(status = 'ready' AND available_at_ms <= ?1), 0),
                       -- Every status, not just 'done'. A job that fails
                       -- stamps `completed_at_ms` too, and a queue failing
                       -- fast is making progress in the only sense this
                       -- field measures: it is not stuck. Filtering to
                       -- 'done' would report a fast-failing pipeline as
                       -- idle, which is the misdiagnosis this exists to
                       -- avoid. Supersession wants the opposite reading and
                       -- gets its own field on `QueueFailure`.
                       MAX(completed_at_ms),
                       MIN(CASE WHEN status = 'ready' AND available_at_ms <= ?1
                                THEN available_at_ms END)
                     FROM mem_tree_jobs
                     WHERE (?2 IS NULL OR kind = ?2)",
                    rusqlite::params![now_ms, kind],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                            row.get(6)?,
                            row.get(7)?,
                        ))
                    },
                )?;
                let count = |n: i64| u64::try_from(n).unwrap_or(0);
                Ok(QueueStats {
                    ready: count(ready),
                    running: count(running),
                    done: count(done),
                    failed: count(failed),
                    failed_unrecoverable: count(failed_unrecoverable),
                    eligible_now: count(eligible_now),
                    last_completed_ms,
                    oldest_eligible_ms,
                })
            })
            .map_err(|error| anyhow::anyhow!("queue stats: {error}"))
        })
        .await
    }

    async fn latest_queue_failure(&self) -> Result<Option<QueueFailure>, MemoryError> {
        blocking(
            self.config.clone(),
            "read latest queue failure",
            move |config| {
                tinymemory_core::store::chunks::store::with_connection(config, |conn| {
                    use rusqlite::OptionalExtension;
                    let Some(mut failure) = conn
                        .query_row(
                            "SELECT failure_reason, failure_class, completed_at_ms
                           FROM mem_tree_jobs
                          WHERE status = 'failed' AND failure_reason IS NOT NULL
                          ORDER BY completed_at_ms DESC
                          LIMIT 1",
                            [],
                            |row| {
                                Ok(QueueFailure {
                                    reason: row.get(0)?,
                                    class: row.get(1)?,
                                    completed_at_ms: row.get(2)?,
                                    last_success_ms: None,
                                })
                            },
                        )
                        .optional()?
                    else {
                        return Ok(None);
                    };

                    // Still inside the same `with_connection`, which holds the
                    // connection for the whole closure: no job can settle
                    // between the two reads and flip a supersession decision
                    // made from them.
                    //
                    // Only worth asking when the failure is timestamped —
                    // without one there is nothing to compare a success
                    // against, and the caller has to surface it either way.
                    if failure.completed_at_ms.is_some() {
                        failure.last_success_ms = conn
                            .query_row(
                                "SELECT MAX(completed_at_ms) FROM mem_tree_jobs
                              WHERE status = 'done'",
                                [],
                                |row| row.get(0),
                            )
                            .optional()
                            .map(Option::flatten)?;
                    }

                    Ok(Some(failure))
                })
                .map_err(|error| anyhow::anyhow!("latest queue failure: {error}"))
            },
        )
        .await
    }

    async fn flush_pending(&self) -> Result<FlushOutcome, MemoryError> {
        blocking(
            self.config.clone(),
            "flush pending buffers",
            move |config| {
                let now = chrono::Utc::now();
                let stale = tinymemory_core::store::trees::store::list_stale_buffers(config, now)?;
                let stale_buffers = u64::try_from(stale.len()).unwrap_or(u64::MAX);

                // `max_age_secs: 0` is what "now" means here: consider every buffer
                // rather than only those past the scheduled age.
                let payload = tinymemory_core::queue::types::FlushStalePayload {
                    max_age_secs: Some(0),
                };
                // The key is date + three-hour block, so a second flush inside the
                // same window deduplicates against the first instead of scheduling
                // the work twice. `enqueue` answering `None` is that deduplication,
                // not a failure — which is why the outcome carries the buffer count
                // beside it, so a caller can tell "nothing to do" from "already
                // scheduled".
                let date_iso = now.format("%Y-%m-%d").to_string();
                let hour_block = chrono::Timelike::hour(&now) / 3;
                let job = tinymemory_core::queue::types::NewJob::flush_stale(
                    &payload, &date_iso, hour_block,
                )?;
                let enqueued = tinymemory_core::queue::store::enqueue(config, &job)?.is_some();
                if enqueued {
                    tinymemory_core::queue::wake_workers();
                }
                Ok(FlushOutcome {
                    enqueued,
                    stale_buffers,
                })
            },
        )
        .await
    }

    async fn reset_derived_index(&self) -> Result<ResetOutcome, MemoryError> {
        blocking(self.config.clone(), "reset derived index", move |config| {
            // Everything here is derived from `mem_tree_chunks`, which is NOT
            // in the list and is never deleted. That is the invariant the
            // contract promises: nothing a caller wrote is lost, only what was
            // computed from it.
            const DERIVED_TABLES: &[&str] = &[
                "mem_tree_summaries",
                "mem_tree_buffers",
                "mem_tree_jobs",
                "mem_tree_entity_index",
                "mem_tree_trees",
            ];
            let rows_deleted =
                tinymemory_core::store::chunks::store::with_connection(config, |conn| {
                    let tx = conn.unchecked_transaction()?;
                    let mut total: u64 = 0;
                    for table in DERIVED_TABLES {
                        total += tx.execute(&format!("DELETE FROM {table}"), [])? as u64;
                    }
                    tx.commit()?;
                    Ok(total)
                })?;

            // Second transaction, deliberately. The delete above drops the job
            // table, so re-enqueueing in the same one would race its own
            // truncation.
            let (chunks_requeued, jobs_enqueued) =
                tinymemory_core::store::chunks::store::with_connection(config, |conn| {
                    let tx = conn.unchecked_transaction()?;
                    let chunks_requeued = tx.execute(
                        "UPDATE mem_tree_chunks SET lifecycle_status = 'pending_extraction'",
                        [],
                    )? as u64;
                    let chunk_ids: Vec<String> = {
                        let mut stmt = tx.prepare("SELECT id FROM mem_tree_chunks")?;
                        // Bound rather than returned directly: the rows borrow
                        // `stmt`, which the block would otherwise drop first.
                        let rows = stmt
                            .query_map([], |row| row.get::<_, String>(0))?
                            .collect::<rusqlite::Result<Vec<_>>>()?;
                        rows
                    };
                    let mut jobs_enqueued: u64 = 0;
                    for chunk_id in &chunk_ids {
                        let payload = tinymemory_core::queue::types::ExtractChunkPayload {
                            chunk_id: chunk_id.clone(),
                        };
                        let job = tinymemory_core::queue::types::NewJob::extract_chunk(&payload)?;
                        // Keyed, so a chunk already queued is a no-op rather
                        // than a duplicate job — which is why this count can be
                        // lower than `chunks_requeued`.
                        if tinymemory_core::queue::store::enqueue_tx(&tx, &job)?.is_some() {
                            jobs_enqueued += 1;
                        }
                    }
                    tx.commit()?;
                    Ok((chunks_requeued, jobs_enqueued))
                })?;

            // The work is scheduled; nothing drains it until a worker is woken.
            tinymemory_core::queue::wake_workers();

            Ok(ResetOutcome {
                rows_deleted,
                chunks_requeued,
                jobs_enqueued,
            })
        })
        .await
    }

    async fn purge_all(&self) -> Result<PurgeOutcome, MemoryError> {
        // The opposite end of the scale from `reset_derived_index` above, and
        // the contrast is the whole reason both exist: that one is guaranteed
        // to keep `mem_tree_chunks`, this one is guaranteed not to. Neither is
        // a safer spelling of the other, so a caller has to pick, and the two
        // names say which it picked.
        //
        // The database is the whole of this call. Content files on disk stay
        // the caller's — the vault root is a host path the driver is handed,
        // and a driver deleting directories under it would be acting on
        // filesystem policy it does not own.
        let chunks_removed = blocking(self.config.clone(), "purge the store", move |config| {
            tinymemory_core::store::chunks::purge_all(config)
        })
        .await?;
        Ok(PurgeOutcome {
            rows_deleted: u64::try_from(chunks_removed).unwrap_or(u64::MAX),
        })
    }

    async fn backfill_in_progress(&self) -> Result<bool, MemoryError> {
        // A process-global the backfill chain owns, not a column — and not one
        // this engine can narrow, since `tinymemory_core::queue` tracks the
        // chain for the process rather than per workspace. No `blocking`: it is
        // an atomic load, not a query.
        //
        // The contract member says process-wide in its own signature, which is
        // the whole reason this is not a `QueueStats` field: `queue_stats` is
        // asked of one bound provider for one store, and a global answered
        // there would read as store-scoped to every caller.
        Ok(tinymemory_core::queue::backfill_in_progress())
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

    /// The same pass [`MemoryMaintenance::doctor`] runs, reported in full.
    ///
    /// Both members call `async_run_doctor` and neither runs it twice for the
    /// other: `doctor` throws away the classification, the degradation flags
    /// and the counters to fit the family's uniform report, and this one keeps
    /// them. That is the whole difference, and it is why the pair is not a
    /// duplicate — the engine work is one call, and the two projections have
    /// different readers.
    ///
    /// It cannot fail. `async_run_doctor` is best-effort by construction —
    /// counter reads that error degrade to zero, and a panicking blocking task
    /// still yields a shaped report with a transient cause — so there is no
    /// error path to map. `Ok` is the honest return, not a swallowed failure.
    async fn diagnose(&self) -> Result<Diagnosis, MemoryError> {
        let report = tinymemory_core::tree::health::async_run_doctor(&self.config).await;
        Ok(Diagnosis {
            healthy: report.healthy,
            stages: report
                .stages
                .into_iter()
                .map(|stage| DiagnosisStage {
                    stage: stage.stage,
                    ok: stage.ok,
                    failure: stage.failure.as_ref().map(diagnosis_failure),
                    note: stage.note,
                })
                .collect(),
            first_blocking_cause: report.first_blocking_cause.as_ref().map(diagnosis_failure),
            degraded: DegradedCapabilities {
                semantic_recall: report.degraded.semantic_recall,
                structure: report.degraded.structure,
                storage: report.degraded.storage,
                cause: report.degraded.cause.as_ref().map(diagnosis_failure),
            },
            counters: DiagnosisCounters {
                total_chunks: report.counters.total_chunks,
                jobs_ready: report.counters.jobs_ready,
                jobs_running: report.counters.jobs_running,
                jobs_failed: report.counters.jobs_failed,
                extraction_coverage: report.counters.extraction_coverage,
            },
        })
    }
}

/// Carry one engine pipeline failure across as the contract's own shape.
///
/// The codes and classes cross as **strings**, and the strings are the
/// engine's own `as_str` rather than anything invented here: the frontend
/// resolves `remediation_key` to localised text and compares `code` for
/// equality, so a re-spelling on this side would silently stop matching the
/// keys that already exist. `class` is always `Some` because this engine always
/// derives one from the code; the contract keeps it optional for an engine that
/// classifies a cause without deciding a retry policy for it.
fn diagnosis_failure(failure: &tinymemory_core::tree::health::PipelineFailure) -> DiagnosisFailure {
    DiagnosisFailure {
        code: failure.code.as_str().to_string(),
        class: Some(failure.class.as_str().to_string()),
        remediation_key: failure.remediation_key.clone(),
        detail: failure.detail.clone(),
    }
}

#[async_trait]
impl MemoryProvider for TinycortexProvider {
    fn driver_id(&self) -> &str {
        &self.driver_id
    }
    fn capabilities(&self) -> Capabilities {
        advertised_capabilities()
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
        // Reachable only when the git-backed snapshot store is compiled in;
        // see the `memory-git` feature in this crate's manifest.
        #[cfg(feature = "memory-git")]
        {
            Some(self)
        }
        #[cfg(not(feature = "memory-git"))]
        {
            None
        }
    }
    fn as_sources(&self) -> Option<&dyn MemorySourceSink> {
        Some(self)
    }
    fn as_maintenance(&self) -> Option<&dyn MemoryMaintenance> {
        Some(self)
    }
    fn as_people(&self) -> Option<&dyn MemoryPeople> {
        Some(self)
    }
    fn as_chunks(&self) -> Option<&dyn MemoryChunks> {
        Some(self)
    }
    fn as_retrieval(&self) -> Option<&dyn MemoryRetrieval> {
        Some(self)
    }
    fn as_profile(&self) -> Option<&dyn MemoryProfile> {
        Some(self)
    }
    fn as_episodic(&self) -> Option<&dyn MemoryEpisodic> {
        Some(self)
    }
    fn as_source_sync(&self) -> Option<&dyn MemorySourceSync> {
        Some(self)
    }
    fn as_coding_sessions(&self) -> Option<&dyn MemoryCodingSessions> {
        Some(self)
    }
}

// ── Source sync ──────────────────────────────────────────────────────────────
//
// Everything below delegates to `tinymemory_core`'s sync layer, which is where
// the pipelines, the cursor store and the audit log already live. Nothing here
// re-implements a fetch; this file's whole job is to say the same things in the
// contract's vocabulary.
//
// The conversions destructure rather than round-trip through `Self::cross`, for
// the reason the People section below gives at length: a serde round-trip
// agrees only while the field *names* agree on both sides, and it fails at
// runtime rather than at compile time when they stop.

/// Toolkits with no native pipeline are refused before anything is dispatched.
///
/// The pipeline builder refuses them too, but as a `PipelineFailure` carrying a
/// message — and by the time it does, this adapter can no longer tell "you
/// asked for a provider that does not exist" apart from "the provider failed".
/// The contract promises [`MemoryError::Invalid`] for the first, and on a call
/// that costs money the difference decides whether a caller retries.
fn ensure_syncable_toolkit(toolkit: &str) -> Result<(), MemoryError> {
    if tinymemory_core::sync::pipelines::host::is_composio_toolkit_syncable(toolkit) {
        return Ok(());
    }
    Err(MemoryError::Invalid(format!(
        "memory sync has no pipeline for toolkit '{toolkit}'"
    )))
}

/// Carry one audit row across as the contract's own shape.
///
/// Field-for-field, and the field names are identical on both sides because
/// the contract copied the driver's on-disk format deliberately — see
/// `tinymemory_bus::provider::sync::SyncAuditEntry`. The `estimated_cost_usd`
/// this carries was priced when the row was written, which is why
/// `estimate_sync_cost_usd` has to answer from the same constants rather than
/// letting a caller re-derive it.
fn audit_entry(entry: tinymemory_core::sync::audit::SyncAuditEntry) -> SyncAuditEntry {
    let tinymemory_core::sync::audit::SyncAuditEntry {
        timestamp,
        source_id,
        source_kind,
        scope,
        items_fetched,
        batches,
        input_tokens,
        output_tokens,
        estimated_cost_usd,
        composio_actions_called,
        composio_cost_usd,
        actual_charged_usd,
        duration_ms,
        success,
        error,
    } = entry;
    SyncAuditEntry {
        timestamp,
        source_id,
        source_kind,
        scope,
        items_fetched,
        batches,
        input_tokens,
        output_tokens,
        estimated_cost_usd,
        composio_actions_called,
        composio_cost_usd,
        actual_charged_usd,
        duration_ms,
        success,
        error,
    }
}

/// How many audit rows one read may return when the caller names no limit.
///
/// The log is append-only for the life of a workspace, so "all of it" is not a
/// bound. A thousand rows is far more than any surface renders and still fits a
/// frame with room to spare; a caller wanting a longer history asks for it and
/// is refused by the module's size check rather than by silence.
const DEFAULT_AUDIT_ROWS: usize = 1_000;

/// The ceiling a caller's own `limit` is clamped to.
const MAX_AUDIT_ROWS: usize = 10_000;

#[async_trait]
impl MemorySourceSync for TinycortexProvider {
    async fn run_connection_sync(
        &self,
        toolkit: &str,
        connection_id: &str,
    ) -> Result<SyncRunOutcome, MemoryError> {
        ensure_syncable_toolkit(toolkit)?;
        // The registry lookup, the per-source budgets and the pipeline dispatch
        // all happen inside this call — which is why the contract carries no
        // budget arguments: they are already recorded against the source.
        let outcome = tinymemory_core::tinycortex::run_composio_connection(
            toolkit,
            connection_id,
            &self.config,
        )
        .await
        .map_err(|failure| {
            // The usage travels in the message rather than being dropped: a run
            // that failed after calling four provider actions and spending real
            // money has to say so somewhere, and `IngestOutcome`-style partial
            // success is not available on an error path.
            MemoryError::Other(anyhow::anyhow!(
                "sync {toolkit} connection: {} (actions_called={}, provider_cost_usd={})",
                failure.message,
                failure.actions_called,
                failure.provider_cost_usd
            ))
        })?;
        Ok(SyncRunOutcome {
            records_ingested: outcome.records_ingested,
            more_pending: outcome.more_pending,
            actions_called: outcome.actions_called,
            provider_cost_usd: outcome.provider_cost_usd,
            note: outcome.note,
        })
    }

    async fn source_sync_state(
        &self,
        toolkit: &str,
        connection_id: &str,
    ) -> Result<Option<SourceSyncState>, MemoryError> {
        use tinymemory_core::sync::composio::providers::sync_state::{SyncState, STATE_NAMESPACE};

        // Read the row rather than calling `SyncState::load`, which materialises
        // a fresh default when nothing is persisted. That default is right for a
        // *run* — it is the state a first sync starts from — and wrong here: the
        // contract distinguishes "never synced" from "synced and holding no
        // cursor", and `load` cannot.
        //
        // The namespace and the key come from the engine's own constant and its
        // own `key`, so this read cannot address a different row than the writes
        // do; a literal here would be a second spelling of a durable key.
        let key = SyncState::key(toolkit, connection_id);
        let stored = self
            .client
            .kv_get(Some(STATE_NAMESPACE), &key)
            .await
            .map_err(|error| Self::other("read composio sync state", error))?;
        let Some(stored) = stored else {
            return Ok(None);
        };
        let state: SyncState = serde_json::from_value(stored)
            .map_err(|error| Self::other("decode composio sync state", error))?;

        // `remaining()` applies the engine's own day-rollover rule, so a budget
        // last written yesterday reads as fully available today. Deriving the
        // used count from it rather than from `requests_used` keeps that rule in
        // one place — a second date comparison here would show yesterday's spend
        // as today's the moment the two disagreed about what a day is.
        let limit = state.daily_budget.limit;
        let used = limit.saturating_sub(state.daily_budget.remaining());
        Ok(Some(SourceSyncState {
            toolkit: state.toolkit,
            connection_id: state.connection_id,
            cursor: state.cursor,
            synced_item_count: u64::try_from(state.synced_ids.len()).unwrap_or(u64::MAX),
            last_seen_id: state.last_seen_id,
            last_sync_at_ms: state.last_sync_at_ms,
            daily_requests_used: used,
            daily_request_limit: limit,
        }))
    }

    async fn sync_audit_log(
        &self,
        limit: Option<usize>,
    ) -> Result<Vec<SyncAuditEntry>, MemoryError> {
        // Clamped rather than trusted: `limit` reaches this from an RPC
        // argument, and the log grows without bound, so an unclamped read is a
        // response size a caller chooses.
        let take = limit.unwrap_or(DEFAULT_AUDIT_ROWS).min(MAX_AUDIT_ROWS);
        let entries = blocking(
            self.config.clone(),
            "read the sync audit log",
            move |config| {
                // Already newest-first: the reader reverses the append-only file.
                Ok(tinymemory_core::tinycortex::read_audit_log(config))
            },
        )
        .await?;
        Ok(entries.into_iter().take(take).map(audit_entry).collect())
    }

    async fn estimate_sync_cost_usd(
        &self,
        input_tokens: u64,
        output_tokens: u64,
    ) -> Result<f64, MemoryError> {
        // The one place these constants are read from outside the audit writer.
        // Pure arithmetic, so no blocking hop and no failure path.
        Ok(tinymemory_core::tinycortex::estimate_cost_usd(
            input_tokens,
            output_tokens,
        ))
    }

    async fn sync_statuses(&self) -> Result<Vec<SourceSyncStatus>, MemoryError> {
        let statuses = blocking(self.config.clone(), "list sync statuses", move |config| {
            // `engine_config` is the engine's `MemoryConfig` built from the host
            // config this driver already holds. It is built *here*, inside the
            // driver, which is the whole point: the caller used to build it and
            // pass it in, which meant the caller had to name an engine type.
            let engine = tinymemory_core::tinycortex::engine_config(config);
            tinycortex::memory::sync::list_sync_statuses(&engine)
        })
        .await?;
        Ok(statuses
            .into_iter()
            .map(|status| SourceSyncStatus {
                provider: status.provider,
                chunks_synced: status.chunks_synced,
                chunks_pending: status.chunks_pending,
                batch_total: status.batch_total,
                batch_processed: status.batch_processed,
                last_chunk_at_ms: status.last_chunk_at_ms,
                freshness: match status.freshness {
                    tinycortex::memory::sync::FreshnessLabel::Active => SyncFreshness::Active,
                    tinycortex::memory::sync::FreshnessLabel::Recent => SyncFreshness::Recent,
                    tinycortex::memory::sync::FreshnessLabel::Idle => SyncFreshness::Idle,
                },
            })
            .collect())
    }

    async fn raw_archive_coverage(
        &self,
        tree_scope: &str,
        archive_source_id: &str,
    ) -> Result<RawArchiveCoverage, MemoryError> {
        let tree_scope = tree_scope.to_string();
        let archive_source_id = archive_source_id.to_string();
        let coverage = blocking(
            self.config.clone(),
            "scan raw archive coverage",
            move |config| {
                tinymemory_core::tinycortex::raw_coverage(config, &tree_scope, &archive_source_id)
            },
        )
        .await?;
        // The engine's scan carries each pending file's absolute path inside
        // this driver's content vault. Only the count crosses — a path describes
        // storage layout no caller may depend on, and the repair takes the same
        // scope rather than a file list, so nothing downstream can use one.
        Ok(RawArchiveCoverage {
            total: u64::try_from(coverage.total).unwrap_or(u64::MAX),
            covered: u64::try_from(coverage.covered).unwrap_or(u64::MAX),
            pending: u64::try_from(coverage.pending.len()).unwrap_or(u64::MAX),
        })
    }

    async fn rebuild_from_raw_archive(
        &self,
        tree_scope: &str,
        archive_source_id: &str,
    ) -> Result<RawRebuildOutcome, MemoryError> {
        // Async rather than `blocking`: the rebuild summarises through the host
        // summariser, so it awaits inference between batches.
        let outcome = tinymemory_core::tinycortex::rebuild_tree_from_raw(
            &self.config,
            tree_scope,
            archive_source_id,
        )
        .await
        .map_err(|error| Self::other("rebuild the tree from its raw archive", error))?;
        Ok(RawRebuildOutcome {
            files_read: u64::try_from(outcome.files_read).unwrap_or(u64::MAX),
            batches: u64::try_from(outcome.batches).unwrap_or(u64::MAX),
            input_tokens: outcome.input_tokens,
            output_tokens: outcome.output_tokens,
            estimated_cost_usd: outcome.estimated_cost_usd,
            actual_charged_usd: outcome.actual_charged_usd,
        })
    }
}

// ── Coding sessions ──────────────────────────────────────────────────────────

#[async_trait]
impl MemoryCodingSessions for TinycortexProvider {
    async fn coding_session_status(&self) -> Result<Vec<CodingSessionSource>, MemoryError> {
        // A bounded `walkdir` over two session roots plus a parse of each file:
        // synchronous filesystem work, so it hops off the executor like every
        // other synchronous read here. It takes no config — the roots come from
        // the environment, driver-side, which is why no path appears in the
        // contract.
        let statuses =
            tokio::task::spawn_blocking(tinymemory_core::tinycortex::coding_session_status)
                .await
                .map_err(|error| Self::other("scan coding sessions", error))?;
        Ok(statuses
            .into_iter()
            .map(|status| CodingSessionSource {
                kind: status.kind,
                available: status.available,
                session_files: status.session_files,
                evidence_units: status.evidence_units,
                invalid_files: status.invalid_files,
                scan_truncated: status.scan_truncated,
            })
            .collect())
    }

    async fn ingest_coding_sessions(
        &self,
        request: CodingSessionIngestRequest,
    ) -> Result<CodingSessionIngestReport, MemoryError> {
        let config = self.config.clone();
        // The persona pipeline holds borrowed path state across its awaits and
        // is therefore **not** `Send`, while this trait's future must be. So the
        // future is built and driven inside a blocking worker, on the ambient
        // runtime, and only the finished report crosses back. Dropping the
        // `spawn_blocking` and awaiting directly does not compile; replacing it
        // with a second runtime would give the pipeline a different reactor from
        // the one its inference calls are registered on.
        let handle = tokio::runtime::Handle::current();
        let engine_request = tinymemory_core::tinycortex::CodingSessionIngestRequest {
            backfill: request.backfill,
            // Clamped driver-side, as the contract says: `max_sessions` reaches
            // here from an RPC argument, and each session is one or more
            // sequential model calls.
            max_sessions: request.max_sessions,
        };
        let response = tokio::task::spawn_blocking(move || {
            handle.block_on(tinymemory_core::tinycortex::ingest_coding_sessions(
                &config,
                engine_request,
            ))
        })
        .await
        .map_err(|error| Self::other("join coding-session ingestion", error))?
        .map_err(|error| Self::other("ingest coding sessions", error))?;
        Ok(CodingSessionIngestReport {
            mode: response.mode,
            files_seen: response.files_seen,
            sessions_processed: response.sessions_processed,
            sessions_skipped: response.sessions_skipped,
            sessions_failed: response.sessions_failed,
            evidence_units: response.evidence_units,
            observations: response.observations,
            budget_hit: response.budget_hit,
            pack_path: response.pack_path,
        })
    }
}

// ── People ───────────────────────────────────────────────────────────────────
//
// The conversions below destructure both sides exhaustively rather than
// round-tripping through `Self::cross`. That is deliberate. `cross` is a serde
// value round-trip, so it agrees only while the two crates' field *names* agree
// — and they already do not: the engine's `Interaction` names its timestamp
// `ts` where the contract names it `at`. A round-trip would compile and then
// fail at runtime on the first call.
//
// Destructuring makes the opposite trade: a field added or renamed on either
// side is a compile error here, which is the same rule
// `tinymemory-tinycortex::convert` follows and the same reasoning that governs
// the two copies of the contract itself.

/// The engine's people store for this module's workspace.
///
/// `for_workspace` caches per workspace directory, so this is a map lookup
/// after the first call rather than a database open.
fn people_store(
    workspace: &std::path::Path,
) -> Result<Arc<tinycortex::memory::people::store::PeopleStore>, MemoryError> {
    tinycortex::memory::people::store::for_workspace(workspace)
        .map_err(|error| MemoryError::Other(anyhow::anyhow!("open people store: {error}")))
}

fn handle_to_engine(handle: &PersonHandle) -> tinycortex::memory::people::types::Handle {
    use tinycortex::memory::people::types::Handle as EngineHandle;
    match handle {
        PersonHandle::IMessage(value) => EngineHandle::IMessage(value.clone()),
        PersonHandle::Email(value) => EngineHandle::Email(value.clone()),
        PersonHandle::DisplayName(value) => EngineHandle::DisplayName(value.clone()),
    }
}

fn handle_to_contract(handle: tinycortex::memory::people::types::Handle) -> PersonHandle {
    use tinycortex::memory::people::types::Handle as EngineHandle;
    match handle {
        EngineHandle::IMessage(value) => PersonHandle::IMessage(value),
        EngineHandle::Email(value) => PersonHandle::Email(value),
        EngineHandle::DisplayName(value) => PersonHandle::DisplayName(value),
    }
}

fn person_to_contract(person: tinycortex::memory::people::types::Person) -> PersonRecord {
    let tinycortex::memory::people::types::Person {
        id,
        display_name,
        primary_email,
        primary_phone,
        handles,
        created_at,
        updated_at,
    } = person;
    PersonRecord {
        id: id.to_string(),
        display_name,
        primary_email,
        primary_phone,
        handles: handles.into_iter().map(handle_to_contract).collect(),
        created_at: created_at.to_rfc3339(),
        updated_at: updated_at.to_rfc3339(),
    }
}

fn score_to_contract(
    score: tinycortex::memory::people::types::ScoreComponents,
    interaction_count: usize,
) -> PersonScore {
    let tinycortex::memory::people::types::ScoreComponents {
        recency,
        frequency,
        reciprocity,
        depth,
        score,
    } = score;
    PersonScore {
        recency,
        frequency,
        reciprocity,
        depth,
        score,
        interaction_count,
    }
}

/// Parse a caller-supplied person id.
///
/// `PersonRef` is opaque to the caller by contract, so an unparseable one is a
/// caller mistake — `Invalid`, not `NotFound`. Reporting `NotFound` would tell
/// a caller the id was well-formed but absent, which would send them looking
/// for a deleted person rather than at the id they built.
fn parse_person_id(
    person_id: &str,
) -> Result<tinycortex::memory::people::types::PersonId, MemoryError> {
    person_id
        .parse::<uuid::Uuid>()
        .map(tinycortex::memory::people::types::PersonId)
        .map_err(|_| MemoryError::Invalid(format!("malformed person id: {person_id}")))
}

#[async_trait]
impl MemoryPeople for TinycortexProvider {
    async fn list_people(&self, limit: Option<usize>) -> Result<Vec<RankedPerson>, MemoryError> {
        let store = people_store(&self.config.workspace_dir)?;
        let people = store
            .list()
            .await
            .map_err(|error| Self::other("list people", error))?;

        let ids: Vec<_> = people.iter().map(|person| person.id).collect();
        let interactions = store
            .batch_interactions_for(&ids)
            .await
            .map_err(|error| Self::other("load interactions", error))?;

        let now = Utc::now();
        let mut ranked: Vec<RankedPerson> = people
            .into_iter()
            .map(|person| {
                let observed = interactions.get(&person.id).map_or(&[][..], Vec::as_slice);
                let closeness = tinycortex::memory::people::scorer::score(observed, now);
                RankedPerson {
                    person: person_to_contract(person),
                    score: score_to_contract(closeness, observed.len()),
                }
            })
            .collect();

        // Descending by composite score. `total_cmp` rather than `partial_cmp`:
        // a NaN from a degenerate score would make `partial_cmp` return `None`,
        // and an ordering that is not total is undefined behaviour's
        // well-behaved cousin — `sort_by` may panic or produce garbage order.
        ranked.sort_by(|a, b| b.score.score.total_cmp(&a.score.score));
        if let Some(limit) = limit {
            ranked.truncate(limit);
        }
        Ok(ranked)
    }

    async fn get_person(&self, person_id: &str) -> Result<Option<PersonRecord>, MemoryError> {
        let store = people_store(&self.config.workspace_dir)?;
        let id = parse_person_id(person_id)?;
        Ok(store
            .get(id)
            .await
            .map_err(|error| Self::other("get person", error))?
            .map(person_to_contract))
    }

    async fn resolve_handle(
        &self,
        handle: &PersonHandle,
        create_if_missing: bool,
    ) -> Result<Option<ResolvedPerson>, MemoryError> {
        let store = people_store(&self.config.workspace_dir)?;
        let resolver = tinycortex::memory::people::resolver::HandleResolver::new(&store);
        let engine_handle = handle_to_engine(handle);

        if create_if_missing {
            let (id, created) = resolver
                .resolve_or_create_with_status(&engine_handle)
                .await
                .map_err(|error| Self::other("resolve or create handle", error))?;
            return Ok(Some(ResolvedPerson {
                id: id.to_string(),
                created,
            }));
        }

        Ok(resolver
            .resolve(&engine_handle)
            .await
            .map_err(|error| Self::other("resolve handle", error))?
            .map(|id| ResolvedPerson {
                id: id.to_string(),
                created: false,
            }))
    }

    async fn add_handle_alias(
        &self,
        person_id: &str,
        handle: &PersonHandle,
    ) -> Result<(), MemoryError> {
        let store = people_store(&self.config.workspace_dir)?;
        let id = parse_person_id(person_id)?;
        if store
            .get(id)
            .await
            .map_err(|error| Self::other("look up person", error))?
            .is_none()
        {
            return Err(MemoryError::NotFound(format!("person {person_id}")));
        }
        store
            .add_alias(id, handle_to_engine(handle).canonicalize())
            .await
            .map_err(|error| Self::other("add handle alias", error))
    }

    async fn score_person(&self, person_id: &str) -> Result<Option<PersonScore>, MemoryError> {
        let store = people_store(&self.config.workspace_dir)?;
        let id = parse_person_id(person_id)?;
        if store
            .get(id)
            .await
            .map_err(|error| Self::other("look up person", error))?
            .is_none()
        {
            return Ok(None);
        }
        let interactions = store
            .interactions_for(id)
            .await
            .map_err(|error| Self::other("load interactions", error))?;
        Ok(Some(score_to_contract(
            tinycortex::memory::people::scorer::score(&interactions, Utc::now()),
            interactions.len(),
        )))
    }

    async fn record_interaction(&self, interaction: &PersonInteraction) -> Result<(), MemoryError> {
        let store = people_store(&self.config.workspace_dir)?;
        let PersonInteraction {
            person_id,
            at,
            is_outbound,
            length,
        } = interaction;
        let id = parse_person_id(person_id)?;
        let ts = chrono::DateTime::parse_from_rfc3339(at)
            .map_err(|error| MemoryError::Invalid(format!("malformed interaction time: {error}")))?
            .with_timezone(&Utc);
        if store
            .get(id)
            .await
            .map_err(|error| Self::other("look up person", error))?
            .is_none()
        {
            return Err(MemoryError::NotFound(format!("person {person_id}")));
        }
        store
            .record_interaction(tinycortex::memory::people::types::Interaction {
                person_id: id,
                ts,
                is_outbound: *is_outbound,
                length: *length,
            })
            .await
            .map_err(|error| Self::other("record interaction", error))
    }

    async fn seed_from_address_book(&self) -> Result<AddressBookSeedOutcome, MemoryError> {
        let store = people_store(&self.config.workspace_dir)?;
        let resolver = tinycortex::memory::people::resolver::HandleResolver::new(&store);
        let source = tinycortex::memory::people::address_book::SystemContactsSource;
        let (seeded, skipped) = resolver
            .seed_from_address_book(&source)
            .await
            .map_err(|error| Self::other("seed from address book", error))?;
        Ok(AddressBookSeedOutcome { seeded, skipped })
    }
}

// ── Chunks and Retrieval ─────────────────────────────────────────────────────
//
// Both families take the source scope as an **argument** and never read the
// ambient one. `tinymemory_core`'s in-process entry points resolve it from a
// task-local, which the host sets on its own side of the bus — it is simply not
// present in this process. Reading it here would yield `None`, and `None` means
// *unrestricted*, so a per-profile source gate would fail open. That is why the
// `*_scoped` variants exist and why these call them.

/// Convert a contract scope into the engine's allowlist form.
fn scope_to_engine(scope: Option<&SourceScope>) -> Option<HashSet<String>> {
    scope.map(|scope| scope.allow.iter().cloned().collect())
}

/// Convert a contract chunk query into the engine's.
///
/// Shared by `list_chunks`, `count_chunks` and `list_chunk_details` so all
/// three ask the engine the same question. Two copies of this conversion would
/// compile identically today and drift the first time a filter is added to one
/// of them — and the symptom of that drift is a total that no amount of paging
/// can reach.
///
/// The page bounds are carried across unchanged; dropping them for the count
/// is the engine's job, because that is where the `LIMIT` is appended.
///
/// The destructure is exhaustive on purpose. A field added to [`ChunkQuery`]
/// and forgotten here does not fail — it silently widens every query that used
/// it, which is a wrong answer rather than an error, and the caller has no way
/// to tell. Binding every field by name makes that a build failure instead.
///
/// The plural filters carry their empty form through as *no filter*, matching
/// the engine, and that is the deliberate opposite of `source_scope`, which
/// denies when empty. A scope is a gate and fails closed; these are
/// narrowings, and a narrowing that failed closed on an empty list would
/// silently blank a page a caller assembled from nothing.
fn chunk_query_to_engine(
    query: &ChunkQuery,
    scope: Option<&SourceScope>,
) -> Result<tinymemory_core::store::chunks::ListChunksQuery, MemoryError> {
    let ChunkQuery {
        ids,
        source_kind,
        source_kinds,
        source_id,
        source_ids,
        owner,
        entity_ids,
        entity_kinds,
        content_contains,
        since_ms,
        until_ms,
        limit,
        offset,
        exclude_dropped,
    } = query.clone();
    Ok(tinymemory_core::store::chunks::ListChunksQuery {
        ids,
        source_kind: source_kind
            .map(|kind| TinycortexProvider::cross(&kind, "convert source kind"))
            .transpose()?,
        source_kinds: source_kinds
            .iter()
            .map(|kind| TinycortexProvider::cross(kind, "convert source kind"))
            .collect::<Result<Vec<_>, MemoryError>>()?,
        source_id,
        source_ids,
        owner,
        entity_ids,
        entity_kinds,
        content_contains,
        since_ms,
        until_ms,
        limit,
        offset,
        source_scope: scope_to_engine(scope),
        exclude_dropped,
    })
}

#[async_trait]
impl MemoryChunks for TinycortexProvider {
    async fn list_chunks(
        &self,
        query: &ChunkQuery,
        scope: Option<&SourceScope>,
    ) -> Result<Vec<Chunk>, MemoryError> {
        let engine_query = chunk_query_to_engine(query, scope)?;
        let chunks = blocking(self.config.clone(), "list chunks", move |config| {
            tinymemory_core::store::chunks::list_chunks(config, &engine_query)
        })
        .await?;
        Self::cross(&chunks, "convert chunks")
    }

    async fn count_chunks(
        &self,
        query: &ChunkQuery,
        scope: Option<&SourceScope>,
    ) -> Result<u64, MemoryError> {
        // Same conversion as the listing, then the engine's counting sibling,
        // which builds its `WHERE` clause from the listing's own and simply
        // never appends the `LIMIT`. The page bounds therefore travel and are
        // ignored, rather than being cleared here where a later filter could
        // be cleared with them by accident.
        let engine_query = chunk_query_to_engine(query, scope)?;
        blocking(self.config.clone(), "count chunks", move |config| {
            tinymemory_core::store::chunks::count_chunks_matching(config, &engine_query)
        })
        .await
    }

    async fn list_chunk_details(
        &self,
        query: &ChunkQuery,
        scope: Option<&SourceScope>,
    ) -> Result<Vec<ChunkListRow>, MemoryError> {
        // The same conversion the page and the total use, so a caller that
        // renders "20 of 431" out of `count_chunks` and fills the table from
        // here is looking at one predicate rather than three that agree today.
        let engine_query = chunk_query_to_engine(query, scope)?;
        let rows = blocking(self.config.clone(), "list chunk details", move |config| {
            tinymemory_core::store::chunks::list_chunk_details(config, &engine_query)
        })
        .await?;
        // The engine's row is field-for-field the contract's, body included —
        // which is to say body-excluded: neither type carries one, for the
        // reason `ChunkListRow`'s own docs give. Crossed rather than moved
        // because a host that resolves the contract crate twice has two
        // `Chunk` types with one shape, the hazard every other conversion here
        // is written against.
        rows.into_iter()
            .map(|row| {
                Ok(ChunkListRow {
                    chunk: Self::cross(&row.chunk, "convert chunk")?,
                    content_path: row.content_path,
                    lifecycle_status: row.lifecycle_status,
                    has_embedding: row.has_embedding,
                })
            })
            .collect()
    }

    async fn source_totals(
        &self,
        limit: usize,
        scope: Option<&SourceScope>,
    ) -> Result<Vec<SourceTotal>, MemoryError> {
        let allowed = scope_to_engine(scope);
        let totals = blocking(self.config.clone(), "read source totals", move |config| {
            // The engine takes `Option<usize>` so an internal caller can ask
            // for its default page; the contract does not offer that spelling,
            // and passing the caller's number through unchanged keeps the
            // clamp in one place — the engine's, which is also the one
            // `ChunkQuery::limit` is clamped by.
            tinymemory_core::store::chunks::source_totals(config, Some(limit), allowed.as_ref())
        })
        .await?;
        totals
            .into_iter()
            .map(|total| {
                Ok(SourceTotal {
                    source_kind: Self::cross(&total.source_kind, "convert source kind")?,
                    source_id: total.source_id,
                    chunk_count: total.chunk_count,
                    most_recent_ms: total.last_timestamp_ms,
                })
            })
            .collect()
    }

    async fn get_chunk(&self, chunk_id: &str) -> Result<Option<Chunk>, MemoryError> {
        let id = chunk_id.to_string();
        let chunk = blocking(self.config.clone(), "get chunk", move |config| {
            tinymemory_core::store::chunks::get_chunk(config, &id)
        })
        .await?;
        match chunk {
            Some(chunk) => Ok(Some(Self::cross(&chunk, "convert chunk")?)),
            None => Ok(None),
        }
    }

    async fn chunk_detail(&self, chunk_id: &str) -> Result<Option<ChunkDetail>, MemoryError> {
        let id = chunk_id.to_string();
        let detail = blocking(self.config.clone(), "chunk detail", move |config| {
            let Some(chunk) = tinymemory_core::store::chunks::get_chunk(config, &id)? else {
                return Ok(None);
            };
            // The vault read is best-effort: a missing body is reported as
            // `None` so the caller can fall back to the row's own content,
            // rather than failing the whole detail view over a preview.
            let body = tinymemory_core::store::content::read::read_chunk_body(config, &id).ok();
            let has_embedding =
                tinymemory_core::store::chunks::get_chunk_embedding(config, &id)?.is_some();
            let lifecycle_status =
                tinymemory_core::store::chunks::get_chunk_lifecycle_status(config, &id)?;
            let content_path = tinymemory_core::store::chunks::get_chunk_content_path(config, &id)?;
            Ok(Some((
                chunk,
                body,
                has_embedding,
                lifecycle_status,
                content_path,
            )))
        })
        .await?;

        let Some((chunk, body, has_embedding, lifecycle_status, content_path)) = detail else {
            return Ok(None);
        };
        Ok(Some(ChunkDetail {
            chunk: Self::cross(&chunk, "convert chunk")?,
            body,
            content_path,
            lifecycle_status,
            has_embedding,
        }))
    }

    async fn storage_kinds(&self) -> Result<Vec<String>, MemoryError> {
        Ok(tinymemory_core::store::MemoryKind::ALL
            .iter()
            .map(|kind| kind.as_str().to_string())
            .collect())
    }

    async fn chunk_embeddings(
        &self,
        chunk_ids: &[String],
        model_signature: &str,
    ) -> Result<Vec<ChunkEmbedding>, MemoryError> {
        let ids = chunk_ids.to_vec();
        let signature = model_signature.to_string();
        let vectors = blocking(
            self.config.clone(),
            "load chunk embeddings",
            move |config| {
                tinymemory_core::store::chunks::get_chunk_embeddings_for_signature_batch(
                    config, &ids, &signature,
                )
            },
        )
        .await?;
        // Sorted so the response is deterministic: the engine returns a
        // `HashMap`, whose iteration order varies per process and would make an
        // otherwise-identical call return a differently-ordered list.
        let mut embeddings: Vec<ChunkEmbedding> = vectors
            .into_iter()
            .map(|(chunk_id, vector)| ChunkEmbedding { chunk_id, vector })
            .collect();
        embeddings.sort_by(|a, b| a.chunk_id.cmp(&b.chunk_id));
        Ok(embeddings)
    }
}

#[async_trait]
impl MemoryRetrieval for TinycortexProvider {
    async fn fast_retrieve(
        &self,
        query: &str,
        options: FastRetrieveQuery,
        scope: Option<&SourceScope>,
    ) -> Result<RetrievalResponse, MemoryError> {
        if query.trim().is_empty() {
            return Err(MemoryError::Invalid("query must not be empty".to_string()));
        }
        let engine_options = tinymemory_core::tree::retrieval::FastRetrieveOptions {
            limit: options.limit,
            max_hops: options.max_hops,
            time_window_days: options.time_window_days,
        };
        let response = tinymemory_core::tree::retrieval::fast_retrieve_scoped(
            &self.config,
            query,
            engine_options,
            scope_to_engine(scope),
        )
        .await
        .map_err(|error| Self::other("fast retrieve", error))?;
        Self::cross(&response, "convert retrieval response")
    }

    async fn cover_window(
        &self,
        window: &CoverWindowQuery,
        scope: Option<&SourceScope>,
    ) -> Result<RetrievalResponse, MemoryError> {
        let CoverWindowQuery {
            since_ms,
            until_ms,
            source_id,
            source_kind,
            limit,
        } = window.clone();
        let engine_kind = source_kind
            .map(|kind| Self::cross(&kind, "convert source kind"))
            .transpose()?;
        let response = tinymemory_core::tree::retrieval::cover_window_scoped(
            &self.config,
            since_ms,
            until_ms,
            source_id.as_deref(),
            engine_kind,
            // 0 is the engine's "no caller preference" sentinel, not a request
            // for zero rows: `cover_window_scoped` substitutes its own
            // DEFAULT_LIMIT for it. Mapping `None` to 0 therefore asks for the
            // default, which is what an absent limit means.
            limit.unwrap_or(0),
            scope_to_engine(scope),
        )
        .await
        .map_err(|error| Self::other("cover window", error))?;
        Self::cross(&response, "convert retrieval response")
    }

    async fn retrieve_source(
        &self,
        query: &SourceRetrievalQuery,
        scope: Option<&SourceScope>,
    ) -> Result<RetrievalResponse, MemoryError> {
        let SourceRetrievalQuery {
            source_id,
            source_kind,
            time_window_days,
            query: text,
            limit,
        } = query.clone();
        let engine_kind = source_kind
            .map(|kind| Self::cross(&kind, "convert source kind"))
            .transpose()?;
        let response = tinymemory_core::tree::retrieval::source::query_source_scoped(
            &self.config,
            tinymemory_core::tree::retrieval::source::SourceQuery {
                source_id: source_id.as_deref(),
                source_kind: engine_kind,
                time_window_days,
                query: text.as_deref(),
                limit,
            },
            scope_to_engine(scope),
        )
        .await
        .map_err(|error| Self::other("retrieve source", error))?;
        Self::cross(&response, "convert retrieval response")
    }

    async fn retrieve_children(
        &self,
        node_id: &str,
        max_depth: u32,
        query: Option<&str>,
        limit: Option<usize>,
        scope: Option<&SourceScope>,
    ) -> Result<Vec<RetrievalHit>, MemoryError> {
        let hits = tinymemory_core::tree::retrieval::drill_down::drill_down_scoped(
            &self.config,
            node_id,
            max_depth,
            query,
            limit,
            scope_to_engine(scope),
        )
        .await
        .map_err(|error| Self::other("drill down", error))?;
        Self::cross(&hits, "convert retrieval hits")
    }

    async fn retrieve_leaves(
        &self,
        chunk_ids: &[String],
        scope: Option<&SourceScope>,
    ) -> Result<Vec<RetrievalHit>, MemoryError> {
        let hits = tinymemory_core::tree::retrieval::fetch::fetch_leaves_scoped(
            &self.config,
            chunk_ids,
            scope_to_engine(scope),
        )
        .await
        .map_err(|error| Self::other("fetch leaves", error))?;
        Self::cross(&hits, "convert retrieval hits")
    }

    async fn recall_namespace_scored(
        &self,
        namespace: &str,
        query: &str,
        limit: usize,
        exclude_session_id: Option<&str>,
    ) -> Result<Vec<NamespaceMemoryHit>, MemoryError> {
        let hits = self
            .client
            .unified_handle()
            .query_namespace_hits_excluding_session(
                namespace,
                query,
                u32::try_from(limit).unwrap_or(u32::MAX),
                exclude_session_id,
            )
            .await
            .map_err(|error| Self::other("recall namespace scored", error))?;
        Self::cross(&hits, "convert namespace hits")
    }

    async fn recall_namespace_recent(
        &self,
        namespace: &str,
        limit: usize,
    ) -> Result<Vec<NamespaceMemoryHit>, MemoryError> {
        // `recall_namespace_memories`, deliberately — NOT
        // `query_namespace_hits_excluding_session` with an empty query. The two
        // share `load_documents_for_scope` + `kv_records_for_scope` and diverge
        // after it: one ranks against the query, this one scores freshness and
        // priority. Passing "" to the scored path does not degrade to recency.
        let hits = self
            .client
            .unified_handle()
            .recall_namespace_memories(namespace, u32::try_from(limit).unwrap_or(u32::MAX))
            .await
            .map_err(|error| Self::other("recall namespace recent", error))?;
        Self::cross(&hits, "convert namespace hits")
    }

    async fn search_entities(
        &self,
        query: &str,
        kinds: Option<&[String]>,
        limit: usize,
    ) -> Result<Vec<EntityMatch>, MemoryError> {
        // Request kinds are validated, unlike response kinds which pass through
        // as an open vocabulary. An unknown filter that silently matched nothing
        // would be indistinguishable from a genuine empty result.
        let engine_kinds = match kinds {
            Some(kinds) => Some(
                kinds
                    .iter()
                    .map(|kind| {
                        tinymemory_core::tree::score::extract::EntityKind::parse(kind).map_err(
                            |_| MemoryError::Invalid(format!("unknown entity kind: {kind}")),
                        )
                    })
                    .collect::<Result<Vec<_>, MemoryError>>()?,
            ),
            None => None,
        };
        let matches = tinymemory_core::tree::retrieval::search_entities(
            &self.config,
            query,
            engine_kinds,
            limit,
        )
        .await
        .map_err(|error| Self::other("search entities", error))?;
        Self::cross(&matches, "convert entity matches")
    }
}

// ── Profile ──────────────────────────────────────────────────────────────────
//
// `ProfileStore`'s methods are synchronous and hold a `parking_lot::Mutex`
// across a SQLite call, so each one goes through `spawn_blocking` rather than
// being awaited on the runtime thread. The store is cheap to obtain — it is a
// handle over the client's connection, not an open — so it is fetched inside
// the blocking closure rather than held across an await.

fn event_kind_to_engine(kind: EventKind) -> tinymemory_core::store::events::EventType {
    use tinymemory_core::store::events::EventType as Engine;
    match kind {
        EventKind::Fact => Engine::Fact,
        EventKind::Decision => Engine::Decision,
        EventKind::Commitment => Engine::Commitment,
        EventKind::Preference => Engine::Preference,
        EventKind::Question => Engine::Question,
        EventKind::Foresight => Engine::Foresight,
    }
}

fn facet_type_to_engine(
    facet_type: FacetType,
) -> tinymemory_core::store::namespace_store::profile::FacetType {
    use tinymemory_core::store::namespace_store::profile::FacetType as Engine;
    match facet_type {
        FacetType::Preference => Engine::Preference,
        FacetType::Workflow => Engine::Workflow,
        FacetType::Role => Engine::Role,
        FacetType::Personality => Engine::Personality,
        FacetType::Context => Engine::Context,
    }
}

#[async_trait]
impl MemoryProfile for TinycortexProvider {
    async fn list_active_facets(&self) -> Result<Vec<ProfileFacet>, MemoryError> {
        let client = Arc::clone(&self.client);
        let facets = tokio::task::spawn_blocking(move || client.profile_store().list_active())
            .await
            .map_err(|e| Self::other("join list_active_facets", e))?
            .map_err(|e| Self::other("list_active_facets", e))?;
        Self::cross(&facets, "convert facets")
    }

    async fn list_all_facets(&self) -> Result<Vec<ProfileFacet>, MemoryError> {
        let client = Arc::clone(&self.client);
        let facets = tokio::task::spawn_blocking(move || client.profile_store().list_all())
            .await
            .map_err(|e| Self::other("join list_all_facets", e))?
            .map_err(|e| Self::other("list_all_facets", e))?;
        Self::cross(&facets, "convert facets")
    }

    async fn get_facet(&self, key: &str) -> Result<Option<ProfileFacet>, MemoryError> {
        let client = Arc::clone(&self.client);
        let key = key.to_string();
        let facet = tokio::task::spawn_blocking(move || client.profile_store().get(&key))
            .await
            .map_err(|e| Self::other("join get_facet", e))?
            .map_err(|e| Self::other("get_facet", e))?;
        match facet {
            Some(facet) => Ok(Some(Self::cross(&facet, "convert facet")?)),
            None => Ok(None),
        }
    }

    async fn facets_by_type(
        &self,
        facet_type: FacetType,
    ) -> Result<Vec<ProfileFacet>, MemoryError> {
        let client = Arc::clone(&self.client);
        let engine = facet_type_to_engine(facet_type);
        let facets =
            tokio::task::spawn_blocking(move || client.profile_store().facets_by_type(&engine))
                .await
                .map_err(|e| Self::other("join facets_by_type", e))?
                .map_err(|e| Self::other("facets_by_type", e))?;
        Self::cross(&facets, "convert facets")
    }

    async fn upsert_facet(&self, facet: &ProfileFacet) -> Result<(), MemoryError> {
        let client = Arc::clone(&self.client);
        let engine: tinymemory_core::store::namespace_store::profile::ProfileFacet =
            Self::cross(facet, "convert facet")?;
        tokio::task::spawn_blocking(move || client.profile_store().upsert_full(&engine))
            .await
            .map_err(|e| Self::other("join upsert_facet", e))?
            .map_err(|e| Self::other("upsert_facet", e))
    }

    async fn upsert_provider_facet(
        &self,
        facet_id: &str,
        facet_type: FacetType,
        key: &str,
        value: &str,
        confidence: f64,
        segment_id: Option<&str>,
        observed_at: f64,
    ) -> Result<(), MemoryError> {
        let client = Arc::clone(&self.client);
        let engine = facet_type_to_engine(facet_type);
        let (facet_id, key, value) = (facet_id.to_string(), key.to_string(), value.to_string());
        let segment_id = segment_id.map(str::to_string);
        tokio::task::spawn_blocking(move || {
            client.profile_store().upsert_provider_facet(
                &facet_id,
                &engine,
                &key,
                &value,
                confidence,
                segment_id.as_deref(),
                observed_at,
            )
        })
        .await
        .map_err(|e| Self::other("join upsert_provider_facet", e))?
        .map_err(|e| Self::other("upsert_provider_facet", e))
    }

    async fn set_facet_user_state(
        &self,
        key: &str,
        user_state: UserState,
    ) -> Result<bool, MemoryError> {
        use tinymemory_core::store::namespace_store::profile::UserState as Engine;
        let client = Arc::clone(&self.client);
        let key = key.to_string();
        let engine = match user_state {
            UserState::Auto => Engine::Auto,
            UserState::Pinned => Engine::Pinned,
            UserState::Forgotten => Engine::Forgotten,
        };
        tokio::task::spawn_blocking(move || client.profile_store().set_user_state(&key, engine))
            .await
            .map_err(|e| Self::other("join set_facet_user_state", e))?
            .map_err(|e| Self::other("set_facet_user_state", e))
    }

    async fn delete_facet(&self, key: &str) -> Result<bool, MemoryError> {
        let client = Arc::clone(&self.client);
        let key = key.to_string();
        tokio::task::spawn_blocking(move || client.profile_store().delete(&key))
            .await
            .map_err(|e| Self::other("join delete_facet", e))?
            .map_err(|e| Self::other("delete_facet", e))
    }

    async fn delete_facet_by_id(&self, facet_id: &str) -> Result<bool, MemoryError> {
        let client = Arc::clone(&self.client);
        let facet_id = facet_id.to_string();
        tokio::task::spawn_blocking(move || client.profile_store().delete_by_facet_id(&facet_id))
            .await
            .map_err(|e| Self::other("join delete_facet_by_id", e))?
            .map_err(|e| Self::other("delete_facet_by_id", e))
    }

    async fn drop_facets_below(&self, threshold: f64) -> Result<usize, MemoryError> {
        let client = Arc::clone(&self.client);
        tokio::task::spawn_blocking(move || client.profile_store().drop_below_threshold(threshold))
            .await
            .map_err(|e| Self::other("join drop_facets_below", e))?
            .map_err(|e| Self::other("drop_facets_below", e))
    }

    async fn workflow_identity_matches(&self, key_pattern: &str, canonical_value: &str) -> bool {
        let client = Arc::clone(&self.client);
        let (pattern, value) = (key_pattern.to_string(), canonical_value.to_string());
        tokio::task::spawn_blocking(move || {
            client
                .profile_store()
                .skill_identity_matches(&pattern, &value)
        })
        .await
        // A join failure reads as "no", like every other error on this
        // predicate — see the trait docs. But it is logged first: the two
        // cases behind it are a cancelled task and a panic inside
        // `skill_identity_matches`, and a panic is a defect. Answering a bare
        // `false` would make that defect look exactly like a legitimate
        // non-match, which is the one reading that guarantees nobody
        // investigates it.
        .inspect_err(|error| {
            log::error!(
                "[tinymemory:module] workflow_identity_matches join failed, answering false: \
                 {error}"
            );
        })
        .unwrap_or(false)
    }
}

/// Episodic capture: the turn-by-turn record and its segment lifecycle.
///
/// Every method hops to `spawn_blocking` for the same reason the profile family
/// does — these are synchronous `rusqlite` calls behind a `parking_lot::Mutex`,
/// and blocking a tinybus executor thread on a database lock would stall every
/// other call the module is serving.
///
/// The boundary-detection and summary-composition halves of the archivist are
/// **not** here: they touch no database and are host policy. See the family's
/// contract docs.
#[async_trait]
impl MemoryEpisodic for TinycortexProvider {
    async fn insert_turn(&self, turn: &EpisodicTurn) -> Result<i64, MemoryError> {
        let conn = self.client.profile_conn();
        let entry = tinymemory_core::store::fts5::EpisodicEntry {
            id: None,
            session_id: turn.session_id.clone(),
            timestamp: turn.timestamp,
            role: turn.role.clone(),
            content: turn.content.clone(),
            lesson: turn.lesson.clone(),
            tool_calls_json: turn.tool_calls_json.clone(),
            // The contract carries this signed because a cost is a plain number
            // on the wire; the engine column is unsigned. A negative value is
            // not meaningful, so it clamps rather than wrapping.
            cost_microdollars: u64::try_from(turn.cost_microdollars).unwrap_or(0),
        };
        tokio::task::spawn_blocking(move || {
            tinymemory_core::store::fts5::episodic_insert(&conn, &entry)
        })
        .await
        .map_err(|e| Self::other("join insert_turn", e))?
        .map_err(|e| Self::other("insert_turn", e))
    }

    async fn session_turns(&self, session_id: &str) -> Result<Vec<EpisodicTurn>, MemoryError> {
        let conn = self.client.profile_conn();
        let session_id = session_id.to_string();
        let entries = tokio::task::spawn_blocking(move || {
            tinymemory_core::store::fts5::episodic_session_entries(&conn, &session_id)
        })
        .await
        .map_err(|e| Self::other("join session_turns", e))?
        .map_err(|e| Self::other("session_turns", e))?;
        Ok(entries.into_iter().map(episodic_to_contract).collect())
    }

    async fn open_segment(
        &self,
        session_id: &str,
    ) -> Result<Option<ConversationSegment>, MemoryError> {
        let conn = self.client.profile_conn();
        let session_id = session_id.to_string();
        let segment = tokio::task::spawn_blocking(move || {
            tinymemory_core::store::segments::open_segment_for_session(&conn, &session_id)
        })
        .await
        .map_err(|e| Self::other("join open_segment", e))?
        .map_err(|e| Self::other("open_segment", e))?;
        Ok(segment.map(segment_to_contract))
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "trait signature; see the contract's rationale"
    )]
    async fn create_segment(
        &self,
        segment_id: &str,
        session_id: &str,
        namespace: &str,
        start_episodic_id: i64,
        start_seq: Option<u32>,
        start_timestamp: f64,
        now: f64,
    ) -> Result<(), MemoryError> {
        let conn = self.client.profile_conn();
        let (segment_id, session_id, namespace) = (
            segment_id.to_string(),
            session_id.to_string(),
            namespace.to_string(),
        );
        tokio::task::spawn_blocking(move || {
            tinymemory_core::store::segments::segment_create(
                &conn,
                &segment_id,
                &session_id,
                &namespace,
                start_episodic_id,
                start_seq,
                start_timestamp,
                now,
            )
        })
        .await
        .map_err(|e| Self::other("join create_segment", e))?
        .map_err(|e| Self::other("create_segment", e))
    }

    async fn append_turn(
        &self,
        segment_id: &str,
        episodic_id: i64,
        seq: Option<u32>,
        timestamp: f64,
        now: f64,
    ) -> Result<(), MemoryError> {
        let conn = self.client.profile_conn();
        let segment_id = segment_id.to_string();
        tokio::task::spawn_blocking(move || {
            tinymemory_core::store::segments::segment_append_turn(
                &conn,
                &segment_id,
                episodic_id,
                seq,
                timestamp,
                now,
            )
        })
        .await
        .map_err(|e| Self::other("join append_turn", e))?
        .map_err(|e| Self::other("append_turn", e))
    }

    async fn close_segment(&self, segment_id: &str, now: f64) -> Result<(), MemoryError> {
        let conn = self.client.profile_conn();
        let segment_id = segment_id.to_string();
        tokio::task::spawn_blocking(move || {
            tinymemory_core::store::segments::segment_close(&conn, &segment_id, now)
        })
        .await
        .map_err(|e| Self::other("join close_segment", e))?
        .map_err(|e| Self::other("close_segment", e))
    }

    async fn set_segment_summary(
        &self,
        segment_id: &str,
        summary: &str,
        now: f64,
    ) -> Result<(), MemoryError> {
        let conn = self.client.profile_conn();
        let (segment_id, summary) = (segment_id.to_string(), summary.to_string());
        tokio::task::spawn_blocking(move || {
            tinymemory_core::store::segments::segment_set_summary(&conn, &segment_id, &summary, now)
        })
        .await
        .map_err(|e| Self::other("join set_segment_summary", e))?
        .map_err(|e| Self::other("set_segment_summary", e))
    }

    async fn insert_event(&self, event: &EpisodicEvent) -> Result<(), MemoryError> {
        let conn = self.client.profile_conn();
        let record = tinymemory_core::store::events::EventRecord {
            event_id: event.event_id.clone(),
            segment_id: event.segment_id.clone(),
            session_id: event.session_id.clone(),
            namespace: event.namespace.clone(),
            event_type: event_kind_to_engine(event.kind),
            content: event.content.clone(),
            subject: event.subject.clone(),
            timestamp_ref: event.timestamp_ref.clone(),
            confidence: event.confidence,
            embedding: event.embedding.clone(),
            source_turn_ids: event.source_turn_ids.clone(),
            created_at: event.created_at,
        };
        tokio::task::spawn_blocking(move || {
            tinymemory_core::store::events::event_insert(&conn, &record)
        })
        .await
        .map_err(|error| Self::other("insert event", error))?
        .map_err(|error| Self::other("insert event", error))
    }

    async fn upsert_segment_embedding(
        &self,
        segment_id: &str,
        model_signature: &str,
        embedding: &[f32],
        created_at: f64,
    ) -> Result<(), MemoryError> {
        let conn = self.client.profile_conn();
        let (segment_id, model_signature) = (segment_id.to_string(), model_signature.to_string());
        let embedding = embedding.to_vec();
        tokio::task::spawn_blocking(move || {
            tinymemory_core::store::segments::segment_embedding_upsert(
                &conn,
                &segment_id,
                &model_signature,
                &embedding,
                created_at,
            )
        })
        .await
        .map_err(|e| Self::other("join upsert_segment_embedding", e))?
        .map_err(|e| Self::other("upsert_segment_embedding", e))
    }
}

/// Engine episodic row -> contract turn.
fn episodic_to_contract(entry: tinymemory_core::store::fts5::EpisodicEntry) -> EpisodicTurn {
    EpisodicTurn {
        id: entry.id,
        session_id: entry.session_id,
        timestamp: entry.timestamp,
        role: entry.role,
        content: entry.content,
        lesson: entry.lesson,
        tool_calls_json: entry.tool_calls_json,
        cost_microdollars: i64::try_from(entry.cost_microdollars).unwrap_or(i64::MAX),
    }
}

/// Engine segment row -> contract segment.
///
/// Written out rather than derived: the engine row carries fields the contract
/// deliberately does not expose (`topic_keywords`, `created_at`), and a
/// blanket conversion would quietly start shipping them if the contract ever
/// grew a matching name.
///
/// The seq pair used to be on that withheld list; it is contract vocabulary
/// now, because segment selection prefers it — the md-backed archivist store
/// rounds timestamps to milliseconds, and the sequence is the identity that
/// survives the rounding.
fn segment_to_contract(
    segment: tinymemory_core::store::segments::ConversationSegment,
) -> ConversationSegment {
    use tinymemory_core::store::segments::SegmentStatus;
    ConversationSegment {
        segment_id: segment.segment_id,
        session_id: segment.session_id,
        namespace: segment.namespace,
        start_episodic_id: segment.start_episodic_id,
        end_episodic_id: segment.end_episodic_id,
        start_timestamp: segment.start_timestamp,
        end_timestamp: segment.end_timestamp,
        turn_count: segment.turn_count,
        summary: segment.summary,
        embedding: segment.embedding,
        open: matches!(segment.status, SegmentStatus::Open),
        start_seq: segment.start_seq,
        end_seq: segment.end_seq,
    }
}

#[cfg(test)]
mod test;
