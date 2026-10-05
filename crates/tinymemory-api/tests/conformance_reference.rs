//! The suite against the reference engine, and against deliberately broken
//! wrappers of it: the reference must pass, and each fault must be caught by
//! the check written for it. Without the second half a suite that asserted
//! nothing would also be green.

use async_trait::async_trait;
use tinymemory_api::conformance::{Error, ReferenceEngine, run};
use tinymemory_api::{
    BELIEF_TAG, BeliefsRequest, ConsolidateReceipt, ConsolidateRequest, ConsolidateStatus,
    Consolidation, EngineDescriptor, EngineHealth, ExplorePage, ExploreRequest, FetchMode,
    FetchPage, FetchRequest, ForgetReport, ForgetTarget, GetRequest, Hit, ItemId, ItemKind,
    ListPage, ListRequest, MemoryEngine, MemoryMeta, MetaFilter, Namespace, RecallAnswer,
    RecallRequest, Result, StoreItem, StoreReceipt, WaitFor, WriteOptions,
};

#[tokio::test]
async fn the_reference_engine_passes_and_cleans_up() {
    let engine = ReferenceEngine::new();
    run(&engine).await.expect("the reference engine conforms");
    assert!(engine.is_empty());
}

#[tokio::test]
async fn the_suite_leaves_foreign_items_alone() {
    let engine = ReferenceEngine::new();
    engine
        .store(StoreItem::document(
            "someone else's note",
            Default::default(),
        ))
        .await
        .expect("store");
    run(&engine).await.expect("conforms");
    assert_eq!(engine.len(), 1);
}

#[derive(Clone, Copy, Debug)]
enum Fault {
    IgnoreFetchFilter,
    NeverReplay,
    AcceptEmptyForget,
    ClaimEveryMode,
    CiteUnknownIds,
    Down,
    /// Counts every bucket one short.
    UndercountExplore,
    /// Returns what it found in its own order rather than the order asked.
    GetUnordered,
    /// Returns bulk receipts in reverse order.
    StoreManyUnordered,
    /// Lists every namespace whatever the reach.
    ListIgnoresReach,
    /// Reads ids by `get` whatever the reach.
    GetIgnoresReach,
    /// Answers an accepted store with an id other than the item's.
    AcceptedWrongId,
    /// Declares on-demand consolidation but only acknowledges a schedule.
    ConsolidateOffPromise,
    /// Declares no consolidation yet answers a request.
    ConsolidateUndeclared,
    /// Builds without validating the request.
    ConsolidateUnvalidated,
    /// Returns beliefs that are not tagged as beliefs.
    BeliefsUntagged,
    /// Returns beliefs outside the reach asked for.
    BeliefsOutOfReach,
}

struct Faulty {
    inner: ReferenceEngine,
    fault: Fault,
    descriptor: EngineDescriptor,
}

impl Faulty {
    fn new(fault: Fault) -> Self {
        let inner = ReferenceEngine::new();
        let mut descriptor = inner.descriptor().clone();
        if matches!(fault, Fault::ClaimEveryMode) {
            descriptor.fetch_modes = vec![FetchMode::Hybrid];
        }
        if matches!(fault, Fault::ConsolidateUndeclared) {
            descriptor.consolidation = Consolidation::None;
        }
        Self {
            inner,
            fault,
            descriptor,
        }
    }
}

#[async_trait]
impl MemoryEngine for Faulty {
    fn descriptor(&self) -> &EngineDescriptor {
        &self.descriptor
    }

    async fn health(&self) -> EngineHealth {
        match self.fault {
            Fault::Down => EngineHealth::Down("broken on purpose".into()),
            _ => self.inner.health().await,
        }
    }

    async fn recall(&self, req: RecallRequest) -> Result<RecallAnswer> {
        let mut answer = self.inner.recall(req).await?;
        if matches!(self.fault, Fault::CiteUnknownIds) {
            for citation in &mut answer.citations {
                citation.id = "unknown".into();
            }
        }
        Ok(answer)
    }

    async fn fetch(&self, mut req: FetchRequest) -> Result<FetchPage> {
        match self.fault {
            Fault::IgnoreFetchFilter => req.filter = MetaFilter::default(),
            // Serves a mode it does not declare instead of refusing it.
            Fault::ClaimEveryMode => req.mode = FetchMode::Hybrid,
            _ => {}
        }
        self.inner.fetch(req).await
    }

    async fn store(&self, item: StoreItem) -> Result<StoreReceipt> {
        let mut receipt = self.inner.store(item).await?;
        if matches!(self.fault, Fault::NeverReplay) {
            receipt.replayed = false;
        }
        Ok(receipt)
    }

    async fn store_with(&self, item: StoreItem, options: WriteOptions) -> Result<StoreReceipt> {
        let accepted = options.wait == WaitFor::Accepted;
        let mut receipt = self.inner.store_with(item, options).await?;
        if accepted && matches!(self.fault, Fault::AcceptedWrongId) {
            receipt.id = "not-the-item".into();
        }
        Ok(receipt)
    }

    async fn consolidate(&self, req: ConsolidateRequest) -> Result<ConsolidateReceipt> {
        match self.fault {
            Fault::ConsolidateOffPromise => {
                req.validate()?;
                Ok(ConsolidateReceipt::scheduled())
            }
            Fault::ConsolidateUnvalidated => Ok(ConsolidateReceipt {
                status: ConsolidateStatus::Completed,
                jobs: Vec::new(),
                scopes: 1,
                built: Some(1),
            }),
            _ => self.inner.consolidate(req).await,
        }
    }

    async fn beliefs(&self, req: BeliefsRequest) -> Result<Vec<Hit>> {
        req.validate()?;
        let namespace = match self.fault {
            Fault::BeliefsUntagged => req.reach.at.clone(),
            Fault::BeliefsOutOfReach => Namespace::agent("somebody-else"),
            _ => return self.inner.beliefs(req).await,
        };
        let tags = match self.fault {
            Fault::BeliefsUntagged => Vec::new(),
            _ => vec![BELIEF_TAG.to_string()],
        };
        Ok(vec![Hit {
            id: ItemId::new("belief-1"),
            kind: ItemKind::Learning,
            text: "a belief".to_string(),
            meta: MemoryMeta {
                namespace,
                tags,
                ..MemoryMeta::default()
            },
            score: 1.0,
            confidence: Some(0.9),
        }])
    }

    async fn forget(&self, target: ForgetTarget) -> Result<ForgetReport> {
        if matches!(self.fault, Fault::AcceptEmptyForget) && target.validate().is_err() {
            return Ok(ForgetReport::default());
        }
        self.inner.forget(target).await
    }

    async fn list(&self, mut req: ListRequest) -> Result<ListPage> {
        if matches!(self.fault, Fault::ListIgnoresReach) {
            req.filter.reach = None;
        }
        self.inner.list(req).await
    }

    async fn explore(&self, req: ExploreRequest) -> Result<ExplorePage> {
        let mut page = self.inner.explore(req).await?;
        if matches!(self.fault, Fault::UndercountExplore) {
            for bucket in &mut page.buckets {
                bucket.count = bucket.count.saturating_sub(1);
            }
        }
        Ok(page)
    }

    async fn store_many(&self, items: Vec<StoreItem>) -> Result<Vec<StoreReceipt>> {
        let mut receipts = self.inner.store_many(items).await?;
        if matches!(self.fault, Fault::StoreManyUnordered) {
            receipts.reverse();
        }
        Ok(receipts)
    }

    async fn get(&self, mut req: GetRequest) -> Result<Vec<Hit>> {
        if matches!(self.fault, Fault::GetIgnoresReach) {
            req.reach = None;
        }
        let mut hits = self.inner.get(req).await?;
        if matches!(self.fault, Fault::GetUnordered) {
            hits.sort_by(|a, b| a.id.cmp(&b.id));
            if hits.windows(2).all(|pair| pair[0].id <= pair[1].id) {
                hits.reverse();
            }
        }
        Ok(hits)
    }
}

#[tokio::test]
async fn each_fault_is_caught_by_its_check() {
    let cases = [
        (Fault::IgnoreFetchFilter, "fetch_filters"),
        (Fault::NeverReplay, "replay"),
        (Fault::AcceptEmptyForget, "empty_forget"),
        (Fault::ClaimEveryMode, "unsupported_modes"),
        (Fault::CiteUnknownIds, "recall"),
        (Fault::Down, "health"),
        (Fault::UndercountExplore, "explore"),
        (Fault::GetUnordered, "get"),
        (Fault::StoreManyUnordered, "store_many"),
        (Fault::ListIgnoresReach, "namespaces"),
        (Fault::GetIgnoresReach, "namespaces"),
        (Fault::AcceptedWrongId, "store_with"),
        (Fault::ConsolidateOffPromise, "consolidate"),
        (Fault::ConsolidateUndeclared, "consolidate"),
        (Fault::ConsolidateUnvalidated, "consolidate"),
        (Fault::BeliefsUntagged, "consolidate"),
        (Fault::BeliefsOutOfReach, "consolidate"),
    ];
    for (fault, expected) in cases {
        let error = run(&Faulty::new(fault))
            .await
            .expect_err("a faulty engine must fail");
        let Error::Check { check, .. } = &error else {
            panic!("{fault:?}: expected a check failure, got {error}");
        };
        assert_eq!(*check, expected, "{fault:?}: {error}");
    }
}

#[tokio::test]
async fn an_engine_without_consolidation_passes_by_refusing_it() {
    /// The reference engine, minus consolidation: the trait's default.
    struct NoBeliefs {
        inner: ReferenceEngine,
        descriptor: EngineDescriptor,
    }

    #[async_trait]
    impl MemoryEngine for NoBeliefs {
        fn descriptor(&self) -> &EngineDescriptor {
            &self.descriptor
        }
        async fn health(&self) -> EngineHealth {
            self.inner.health().await
        }
        async fn recall(&self, req: RecallRequest) -> Result<RecallAnswer> {
            self.inner.recall(req).await
        }
        async fn fetch(&self, req: FetchRequest) -> Result<FetchPage> {
            self.inner.fetch(req).await
        }
        async fn store(&self, item: StoreItem) -> Result<StoreReceipt> {
            self.inner.store(item).await
        }
        async fn forget(&self, target: ForgetTarget) -> Result<ForgetReport> {
            self.inner.forget(target).await
        }
        async fn list(&self, req: ListRequest) -> Result<ListPage> {
            self.inner.list(req).await
        }
    }

    let inner = ReferenceEngine::new();
    let descriptor = EngineDescriptor {
        consolidation: Consolidation::None,
        ..inner.descriptor().clone()
    };
    let engine = NoBeliefs { inner, descriptor };
    run(&engine).await.expect("refusing consolidation conforms");
    assert!(engine.inner.is_empty());
}

#[tokio::test]
async fn two_users_on_separate_stores_are_isolated_and_cleaned_up() {
    let a = ReferenceEngine::new();
    let b = ReferenceEngine::new();
    tinymemory_api::conformance::run_isolation(&a, &b)
        .await
        .expect("separate stores are isolated");
    assert!(a.is_empty() && b.is_empty());
}

#[tokio::test]
async fn one_store_shared_by_two_users_is_caught() {
    let shared = ReferenceEngine::new();
    let error = tinymemory_api::conformance::run_isolation(&shared, &shared)
        .await
        .expect_err("a shared store must not pass as isolated");
    assert!(
        matches!(
            &error,
            Error::Check {
                check: "isolation",
                ..
            }
        ),
        "{error:?}"
    );
    assert!(shared.is_empty(), "the check cleans up after a failure");
}
