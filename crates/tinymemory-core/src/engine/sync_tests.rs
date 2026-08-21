//! Tests for the surrounding module.

use super::{
    build_pipeline, run_composio_connection, run_composio_connection_with_budgets,
    run_gmail_backfill, run_slack_search_backfill, run_source_pipeline,
};
use crate::sources::MemorySourceEntry;
use crate::sync::composio::{get_composio_sync_provider, init_default_composio_sync_providers};
use crate::sync::pipelines::host::{is_composio_toolkit_syncable, syncable_composio_toolkits};

fn memory_fixture() -> (
    tempfile::TempDir,
    tinymemory_api::host::test_support::TestHostConfig,
    crate::store::MemoryClientRef,
) {
    crate::test_seams::init();
    let workspace = tempfile::tempdir().expect("workspace");
    let mut config = tinymemory_api::host::test_support::TestHostConfig::default();
    config.workspace_dir = workspace.path().join("memory");
    let client = std::sync::Arc::new(
        crate::store::MemoryClient::from_workspace_dir(config.workspace_dir.clone())
            .expect("memory client"),
    );
    (workspace, config, client)
}

fn source(kind: &str, fields: serde_json::Value) -> MemorySourceEntry {
    let mut value = serde_json::json!({
        "id": format!("source-{kind}"),
        "kind": kind,
        "label": format!("{kind} source"),
    });
    value
        .as_object_mut()
        .expect("source object")
        .extend(fields.as_object().expect("fields object").clone());
    serde_json::from_value(value).expect("valid source fixture")
}

#[tokio::test]
async fn failure_and_context_helpers_preserve_contract_state() {
    let failure = super::SourcePipelineFailure::without_usage("offline");
    assert_eq!(failure.to_string(), "offline");
    assert_eq!(failure.actions_called, 0);
    assert_eq!(failure.provider_cost_usd, 0.0);

    let (_workspace, config, client) = memory_fixture();
    let context = super::sync_context(client.clone());
    assert!(context.local_documents.is_none());
    assert!(context.external_sources.is_none());
    assert!(context.summariser.is_none());

    let local = super::source_sync_context(client.clone(), &config, true);
    assert!(local.local_documents.is_some());
    assert!(local.external_sources.is_some());
    assert!(local.summariser.is_some());

    let remote = super::source_sync_context(client, &config, false);
    assert!(remote.local_documents.is_none());
    assert!(remote.external_sources.is_none());
    assert!(remote.summariser.is_none());
}

#[tokio::test]
async fn adapter_state_and_document_seams_round_trip_locally() {
    use tinycortex::memory::sync::{SkillDocSink, SkillDocument, SyncStateStore};

    let (_workspace, _config, client) = memory_fixture();
    let adapter = super::HostSyncAdapter::new(client.clone());
    let value = serde_json::json!({"cursor": "next"});

    SyncStateStore::set(&adapter, "sync-state", "gmail", &value)
        .await
        .expect("set engine state");
    assert_eq!(
        SyncStateStore::get(&adapter, "sync-state", "gmail")
            .await
            .expect("get engine state"),
        Some(value.clone())
    );
    crate::sync::composio::providers::sync_state::SyncStateStore::set(
        &adapter,
        "host-state",
        "slack",
        &value,
    )
    .await
    .expect("set host state");
    assert_eq!(
        crate::sync::composio::providers::sync_state::SyncStateStore::get(
            &adapter,
            "host-state",
            "slack",
        )
        .await
        .expect("get host state"),
        Some(value)
    );

    SkillDocSink::store(
        &adapter,
        SkillDocument {
            namespace_skill_id: "gmail".into(),
            connection_id: "connection-1".into(),
            document_id: "message-1".into(),
            title: "Planning".into(),
            content: "The launch is Tuesday.".into(),
            toolkit: "gmail".into(),
            metadata: serde_json::json!({"thread": "t-1"}),
        },
    )
    .await
    .expect("store synchronized document");
    let stored = client
        .get_document("skill-gmail", "message-1")
        .await
        .expect("read synchronized document");
    assert_eq!(
        stored.as_ref().map(|document| document.content.as_str()),
        Some("The launch is Tuesday.")
    );
    SkillDocSink::delete(&adapter, "gmail", "message-1")
        .await
        .expect("delete synchronized document");
    assert!(client
        .get_document("skill-gmail", "message-1")
        .await
        .expect("read after delete")
        .is_none());
}

#[tokio::test]
async fn configless_local_and_external_seams_fail_before_io() {
    use tinycortex::memory::sync::{ExternalSourceReader, LocalDocument, LocalDocumentSink};

    let (_workspace, _config, client) = memory_fixture();
    let adapter = super::HostSyncAdapter::new(client);
    let local = LocalDocument {
        source_id: "local:one".into(),
        path_scope: None,
        owner: "folder:one".into(),
        tags: vec!["local".into()],
        title: "Local".into(),
        body: "body".into(),
        modified_at: chrono::Utc::now(),
        source_ref: None,
    };
    assert!(LocalDocumentSink::upsert(&adapter, local)
        .await
        .expect_err("configless upsert must fail")
        .to_string()
        .contains("missing host config"));
    assert!(LocalDocumentSink::delete(&adapter, "local:one")
        .await
        .expect_err("configless delete must fail")
        .to_string()
        .contains("missing host config"));

    let host_source = source("folder", serde_json::json!({"path": "."}));
    let engine_source =
        serde_json::from_value(serde_json::to_value(host_source).expect("serialize source"))
            .expect("engine source");
    assert!(ExternalSourceReader::list_items(&adapter, &engine_source)
        .await
        .expect_err("configless list must fail")
        .to_string()
        .contains("requires host config"));
    assert!(
        ExternalSourceReader::read_item(&adapter, &engine_source, "item")
            .await
            .expect_err("configless read must fail")
            .to_string()
            .contains("requires host config")
    );
}

#[tokio::test]
async fn configured_local_document_sink_upserts_and_deletes_chunks() {
    use tinycortex::memory::sync::{LocalDocument, LocalDocumentSink};

    crate::test_seams::init();
    let (_workspace, config, client) = memory_fixture();
    let adapter = super::HostSyncAdapter::with_config(
        client,
        tinymemory_api::host::MemoryHostConfig::to_arc(&config),
    );
    LocalDocumentSink::upsert(
        &adapter,
        LocalDocument {
            source_id: "folder:notes:one".into(),
            path_scope: Some("folder:notes".into()),
            owner: "folder-sync".into(),
            tags: vec!["notes".into()],
            title: "One".into(),
            body: "A deterministic local document body.".into(),
            modified_at: chrono::DateTime::from_timestamp_millis(1_700_000_000_000)
                .expect("fixed timestamp"),
            source_ref: Some("one.md".into()),
        },
    )
    .await
    .expect("upsert local document");
    let chunks = crate::store::chunks::store::list_chunks(
        &config,
        &tinycortex::memory::chunks::ListChunksQuery {
            source_id: Some("folder:notes:one".into()),
            ..Default::default()
        },
    )
    .expect("list local chunks");
    assert_eq!(chunks.len(), 1);
    assert_eq!(
        chunks[0].metadata.path_scope.as_deref(),
        Some("folder:notes")
    );

    LocalDocumentSink::delete(&adapter, "folder:notes:one")
        .await
        .expect("delete local document");
    assert!(crate::store::chunks::store::list_chunks(
        &config,
        &tinycortex::memory::chunks::ListChunksQuery {
            source_id: Some("folder:notes:one".into()),
            ..Default::default()
        },
    )
    .expect("list after delete")
    .is_empty());
}

#[tokio::test]
async fn configured_external_reader_lists_and_reads_a_local_folder() {
    use tinycortex::memory::sync::ExternalSourceReader;

    let workspace = tempfile::tempdir().expect("workspace");
    let source_dir = workspace.path().join("source");
    std::fs::create_dir_all(&source_dir).expect("create source directory");
    std::fs::write(source_dir.join("note.md"), "# Note\n\nLocal body.")
        .expect("write source document");
    let mut config = tinymemory_api::host::test_support::TestHostConfig::default();
    config.workspace_dir = workspace.path().join("memory");
    let client = std::sync::Arc::new(
        crate::store::MemoryClient::from_workspace_dir(config.workspace_dir.clone())
            .expect("memory client"),
    );
    let adapter = super::HostSyncAdapter::with_config(
        client,
        tinymemory_api::host::MemoryHostConfig::to_arc(&config),
    );
    let host_source = source(
        "folder",
        serde_json::json!({"path": source_dir, "glob": "**/*.md"}),
    );
    let engine_source =
        serde_json::from_value(serde_json::to_value(host_source).expect("serialize source"))
            .expect("engine source");
    let items = ExternalSourceReader::list_items(&adapter, &engine_source)
        .await
        .expect("list folder items");
    assert_eq!(items.len(), 1);
    let content = ExternalSourceReader::read_item(&adapter, &engine_source, &items[0].id)
        .await
        .expect("read folder item");
    assert_eq!(content.body, "# Note\n\nLocal body.");
}

#[test]
fn pipeline_builder_covers_every_tree_coupled_source_kind() {
    let config = tinymemory_api::host::test_support::TestHostConfig::default();
    let mut memory_config =
        tinycortex::memory::config::MemoryConfig::new(config.workspace_dir.clone());
    let fixtures = [
        source("folder", serde_json::json!({"path": "."})),
        source(
            "github_repo",
            serde_json::json!({"url": "https://github.com/tinyhumansai/tinymemory"}),
        ),
        source(
            "rss_feed",
            serde_json::json!({"url": "https://example.com/feed.xml"}),
        ),
        source(
            "web_page",
            serde_json::json!({"url": "https://example.com/page"}),
        ),
        source("conversation", serde_json::json!({})),
    ];
    for fixture in fixtures {
        let pipeline = super::build_pipeline(&fixture, &config, &mut memory_config)
            .unwrap_or_else(|error| panic!("{} pipeline: {error}", fixture.kind.as_str()));
        assert!(!pipeline.id().is_empty());
    }
}

#[tokio::test]
async fn composio_validation_reports_missing_fields_without_usage() {
    let config = tinymemory_api::host::test_support::TestHostConfig::default();
    let missing_toolkit = source(
        "composio",
        serde_json::json!({"connection_id": "connection-1"}),
    );
    let failure = super::run_source_pipeline(&missing_toolkit, &config)
        .await
        .expect_err("toolkit is required");
    assert!(failure.message.contains("missing toolkit"));
    assert_eq!(failure.actions_called, 0);
    assert_eq!(failure.provider_cost_usd, 0.0);

    let missing_connection = source("composio", serde_json::json!({"toolkit": "gmail"}));
    let failure = super::run_source_pipeline(&missing_connection, &config)
        .await
        .expect_err("connection is required");
    assert!(failure.message.contains("missing connection_id"));
    assert_eq!(failure.actions_called, 0);
}

#[tokio::test]
async fn raw_archive_and_stage_helpers_cover_empty_local_state() {
    let (_workspace, config, _client) = memory_fixture();
    assert!(super::read_audit_log(&config).is_empty());
    assert_eq!(super::estimate_cost_usd(0, 0), 0.0);
    let coverage =
        super::raw_coverage(&config, "gmail:one", "gmail.com:one").expect("empty archive coverage");
    assert_eq!(coverage.total, 0);
    assert_eq!(coverage.covered, 0);
    assert!(!super::needs_rebuild(&config, "gmail:one", "gmail.com:one"));
    assert_eq!(
        [
            tinycortex::memory::sync::SyncStage::Requested,
            tinycortex::memory::sync::SyncStage::Fetching,
            tinycortex::memory::sync::SyncStage::Stored,
            tinycortex::memory::sync::SyncStage::Ingesting,
            tinycortex::memory::sync::SyncStage::Completed,
            tinycortex::memory::sync::SyncStage::Failed,
        ]
        .map(super::stage_name),
        [
            "requested",
            "fetching",
            "stored",
            "ingesting",
            "completed",
            "failed",
        ]
    );
}

#[tokio::test]
async fn public_composio_wrappers_fail_before_transport_and_preserve_zero_usage() {
    let (_tmp, config, _memory) = memory_fixture();
    for result in [
        run_composio_connection("gmail", "connection-missing-auth", &config).await,
        run_composio_connection_with_budgets(
            "slack",
            "connection-missing-auth",
            &config,
            Some(7),
            Some(2),
        )
        .await,
        run_slack_search_backfill("connection-missing-auth", 14, &config).await,
        run_gmail_backfill(
            "connection-missing-auth",
            "after:2024/01/01",
            2,
            25,
            &config,
        )
        .await,
    ] {
        let failure = result.unwrap_err();
        assert_eq!(failure.actions_called, 0);
        assert_eq!(failure.provider_cost_usd, 0.0);
        assert!(!failure.message.is_empty());
    }
}

#[tokio::test]
async fn source_pipeline_invalid_local_input_fails_without_provider_usage() {
    let (_tmp, config, _memory) = memory_fixture();
    let invalid = source("web_page", serde_json::json!({}));
    let failure = run_source_pipeline(&invalid, &config).await.unwrap_err();
    assert_eq!(failure.actions_called, 0);
    assert_eq!(failure.provider_cost_usd, 0.0);
    assert!(!failure.message.is_empty());
}

/// The advertised set (`memory_sources.supported_toolkits`, sourced from the
/// provider registry) and the syncable set (`build_pipeline`) must not
/// diverge: a toolkit that is advertised but has no pipeline reports ACTIVE
/// and then silently never ingests — the exact defect of #4957.
///
/// Both directions are asserted against an explicit built-in slug set. The
/// provider registry is process-global and sibling tests register throwaway
/// providers into it without unregistering, so walking it directly would be
/// order-flaky; pinning the built-in set keeps this deterministic.
#[test]
fn advertised_and_syncable_toolkit_sets_cannot_diverge() {
    init_default_composio_sync_providers();

    // Every syncable toolkit must have a registered provider — otherwise it
    // could never be advertised or auto-registered in the first place.
    for &slug in syncable_composio_toolkits() {
        assert!(
            get_composio_sync_provider(slug).is_some(),
            "syncable toolkit `{slug}` has no registered memory-sync provider"
        );
    }

    // Every built-in provider shipped by `init_default_composio_sync_providers`
    // must be syncable. This is the #4957 direction: advertising a provider
    // that `build_pipeline` rejects is the silent failure we guard against.
    //
    // We pin the built-in slug set explicitly rather than walking
    // `all_composio_sync_providers()`: that registry is process-global and
    // sibling tests register throwaway providers into it that they never
    // unregister (e.g. `provideronly` in composio/tools_tests.rs, `stub-no-active`
    // in composio/identity.rs), so a raw registry walk fails nondeterministically
    // depending on test execution order. A new built-in toolkit must be added to
    // this list, to `syncable_composio_toolkits`, and to `build_pipeline` together
    // — the assert_eq below fails loudly if the first two ever drift apart.
    const BUILTIN_SYNC_PROVIDERS: &[&str] =
        &["clickup", "github", "gmail", "linear", "notion", "slack"];

    let mut builtin = BUILTIN_SYNC_PROVIDERS.to_vec();
    builtin.sort_unstable();
    let mut syncable = syncable_composio_toolkits().to_vec();
    syncable.sort_unstable();
    assert_eq!(
        builtin, syncable,
        "the built-in provider set and syncable set diverged — a provider is \
         advertised without a matching `build_pipeline` arm, or vice versa (#4957)"
    );

    for &slug in BUILTIN_SYNC_PROVIDERS {
        assert!(
            get_composio_sync_provider(slug).is_some(),
            "built-in provider `{slug}` is not registered by \
             init_default_composio_sync_providers"
        );
        assert!(
            is_composio_toolkit_syncable(slug),
            "built-in provider `{slug}` is advertised but has no build_pipeline arm — \
             it would report ACTIVE and silently fail to sync (#4957)"
        );
    }
}

/// Behavioural regression for #4957: an unsupported Composio toolkit is
/// rejected by `build_pipeline` *before* any credential/client resolution.
///
/// We hand it a default `Config` (no Composio auth configured). If the gate
/// ran AFTER config resolution we would get a config error ("backend bearer
/// token is not configured" / "direct API key is not configured"); instead
/// we must get the unsupported-toolkit error, proving the fail-closed
/// ordering that stops an unsyncable toolkit from ever reaching a pipeline.
#[test]
fn build_pipeline_refuses_composio_sources() {
    // `googlecalendar` is a real Composio toolkit with no native pipeline —
    // exactly the prod case from #4957.
    let source: MemorySourceEntry = serde_json::from_value(serde_json::json!({
        "id": "composio:googlecalendar:conn-1",
        "kind": "composio",
        "label": "googlecalendar connection",
        "toolkit": "googlecalendar",
        "connection_id": "conn-1",
    }))
    .expect("construct composio source");

    let config = tinymemory_api::host::test_support::TestHostConfig::default();
    let mut memory_config = tinycortex::memory::config::MemoryConfig::new("/tmp/openhuman-test-ws");

    // Composio never reaches this seam any more: `run_source_pipeline`
    // routes it to the engine-free pipelines (#18 §B1). The seam's job is
    // to say so, not to half-build one.
    let err = match build_pipeline(&source, &config, &mut memory_config) {
        Ok(_) => panic!("the engine seam must refuse composio sources"),
        Err(e) => e,
    };
    assert!(
        err.contains("does not build composio pipelines"),
        "expected the composio refusal, got: {err}"
    );
}

/// Locks the reported prod failures (googlecalendar / googlesheets) as
/// non-syncable, and pins case-insensitive/trimming behaviour.
#[test]
fn is_composio_toolkit_syncable_classifies_known_slugs() {
    assert!(!is_composio_toolkit_syncable("googlecalendar"));
    assert!(!is_composio_toolkit_syncable("googlesheets"));
    assert!(!is_composio_toolkit_syncable("discord"));
    assert!(!is_composio_toolkit_syncable(""));
    assert!(is_composio_toolkit_syncable("gmail"));
    assert!(is_composio_toolkit_syncable("Gmail"));
    assert!(is_composio_toolkit_syncable("  slack "));
}

/// Regression for #5473: a Composio connector sync must feed the memory tree,
/// not just the `skill-<toolkit>` document store. The TinyCortex migration
/// (#4794) dropped the tree-ingest half, so synced items stopped producing
/// `mem_tree_chunks` rows and fell out of tree-backed recall. This fails if
/// the `SkillDocSink` store path ever stops writing tree chunks again.
#[tokio::test]
async fn composio_sync_document_reaches_memory_tree() {
    use crate::store::{MemoryClient, MemoryClientRef};
    use std::sync::Arc;
    use tinycortex::memory::sync::{SkillDocSink, SkillDocument};
    use tinymemory_api::host::test_support::TestHostConfig;
    use tinymemory_api::host::MemoryHostConfig;

    crate::test_seams::init();
    let workspace = tempfile::tempdir().expect("workspace");
    let workspace_dir = workspace.path().join("workspace");

    let mut host = TestHostConfig::default();
    host.workspace_dir = workspace_dir.clone();
    let config = host.to_arc();

    let client: MemoryClientRef = Arc::new(
        MemoryClient::from_workspace_dir(workspace_dir)
            .expect("memory client initialises against a fresh workspace"),
    );
    let adapter = super::HostSyncAdapter::with_config(client, config.clone());

    // Precondition: a fresh tree is empty, so a post-store non-zero count is
    // attributable to the sync path rather than to pre-existing state.
    assert_eq!(
        crate::store::chunks::store::count_chunks(&*config).expect("count chunks"),
        0,
        "fresh workspace must start with an empty memory tree"
    );

    adapter
        .store(SkillDocument {
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

    let chunks = crate::store::chunks::store::count_chunks(&*config).expect("count chunks");
    assert!(
        chunks > 0,
        "a Composio sync must add mem_tree_chunks rows for the ingested item (#5473)"
    );

    // The chunk must carry the deterministic per-item source id
    // `{toolkit}:{connection_id}:{document_id}`; its `path_scope`
    // (`gmail:conn-1`) is what tree retrieval resolves by platform prefix.
    // A drift here is the silent "ingests but is never retrievable" trap.
    let scoped = crate::store::chunks::store::list_chunks(
        &*config,
        &tinycortex::memory::chunks::ListChunksQuery {
            source_id: Some("gmail:conn-1:gmail:msg-1".into()),
            limit: Some(8),
            ..Default::default()
        },
    )
    .expect("list chunks by source id");
    assert!(
        !scoped.is_empty(),
        "ingested chunks must be keyed by the deterministic connector source id"
    );
    assert!(
        scoped
            .iter()
            .all(|chunk| chunk.metadata.path_scope.as_deref() == Some("gmail:conn-1")),
        "connector chunks must carry the `{{toolkit}}:{{connection_id}}` tree scope so \
         query_source resolves them (gmail → email)"
    );

    // Retrievability is the real goal, and L0 chunks alone do NOT imply it:
    // `query_source` reads sealed summaries and skips unsealed trees, so
    // before a seal the freshly-ingested item is not yet retrievable.
    let before =
        crate::tree::retrieval::query_source(&*config, Some("gmail:conn-1"), None, None, None, 10)
            .await
            .expect("query_source before seal");
    assert!(
        before.hits.is_empty(),
        "an unsealed connector tree must not yet be retrievable"
    );

    // Drive the async extract worker to append the leaf, then force-seal the
    // buffer (the time-based flush path) so a level-1 summary exists.
    crate::queue::drain_until_idle(&*config)
        .await
        .expect("drain tree jobs");
    crate::tree::tree::flush::flush_stale_buffers(
        &*config,
        chrono::Duration::zero(),
        &crate::tree::tree::bucket_seal::LabelStrategy::Empty,
    )
    .await
    .expect("force-seal stale buffers");

    // Now the connector item is retrievable through the same path the
    // product uses for tree-backed recall — the property #5473 restores.
    let after =
        crate::tree::retrieval::query_source(&*config, Some("gmail:conn-1"), None, None, None, 10)
            .await
            .expect("query_source after seal");
    assert!(
        !after.hits.is_empty(),
        "a sealed connector tree must be retrievable via query_source (#5473)"
    );
}

/// The tree-ingest half of `store` is best-effort: when
/// `ingest_document_with_scope` fails, `store` must log and still return
/// `Ok(())`, so one deterministically-poisonous item cannot abort the whole
/// connector run and re-fetch the page (Composio spend) on every retry — the
/// #4947 stall that propagating the error re-created (sanil-23's review
/// blocker #2). The skill store runs first and is the source of truth, so it
/// must remain committed. This forces a real ingest failure by pointing the
/// adapter's tree-ingest `config.workspace_dir` under a regular file (so the
/// tree store cannot be created) while the skill-store client keeps a healthy
/// workspace — isolating the failure to the tree half. If `store` ever
/// propagates the ingest error again, the `.expect` on the store call fails.
#[tokio::test]
async fn tree_ingest_failure_is_tolerated_and_skill_store_is_retained() {
    use crate::store::{MemoryClient, MemoryClientRef};
    use std::sync::Arc;
    use tinycortex::memory::sync::{SkillDocSink, SkillDocument};
    use tinymemory_api::host::test_support::TestHostConfig;
    use tinymemory_api::host::MemoryHostConfig;

    crate::test_seams::init();
    let workspace = tempfile::tempdir().expect("workspace");

    // The skill store (source of truth) gets a healthy workspace …
    let client: MemoryClientRef = Arc::new(
        MemoryClient::from_workspace_dir(workspace.path().join("skill-store"))
            .expect("memory client initialises against a fresh workspace"),
    );

    // … but the tree-ingest config points at a workspace *under* a regular
    // file, so `ingest_document_with_scope` cannot create its store and
    // returns `Err` (same failure shape as the `fallible_audit_read` guard).
    let blocker = workspace.path().join("blocker");
    std::fs::write(&blocker, b"not a directory").expect("write blocker file");
    let mut host = TestHostConfig::default();
    host.workspace_dir = blocker.join("workspace");
    let config = host.to_arc();

    let adapter = super::HostSyncAdapter::with_config(client.clone(), config.clone());
    let document = SkillDocument {
        namespace_skill_id: "gmail".into(),
        connection_id: "conn-1".into(),
        document_id: "gmail:msg-1".into(),
        title: "Quarterly planning".into(),
        content: "Let's finalise the Q3 roadmap.".into(),
        toolkit: "gmail".into(),
        metadata: serde_json::json!({ "source": "composio-provider-incremental" }),
    };

    // Guard against a vacuous test: the tree-ingest half must *genuinely*
    // fail under the broken config. If the lever ever stops failing (e.g.
    // ingest resolves its store path elsewhere), this fires rather than the
    // test silently passing without exercising the tolerance path.
    assert!(
        adapter
            .ingest_document_into_memory_tree(&*config, &document)
            .await
            .is_err(),
        "the broken tree-ingest workspace must make ingest fail"
    );

    // `store` must swallow that tree-ingest failure and still succeed.
    adapter
        .store(document)
        .await
        .expect("store must tolerate a memory-tree ingest failure (best-effort tree)");

    // The skill store, committed before the tree half, still holds the item —
    // best-effort tree ingest must never cost the durable skill write.
    let skill_docs = client
        .list_documents(Some("skill-gmail"))
        .await
        .expect("list skill-gmail documents");
    let documents = skill_docs
        .get("documents")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        documents.len(),
        1,
        "the skill store must retain the synced document even when tree ingest fails"
    );
    let persisted = serde_json::to_string(&documents).expect("serialise skill documents");
    assert!(
        persisted.contains("gmail:msg-1"),
        "the retained skill document must carry the synced id"
    );
}

/// The config-less adapter (`sync_context`) has no ingest pipeline and is not
/// on the connector sync path, so it stores the skill document without
/// touching the memory tree. Guards the `None` branch of `store` from
/// regressing into a panic or an accidental (workspace-less) ingest.
#[tokio::test]
async fn config_less_adapter_skips_memory_tree_ingest() {
    use crate::store::{MemoryClient, MemoryClientRef};
    use std::sync::Arc;
    use tinycortex::memory::sync::{SkillDocSink, SkillDocument};
    use tinymemory_api::host::test_support::TestHostConfig;
    use tinymemory_api::host::MemoryHostConfig;

    crate::test_seams::init();
    let workspace = tempfile::tempdir().expect("workspace");
    let workspace_dir = workspace.path().join("workspace");

    let mut host = TestHostConfig::default();
    host.workspace_dir = workspace_dir.clone();
    let config = host.to_arc();

    let client: MemoryClientRef = Arc::new(
        MemoryClient::from_workspace_dir(workspace_dir)
            .expect("memory client initialises against a fresh workspace"),
    );
    // `new` leaves `config: None` — the config-less variant. Keep a handle
    // to the shared client so we can read the skill store back afterwards.
    let store_client = client.clone();
    let adapter = super::HostSyncAdapter::new(client);

    adapter
        .store(SkillDocument {
            namespace_skill_id: "gmail".into(),
            connection_id: "conn-1".into(),
            document_id: "gmail:msg-1".into(),
            title: "Quarterly planning".into(),
            content: "Let's finalise the Q3 roadmap and align on the launch date.".into(),
            toolkit: "gmail".into(),
            metadata: serde_json::json!({ "source": "composio-provider-incremental" }),
        })
        .await
        .expect("config-less store must still persist the skill document");

    // The skill store still receives the document (the always-on half of
    // `store`), keyed by its stable document id under `skill-gmail`.
    let skill_docs = store_client
        .list_documents(Some("skill-gmail"))
        .await
        .expect("list skill-gmail documents");
    let documents = skill_docs
        .get("documents")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        documents.len(),
        1,
        "config-less store must persist exactly the one synced skill document"
    );
    let persisted = serde_json::to_string(&documents).expect("serialise skill documents");
    assert!(
        persisted.contains("gmail:msg-1") && persisted.contains("Quarterly planning"),
        "the persisted skill document must carry the synced id and title"
    );

    // …but the tree is untouched, because the config-less adapter has no
    // ingest pipeline.
    assert_eq!(
        crate::store::chunks::store::count_chunks(&*config).expect("count chunks"),
        0,
        "a config-less adapter must not ingest into the memory tree"
    );
}

/// The blank-scope guard: an item whose toolkit is empty would form an
/// unreachable `":conn"` tree scope, so `ingest_document_into_memory_tree`
/// skips it — the skill store still receives it, the tree does not. Covers
/// the early-return branch (a valid toolkit yields chunks, as the retrieval
/// test proves; a blank one must not).
#[tokio::test]
async fn blank_scope_item_is_skipped_for_memory_tree_ingest() {
    use crate::store::{MemoryClient, MemoryClientRef};
    use std::sync::Arc;
    use tinycortex::memory::sync::{SkillDocSink, SkillDocument};
    use tinymemory_api::host::test_support::TestHostConfig;
    use tinymemory_api::host::MemoryHostConfig;

    crate::test_seams::init();
    let workspace = tempfile::tempdir().expect("workspace");
    let workspace_dir = workspace.path().join("workspace");
    let mut host = TestHostConfig::default();
    host.workspace_dir = workspace_dir.clone();
    let config = host.to_arc();
    let client: MemoryClientRef = Arc::new(
        MemoryClient::from_workspace_dir(workspace_dir).expect("memory client initialises"),
    );
    let adapter = super::HostSyncAdapter::with_config(client, config.clone());

    adapter
        .store(SkillDocument {
            namespace_skill_id: "gmail".into(),
            connection_id: "conn-1".into(),
            document_id: "gmail:msg-1".into(),
            title: "Quarterly planning".into(),
            content: "Let's finalise the Q3 roadmap.".into(),
            // Blank after trim — no platform scope can be formed.
            toolkit: "   ".into(),
            metadata: serde_json::json!({}),
        })
        .await
        .expect("store must still succeed for an item without a tree scope");

    assert_eq!(
        crate::store::chunks::store::count_chunks(&*config).expect("count chunks"),
        0,
        "an item without a toolkit/connection scope must be skipped for tree ingest"
    );
}
