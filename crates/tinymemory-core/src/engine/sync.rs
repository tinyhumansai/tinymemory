//! OpenHuman service adapters for tinycortex live synchronization.

use async_trait::async_trait;
use std::sync::Arc;
use tinycortex::memory::sync::{
    ExternalSourceReader, GithubRepoSyncPipeline, LocalDocument, LocalDocumentSink, SkillDocSink,
    SkillDocument, SyncContext, SyncDispatcher, SyncEvent, SyncEventSink, SyncOutcome,
    SyncPipeline, SyncStage, SyncStateStore, WorkspaceSourcePipeline,
};

use crate::sources::{MemorySourceEntry, SourceKind};
use crate::store::MemoryClientRef;
use crate::Config;

/// The KV namespace Composio sync state is persisted under.
///
/// Re-exported from the engine rather than re-declared. It was a second
/// `const` holding the same literal as
/// `tinycortex::memory::sync::state::STATE_NAMESPACE`, so the host and the
/// engine agreed only by coincidence of the string: change either and the two
/// would silently read and write *different* namespaces, stranding every
/// persisted sync cursor with no error anywhere. A duplicated literal is a
/// drift hazard precisely when the thing it names is durable (#18 §B2).
pub use tinycortex::memory::sync::state::STATE_NAMESPACE as HOST_SYNC_STATE_NAMESPACE;
pub use tinycortex::memory::sync::{RawCoverage, RawFileRef, RealCostAccumulator, RebuildOutcome};

pub struct HostSyncAdapter {
    memory: MemoryClientRef,
    config: Option<Arc<Config>>,
}

#[derive(Debug)]
pub struct SourcePipelineFailure {
    pub message: String,
    pub actions_called: u32,
    pub provider_cost_usd: f64,
}

impl std::fmt::Display for SourcePipelineFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl SourcePipelineFailure {
    fn without_usage(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            actions_called: 0,
            provider_cost_usd: 0.0,
        }
    }
}

impl HostSyncAdapter {
    pub fn new(memory: MemoryClientRef) -> Self {
        Self {
            memory,
            config: None,
        }
    }

    fn with_config(memory: MemoryClientRef, config: Arc<Config>) -> Self {
        Self {
            memory,
            config: Some(config),
        }
    }

    /// Reconnect a synced Composio document to the memory tree (#5473).
    ///
    /// The TinyCortex migration (#4794) dropped the per-provider tree-ingest
    /// half of the connector sync: synced items reached the `skill-<toolkit>`
    /// document store but never `mem_tree_chunks`, so connector memories fell
    /// out of tree-backed recall. This routes each synced item through the
    /// engine's document ingest — the same L0-chunk path local folder sources
    /// use via [`LocalDocumentSink`] — additively alongside the skill store.
    ///
    /// Scope naming matches the tree retrieval contract: the tree scope
    /// (`path_scope`) is `"{toolkit}:{connection_id}"` so `query_source` resolves
    /// it by platform prefix (`gmail:` → email, `slack:` → chat, …), while the
    /// per-item `source_id` carries the document id so each message admits
    /// independently rather than colliding on one dedup key.
    ///
    /// `ingest_document` writes the L0 chunk rows synchronously and enqueues the
    /// summary seal on the async extract worker. Retrieval (`query_source`) reads
    /// sealed summaries, so an item becomes retrievable once its buffer seals —
    /// on the token threshold or the time-based `flush_stale_buffers` — and the
    /// seal degrades to a fallback summary when no LLM is available.
    async fn ingest_document_into_memory_tree(
        &self,
        config: &Config,
        document: &SkillDocument,
    ) -> anyhow::Result<()> {
        let toolkit = document.toolkit.trim().to_ascii_lowercase();
        let connection_id = document.connection_id.trim();
        // A blank toolkit/connection would yield a scope with no platform prefix
        // (`":conn"`), which no retrieval kind matches; skip rather than write an
        // unreachable tree. The skill store still holds the item.
        if toolkit.is_empty() || connection_id.is_empty() {
            tracing::debug!(
                document_id = %document.document_id,
                "[tinycortex:sync] skipping memory-tree ingest: item has no toolkit/connection scope"
            );
            return Ok(());
        }
        let tree_scope = format!("{toolkit}:{connection_id}");
        let source_id = format!("{tree_scope}:{}", document.document_id);
        let owner = format!("{toolkit}-sync:{connection_id}");
        let input = tinycortex::memory::ingest::canonicalize::document::DocumentInput {
            provider: format!("composio:{toolkit}"),
            title: document.title.clone(),
            body: document.content.clone(),
            modified_at: chrono::Utc::now(),
            source_ref: Some(document.document_id.clone()),
        };
        crate::ingest_pipeline::ingest_document_with_scope(
            config,
            &source_id,
            &owner,
            vec![toolkit],
            input,
            Some(tree_scope),
        )
        .await
        .map(|_| ())
        .map_err(|error| {
            anyhow::anyhow!("memory-tree ingest failed for source `{source_id}`: {error}")
        })
    }
}

/// Read persisted sync audit records for best-effort RPC and reporting surfaces.
///
/// Backed by `crate::sync::audit` — the host-owned log — not the engine;
/// this stays in the engine module only because OpenHuman reaches it through
/// the engine shim path.
pub fn read_audit_log(config: &Config) -> Vec<crate::sync::audit::SyncAuditEntry> {
    crate::sync::audit::read_audit_log(config.workspace_dir()).unwrap_or_default()
}

/// Estimate sync inference cost using TinyCortex's canonical pricing model.
/// Delegates to the host-owned pricing (#18 §B1); kept because OpenHuman
/// reaches it through the engine shim path.
pub fn estimate_cost_usd(input_tokens: u64, output_tokens: u64) -> f64 {
    crate::sync::audit::estimate_cost_usd(input_tokens, output_tokens)
}

/// Measure coverage of a raw archive by its TinyCortex memory tree.
pub fn raw_coverage(
    config: &Config,
    tree_scope: &str,
    archive_source_id: &str,
) -> anyhow::Result<RawCoverage> {
    tracing::debug!("[tinycortex:sync] raw coverage scan starting");
    let memory_config = super::memory_config_from(config, config.workspace_dir().clone());
    let coverage =
        tinycortex::memory::sync::raw_coverage(&memory_config, tree_scope, archive_source_id)
            .map_err(|error| {
                tracing::warn!(%error, "[tinycortex:sync] raw coverage scan failed");
                error
            })?;
    tracing::debug!(
        total = coverage.total,
        covered = coverage.covered,
        pending = coverage.pending.len(),
        "[tinycortex:sync] raw coverage scan completed"
    );
    Ok(coverage)
}

/// Return whether a raw archive contains records absent from its memory tree.
pub fn needs_rebuild(config: &Config, tree_scope: &str, archive_source_id: &str) -> bool {
    let memory_config = super::memory_config_from(config, config.workspace_dir().clone());
    let required =
        tinycortex::memory::sync::needs_rebuild(&memory_config, tree_scope, archive_source_id);
    tracing::debug!(
        required,
        "[tinycortex:sync] raw rebuild requirement evaluated"
    );
    required
}

/// Rebuild a memory tree from its raw archive through the host summarizer.
pub async fn rebuild_tree_from_raw(
    config: &Config,
    tree_scope: &str,
    archive_source_id: &str,
) -> anyhow::Result<RebuildOutcome> {
    tracing::info!("[tinycortex:sync] raw rebuild starting");
    let memory_config = super::memory_config_from(config, config.workspace_dir().clone());
    let summariser = super::HostSummariser::new(config.to_arc());
    let outcome = tinycortex::memory::sync::rebuild_tree_from_raw(
        &memory_config,
        tree_scope,
        archive_source_id,
        &summariser,
    )
    .await
    .map_err(|error| {
        tracing::warn!(%error, "[tinycortex:sync] raw rebuild failed");
        error
    })?;
    tracing::info!(
        files_read = outcome.files_read,
        batches = outcome.batches,
        "[tinycortex:sync] raw rebuild completed"
    );
    Ok(outcome)
}

/// Run a registered GitHub repository source through TinyCortex synchronization.
pub async fn run_github_sync(
    source: &MemorySourceEntry,
    config: &Config,
) -> anyhow::Result<SyncOutcome> {
    tracing::info!("[tinycortex:sync] GitHub repository sync starting");
    if crate::global::client_if_ready().is_none() {
        tracing::debug!("[tinycortex:sync] GitHub sync initializing memory client");
        crate::global::init(config.workspace_dir().clone())
            .map_err(anyhow::Error::msg)
            .map_err(|error| {
                tracing::warn!(%error, "[tinycortex:sync] GitHub sync memory initialization failed");
                error
            })?;
    }
    let outcome = run_source_pipeline(source, config)
        .await
        .map_err(|error| anyhow::anyhow!(error.to_string()))
        .map_err(|error| {
            tracing::warn!(%error, "[tinycortex:sync] GitHub repository sync failed");
            error
        })?;
    tracing::info!(
        records_ingested = outcome.records_ingested,
        more_pending = outcome.more_pending,
        actions_called = outcome.actions_called,
        "[tinycortex:sync] GitHub repository sync completed"
    );
    Ok(outcome)
}

#[async_trait]
impl ExternalSourceReader for HostSyncAdapter {
    async fn list_items(
        &self,
        source: &tinycortex::memory::sources::MemorySourceEntry,
    ) -> anyhow::Result<Vec<tinycortex::memory::sources::SourceItem>> {
        let config = self
            .config
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("external source reader requires host config"))?;
        let host_source: MemorySourceEntry = serde_json::from_value(serde_json::to_value(source)?)?;
        let reader = crate::sources::readers::reader_for(&host_source.kind);
        let items = reader
            .list_items(&host_source, &**config)
            .await
            .map_err(anyhow::Error::msg)?;
        serde_json::from_value(serde_json::to_value(items)?).map_err(Into::into)
    }

    async fn read_item(
        &self,
        source: &tinycortex::memory::sources::MemorySourceEntry,
        item_id: &str,
    ) -> anyhow::Result<tinycortex::memory::sources::SourceContent> {
        let config = self
            .config
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("external source reader requires host config"))?;
        let host_source: MemorySourceEntry = serde_json::from_value(serde_json::to_value(source)?)?;
        let reader = crate::sources::readers::reader_for(&host_source.kind);
        let content = reader
            .read_item(&host_source, item_id, &**config)
            .await
            .map_err(anyhow::Error::msg)?;
        serde_json::from_value(serde_json::to_value(content)?).map_err(Into::into)
    }
}

pub fn sync_context(memory: MemoryClientRef) -> SyncContext {
    let adapter = std::sync::Arc::new(HostSyncAdapter::new(memory));
    SyncContext {
        events: adapter.clone(),
        documents: adapter.clone(),
        state: adapter,
        local_documents: None,
        external_sources: None,
        summariser: None,
    }
}

fn source_sync_context(memory: MemoryClientRef, config: &Config, local: bool) -> SyncContext {
    let adapter = std::sync::Arc::new(HostSyncAdapter::with_config(memory, config.to_arc()));
    SyncContext {
        events: adapter.clone(),
        documents: adapter.clone(),
        state: adapter.clone(),
        local_documents: local.then(|| adapter.clone() as std::sync::Arc<dyn LocalDocumentSink>),
        external_sources: local.then_some(adapter as std::sync::Arc<dyn ExternalSourceReader>),
        summariser: local.then(|| {
            std::sync::Arc::new(super::HostSummariser::new(config.to_arc()))
                as std::sync::Arc<dyn tinycortex::memory::tree::Summariser>
        }),
    }
}

pub async fn run_source_pipeline(
    source: &MemorySourceEntry,
    config: &Config,
) -> Result<SyncOutcome, SourcePipelineFailure> {
    // Composio sources run on the engine-free pipelines (#18 §B1); this seam
    // keeps only the tree-coupled kinds (folder/repo/rss/web — they summarise
    // into the engine tree by design) and converts at the boundary for its
    // OpenHuman-facing callers.
    if source.kind == SourceKind::Composio {
        let toolkit = source
            .toolkit
            .as_deref()
            .map(str::trim)
            .filter(|toolkit| !toolkit.is_empty())
            .ok_or_else(|| SourcePipelineFailure::without_usage("composio source missing toolkit"))?
            .to_ascii_lowercase();
        let connection_id = source
            .connection_id
            .as_deref()
            .map(str::trim)
            .filter(|connection_id| !connection_id.is_empty())
            .ok_or_else(|| {
                SourcePipelineFailure::without_usage("composio source missing connection_id")
            })?;
        let outcome = crate::sync::pipelines::host::run_composio_connection_with_caps(
            &toolkit,
            connection_id,
            config,
            crate::sync::pipelines::host::SourceCaps::from_source(source),
        )
        .await
        .map_err(|failure| SourcePipelineFailure {
            message: failure.message,
            actions_called: failure.actions_called,
            provider_cost_usd: failure.provider_cost_usd,
        })?;
        return Ok(SyncOutcome {
            records_ingested: outcome.records_ingested,
            more_pending: outcome.more_pending,
            actions_called: outcome.actions_called,
            provider_cost_usd: outcome.provider_cost_usd,
            note: outcome.note,
        });
    }

    let memory = crate::global::client_if_ready()
        .ok_or_else(|| SourcePipelineFailure::without_usage("memory client is not ready"))?;
    let mut memory_config = super::memory_config_from(config, config.workspace_dir().clone());
    memory_config.sync.interval_secs = config.memory_sync_interval_secs();
    memory_config.sync.budget.max_items = source.max_items;
    memory_config.sync.budget.max_tokens_per_sync = source.max_tokens_per_sync;
    memory_config.sync.budget.max_cost_per_sync_usd = source.max_cost_per_sync_usd;
    memory_config.sync.budget.sync_depth_days = source.sync_depth_days;

    let pipeline = build_pipeline(source, config, &mut memory_config)
        .map_err(SourcePipelineFailure::without_usage)?;
    let pipeline_id = pipeline.id().to_owned();
    let mut dispatcher = SyncDispatcher::new();
    dispatcher
        .register(pipeline)
        .map_err(|error| SourcePipelineFailure::without_usage(error.to_string()))?;
    dispatcher
        .tick(
            &pipeline_id,
            &memory_config,
            &source_sync_context(memory, config, source.kind != SourceKind::Composio),
        )
        .await
        .map_err(|error| {
            let usage = error.downcast_ref::<tinycortex::memory::sync::SyncRunError>();
            SourcePipelineFailure {
                message: error.to_string(),
                actions_called: usage.map_or(0, |error| error.actions_called),
                provider_cost_usd: usage.map_or(0.0, |error| error.provider_cost_usd),
            }
        })
}

/// Run a Composio connection through tinycortex, preserving any source-level
/// budgets already configured in OpenHuman's registry.
pub async fn run_composio_connection(
    toolkit: &str,
    connection_id: &str,
    config: &Config,
) -> Result<SyncOutcome, SourcePipelineFailure> {
    run_composio_connection_with_budgets(toolkit, connection_id, config, None, None).await
}

/// Run a Composio connection with request-scoped budget overrides.
///
/// Provider RPCs carry these values in `ProviderContext`, before a source has
/// necessarily been persisted in the registry. Explicit values therefore take
/// precedence, while `None` preserves the registered/default source budget.
pub async fn run_composio_connection_with_budgets(
    toolkit: &str,
    connection_id: &str,
    config: &Config,
    max_items: Option<u32>,
    sync_depth_days: Option<u32>,
) -> Result<SyncOutcome, SourcePipelineFailure> {
    let mut source = crate::sources::decode_memory_sources(config)
        .iter()
        .find(|source| {
            source.kind == SourceKind::Composio
                && source.connection_id.as_deref() == Some(connection_id)
        })
        .cloned()
        .unwrap_or_else(|| {
            let (max_items, sync_depth_days) =
                crate::sources::memory_sync_defaults_for_toolkit(toolkit);
            MemorySourceEntry {
                id: format!("composio:{toolkit}:{connection_id}"),
                kind: SourceKind::Composio,
                label: format!("{toolkit} connection"),
                enabled: true,
                toolkit: Some(toolkit.to_ascii_lowercase()),
                connection_id: Some(connection_id.to_string()),
                path: None,
                glob: None,
                url: None,
                branch: None,
                paths: Vec::new(),
                max_commits: None,
                max_issues: None,
                max_prs: None,
                query: None,
                since_days: None,
                max_items,
                selector: None,
                max_tokens_per_sync: None,
                max_cost_per_sync_usd: None,
                sync_depth_days,
            }
        });

    source.max_items = max_items;
    source.sync_depth_days = sync_depth_days;

    tracing::debug!(
        toolkit,
        connection_id,
        source_id = %source.id,
        max_items = ?source.max_items,
        sync_depth_days = ?source.sync_depth_days,
        "[tinycortex:sync] dispatching Composio connection"
    );
    run_source_pipeline(&source, config).await
}

/// Load the persisted Composio sync state, in core's own vocabulary.
///
/// Was typed with the engine's `SyncState`; the copies share one serde shape
/// and one KV namespace (pinned by tests in
/// `sync::composio::providers::sync_state`), so the retype changes no bytes.
/// Kept in the engine module only because OpenHuman reaches it through the
/// engine shim path.
pub async fn load_composio_sync_state(
    toolkit: &str,
    connection_id: &str,
) -> anyhow::Result<crate::sync::composio::providers::sync_state::SyncState> {
    let memory = crate::global::client_if_ready()
        .ok_or_else(|| anyhow::anyhow!("memory client is not ready"))?;
    let host = crate::sync::pipelines::host::PipelineHost::without_tree_ingest(memory);
    crate::sync::composio::providers::sync_state::SyncState::load(&host, toolkit, connection_id)
        .await
}

pub async fn run_slack_search_backfill(
    connection_id: &str,
    backfill_days: i64,
    config: &Config,
) -> Result<SyncOutcome, SourcePipelineFailure> {
    // Delegates to the engine-free pipelines (#18 §B1); kept here because
    // OpenHuman reaches this function through the engine shim path.
    let outcome = crate::sync::pipelines::host::run_slack_search_backfill(
        connection_id,
        backfill_days,
        config,
    )
    .await
    .map_err(|failure| SourcePipelineFailure {
        message: failure.message,
        actions_called: failure.actions_called,
        provider_cost_usd: failure.provider_cost_usd,
    })?;
    Ok(SyncOutcome {
        records_ingested: outcome.records_ingested,
        more_pending: outcome.more_pending,
        actions_called: outcome.actions_called,
        provider_cost_usd: outcome.provider_cost_usd,
        note: outcome.note,
    })
}

/// Delegates to the engine-free pipelines (#18 §B1); kept because OpenHuman's
/// backfill binary reaches it through the engine shim path.
pub async fn run_gmail_backfill(
    connection_id: &str,
    query: &str,
    max_pages: usize,
    page_size: usize,
    config: &Config,
) -> Result<SyncOutcome, SourcePipelineFailure> {
    let outcome = crate::sync::pipelines::host::run_gmail_backfill(
        connection_id,
        query,
        max_pages,
        page_size,
        config,
    )
    .await
    .map_err(|failure| SourcePipelineFailure {
        message: failure.message,
        actions_called: failure.actions_called,
        provider_cost_usd: failure.provider_cost_usd,
    })?;
    Ok(SyncOutcome {
        records_ingested: outcome.records_ingested,
        more_pending: outcome.more_pending,
        actions_called: outcome.actions_called,
        provider_cost_usd: outcome.provider_cost_usd,
        note: outcome.note,
    })
}

fn build_pipeline(
    source: &MemorySourceEntry,
    _config: &Config,
    _memory_config: &mut tinycortex::memory::config::MemoryConfig,
) -> Result<std::sync::Arc<dyn SyncPipeline>, String> {
    if source.kind != SourceKind::Composio {
        let crate_source: tinycortex::memory::sources::MemorySourceEntry = serde_json::from_value(
            serde_json::to_value(source).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        if source.kind == SourceKind::GithubRepo {
            return GithubRepoSyncPipeline::new(crate_source)
                .map(|pipeline| std::sync::Arc::new(pipeline) as std::sync::Arc<dyn SyncPipeline>)
                .map_err(|error| error.to_string());
        }
        return WorkspaceSourcePipeline::new(crate_source)
            .map(|pipeline| std::sync::Arc::new(pipeline) as std::sync::Arc<dyn SyncPipeline>)
            .map_err(|error| error.to_string());
    }

    // Composio sources never reach this seam: `run_source_pipeline` routes
    // them to `crate::sync::pipelines` (#18 §B1) before building. Only the
    // tree-coupled kinds are built here.
    Err(format!(
        "engine seam does not build composio pipelines (kind {:?} unexpected here)",
        source.kind
    ))
}

#[async_trait]
impl SkillDocSink for HostSyncAdapter {
    async fn store(&self, document: SkillDocument) -> anyhow::Result<()> {
        tracing::debug!(
            toolkit = %document.toolkit,
            connection_id = %document.connection_id,
            document_id = %document.document_id,
            "[tinycortex:sync] storing synchronized document"
        );
        self.memory
            .store_skill_sync(
                &document.namespace_skill_id,
                &document.connection_id,
                &document.title,
                &document.content,
                Some("tinycortex-sync".into()),
                Some(document.metadata.clone()),
                Some("medium".into()),
                None,
                None,
                Some(document.document_id.clone()),
            )
            .await
            .map_err(anyhow::Error::msg)?;

        // #5473: additively reconnect the synced item to the memory tree. This
        // is a best-effort secondary index over the skill store, which is the
        // source of truth and has already committed above. A failure here must
        // NOT abort the connector sync: most providers do not tolerate scope
        // errors, so the orchestrator turns a `store` error into a run-aborting
        // `Err` — propagating would let one deterministically-poisonous item
        // stall the whole connection and re-fetch the page (Composio spend) on
        // every retry. Log and continue; the per-item source gate re-attempts
        // the item on a later sync, and an operator rebuild can backfill.
        // The config-less adapter (`sync_context`) has no ingest pipeline and is
        // not on the connector sync path, so it skips tree ingest entirely.
        if let Some(config) = self.config.as_deref() {
            if let Err(error) = self
                .ingest_document_into_memory_tree(config, &document)
                .await
            {
                tracing::warn!(
                    toolkit = %document.toolkit,
                    connection_id = %document.connection_id,
                    document_id = %document.document_id,
                    %error,
                    "[tinycortex:sync] memory-tree ingest failed; skill store retained"
                );
            }
        }
        Ok(())
    }

    async fn delete(&self, namespace_skill_id: &str, document_id: &str) -> anyhow::Result<()> {
        let namespace = format!("skill-{}", namespace_skill_id.trim());
        tracing::debug!(
            namespace,
            document_id,
            "[tinycortex:sync] deleting synchronized document"
        );
        self.memory
            .delete_document(&namespace, document_id)
            .await
            .map(|_| ())
            .map_err(anyhow::Error::msg)
    }
}

#[async_trait]
impl LocalDocumentSink for HostSyncAdapter {
    async fn upsert(&self, document: LocalDocument) -> anyhow::Result<()> {
        let config = self
            .config
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("local document sink missing host config"))?;
        let input = tinycortex::memory::ingest::canonicalize::document::DocumentInput {
            provider: "memory_sources:local".into(),
            title: document.title,
            body: document.body,
            modified_at: document.modified_at,
            source_ref: document.source_ref,
        };
        crate::ingest_pipeline::ingest_document_with_scope(
            &**config,
            &document.source_id,
            &document.owner,
            document.tags,
            input,
            document.path_scope,
        )
        .await
        .map(|_| ())
        .map_err(anyhow::Error::msg)
    }

    async fn delete(&self, source_id: &str) -> anyhow::Result<()> {
        let config = self
            .config
            .clone()
            .ok_or_else(|| anyhow::anyhow!("local document sink missing host config"))?;
        let source_id = source_id.to_owned();
        tokio::task::spawn_blocking(move || {
            crate::store::chunks::store::delete_chunks_by_source(
                &*config,
                crate::store::chunks::types::SourceKind::Document,
                &source_id,
            )
        })
        .await
        .map_err(|error| anyhow::anyhow!("local delete task failed: {error}"))??;
        Ok(())
    }
}

#[async_trait]
impl SyncStateStore for HostSyncAdapter {
    async fn get(&self, namespace: &str, key: &str) -> anyhow::Result<Option<serde_json::Value>> {
        self.memory
            .kv_get(Some(namespace), key)
            .await
            .map_err(anyhow::Error::msg)
    }

    async fn set(
        &self,
        namespace: &str,
        key: &str,
        value: &serde_json::Value,
    ) -> anyhow::Result<()> {
        self.memory
            .kv_set(Some(namespace), key, value)
            .await
            .map_err(anyhow::Error::msg)
    }
}

/// Core's state seam, on the engine adapter.
///
/// `SyncState` is core-owned now (#18 §B1a) and its `load`/`save` take
/// core's `SyncStateStore`; OpenHuman pairs that type with this adapter in
/// its integration tests. Same KV calls as the engine-trait impl below —
/// one storage, two trait names during the transition.
#[async_trait]
impl crate::sync::composio::providers::sync_state::SyncStateStore for HostSyncAdapter {
    async fn get(&self, namespace: &str, key: &str) -> anyhow::Result<Option<serde_json::Value>> {
        self.memory
            .kv_get(Some(namespace), key)
            .await
            .map_err(anyhow::Error::msg)
    }

    async fn set(
        &self,
        namespace: &str,
        key: &str,
        value: &serde_json::Value,
    ) -> anyhow::Result<()> {
        self.memory
            .kv_set(Some(namespace), key, value)
            .await
            .map_err(anyhow::Error::msg)
    }
}

#[async_trait]
impl SyncEventSink for HostSyncAdapter {
    async fn emit(&self, event: SyncEvent) -> anyhow::Result<()> {
        crate::events::publish(crate::events::MemoryEvent::SyncStageChanged {
            trigger: "tinycortex".into(),
            stage: stage_name(event.stage).into(),
            provider: Some(event.toolkit),
            connection_id: event.connection_id,
            detail: event.message,
            source_id: Some(event.source_id),
        });
        Ok(())
    }
}

fn stage_name(stage: SyncStage) -> &'static str {
    match stage {
        SyncStage::Requested => "requested",
        SyncStage::Fetching => "fetching",
        SyncStage::Stored => "stored",
        SyncStage::Ingesting => "ingesting",
        SyncStage::Completed => "completed",
        SyncStage::Failed => "failed",
    }
}

#[cfg(test)]
#[path = "sync_tests.rs"]
mod tests;
