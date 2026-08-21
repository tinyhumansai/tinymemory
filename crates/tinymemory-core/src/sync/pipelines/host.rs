//! The host side of the engine-free pipelines (#18 §B1): the sink adapter
//! over [`MemoryClient`](crate::store::MemoryClient), the Composio settings
//! mapping, and the runners the
//! rest of `core/src/sync/` calls.
//!
//! This is the piece §B5's acceptance rests on: a pipeline sees three
//! capabilities — events, documents, state — and every one resolves through
//! [`MemoryClient`](crate::store::MemoryClient), so whatever driver the host
//! bound serves the sync.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use tokio::sync::OwnedMutexGuard;

use crate::store::MemoryClientRef;
use crate::sync::composio::providers::sync_state::SyncStateStore;
use crate::sync::pipelines::composio::{
    ClickUpSyncPipeline, ComposioClient, GitHubSyncPipeline, GmailSyncPipeline, LinearSyncPipeline,
    NotionSyncPipeline, SlackSearchBackfillPipeline, SlackSyncPipeline,
};
use crate::sync::pipelines::dispatcher::SyncDispatcher;
use crate::sync::pipelines::traits::{
    ComposioMode, ComposioSyncConfig, PipelineConfig, SecretString, SkillDocSink, SkillDocument,
    SyncContext, SyncEvent, SyncEventSink, SyncOutcome, SyncPipeline, SyncRunError,
};
use crate::Config;

/// A failed pipeline run, with whatever usage it burned before failing.
#[derive(Debug)]
pub struct PipelineFailure {
    pub message: String,
    pub actions_called: u32,
    pub provider_cost_usd: f64,
}

impl std::fmt::Display for PipelineFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PipelineFailure {}

impl PipelineFailure {
    pub fn without_usage(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            actions_called: 0,
            provider_cost_usd: 0.0,
        }
    }
}

/// Adapter giving the pipelines their three capabilities over the bound
/// memory client. The engine's `HostSyncAdapter` remains for the engine's own
/// pipelines; this one exists so a Composio sync never needs the engine.
pub struct PipelineHost {
    memory: MemoryClientRef,
    config: Option<Arc<Config>>,
}

impl PipelineHost {
    /// An adapter that also feeds the memory tree after each stored document
    /// (parity with the engine adapter's #5473 behaviour).
    pub fn new(memory: MemoryClientRef, config: Arc<Config>) -> Self {
        Self {
            memory,
            config: Some(config),
        }
    }

    /// An adapter with no host config: documents are stored, tree ingest is
    /// skipped. This is the shape a non-TinyCortex host uses.
    pub fn without_tree_ingest(memory: MemoryClientRef) -> Self {
        Self {
            memory,
            config: None,
        }
    }

    /// The pipeline context over this adapter.
    pub fn context(self: &Arc<Self>) -> SyncContext {
        SyncContext {
            events: self.clone(),
            documents: self.clone(),
            state: self.clone(),
        }
    }
}

#[async_trait]
impl SkillDocSink for PipelineHost {
    async fn store(&self, document: SkillDocument) -> anyhow::Result<()> {
        tracing::debug!(
            toolkit = %document.toolkit,
            connection_id = %document.connection_id,
            document_id = %document.document_id,
            "[memory_sync] storing synchronized document"
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

        // #5473: additively reconnect the synced item to the memory tree — a
        // best-effort secondary index; the skill store above is the source of
        // truth and has committed. A failure here must NOT abort the sync (one
        // poisonous item would stall the connection and re-buy the page on
        // every retry). The config-less adapter skips tree ingest entirely.
        if let Some(config) = self.config.as_deref() {
            if let Err(error) = ingest_into_tree(config, &document).await {
                tracing::warn!(
                    %error,
                    document_id = %document.document_id,
                    "[memory_sync] tree ingest failed; skill store remains authoritative"
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
            "[memory_sync] deleting synchronized document"
        );
        self.memory
            .delete_document(&namespace, document_id)
            .await
            .map(|_| ())
            .map_err(anyhow::Error::msg)
    }
}

/// Mirror of the engine adapter's tree reconnect (`engine::sync`'s
/// `ingest_document_into_memory_tree`): route the stored document through
/// core's ingest funnel under the same addressing scheme.
///
/// # The scheme is the contract, not an implementation detail
///
/// The tree scope a chunk seals under is `path_scope`, falling back to
/// `source_id`. Retrieval selects source trees by that scope and classifies
/// them by their **platform prefix** — `gmail:` is email, `slack:` is chat.
/// So the scope has to be `"{toolkit}:{connection_id}"`: one tree per
/// connection, named by a prefix retrieval knows.
///
/// Passing no `path_scope` is not a smaller version of that. It makes each
/// item's own `source_id` the scope, which is a *tree per document*, named by
/// a prefix that matches no platform — the items are stored and then
/// unreachable, which is the #5473 defect this reconnect exists to fix. The
/// `source_id LIKE` prefix the memory-source status and diff snapshots query
/// by is keyed on this same scheme — see `sources::status::source_id_prefix`.
///
/// Tags are a deliberate superset of the engine adapter's: it tags the toolkit
/// alone, this also tags `composio_sync`. Tags feed scoring and filtering, not
/// addressing, so the extra one costs nothing and marks the ingest path.
async fn ingest_into_tree(config: &Config, document: &SkillDocument) -> anyhow::Result<()> {
    let toolkit = document.toolkit.trim().to_ascii_lowercase();
    let connection_id = document.connection_id.trim();
    // A blank toolkit or connection would yield a scope with no platform
    // prefix (`":conn"`, `"gmail:"`), which no retrieval kind matches; skip
    // rather than write an unreachable tree. The skill store still holds the
    // item.
    if toolkit.is_empty() || connection_id.is_empty() {
        tracing::debug!(
            document_id = %document.document_id,
            "[memory_sync] skipping memory-tree ingest: item has no toolkit/connection scope"
        );
        return Ok(());
    }
    let tree_scope = format!("{toolkit}:{connection_id}");
    let source_id = format!("{tree_scope}:{}", document.document_id);
    let owner = format!("{toolkit}-sync:{connection_id}");
    let doc = crate::ingest_pipeline::IngestDocumentInput {
        provider: format!("composio:{toolkit}"),
        title: document.title.clone(),
        body: document.content.clone(),
        modified_at: chrono::Utc::now(),
        source_ref: Some(document.document_id.clone()),
    };
    let tags = vec!["composio_sync".to_string(), toolkit];
    crate::ingest_pipeline::ingest_document_with_scope(
        config,
        &source_id,
        &owner,
        tags,
        doc,
        Some(tree_scope),
    )
    .await
    .map(|_| ())
    .map_err(|error| anyhow::anyhow!("memory-tree ingest failed for source `{source_id}`: {error}"))
}

#[async_trait]
impl SyncEventSink for PipelineHost {
    async fn emit(&self, event: SyncEvent) -> anyhow::Result<()> {
        crate::events::publish(crate::events::MemoryEvent::SyncStageChanged {
            trigger: "tinycortex".into(),
            stage: super::traits::stage_name(event.stage).into(),
            provider: Some(event.toolkit),
            connection_id: event.connection_id,
            detail: event.message,
            source_id: Some(event.source_id),
        });
        Ok(())
    }
}

#[async_trait]
impl SyncStateStore for PipelineHost {
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

/// The Composio connection settings from the host's config — the same
/// resolution the engine seam performs, onto the local types.
pub fn composio_config(config: &Config) -> Result<ComposioSyncConfig, String> {
    if config.composio().mode.eq_ignore_ascii_case("direct") {
        let api_key = crate::composio_host::api_key(config)
            .or_else(|| config.composio().api_key.clone())
            .ok_or_else(|| "Composio direct API key is not configured".to_string())?;
        Ok(ComposioSyncConfig {
            mode: ComposioMode::Direct,
            base_url: "https://backend.composio.dev/api/v3".into(),
            api_key: Some(SecretString::new(api_key)),
            bearer_token: None,
            entity_id: Some(config.composio().entity_id.clone()),
        })
    } else {
        let bearer = config
            .session_token()?
            .ok_or_else(|| "OpenHuman backend bearer token is not configured".to_string())?;
        Ok(ComposioSyncConfig {
            mode: ComposioMode::Proxied,
            base_url: config.effective_backend_api_url(),
            api_key: None,
            bearer_token: Some(SecretString::new(bearer)),
            entity_id: Some(config.composio().entity_id.clone()),
        })
    }
}

/// The toolkits with a native pipeline here. Kept identical to the engine
/// seam's list; `sync_status` advertising draws from the provider registry.
pub fn syncable_composio_toolkits() -> &'static [&'static str] {
    &["clickup", "github", "gmail", "linear", "notion", "slack"]
}

/// Whether `toolkit` has a native pipeline (case-insensitive).
pub fn is_composio_toolkit_syncable(toolkit: &str) -> bool {
    let slug = toolkit.trim().to_ascii_lowercase();
    syncable_composio_toolkits().contains(&slug.as_str())
}

fn build_composio_pipeline(
    toolkit: &str,
    connection_id: &str,
    composio: ComposioSyncConfig,
) -> Result<Arc<dyn SyncPipeline>, String> {
    // Fail closed before resolving credentials for any toolkit without a
    // native pipeline (#4957) — the gate stays a single testable list.
    //
    // Normalise once and match on the normalised slug: the gate accepts
    // `" Gmail "` (trim + lowercase), so matching on the raw input would let a
    // padded or mixed-case toolkit through the gate and into `unreachable!`.
    let slug = toolkit.trim().to_ascii_lowercase();
    if !syncable_composio_toolkits().contains(&slug.as_str()) {
        return Err(format!("memory sync does not support toolkit '{toolkit}'"));
    }
    let client = ComposioClient::new(composio);
    Ok(match slug.as_str() {
        "gmail" => Arc::new(GmailSyncPipeline::new(client, connection_id)),
        "github" => Arc::new(GitHubSyncPipeline::new(client, connection_id)),
        "notion" => Arc::new(NotionSyncPipeline::new(client, connection_id)),
        "linear" => Arc::new(LinearSyncPipeline::new(client, connection_id)),
        "clickup" => Arc::new(ClickUpSyncPipeline::new(client, connection_id)),
        "slack" => Arc::new(SlackSyncPipeline::new(client, connection_id)),
        _ => unreachable!("gated by is_composio_toolkit_syncable"),
    })
}

/// Run one Composio connection through the engine-free pipelines.
pub async fn run_composio_connection(
    toolkit: &str,
    connection_id: &str,
    config: &Config,
    max_items: Option<u32>,
    sync_depth_days: Option<u32>,
) -> Result<SyncOutcome, PipelineFailure> {
    run_composio_connection_with_caps(
        toolkit,
        connection_id,
        config,
        SourceCaps {
            max_items,
            sync_depth_days,
            ..SourceCaps::default()
        },
    )
    .await
}

/// The per-source limits a run honours. All `None` = the source's defaults.
#[derive(Clone, Copy, Debug, Default)]
pub struct SourceCaps {
    pub max_items: Option<u32>,
    pub sync_depth_days: Option<u32>,
    pub max_tokens_per_sync: Option<u64>,
    pub max_cost_per_sync_usd: Option<f64>,
}

impl SourceCaps {
    /// The caps a registry entry carries.
    pub fn from_source(source: &tinymemory_sources::MemorySourceEntry) -> Self {
        Self {
            max_items: source.max_items,
            sync_depth_days: source.sync_depth_days,
            max_tokens_per_sync: source.max_tokens_per_sync,
            max_cost_per_sync_usd: source.max_cost_per_sync_usd,
        }
    }
}

/// Run one Composio connection through the engine-free pipelines, honouring
/// every per-source cap.
pub async fn run_composio_connection_with_caps(
    toolkit: &str,
    connection_id: &str,
    config: &Config,
    caps: SourceCaps,
) -> Result<SyncOutcome, PipelineFailure> {
    let memory = crate::global::client_if_ready()
        .ok_or_else(|| PipelineFailure::without_usage("memory client is not ready"))?;
    let composio = composio_config(config).map_err(PipelineFailure::without_usage)?;
    let pipeline = build_composio_pipeline(toolkit, connection_id, composio)
        .map_err(PipelineFailure::without_usage)?;
    let pipeline_config = PipelineConfig {
        composio: None, // the client already holds the connection settings
        sync_depth_days: caps.sync_depth_days,
        max_items: caps.max_items,
        max_tokens_per_sync: caps.max_tokens_per_sync,
        max_cost_per_sync_usd: caps.max_cost_per_sync_usd,
    };
    let host = Arc::new(PipelineHost::new(memory, config.to_arc()));
    run_pipeline(
        pipeline,
        toolkit,
        connection_id,
        &pipeline_config,
        &host.context(),
    )
    .await
}

/// Run a bounded Gmail backfill through the engine-free pipelines.
pub async fn run_gmail_backfill(
    connection_id: &str,
    query: &str,
    max_pages: usize,
    page_size: usize,
    config: &Config,
) -> Result<SyncOutcome, PipelineFailure> {
    let memory = crate::global::client_if_ready()
        .ok_or_else(|| PipelineFailure::without_usage("memory client is not ready"))?;
    let composio = composio_config(config).map_err(PipelineFailure::without_usage)?;
    let pipeline: Arc<dyn SyncPipeline> = Arc::new(
        GmailSyncPipeline::new(ComposioClient::new(composio), connection_id)
            .with_limits(max_pages, page_size)
            .with_query(query),
    );
    let host = Arc::new(PipelineHost::new(memory, config.to_arc()));
    // The backfill drives the Gmail pipeline, which keys its `SyncState` on
    // `"gmail"`; naming the same toolkit here puts it behind the same guard as
    // a periodic or RPC Gmail sync of this connection.
    run_pipeline(
        pipeline,
        "gmail",
        connection_id,
        &PipelineConfig::default(),
        &host.context(),
    )
    .await
}

/// Run the Slack search backfill through the engine-free pipelines.
pub async fn run_slack_search_backfill(
    connection_id: &str,
    backfill_days: i64,
    config: &Config,
) -> Result<SyncOutcome, PipelineFailure> {
    let memory = crate::global::client_if_ready()
        .ok_or_else(|| PipelineFailure::without_usage("memory client is not ready"))?;
    let composio = composio_config(config).map_err(PipelineFailure::without_usage)?;
    let client = ComposioClient::new(composio);
    let pipeline: Arc<dyn SyncPipeline> = Arc::new(SlackSearchBackfillPipeline::new(
        client,
        connection_id,
        backfill_days,
    ));
    let host = Arc::new(PipelineHost::new(memory, config.to_arc()));
    // `SlackSearchBackfillPipeline` loads and saves the same
    // `("slack", connection_id)` state the Slack sync pipeline does, so the two
    // must share one guard or they clobber each other's cursor and budget.
    run_pipeline(
        pipeline,
        "slack",
        connection_id,
        &PipelineConfig::default(),
        &host.context(),
    )
    .await
}

/// The note a run carries when another run already holds its connection.
///
/// Callers that distinguish "nothing to sync" from "did not sync" match on
/// this rather than on a message they would have to keep in step by hand.
pub const SYNC_ALREADY_RUNNING: &str = "sync already running for this connection";

/// One guard per connection, so two runs cannot clobber each other's state.
type ConnectionLock = Arc<tokio::sync::Mutex<()>>;

/// The process-wide guard table.
///
/// `run_incremental_sync` loads the connection's `SyncState` once, mutates it
/// in memory for the whole run, and saves at the end; the Slack search
/// backfill does the same over the same `("slack", connection_id)` record. Two
/// runs of one connection therefore race on the cursor, the dedup set and the
/// daily budget, and whichever saves last wins — losing either the dedup set
/// (re-fetch, re-spend) or the budget count (overspend past the cap). The
/// periodic loop, the sync RPC and a trigger can each fire the same
/// connection, so the race is reachable as the code stands.
///
/// This is the single-process answer, which is how the loop and the RPC paths
/// actually run. An optimistic version stamp on the KV record is what a
/// multi-process host would need instead.
///
/// The table only ever grows, bounded by the number of connections the host
/// has seen — the same shape, and the same bound, as the periodic scheduler's
/// last-fired map. An entry is one `Arc` and an unlocked mutex.
fn connection_locks() -> &'static Mutex<HashMap<(String, String), ConnectionLock>> {
    static LOCKS: OnceLock<Mutex<HashMap<(String, String), ConnectionLock>>> = OnceLock::new();
    LOCKS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The guard key for a connection.
///
/// Normalised exactly as [`build_composio_pipeline`] normalises the toolkit
/// gate, so `" Gmail "` and `gmail` name one connection rather than two — and
/// so the Slack sync pipeline and the Slack search backfill, which share one
/// `SyncState` record, share one guard.
fn connection_key(toolkit: &str, connection_id: &str) -> (String, String) {
    (
        toolkit.trim().to_ascii_lowercase(),
        connection_id.trim().to_owned(),
    )
}

/// Take the guard for one connection, or `None` if a run already holds it.
///
/// Deliberately non-blocking. Queueing behind the running sync would stall the
/// periodic loop's whole tick — it walks connections sequentially — and then
/// run a second sync of a connection that has just been synced, which is the
/// Composio spend this guard exists to avoid.
fn try_hold_connection(toolkit: &str, connection_id: &str) -> Option<OwnedMutexGuard<()>> {
    let lock = {
        // A panic inside a run cannot corrupt the table: it holds `Arc`s, and
        // the async guard is released by its own `Drop`. Recovering from the
        // poison keeps one panicking sync from disabling every later one.
        let mut locks = connection_locks()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::clone(
            locks
                .entry(connection_key(toolkit, connection_id))
                .or_default(),
        )
    };
    lock.try_lock_owned().ok()
}

async fn run_pipeline(
    pipeline: Arc<dyn SyncPipeline>,
    toolkit: &str,
    connection_id: &str,
    config: &PipelineConfig,
    context: &SyncContext,
) -> Result<SyncOutcome, PipelineFailure> {
    let Some(_connection) = try_hold_connection(toolkit, connection_id) else {
        tracing::debug!(
            toolkit,
            connection_id,
            "[memory_sync] a sync of this connection is already running; skipping"
        );
        return Ok(SyncOutcome {
            note: Some(SYNC_ALREADY_RUNNING.to_owned()),
            ..SyncOutcome::default()
        });
    };
    let pipeline_id = pipeline.id().to_owned();
    let mut dispatcher = SyncDispatcher::new();
    dispatcher
        .register(pipeline)
        .map_err(|error| PipelineFailure::without_usage(error.to_string()))?;
    dispatcher
        .tick(&pipeline_id, config, context)
        .await
        .map_err(|error| {
            let usage = error.downcast_ref::<SyncRunError>();
            PipelineFailure {
                message: error.to_string(),
                actions_called: usage.map_or(0, |error| error.actions_called),
                provider_cost_usd: usage.map_or(0.0, |error| error.provider_cost_usd),
            }
        })
}

#[cfg(test)]
#[path = "host_tests.rs"]
mod tests;
