//! Tests for the surrounding module.

use super::*;

/// #4957: an unsupported toolkit is rejected *before* credentials are
/// resolved — moved here with the gate itself from the engine seam.
#[test]
fn unsupported_toolkit_is_rejected_before_resolving_credentials() {
    let err = build_composio_pipeline("googlecalendar", "conn-1", ComposioSyncConfig::default())
        .err()
        .expect("unsupported toolkit must be rejected");
    assert!(
        err.contains("does not support toolkit 'googlecalendar'"),
        "got: {err}"
    );
}

#[test]
fn the_syncable_set_is_exactly_the_native_pipelines() {
    for toolkit in syncable_composio_toolkits() {
        assert!(
            build_composio_pipeline(toolkit, "conn-1", ComposioSyncConfig::default()).is_ok(),
            "advertised toolkit '{toolkit}' must build"
        );
    }
    assert!(!is_composio_toolkit_syncable("googlecalendar"));
    assert!(is_composio_toolkit_syncable(" Gmail "));
}

/// The gate normalises; the build must match on the same normalised
/// slug, or a padded/mixed-case toolkit passes the gate and panics.
#[test]
fn a_padded_or_mixed_case_toolkit_builds_rather_than_panicking() {
    for toolkit in [" Gmail ", "GMAIL", "gmail\t", " Slack"] {
        assert!(
            build_composio_pipeline(toolkit, "conn-1", ComposioSyncConfig::default()).is_ok(),
            "{toolkit:?} passes the gate and must build"
        );
    }
}

/// The guard table is process-global and shared by every test in this
/// binary, so each test names connections nothing else touches.
#[test]
fn one_connection_admits_one_run_at_a_time() {
    let held = try_hold_connection("gmail", "guard-single").expect("the first run takes the guard");
    assert!(
        try_hold_connection("gmail", "guard-single").is_none(),
        "a second run of the same connection must be refused, not queued"
    );
    drop(held);
    assert!(
        try_hold_connection("gmail", "guard-single").is_some(),
        "the guard must be released when the run ends"
    );
}

/// The guard is per connection, not per toolkit: one slow Gmail sync must
/// not stop every other Gmail connection from syncing.
#[test]
fn different_connections_hold_independent_guards() {
    let first = try_hold_connection("gmail", "guard-independent-a")
        .expect("the first connection takes its guard");
    let second = try_hold_connection("gmail", "guard-independent-b")
        .expect("a different connection has its own guard");
    drop((first, second));
}

/// `build_composio_pipeline` accepts `" Gmail "` by normalising it. The
/// guard key must normalise identically, or a padded toolkit syncs the
/// same connection concurrently with an unpadded one and they clobber each
/// other's state — the defect the guard exists to prevent.
#[test]
fn the_guard_key_normalises_the_toolkit_like_the_gate() {
    assert_eq!(
        connection_key(" Gmail ", " conn-1 "),
        connection_key("gmail", "conn-1")
    );
    let held =
        try_hold_connection("gmail", "guard-normalised").expect("the first run takes the guard");
    assert!(
        try_hold_connection(" GMAIL\t", "guard-normalised").is_none(),
        "a padded, mixed-case toolkit names the same connection"
    );
    drop(held);
}

/// The Slack sync pipeline and the Slack search backfill load and save the
/// same `("slack", connection_id)` state, so they must contend.
#[test]
fn the_slack_backfill_shares_the_slack_sync_guard() {
    assert_eq!(
        connection_key("slack", "guard-slack"),
        connection_key("Slack", "guard-slack")
    );
    let held = try_hold_connection("slack", "guard-slack").expect("the sync takes the guard");
    assert!(
        try_hold_connection("slack", "guard-slack").is_none(),
        "the backfill must not run while a Slack sync of this connection is running"
    );
    drop(held);
}

/// The engine adapter's tree reconnect has this test
/// (`engine::sync`'s `composio_sync_document_reaches_memory_tree`); the
/// engine-free host that replaced it on the live path did not, and drifted
/// — it wrote a `composio:`-prefixed source id and no `path_scope`, so
/// every synced item became its own tree under a scope no platform prefix
/// matches. Chunks existed, recall could not reach them. Asserting the
/// addressing, not merely the row count, is what catches that.
#[tokio::test]
async fn a_synced_document_is_keyed_by_its_connection_scope() {
    use tinymemory_api::host::test_support::TestHostConfig;
    use tinymemory_api::host::MemoryHostConfig;

    crate::test_seams::init();
    let workspace = tempfile::tempdir().expect("workspace");
    let workspace_dir = workspace.path().join("workspace");
    let mut host_config = TestHostConfig::default();
    host_config.workspace_dir = workspace_dir.clone();
    let config = host_config.to_arc();
    let client: MemoryClientRef = Arc::new(
        crate::store::MemoryClient::from_workspace_dir(workspace_dir)
            .expect("memory client initialises against a fresh workspace"),
    );
    let host = PipelineHost::new(client, config.clone());

    // A fresh tree is empty, so a non-zero count after the store is
    // attributable to this sync rather than to pre-existing state.
    assert_eq!(
        crate::store::chunks::store::count_chunks(&*config).expect("count chunks"),
        0,
        "fresh workspace must start with an empty memory tree"
    );

    host.store(SkillDocument {
        namespace_skill_id: "gmail".into(),
        connection_id: "conn-1".into(),
        document_id: "gmail:msg-1".into(),
        title: "Quarterly planning".into(),
        content: "Let's finalise the Q3 roadmap and align on the launch date.".into(),
        toolkit: "gmail".into(),
        metadata: serde_json::json!({ "source": "composio-provider-incremental" }),
    })
    .await
    .expect("storing a synced document must also ingest it into the memory tree");

    let scoped = crate::store::chunks::store::list_chunks(
        &*config,
        &crate::store::chunks::store::ListChunksQuery {
            source_id: Some("gmail:conn-1:gmail:msg-1".into()),
            limit: Some(8),
            ..Default::default()
        },
    )
    .expect("list chunks by source id");
    assert!(
        !scoped.is_empty(),
        "ingested chunks must be keyed by `{{toolkit}}:{{connection_id}}:{{document_id}}` — \
         the scheme the memory-source status and diff snapshots query by"
    );
    assert!(
        scoped
            .iter()
            .all(|chunk| chunk.metadata.path_scope.as_deref() == Some("gmail:conn-1")),
        "connector chunks must carry the `{{toolkit}}:{{connection_id}}` tree scope so \
         query_source resolves them (gmail → email)"
    );
    assert!(
        scoped
            .iter()
            .all(|chunk| chunk.metadata.owner == "gmail-sync:conn-1"),
        "connector chunks must be owned by the connection that synced them"
    );
}

/// A blank toolkit or connection cannot produce a scope any retrieval kind
/// matches, so the tree half is skipped rather than writing an unreachable
/// tree. The skill store, which committed first, still holds the item.
#[tokio::test]
async fn an_item_without_a_connection_scope_skips_the_tree_but_not_the_store() {
    use tinymemory_api::host::test_support::TestHostConfig;
    use tinymemory_api::host::MemoryHostConfig;

    crate::test_seams::init();
    let workspace = tempfile::tempdir().expect("workspace");
    let workspace_dir = workspace.path().join("workspace");
    let mut host_config = TestHostConfig::default();
    host_config.workspace_dir = workspace_dir.clone();
    let config = host_config.to_arc();
    let client: MemoryClientRef = Arc::new(
        crate::store::MemoryClient::from_workspace_dir(workspace_dir)
            .expect("memory client initialises against a fresh workspace"),
    );
    let host = PipelineHost::new(client.clone(), config.clone());

    host.store(SkillDocument {
        namespace_skill_id: "gmail".into(),
        connection_id: "   ".into(),
        document_id: "gmail:msg-2".into(),
        title: "No connection".into(),
        content: "This item has no connection scope.".into(),
        toolkit: "gmail".into(),
        metadata: serde_json::Value::Null,
    })
    .await
    .expect("a scopeless item must not fail the sync");

    assert_eq!(
        crate::store::chunks::store::count_chunks(&*config).expect("count chunks"),
        0,
        "a scopeless item must not write a tree no retrieval can reach"
    );
    let stored = client
        .list_documents(Some("skill-gmail"))
        .await
        .expect("list skill documents");
    let documents = stored
        .get("documents")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        documents.len(),
        1,
        "the skill store is the source of truth and must still hold the item"
    );
}

/// A pipeline that records whether it was ticked, so the refusal path can
/// be shown to skip the run rather than to run and discard the result.
struct RecordingPipeline(Arc<std::sync::atomic::AtomicBool>);

#[async_trait]
impl SyncPipeline for RecordingPipeline {
    fn id(&self) -> &str {
        "test:recording"
    }

    fn kind(&self) -> crate::sync::pipelines::traits::SyncPipelineKind {
        crate::sync::pipelines::traits::SyncPipelineKind::Composio
    }

    async fn init(&self, _: &PipelineConfig, _: &SyncContext) -> anyhow::Result<()> {
        Ok(())
    }

    async fn tick(&self, _: &PipelineConfig, _: &SyncContext) -> anyhow::Result<SyncOutcome> {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(SyncOutcome {
            records_ingested: 7,
            ..SyncOutcome::default()
        })
    }
}

/// End to end: with the connection held, `run_pipeline` returns the note
/// without ticking the pipeline — no fetch, no Composio spend, and no
/// second writer of the connection's `SyncState`.
#[tokio::test]
async fn a_held_connection_short_circuits_the_run() {
    crate::test_seams::init();
    let workspace = tempfile::tempdir().expect("workspace");
    let client: MemoryClientRef = Arc::new(
        crate::store::MemoryClient::from_workspace_dir(workspace.path().join("store"))
            .expect("memory client initialises against a fresh workspace"),
    );
    let host = Arc::new(PipelineHost::without_tree_ingest(client));
    let ticked = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let held =
        try_hold_connection("gmail", "guard-short-circuit").expect("the first run takes the guard");
    let outcome = run_pipeline(
        Arc::new(RecordingPipeline(ticked.clone())),
        "gmail",
        "guard-short-circuit",
        &PipelineConfig::default(),
        &host.context(),
    )
    .await
    .expect("a refused run is not a failure");

    assert_eq!(outcome.note.as_deref(), Some(SYNC_ALREADY_RUNNING));
    assert_eq!(outcome.records_ingested, 0);
    assert!(
        !ticked.load(std::sync::atomic::Ordering::SeqCst),
        "the refused run must not tick the pipeline"
    );

    // Released, the same call runs normally — the guard skips a concurrent
    // run, it does not disable the connection.
    drop(held);
    let outcome = run_pipeline(
        Arc::new(RecordingPipeline(ticked.clone())),
        "gmail",
        "guard-short-circuit",
        &PipelineConfig::default(),
        &host.context(),
    )
    .await
    .expect("the run succeeds once the guard is free");
    assert_eq!(outcome.records_ingested, 7);
    assert!(ticked.load(std::sync::atomic::Ordering::SeqCst));
}

#[test]
fn pipeline_failure_and_source_caps_preserve_operational_details() {
    let failure = PipelineFailure::without_usage("offline");
    assert_eq!(failure.to_string(), "offline");
    assert!(std::error::Error::source(&failure).is_none());
    assert_eq!(failure.actions_called, 0);
    assert_eq!(failure.provider_cost_usd, 0.0);

    let source: tinymemory_sources::MemorySourceEntry = serde_json::from_value(serde_json::json!({
        "id": "source-1",
        "kind": "composio",
        "label": "Mail",
        "enabled": true,
        "max_items": 11,
        "sync_depth_days": 4,
        "max_tokens_per_sync": 500,
        "max_cost_per_sync_usd": 0.25
    }))
    .unwrap();
    let caps = SourceCaps::from_source(&source);
    assert_eq!(caps.max_items, Some(11));
    assert_eq!(caps.sync_depth_days, Some(4));
    assert_eq!(caps.max_tokens_per_sync, Some(500));
    assert_eq!(caps.max_cost_per_sync_usd, Some(0.25));
}

#[test]
fn composio_config_covers_direct_proxied_and_missing_credentials() {
    let mut direct = tinymemory_api::host::test_support::TestHostConfig::default();
    direct.composio.mode = "direct".into();
    direct.composio.entity_id = "entity-1".into();
    assert!(composio_config(&direct).is_err());
    direct.composio.api_key = Some("direct-secret".into());
    let config = composio_config(&direct).unwrap();
    assert_eq!(config.mode, ComposioMode::Direct);
    assert_eq!(config.api_key.as_ref().unwrap().expose(), "direct-secret");
    assert_eq!(config.entity_id.as_deref(), Some("entity-1"));

    let mut proxied = tinymemory_api::host::test_support::TestHostConfig::default();
    assert!(composio_config(&proxied).is_err());
    proxied.session_token = Some("session-secret".into());
    proxied.api_url = Some("https://backend.example".into());
    let config = composio_config(&proxied).unwrap();
    assert_eq!(config.mode, ComposioMode::Proxied);
    assert_eq!(
        config.bearer_token.as_ref().unwrap().expose(),
        "session-secret"
    );
    assert!(config.api_key.is_none());
}

struct FailingPipeline {
    usage: bool,
}

#[async_trait]
impl SyncPipeline for FailingPipeline {
    fn id(&self) -> &str {
        if self.usage {
            "test:usage"
        } else {
            "test:plain"
        }
    }

    fn kind(&self) -> crate::sync::pipelines::traits::SyncPipelineKind {
        crate::sync::pipelines::traits::SyncPipelineKind::Composio
    }

    async fn init(&self, _: &PipelineConfig, _: &SyncContext) -> anyhow::Result<()> {
        Ok(())
    }

    async fn tick(&self, _: &PipelineConfig, _: &SyncContext) -> anyhow::Result<SyncOutcome> {
        if self.usage {
            Err(anyhow::Error::new(SyncRunError::new(
                "spent failure",
                3,
                0.75,
            )))
        } else {
            anyhow::bail!("plain failure")
        }
    }
}

#[tokio::test]
async fn run_pipeline_preserves_typed_usage_and_defaults_plain_errors() {
    crate::test_seams::init();
    let workspace = tempfile::tempdir().unwrap();
    let memory: MemoryClientRef = Arc::new(
        crate::store::MemoryClient::from_workspace_dir(workspace.path().join("store")).unwrap(),
    );
    let host = Arc::new(PipelineHost::without_tree_ingest(memory));
    let typed = run_pipeline(
        Arc::new(FailingPipeline { usage: true }),
        "gmail",
        "failure-usage",
        &PipelineConfig::default(),
        &host.context(),
    )
    .await
    .unwrap_err();
    assert_eq!(typed.message, "spent failure");
    assert_eq!(typed.actions_called, 3);
    assert_eq!(typed.provider_cost_usd, 0.75);

    let plain = run_pipeline(
        Arc::new(FailingPipeline { usage: false }),
        "gmail",
        "failure-plain",
        &PipelineConfig::default(),
        &host.context(),
    )
    .await
    .unwrap_err();
    assert_eq!(plain.message, "plain failure");
    assert_eq!(plain.actions_called, 0);
    assert_eq!(plain.provider_cost_usd, 0.0);
}

#[tokio::test]
async fn pipeline_host_state_event_and_delete_capabilities_round_trip() {
    crate::test_seams::init();
    let workspace = tempfile::tempdir().unwrap();
    let memory: MemoryClientRef = Arc::new(
        crate::store::MemoryClient::from_workspace_dir(workspace.path().join("store")).unwrap(),
    );
    let host = Arc::new(PipelineHost::without_tree_ingest(memory.clone()));
    SyncStateStore::set(&*host, "sync", "cursor", &serde_json::json!({"page": 2}))
        .await
        .unwrap();
    assert_eq!(
        SyncStateStore::get(&*host, "sync", "cursor").await.unwrap(),
        Some(serde_json::json!({"page": 2}))
    );
    let sink = crate::events::RecordingSink::install();
    host.emit(SyncEvent {
        source_id: "source-1".into(),
        toolkit: "gmail".into(),
        connection_id: Some("connection-1".into()),
        stage: crate::sync::pipelines::traits::SyncStage::Completed,
        message: Some("done".into()),
    })
    .await
    .unwrap();
    assert!(!sink.drain().is_empty());
    host.delete("gmail", "missing-document").await.unwrap();
}
