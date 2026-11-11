//! Request shaping, confinement, and deletion behavior for memory operations.

use std::sync::atomic::{AtomicUsize, Ordering};

use chrono::{NaiveDate, TimeZone, Utc};
use tinymemory_api::conformance::ReferenceEngine;
use tinymemory_api::{
    EngineDescriptor, EngineHealth, EraseRequest, ExplorePage, ExploreRequest, Facet, FetchMode,
    FetchPage, FetchRequest, ForgetReport, ForgetTarget, GetRequest, Hit, ItemId, LearningKind,
    ListPage, ListRequest, MemoryEngine, MemoryMeta, MetaFilter, Namespace, Reach, RecallAnswer,
    RecallMode, RecallRequest, SourceKind, SourceRef, StoreItem, StoreReceipt, WriteOptions,
};

use crate::ops::{
    DEFAULT_BUCKETS, DEFAULT_LEARNING_CONFIDENCE, DEFAULT_LIMIT, DateRange, ExploreOptions,
    FetchOptions, LearningOptions, ListOptions, MAX_BUCKETS, MAX_LIMIT, MAX_PATH_STEPS, MemoryOps,
    PathStep, RecallOptions, clamp_limit, time_hint,
};
use crate::{Confinement, Policy};

struct CountingEngine {
    inner: ReferenceEngine,
    descriptor: EngineDescriptor,
    explores: AtomicUsize,
    forgets: AtomicUsize,
    lists: AtomicUsize,
    gets: AtomicUsize,
    fail_forget: bool,
}

impl CountingEngine {
    fn new(fail_forget: bool) -> Self {
        let inner = ReferenceEngine::new();
        let descriptor = inner.descriptor().clone();
        Self {
            inner,
            descriptor,
            explores: AtomicUsize::new(0),
            forgets: AtomicUsize::new(0),
            lists: AtomicUsize::new(0),
            gets: AtomicUsize::new(0),
            fail_forget,
        }
    }
}

#[async_trait::async_trait]
impl MemoryEngine for CountingEngine {
    fn descriptor(&self) -> &EngineDescriptor {
        &self.descriptor
    }

    async fn health(&self) -> EngineHealth {
        self.inner.health().await
    }

    async fn recall(&self, request: RecallRequest) -> tinymemory_api::Result<RecallAnswer> {
        self.inner.recall(request).await
    }

    async fn fetch(&self, request: FetchRequest) -> tinymemory_api::Result<FetchPage> {
        self.inner.fetch(request).await
    }

    async fn store(&self, item: StoreItem) -> tinymemory_api::Result<StoreReceipt> {
        self.inner.store(item).await
    }

    async fn store_with(
        &self,
        item: StoreItem,
        options: WriteOptions,
    ) -> tinymemory_api::Result<StoreReceipt> {
        self.inner.store_with(item, options).await
    }

    async fn forget(&self, target: ForgetTarget) -> tinymemory_api::Result<ForgetReport> {
        self.forgets.fetch_add(1, Ordering::Relaxed);
        if self.fail_forget {
            return Err(tinymemory_api::Error::InvalidRequest("engine down".into()));
        }
        self.inner.forget(target).await
    }

    async fn list(&self, request: ListRequest) -> tinymemory_api::Result<ListPage> {
        self.lists.fetch_add(1, Ordering::Relaxed);
        self.inner.list(request).await
    }

    async fn get(&self, request: GetRequest) -> tinymemory_api::Result<Vec<Hit>> {
        self.gets.fetch_add(1, Ordering::Relaxed);
        self.inner.get(request).await
    }

    async fn erase(
        &self,
        request: EraseRequest,
    ) -> tinymemory_api::Result<tinymemory_api::EraseReport> {
        self.inner.erase(request).await
    }

    async fn explore(&self, request: ExploreRequest) -> tinymemory_api::Result<ExplorePage> {
        self.explores.fetch_add(1, Ordering::Relaxed);
        self.inner.explore(request).await
    }
}

fn namespace(value: &str) -> Namespace {
    value.parse().unwrap()
}

fn operations(policy: Policy) -> MemoryOps {
    MemoryOps::new(Confinement {
        allowed: Reach::subtree(namespace("team:acme")),
        policy,
    })
}

fn step(facet: Facet, value: &str) -> PathStep {
    PathStep {
        facet,
        value: value.to_string(),
    }
}

fn document(text: &str, folder: &str) -> StoreItem {
    let mut meta = MemoryMeta::from_source(SourceKind::Folder, Some("notes".into()));
    meta.namespace = namespace("team:acme/agent:writer");
    meta.folder = Some(folder.to_string());
    meta.file_path = Some(format!("{folder}/{text}.md"));
    StoreItem::document(text, meta)
}

fn learning_in(namespace_path: &str, text: &str, tags: &[&str]) -> StoreItem {
    StoreItem::learning(
        text,
        LearningKind::Fact,
        0.8,
        MemoryMeta {
            namespace: namespace(namespace_path),
            tags: tags.iter().map(|tag| (*tag).to_string()).collect(),
            ..MemoryMeta::default()
        },
    )
}

#[test]
fn request_limits_and_learning_confidence_have_one_public_default() {
    assert_eq!(DEFAULT_LIMIT, 10);
    assert_eq!(MAX_LIMIT, 100);
    assert_eq!(DEFAULT_BUCKETS, 50);
    assert_eq!(MAX_BUCKETS, 500);
    assert_eq!(DEFAULT_LEARNING_CONFIDENCE, 0.8);
    assert_eq!(clamp_limit(None), DEFAULT_LIMIT);
    assert_eq!(clamp_limit(Some(0)), 1);
    assert_eq!(clamp_limit(Some(usize::MAX)), MAX_LIMIT);
}

#[test]
fn recall_fetch_and_list_requests_apply_limits_and_confinement() {
    let ops = operations(Policy::Clamp);
    let recall = ops
        .recall_request(RecallOptions {
            question: "coffee".into(),
            filter: Some(MetaFilter {
                reach: Some(Reach::subtree(Namespace::ROOT)),
                workspace: Some("repo".into()),
                ..MetaFilter::default()
            }),
            limit: Some(usize::MAX),
            instructions: None,
            refers_to: None,
            exclude_thread: None,
            mode: RecallMode::Answer,
        })
        .unwrap();
    assert_eq!(recall.limit, MAX_LIMIT);
    assert_eq!(
        recall.filter.reach,
        Some(ops.confinement().unwrap().allowed.clone())
    );
    assert_eq!(recall.filter.workspace.as_deref(), Some("repo"));

    let descriptor = ReferenceEngine::new().descriptor().clone();
    let fetch = ops
        .fetch_request(
            FetchOptions {
                query: "coffee".into(),
                mode: None,
                filter: None,
                limit: None,
                cursor: None,
                refers_to: None,
                exclude_thread: None,
            },
            &descriptor,
        )
        .unwrap();
    assert_eq!(fetch.mode, *descriptor.fetch_modes.first().unwrap());
    assert_eq!(fetch.limit, DEFAULT_LIMIT);
    assert_eq!(
        fetch.filter.reach,
        Some(ops.confinement().unwrap().allowed.clone())
    );

    let list = ops
        .list_request(ListOptions {
            filter: None,
            path: Vec::new(),
            limit: Some(usize::MAX),
            cursor: None,
        })
        .unwrap();
    assert_eq!(list.limit, MAX_LIMIT);
    assert_eq!(
        list.filter.reach,
        Some(ops.confinement().unwrap().allowed.clone())
    );
}

#[test]
fn fetch_uses_engine_descriptor_and_refuses_unsupported_modes() {
    let ops = operations(Policy::Refuse);
    let descriptor = EngineDescriptor {
        fetch_modes: vec![FetchMode::Keyword],
        ..ReferenceEngine::new().descriptor().clone()
    };
    let defaulted = ops
        .fetch_request(
            FetchOptions {
                query: "x".into(),
                mode: None,
                filter: None,
                limit: None,
                cursor: None,
                refers_to: None,
                exclude_thread: None,
            },
            &descriptor,
        )
        .unwrap();
    assert_eq!(defaulted.mode, FetchMode::Keyword);

    let error = ops
        .fetch_request(
            FetchOptions {
                query: "x".into(),
                mode: Some(FetchMode::Vector),
                filter: None,
                limit: None,
                cursor: None,
                refers_to: None,
                exclude_thread: None,
            },
            &descriptor,
        )
        .unwrap_err();
    assert!(matches!(error, tinymemory_api::Error::Unsupported(_)));

    let empty = EngineDescriptor {
        fetch_modes: Vec::new(),
        ..descriptor
    };
    assert!(
        ops.fetch_request(
            FetchOptions {
                query: "x".into(),
                mode: None,
                filter: None,
                limit: None,
                cursor: None,
                refers_to: None,
                exclude_thread: None,
            },
            &empty,
        )
        .is_err()
    );
}

#[test]
fn explorer_paths_validate_and_confine_before_a_request_can_be_called() {
    let ops = operations(Policy::Refuse);
    let request = ops
        .explore_request(ExploreOptions {
            facet: Facet::Folder,
            path: vec![step(Facet::Kind, "document")],
            filter: None,
            limit: None,
            scan_limit: None,
        })
        .unwrap();
    assert_eq!(request.limit, DEFAULT_BUCKETS);
    assert_eq!(request.filter.kinds, [tinymemory_api::ItemKind::Document]);
    assert_eq!(
        request.filter.reach,
        Some(ops.confinement().unwrap().allowed.clone())
    );

    for path in [
        vec![step(Facet::Kind, "unknown-kind")],
        vec![step(Facet::Tag, "  ")],
        (0..=MAX_PATH_STEPS)
            .map(|i| step(Facet::Tag, &i.to_string()))
            .collect(),
    ] {
        assert!(
            ops.explore_request(ExploreOptions {
                facet: Facet::Kind,
                path,
                filter: None,
                limit: None,
                scan_limit: None,
            })
            .is_err()
        );
    }
    assert!(
        ops.list_request(ListOptions {
            filter: None,
            path: vec![step(Facet::Namespace, "team:other")],
            limit: None,
            cursor: None,
        })
        .is_err()
    );
}

#[tokio::test]
async fn invalid_explorer_paths_never_call_the_engine() {
    let ops = operations(Policy::Clamp);
    let engine = CountingEngine::new(false);
    assert!(
        ops.explore(
            &engine,
            ExploreOptions {
                facet: Facet::Kind,
                path: vec![step(Facet::Kind, "not-a-kind")],
                filter: None,
                limit: None,
                scan_limit: None,
            },
        )
        .await
        .is_err()
    );
    assert_eq!(engine.explores.load(Ordering::Relaxed), 0);
}

#[test]
fn time_hint_preserves_inclusive_local_dates_and_rejects_bad_ranges_or_zones() {
    let from = NaiveDate::from_ymd_opt(2026, 2, 3).unwrap();
    let to = NaiveDate::from_ymd_opt(2026, 2, 5).unwrap();
    let hint = time_hint(Some(DateRange { from, to: Some(to) }), Some("Asia/Kolkata"))
        .unwrap()
        .unwrap();
    assert_eq!(hint.from, from);
    assert_eq!(hint.to, to);
    assert_eq!(hint.zone.as_deref(), Some("Asia/Kolkata"));

    assert!(
        time_hint(
            Some(DateRange {
                from: to,
                to: Some(from)
            }),
            Some("UTC")
        )
        .is_err()
    );
    assert!(time_hint(Some(DateRange { from, to: None }), Some("not/a-zone")).is_err());
    assert!(time_hint(None, None).unwrap().is_none());
}

#[test]
fn learning_item_merges_host_metadata_and_applies_confinement() {
    let ops = operations(Policy::Clamp);
    let item = ops
        .learning_item(LearningOptions {
            text: "  prefers tea  ".into(),
            kind: Some(LearningKind::Preference),
            confidence: None,
            meta: Some(MemoryMeta {
                thread_id: Some("forged".into()),
                repo: Some("owner/repo".into()),
                channel: Some("forged-channel".into()),
                tags: vec!["caller".into()],
                namespace: namespace("team:other"),
                ..MemoryMeta::default()
            }),
            host_meta: Some(MemoryMeta {
                thread_id: Some("thread-1".into()),
                channel: Some("telegram".into()),
                tags: vec!["host".into(), "caller".into()],
                source: SourceRef {
                    kind: SourceKind::Agent,
                    id: None,
                },
                ..MemoryMeta::default()
            }),
            default_namespace: namespace("team:acme/agent:writer"),
            now: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
        })
        .unwrap();
    assert_eq!(item.meta().thread_id.as_deref(), Some("thread-1"));
    assert_eq!(item.meta().repo.as_deref(), Some("owner/repo"));
    assert_eq!(item.meta().channel.as_deref(), Some("telegram"));
    assert_eq!(item.meta().namespace, namespace("team:acme/agent:writer"));
    assert_eq!(item.meta().tags, ["caller", "host"]);
    match item {
        StoreItem::Learning {
            text,
            confidence,
            meta,
            ..
        } => {
            assert_eq!(text, "prefers tea");
            assert_eq!(confidence, DEFAULT_LEARNING_CONFIDENCE);
            assert_eq!(
                meta.observed_at.unwrap(),
                Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
            );
        }
        other => panic!("expected a learning item, got {:?}", other.kind()),
    }
}

#[test]
fn learning_confidence_must_be_finite_and_within_unit_interval() {
    let ops = operations(Policy::Clamp);
    for confidence in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -0.1, 1.1] {
        assert!(
            ops.learning_item(LearningOptions {
                text: "fact".into(),
                kind: None,
                confidence: Some(confidence),
                meta: None,
                host_meta: None,
                default_namespace: namespace("team:acme/agent:writer"),
                now: Utc::now(),
            })
            .is_err()
        );
    }
    assert!(
        ops.learning_item(LearningOptions {
            text: "  ".into(),
            kind: None,
            confidence: None,
            meta: None,
            host_meta: None,
            default_namespace: namespace("team:acme/agent:writer"),
            now: Utc::now(),
        })
        .is_err()
    );
}

#[test]
fn clamp_policy_moves_stored_items_to_the_allowed_root() {
    let item = operations(Policy::Clamp)
        .store_item(StoreItem::learning(
            "outside",
            LearningKind::Fact,
            0.8,
            MemoryMeta {
                namespace: namespace("team:other"),
                ..MemoryMeta::default()
            },
        ))
        .unwrap();
    assert_eq!(item.meta().namespace, namespace("team:acme"));
}

#[tokio::test]
async fn explorer_counts_and_drills_down_then_lists_the_matching_path() {
    let ops = operations(Policy::Clamp);
    let engine = CountingEngine::new(false);
    for item in [
        document("alpha", "/notes"),
        document("beta", "/notes"),
        document("gamma", "/notes/deep"),
        StoreItem::learning(
            "prefers tea",
            LearningKind::Preference,
            0.9,
            MemoryMeta {
                namespace: namespace("team:acme/agent:writer"),
                source: SourceRef {
                    kind: SourceKind::Agent,
                    id: None,
                },
                ..MemoryMeta::default()
            },
        ),
    ] {
        engine.store(item).await.unwrap();
    }

    let kinds = ops
        .explore(
            &engine,
            ExploreOptions {
                facet: Facet::Kind,
                path: Vec::new(),
                filter: None,
                limit: None,
                scan_limit: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(kinds.total, 4);
    assert_eq!(
        kinds
            .buckets
            .iter()
            .map(|bucket| (bucket.value.as_str(), bucket.count))
            .collect::<Vec<_>>(),
        [("document", 3), ("learning", 1)]
    );

    let folders = ops
        .explore(
            &engine,
            ExploreOptions {
                facet: Facet::Folder,
                path: vec![step(Facet::Kind, "document")],
                filter: None,
                limit: None,
                scan_limit: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(folders.total, 3);
    assert_eq!(folders.buckets[0].value, "/notes");
    assert_eq!(folders.buckets[0].count, 2);

    let listed = ops
        .list(
            &engine,
            ListOptions {
                path: vec![step(Facet::Kind, "document"), step(Facet::Folder, "/notes")],
                ..ListOptions::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(listed.items.len(), 3);
}

#[tokio::test]
async fn explorer_path_narrows_on_top_of_the_supplied_filter() {
    let ops = operations(Policy::Clamp);
    let engine = CountingEngine::new(false);
    engine
        .store(StoreItem::learning(
            "fact",
            LearningKind::Fact,
            0.9,
            MemoryMeta {
                namespace: namespace("team:acme/agent:writer"),
                source: SourceRef {
                    kind: SourceKind::Agent,
                    id: None,
                },
                ..MemoryMeta::default()
            },
        ))
        .await
        .unwrap();
    engine
        .store(StoreItem::document(
            "doc",
            MemoryMeta {
                namespace: namespace("team:acme/agent:writer"),
                source: SourceRef {
                    kind: SourceKind::File,
                    id: None,
                },
                ..MemoryMeta::default()
            },
        ))
        .await
        .unwrap();

    let page = ops
        .explore(
            &engine,
            ExploreOptions {
                facet: Facet::Source,
                path: vec![step(Facet::Source, "agent")],
                filter: Some(MetaFilter::kinds([tinymemory_api::ItemKind::Learning])),
                limit: None,
                scan_limit: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(page.buckets[0].value, "agent");
}

#[tokio::test]
async fn items_get_keeps_asked_order_and_exact_reach() {
    let ops = operations(Policy::Clamp);
    let engine = CountingEngine::new(false);
    let root_item = engine
        .store(StoreItem::learning(
            "at root",
            LearningKind::Fact,
            0.9,
            MemoryMeta {
                namespace: namespace("team:acme"),
                ..MemoryMeta::default()
            },
        ))
        .await
        .unwrap()
        .id;
    let child_item = engine
        .store(StoreItem::learning(
            "at child",
            LearningKind::Fact,
            0.9,
            MemoryMeta {
                namespace: namespace("team:acme/agent:writer"),
                ..MemoryMeta::default()
            },
        ))
        .await
        .unwrap()
        .id;
    let view = ops
        .items_get(
            &engine,
            vec![
                child_item.clone(),
                ItemId::new("missing"),
                root_item.clone(),
            ],
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        view.iter().map(|hit| hit.id.clone()).collect::<Vec<_>>(),
        [child_item, root_item.clone()]
    );
    assert_eq!(view[0].text, "at child");
    assert_eq!(view[1].text, "at root");

    let exact = ops
        .items_get(
            &engine,
            vec![
                root_item.clone(),
                ItemId::new("missing"),
                view[0].id.clone(),
            ],
            Some(Reach::exact(namespace("team:acme"))),
        )
        .await
        .unwrap();
    assert_eq!(exact.len(), 1);
    assert_eq!(exact[0].id, root_item);
}

#[tokio::test]
async fn within_reach_chunks_more_ids_than_one_get_request_allows() {
    let engine = CountingEngine::new(false);
    let mut ids = Vec::new();
    for index in 0..=tinymemory_api::explore::MAX_GET_IDS {
        ids.push(
            engine
                .store(StoreItem::learning(
                    format!("fact {index}"),
                    LearningKind::Fact,
                    0.9,
                    MemoryMeta {
                        namespace: namespace("team:acme/agent:writer"),
                        ..MemoryMeta::default()
                    },
                ))
                .await
                .unwrap()
                .id,
        );
    }
    let found =
        crate::ops::within_reach(&engine, ids.clone(), Reach::subtree(namespace("team:acme")))
            .await
            .unwrap();
    assert_eq!(found, ids);
}

#[tokio::test]
async fn learn_recall_fetch_list_and_forget_round_trip() {
    let ops = operations(Policy::Clamp);
    let engine = CountingEngine::new(false);
    let item = ops
        .learning_item(LearningOptions {
            text: "The user prefers dark roast coffee".into(),
            kind: None,
            confidence: None,
            meta: None,
            host_meta: None,
            default_namespace: namespace("team:acme/agent:writer"),
            now: Utc::now(),
        })
        .unwrap();
    let stored = engine.store(item).await.unwrap();

    let recall = ops
        .recall_request(RecallOptions {
            question: "coffee".into(),
            filter: None,
            limit: Some(5),
            instructions: None,
            refers_to: None,
            exclude_thread: None,
            mode: RecallMode::Answer,
        })
        .unwrap();
    assert!(!engine.recall(recall).await.unwrap().citations.is_empty());

    let fetch = ops
        .fetch_request(
            FetchOptions {
                query: "coffee".into(),
                mode: None,
                filter: Some(MetaFilter::kinds([tinymemory_api::ItemKind::Learning])),
                limit: Some(usize::MAX),
                cursor: None,
                refers_to: None,
                exclude_thread: None,
            },
            engine.descriptor(),
        )
        .unwrap();
    assert_eq!(fetch.limit, MAX_LIMIT);
    assert_eq!(engine.fetch(fetch).await.unwrap().hits.len(), 1);

    let listed = ops.list(&engine, ListOptions::default()).await.unwrap();
    assert_eq!(listed.items.len(), 1);
    assert_eq!(listed.items[0].id, stored.id);

    let forgotten = ops
        .forget(
            &engine,
            vec![stored.id.as_str().to_string(), "  ".into()],
            None,
        )
        .await
        .unwrap();
    assert_eq!(forgotten.forgotten, 1);
    assert!(
        ops.list(&engine, ListOptions::default())
            .await
            .unwrap()
            .items
            .is_empty()
    );
}

#[tokio::test]
async fn forget_refuses_empty_ids_and_never_deletes_outside_the_allowed_reach() {
    let ops = operations(Policy::Clamp);
    let engine = CountingEngine::new(false);
    assert!(ops.forget(&engine, vec![" ".into()], None).await.is_err());

    let own = engine
        .store(StoreItem::learning(
            "mine",
            LearningKind::Fact,
            0.8,
            MemoryMeta {
                namespace: namespace("team:acme/agent:writer"),
                ..MemoryMeta::default()
            },
        ))
        .await
        .unwrap()
        .id;
    let other = engine
        .store(StoreItem::learning(
            "theirs",
            LearningKind::Fact,
            0.8,
            MemoryMeta {
                namespace: namespace("team:other/agent:writer"),
                ..MemoryMeta::default()
            },
        ))
        .await
        .unwrap()
        .id;
    let forgotten = ops
        .forget(
            &engine,
            vec![own.as_str().to_string(), other.as_str().to_string()],
            None,
        )
        .await
        .unwrap();
    assert_eq!(forgotten.forgotten, 1);
    assert_eq!(
        engine
            .get(tinymemory_api::GetRequest {
                ids: vec![other],
                reach: None,
            })
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn forget_filter_uses_one_unbounded_filter_call_and_preserves_or_tags() {
    let ops = MemoryOps::unconfined();
    let engine = CountingEngine::new(false);
    engine
        .store(learning_in("team:acme", "own flow", &["flow:f1"]))
        .await
        .unwrap();
    engine
        .store(learning_in("team:other", "all flows", &["flows"]))
        .await
        .unwrap();
    engine
        .store(learning_in("team:other", "other flow", &["flow:f2"]))
        .await
        .unwrap();

    let report = ops
        .forget_filter(&engine, MetaFilter::tags_any(["flow:f1", "flows"]))
        .await
        .unwrap();

    assert_eq!(report.forgotten, 2);
    assert_eq!(engine.forgets.load(Ordering::Relaxed), 1);
    assert_eq!(engine.lists.load(Ordering::Relaxed), 0);
    assert_eq!(engine.gets.load(Ordering::Relaxed), 0);
    let remaining = engine
        .inner
        .list(ListRequest {
            filter: MetaFilter::default(),
            limit: 10,
            cursor: None,
        })
        .await
        .unwrap()
        .items;
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].text, "other flow");
}

#[tokio::test]
async fn forget_filter_clamps_reach_and_preserves_tag_clauses() {
    let ops = operations(Policy::Clamp);
    let engine = CountingEngine::new(false);
    engine
        .store(learning_in(
            "team:acme/agent:writer",
            "own flow",
            &["flow:f1"],
        ))
        .await
        .unwrap();
    engine
        .store(learning_in(
            "team:other/agent:writer",
            "other flow",
            &["flow:f1"],
        ))
        .await
        .unwrap();

    let report = ops
        .forget_filter(&engine, MetaFilter::tags_any(["flow:f1"]))
        .await
        .unwrap();

    assert_eq!(report.forgotten, 1);
    assert_eq!(engine.forgets.load(Ordering::Relaxed), 1);
    let remaining = engine
        .inner
        .list(ListRequest {
            filter: MetaFilter::default(),
            limit: 10,
            cursor: None,
        })
        .await
        .unwrap()
        .items;
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].text, "other flow");
}

#[tokio::test]
async fn forget_filter_rejects_empty_filters_before_confinement() {
    let ops = operations(Policy::Clamp);
    let engine = CountingEngine::new(false);

    let error = ops
        .forget_filter(&engine, MetaFilter::default())
        .await
        .unwrap_err();

    assert!(matches!(error, tinymemory_api::Error::InvalidRequest(_)));
    assert_eq!(engine.forgets.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn forget_filter_refuses_an_out_of_reach_filter_before_engine_call() {
    let ops = operations(Policy::Refuse);
    let engine = CountingEngine::new(false);
    let filter = MetaFilter {
        reach: Some(Reach::subtree(Namespace::ROOT)),
        ..MetaFilter::tags_any(["flow:f1"])
    };

    let error = ops.forget_filter(&engine, filter).await.unwrap_err();

    assert!(matches!(error, tinymemory_api::Error::InvalidRequest(_)));
    assert_eq!(engine.forgets.load(Ordering::Relaxed), 0);
}

#[test]
fn refusing_confinement_rejects_cross_reach_requests_and_writes() {
    let ops = operations(Policy::Refuse);
    assert!(
        ops.recall_request(RecallOptions {
            question: "x".into(),
            filter: Some(MetaFilter {
                reach: Some(Reach::subtree(Namespace::ROOT)),
                ..MetaFilter::default()
            }),
            limit: None,
            instructions: None,
            refers_to: None,
            exclude_thread: None,
            mode: RecallMode::Answer,
        })
        .is_err()
    );
    assert!(
        ops.learning_item(LearningOptions {
            text: "fact".into(),
            kind: None,
            confidence: None,
            meta: Some(MemoryMeta {
                namespace: namespace("team:other"),
                ..MemoryMeta::default()
            }),
            host_meta: None,
            default_namespace: namespace("team:acme/agent:writer"),
            now: Utc::now(),
        })
        .is_err()
    );
    assert!(
        ops.store_item(StoreItem::learning(
            "outside",
            LearningKind::Fact,
            0.8,
            MemoryMeta {
                namespace: namespace("team:other"),
                ..MemoryMeta::default()
            },
        ))
        .is_err()
    );
}

#[test]
fn erase_requires_confirmation_and_builds_an_explicit_whole_tree_request() {
    let ops = operations(Policy::Refuse);
    assert!(ops.erase_all_request(false).is_err());
    let request = ops.erase_all_request(true).unwrap();
    assert!(request.whole_tree);
    assert_eq!(request.reach, Reach::subtree(Namespace::ROOT));
    assert!(request.validate().is_ok());
}

#[tokio::test]
async fn confirmed_whole_tree_erase_removes_items_across_namespaces() {
    let ops = operations(Policy::Refuse);
    let engine = CountingEngine::new(false);
    for root in ["team:acme", "team:other/agent:writer"] {
        engine
            .store(StoreItem::learning(
                "erase me",
                LearningKind::Fact,
                0.9,
                MemoryMeta {
                    namespace: namespace(root),
                    ..MemoryMeta::default()
                },
            ))
            .await
            .unwrap();
    }
    assert_eq!(
        engine
            .list(ListRequest::new(MetaFilter::default(), 10))
            .await
            .unwrap()
            .items
            .len(),
        2
    );
    engine
        .erase(ops.erase_all_request(true).unwrap())
        .await
        .unwrap();
    assert!(
        engine
            .list(ListRequest::new(MetaFilter::default(), 10))
            .await
            .unwrap()
            .items
            .is_empty()
    );
}

#[tokio::test]
async fn deletion_helpers_queue_only_when_needed_and_blank_threads_are_noops() {
    let ops = operations(Policy::Clamp);
    let engine = CountingEngine::new(false);
    let mut queued = Vec::new();

    assert_eq!(
        ops.forget_thread(None, "thread-off", |deletion| queued.push(deletion))
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        queued,
        [crate::deletion::PendingDeletion::Thread {
            thread_id: "thread-off".into()
        }]
    );
    assert_eq!(
        ops.forget_source(None, "source-off", |deletion| queued.push(deletion))
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        queued.last(),
        Some(&crate::deletion::PendingDeletion::Source {
            source_id: "source-off".into()
        })
    );

    let report = ops
        .forget_source(Some(&engine), "source-empty", |_| panic!("must not queue"))
        .await
        .unwrap();
    assert_eq!(report, 0);

    assert_eq!(
        ops.forget_thread(None, "  ", |_| panic!("blank thread must not queue"))
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn failed_deletion_is_queued_once_and_the_engine_error_is_preserved() {
    let ops = operations(Policy::Clamp);
    let engine = CountingEngine::new(true);
    let mut queued = Vec::new();
    let error = ops
        .forget_source(Some(&engine), "source", |deletion| queued.push(deletion))
        .await
        .unwrap_err();
    assert!(matches!(error, tinymemory_api::Error::InvalidRequest(_)));
    assert_eq!(
        queued,
        [crate::deletion::PendingDeletion::Source {
            source_id: "source".into()
        }]
    );
}

#[tokio::test]
async fn thread_and_source_deletions_stay_inside_the_configured_reach() {
    let ops = operations(Policy::Clamp);
    let engine = CountingEngine::new(false);
    let mut thread_ids = Vec::new();
    let mut source_ids = Vec::new();
    for root in ["team:acme/agent:writer", "team:other/agent:writer"] {
        let mut conversation_meta = MemoryMeta {
            namespace: namespace(root),
            thread_id: Some("same-thread-id".into()),
            ..MemoryMeta::default()
        };
        conversation_meta.source.kind = SourceKind::Conversation;
        thread_ids.push(
            engine
                .store(StoreItem::Conversation {
                    turns: vec![tinymemory_api::Turn::new(
                        tinymemory_api::Role::User,
                        "turn",
                    )],
                    meta: conversation_meta,
                })
                .await
                .unwrap()
                .id,
        );
        source_ids.push(
            engine
                .store(StoreItem::learning(
                    "source item",
                    LearningKind::Fact,
                    0.8,
                    MemoryMeta {
                        namespace: namespace(root),
                        source: SourceRef {
                            kind: SourceKind::File,
                            id: Some("same-source-id".into()),
                        },
                        ..MemoryMeta::default()
                    },
                ))
                .await
                .unwrap()
                .id,
        );
    }

    assert_eq!(
        ops.forget_thread(Some(&engine), "same-thread-id", |_| panic!(
            "must not queue"
        ))
        .await
        .unwrap(),
        1
    );
    assert_eq!(
        ops.forget_source(Some(&engine), "same-source-id", |_| panic!(
            "must not queue"
        ))
        .await
        .unwrap(),
        1
    );

    for ids in [thread_ids, source_ids] {
        let remaining = engine
            .get(tinymemory_api::GetRequest { ids, reach: None })
            .await
            .unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(
            remaining[0].meta.namespace,
            namespace("team:other/agent:writer")
        );
    }
}

#[path = "../ops_unconfined_tests.rs"]
mod unconfined_tests;
