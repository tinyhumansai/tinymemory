use super::*;
use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};
use tinymemory_api::conformance::ReferenceEngine;
use tinymemory_api::{
    EngineDescriptor, EngineHealth, Error, FetchPage, FetchRequest, ForgetReport, ForgetTarget,
    ListPage, ListRequest, MemoryMeta, RecallAnswer, RecallRequest, StoreReceipt,
};

fn source(id: &str, kind: SourceKind, mins: Option<u32>) -> RegisteredSource {
    RegisteredSource {
        id: id.into(),
        kind,
        target: "/tmp/notes".into(),
        label: id.into(),
        schedule_mins: mins,
    }
}

#[derive(Default)]
struct FakeRegistry {
    sources: Vec<RegisteredSource>,
    states: Mutex<std::collections::HashMap<String, SyncState>>,
    updates: Mutex<Vec<(String, SyncUpdate)>>,
}

impl SourceRegistry for FakeRegistry {
    fn sources(&self) -> Vec<RegisteredSource> {
        self.sources.clone()
    }
    fn state(&self, id: &str) -> Option<SyncState> {
        self.states.lock().unwrap().get(id).cloned()
    }
    fn record(&self, id: &str, update: SyncUpdate) {
        let mut states = self.states.lock().unwrap();
        let state = states.entry(id.to_string()).or_default();
        match &update {
            SyncUpdate::Started => state.status = SyncStatus::Syncing,
            SyncUpdate::Finished { at, .. } => {
                state.status = SyncStatus::Idle;
                state.last_sync_at = Some(*at);
            }
            SyncUpdate::Failed { at, .. } => {
                state.status = SyncStatus::Error;
                state.last_sync_at = Some(*at);
            }
        }
        self.updates.lock().unwrap().push((id.to_string(), update));
    }
}

fn syncer(sources: Vec<RegisteredSource>) -> (Syncer, Arc<FakeRegistry>) {
    let registry = Arc::new(FakeRegistry {
        sources,
        ..FakeRegistry::default()
    });
    (Syncer::new(registry.clone()), registry)
}

#[test]
fn a_second_request_for_a_syncing_source_starts_nothing() {
    let (syncer, registry) = syncer(vec![source("a", SourceKind::Folder, None)]);
    let first = syncer.start(Some("a")).unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(registry.state("a").unwrap().status, SyncStatus::Syncing);
    assert!(syncer.start(Some("a")).unwrap().is_empty());
    assert!(syncer.start(None).unwrap().is_empty());
    drop(first);
    assert_eq!(syncer.start(Some("a")).unwrap().len(), 1, "a drop releases");
}

#[test]
fn an_unknown_source_is_invalid() {
    let (syncer, _) = syncer(vec![]);
    assert!(matches!(
        syncer.start(Some("nope")),
        Err(SyncError::Invalid(_))
    ));
}

#[test]
fn finishing_records_the_outcome_and_releases_the_claim() {
    let (syncer, registry) = syncer(vec![source("a", SourceKind::Folder, None)]);
    let claim = syncer.start(Some("a")).unwrap().remove(0);
    syncer.finish(claim, &Ok(7));
    assert_eq!(registry.state("a").unwrap().status, SyncStatus::Idle);
    let claim = syncer.start(Some("a")).unwrap().remove(0);
    syncer.finish(claim, &Err(SyncError::Read("boom".into())));
    let state = registry.state("a").unwrap();
    assert_eq!(state.status, SyncStatus::Error);
    let updates = registry.updates.lock().unwrap();
    assert!(matches!(
        updates.last(),
        Some((_, SyncUpdate::Failed { error, .. })) if error.contains("boom")
    ));
    assert!(matches!(
        updates[1].1,
        SyncUpdate::Finished { items: 7, .. }
    ));
}

#[test]
fn due_follows_the_schedule_and_never_picks_a_syncing_source() {
    let now = Utc::now();
    let idle = |ago: i64| SyncState {
        status: SyncStatus::Idle,
        last_sync_at: Some(now - Duration::minutes(ago)),
    };
    assert!(
        !is_due(None, None, now),
        "no schedule syncs on request only"
    );
    assert!(is_due(Some(60), None, now), "never ran");
    assert!(!is_due(Some(60), Some(&idle(30)), now));
    assert!(is_due(Some(60), Some(&idle(61)), now));
    assert!(
        !is_due(Some(1), Some(&idle(10)), now),
        "floor of 15 minutes"
    );
    assert!(is_due(Some(1), Some(&idle(16)), now));
    let syncing = SyncState {
        status: SyncStatus::Syncing,
        last_sync_at: None,
    };
    assert!(!is_due(Some(60), Some(&syncing), now));

    let (syncer, registry) = syncer(vec![
        source("due", SourceKind::Folder, Some(60)),
        source("manual", SourceKind::Folder, None),
    ]);
    assert_eq!(syncer.due(now), ["due"]);
    registry.record("due", SyncUpdate::Started);
    assert!(syncer.due(now).is_empty());
}

#[test]
fn entries_carry_the_target_and_the_per_kind_caps() {
    let folder = entry_for(&source("f", SourceKind::Folder, None)).unwrap();
    assert_eq!(folder.path.as_deref(), Some("/tmp/notes"));
    assert_eq!(folder.url, None);

    let mut github = source("g", SourceKind::GithubRepo, None);
    github.target = "https://github.com/acme/widgets".into();
    let entry = entry_for(&github).unwrap();
    assert_eq!(
        entry.url.as_deref(),
        Some("https://github.com/acme/widgets")
    );
    assert_eq!(
        (entry.max_prs, entry.max_issues, entry.max_commits),
        (Some(10), Some(10), Some(50))
    );

    let mut feed = source("r", SourceKind::RssFeed, None);
    feed.target = "https://example.com/feed.xml".into();
    assert_eq!(entry_for(&feed).unwrap().max_items, Some(20));
}

#[test]
fn an_invalid_entry_is_refused() {
    let mut bad = source("has:colon", SourceKind::Folder, None);
    bad.id = "has:colon".into();
    assert!(matches!(entry_for(&bad), Err(SyncError::Invalid(_))));
}

#[test]
fn account_wide_failures_are_told_from_item_failures() {
    let wide = |error| SyncError::Engine(error).is_account_wide();
    assert!(wide(Error::Off("disabled".into())));
    assert!(wide(Error::Unauthorized("x".into())));
    assert!(wide(Error::InsufficientCredits("top up".into())));
    assert!(wide(Error::Unauthorized(
        "[MEMORY_PLAN_REQUIRED] not in the plan".into()
    )));
    assert!(!wide(Error::Unavailable("request timed out".into())));
    assert!(!wide(Error::Unavailable("rate limited".into())));
    assert!(!wide(Error::Engine("[OTHER] nope".into())));
    assert!(!wide(Error::InvalidRequest("x".into())));
    assert!(!SyncError::Read("x".into()).is_account_wide());
}

/// Refuses batches (and one text as an item), or the whole account.
struct Scripted {
    inner: ReferenceEngine,
    refuse_batches: bool,
    bad_text: Option<&'static str>,
    account: Option<Error>,
    batches: AtomicUsize,
    singles: AtomicUsize,
}

impl Scripted {
    fn new() -> Self {
        Self {
            inner: ReferenceEngine::new(),
            refuse_batches: false,
            bad_text: None,
            account: None,
            batches: AtomicUsize::new(0),
            singles: AtomicUsize::new(0),
        }
    }

    fn bad(&self, item: &StoreItem) -> bool {
        self.bad_text
            .is_some_and(|text| format!("{item:?}").contains(text))
    }
}

#[async_trait]
impl MemoryEngine for Scripted {
    fn descriptor(&self) -> &EngineDescriptor {
        self.inner.descriptor()
    }
    async fn health(&self) -> EngineHealth {
        self.inner.health().await
    }
    async fn recall(&self, req: RecallRequest) -> tinymemory_api::Result<RecallAnswer> {
        self.inner.recall(req).await
    }
    async fn fetch(&self, req: FetchRequest) -> tinymemory_api::Result<FetchPage> {
        self.inner.fetch(req).await
    }
    async fn store(&self, item: StoreItem) -> tinymemory_api::Result<StoreReceipt> {
        self.singles.fetch_add(1, Ordering::SeqCst);
        if let Some(error) = &self.account {
            return Err(error.clone());
        }
        if self.bad(&item) {
            return Err(Error::InvalidRequest("too large".into()));
        }
        self.inner.store(item).await
    }
    async fn store_many(&self, items: Vec<StoreItem>) -> tinymemory_api::Result<Vec<StoreReceipt>> {
        self.batches.fetch_add(1, Ordering::SeqCst);
        if let Some(error) = &self.account {
            return Err(error.clone());
        }
        if self.refuse_batches || items.iter().any(|item| self.bad(item)) {
            return Err(Error::InvalidRequest("batch refused".into()));
        }
        self.inner.store_many(items).await
    }
    async fn forget(&self, target: ForgetTarget) -> tinymemory_api::Result<ForgetReport> {
        self.inner.forget(target).await
    }
    async fn list(&self, req: ListRequest) -> tinymemory_api::Result<ListPage> {
        self.inner.list(req).await
    }
}

fn placed(count: usize, node: &str) -> Vec<(Namespace, StoreItem)> {
    let node: Namespace = node.parse().unwrap();
    (0..count)
        .map(|index| {
            let item = StoreItem::document(
                format!("body {index}"),
                MemoryMeta {
                    namespace: node.clone(),
                    ..MemoryMeta::default()
                },
            );
            (node.clone(), item)
        })
        .collect()
}

#[tokio::test]
async fn items_go_in_batches_of_the_contract_cap() {
    let engine = Scripted::new();
    let stored = store_all(&engine, "s", placed(STORE_CHUNK * 2 + 1, "source:files"))
        .await
        .unwrap();
    assert_eq!(stored.stored as usize, STORE_CHUNK * 2 + 1);
    assert_eq!(engine.batches.load(Ordering::SeqCst), 3);
    assert_eq!(engine.singles.load(Ordering::SeqCst), 0);
    assert_eq!(stored.touched.len(), 1);
}

#[tokio::test]
async fn one_invalid_item_costs_only_itself() {
    let mut engine = Scripted::new();
    engine.bad_text = Some("body 2");
    let stored = store_all(&engine, "s", placed(5, "source:files"))
        .await
        .unwrap();
    assert_eq!((stored.stored, stored.failed), (4, 1));
    assert_eq!(engine.singles.load(Ordering::SeqCst), 5);
}

#[tokio::test]
async fn nothing_stored_surfaces_the_item_error() {
    let mut engine = Scripted::new();
    engine.refuse_batches = true;
    engine.bad_text = Some("body");
    let error = store_all(&engine, "s", placed(2, "source:files"))
        .await
        .unwrap_err();
    assert!(matches!(error, SyncError::Engine(Error::InvalidRequest(_))));
}

#[tokio::test]
async fn an_account_wide_refusal_stops_without_retrying_item_by_item() {
    for error in [
        Error::Off("disabled".into()),
        Error::Unauthorized("rejected".into()),
        Error::InsufficientCredits("top up".into()),
        Error::Unauthorized("[MEMORY_PLAN_REQUIRED] not entitled".into()),
    ] {
        let mut engine = Scripted::new();
        engine.account = Some(error);
        let error = store_all(&engine, "s", placed(3, "source:files"))
            .await
            .unwrap_err();
        assert!(error.is_account_wide());
        assert_eq!(engine.singles.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn request_timeouts_and_rate_limits_retry_batch_items_individually() {
    for message in ["request timed out", "rate limited"] {
        let mut engine = Scripted::new();
        engine.account = Some(Error::Unavailable(message.into()));
        let error = store_all(&engine, "s", placed(3, "source:files"))
            .await
            .unwrap_err();
        assert!(matches!(error, SyncError::Engine(Error::Unavailable(_))));
        assert!(!error.is_account_wide());
        assert_eq!(engine.batches.load(Ordering::SeqCst), 1);
        assert_eq!(
            engine.singles.load(Ordering::SeqCst),
            3,
            "a request-specific transient batch failure must not stop its siblings"
        );
    }
}

#[tokio::test]
async fn touched_nodes_are_the_ones_that_received_items() {
    let engine = Scripted::new();
    let mut items = placed(1, "source:files");
    items.extend(placed(1, "source:github/project:acme--widgets"));
    let stored = store_all(&engine, "s", items).await.unwrap();
    let nodes: Vec<String> = stored.touched.iter().map(ToString::to_string).collect();
    assert_eq!(nodes.len(), 2);
    assert!(nodes.contains(&"source:files".to_string()));
}

#[tokio::test]
async fn store_placed_files_items_and_builds_one_job_per_touched_node() {
    use tinymemory_api::{ConsolidateRequest, Consolidation, ItemKind, Reach};
    use tinymemory_tools::BackgroundJob;

    let engine = ReferenceEngine::new().with_consolidation(Consolidation::OnDemand);
    let files: Namespace = "team:acme/source:files".parse().unwrap();
    let github: Namespace = "team:acme/source:github".parse().unwrap();
    let items = vec![
        StoreItem::document(
            "notes",
            MemoryMeta {
                agent_id: Some("agent-1".into()),
                ..MemoryMeta::default()
            },
        ),
        StoreItem::document("repo", MemoryMeta::default()),
        StoreItem::document("another note", MemoryMeta::default()),
    ];
    let mut index = 0;
    let result = store_placed(&engine, "source-1", items, |_| {
        index += 1;
        Ok(if index == 2 {
            github.clone()
        } else {
            files.clone()
        })
    })
    .await
    .unwrap();

    assert_eq!(result.stored.stored, 3);
    assert_eq!(
        result.stored.touched,
        [files.clone(), github.clone()].into()
    );
    assert_eq!(
        result.jobs,
        vec![
            BackgroundJob::BuildBeliefs {
                request: ConsolidateRequest::new(Reach::exact(files.clone()))
                    .kinds([ItemKind::Document]),
            },
            BackgroundJob::BuildBeliefs {
                request: ConsolidateRequest::new(Reach::exact(github.clone()))
                    .kinds([ItemKind::Document]),
            },
        ]
    );
    let listed = engine
        .list(tinymemory_api::ListRequest::new(
            tinymemory_api::MetaFilter::default(),
            10,
        ))
        .await
        .unwrap();
    assert_eq!(listed.items[0].meta.namespace, files);
    assert_eq!(listed.items[0].meta.agent_id, None);
}

#[tokio::test]
async fn store_placed_skips_jobs_without_on_demand_consolidation() {
    use tinymemory_api::Consolidation;

    let node: Namespace = "source:files".parse().unwrap();
    for mode in [
        Consolidation::None,
        Consolidation::Scheduled,
        Consolidation::Automatic,
    ] {
        let engine = ReferenceEngine::new().with_consolidation(mode);
        let result = store_placed(
            &engine,
            "source-1",
            vec![StoreItem::document("notes", MemoryMeta::default())],
            |_| Ok(node.clone()),
        )
        .await
        .unwrap();
        assert_eq!(result.stored.stored, 1, "{mode:?}");
        assert!(result.jobs.is_empty(), "{mode:?}");
    }
}

#[cfg(feature = "sources-network")]
#[tokio::test]
async fn a_folder_source_is_read_into_items() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.md"), "# Title\n\nSome notes.").unwrap();
    let mut folder = source("f", SourceKind::Folder, None);
    folder.target = dir.path().display().to_string();
    let converter = crate::documents::NativeConverter;
    let collected = read_source(&folder, dir.path(), &converter).await.unwrap();
    assert_eq!(collected.items.len(), 1);
}
