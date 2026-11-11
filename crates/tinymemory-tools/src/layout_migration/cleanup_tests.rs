use std::sync::Arc;

use crate::layout::MemoryLayout;
use tinymemory_api::conformance::ReferenceEngine;
use tinymemory_api::{
    EngineDescriptor, EngineHealth, EraseReport, ExportPage, FetchPage, FetchRequest, ForgetReport,
    Hit, LearningKind, ListPage, MemoryEngine, MemoryMeta, RecallAnswer, RecallRequest, Result,
    StoreItem, StoreReceipt, async_trait,
};

use super::super::copy::copy;
use super::super::map::FlowPlacement;
use super::super::state::MemoryStateStore;
use super::*;

fn placement() -> Placement {
    Placement {
        layout: MemoryLayout::new(Namespace::ROOT).unwrap(),
        chat_node: "ws:main".parse().unwrap(),
        flows: FlowPlacement::WithRoot,
        split_github_by_repo: false,
    }
}

async fn seeded(n: usize) -> Arc<ReferenceEngine> {
    let legacy = Arc::new(ReferenceEngine::new());
    for i in 0..n {
        legacy
            .store(StoreItem::learning(
                format!("fact {i}"),
                LearningKind::Fact,
                0.5,
                MemoryMeta {
                    namespace: if i % 2 == 0 {
                        Namespace::ROOT
                    } else {
                        "agent:a".parse().unwrap()
                    },
                    ..MemoryMeta::default()
                },
            ))
            .await
            .unwrap();
    }
    legacy
}

async fn texts(engine: &dyn MemoryEngine) -> Vec<String> {
    let mut texts: Vec<String> = engine
        .list(ListRequest::new(MetaFilter::default(), 100))
        .await
        .unwrap()
        .items
        .into_iter()
        .map(|hit| hit.text)
        .collect();
    texts.sort();
    texts
}

async fn copied(tmp: &dyn MigrationStateStore, engines: &Engines) -> MigrationState {
    let mut state = MigrationState::default();
    copy(tmp, engines, &placement(), &mut state, || async { false })
        .await
        .unwrap();
    state
}

/// A reference engine that cannot erase, as the hosted one cannot.
struct NoErase(ReferenceEngine);

/// A reference engine whose erase must never be called.
struct NeverErase(ReferenceEngine);

#[async_trait]
impl MemoryEngine for NeverErase {
    fn descriptor(&self) -> &EngineDescriptor {
        self.0.descriptor()
    }
    async fn health(&self) -> EngineHealth {
        self.0.health().await
    }
    async fn recall(&self, req: RecallRequest) -> Result<RecallAnswer> {
        self.0.recall(req).await
    }
    async fn fetch(&self, req: FetchRequest) -> Result<FetchPage> {
        self.0.fetch(req).await
    }
    async fn store(&self, item: StoreItem) -> Result<StoreReceipt> {
        self.0.store(item).await
    }
    async fn forget(&self, target: ForgetTarget) -> Result<ForgetReport> {
        self.0.forget(target).await
    }
    async fn list(&self, req: ListRequest) -> Result<ListPage> {
        self.0.list(req).await
    }
    async fn export(&self, req: ListRequest) -> Result<ExportPage> {
        self.0.export(req).await
    }
    async fn get(&self, req: GetRequest) -> Result<Vec<Hit>> {
        self.0.get(req).await
    }
    async fn erase(&self, _: EraseRequest) -> Result<EraseReport> {
        panic!("a shared legacy tree is never erased scope-wide");
    }
}

#[async_trait]
impl MemoryEngine for NoErase {
    fn descriptor(&self) -> &EngineDescriptor {
        self.0.descriptor()
    }
    async fn health(&self) -> EngineHealth {
        self.0.health().await
    }
    async fn recall(&self, req: RecallRequest) -> Result<RecallAnswer> {
        self.0.recall(req).await
    }
    async fn fetch(&self, req: FetchRequest) -> Result<FetchPage> {
        self.0.fetch(req).await
    }
    async fn store(&self, item: StoreItem) -> Result<StoreReceipt> {
        self.0.store(item).await
    }
    async fn forget(&self, target: ForgetTarget) -> Result<ForgetReport> {
        self.0.forget(target).await
    }
    async fn list(&self, req: ListRequest) -> Result<ListPage> {
        self.0.list(req).await
    }
    async fn export(&self, req: ListRequest) -> Result<ExportPage> {
        self.0.export(req).await
    }
    async fn get(&self, req: GetRequest) -> Result<Vec<Hit>> {
        self.0.get(req).await
    }
}

#[tokio::test]
async fn everything_moved_is_erased_from_the_legacy_tree() {
    let store = MemoryStateStore::default();
    let legacy = seeded(10).await;
    let tree = Arc::new(ReferenceEngine::new());
    let engines = Engines {
        legacy: legacy.clone(),
        tree: tree.clone(),
    };
    let mut state = copied(&store, &engines).await;
    cleanup(
        &store,
        &engines,
        &placement(),
        false,
        false,
        &mut state,
        || async { false },
    )
    .await
    .unwrap();
    assert_eq!(state.phase, Phase::Cleaned);
    assert!(texts(legacy.as_ref()).await.is_empty());
    assert_eq!(texts(tree.as_ref()).await.len(), 10, "the moved items stay");
}

#[tokio::test]
async fn an_item_without_its_twin_is_never_removed() {
    let store = MemoryStateStore::default();
    let legacy = seeded(10).await;
    let tree = Arc::new(ReferenceEngine::new());
    let engines = Engines {
        legacy: legacy.clone(),
        tree: tree.clone(),
    };
    let mut state = copied(&store, &engines).await;
    // The per-user copy of "fact 3" is lost after the copy.
    let lost = tree
        .list(ListRequest::new(MetaFilter::default(), 100))
        .await
        .unwrap()
        .items
        .into_iter()
        .find(|hit| hit.text == "fact 3")
        .unwrap();
    tree.forget(ForgetTarget::Ids(vec![lost.id])).await.unwrap();

    cleanup(
        &store,
        &engines,
        &placement(),
        false,
        false,
        &mut state,
        || async { false },
    )
    .await
    .unwrap();
    assert_eq!(state.phase, Phase::Cleaned);
    assert_eq!(texts(legacy.as_ref()).await, vec!["fact 3".to_string()]);
}

#[tokio::test]
async fn an_engine_that_cannot_erase_forgets_by_id() {
    let store = MemoryStateStore::default();
    let legacy = Arc::new(NoErase(ReferenceEngine::new()));
    for i in 0..5 {
        legacy
            .store(StoreItem::learning(
                format!("fact {i}"),
                LearningKind::Fact,
                0.5,
                MemoryMeta::default(),
            ))
            .await
            .unwrap();
    }
    let engines = Engines {
        legacy: legacy.clone(),
        tree: Arc::new(ReferenceEngine::new()),
    };
    let mut state = copied(&store, &engines).await;
    cleanup(
        &store,
        &engines,
        &placement(),
        false,
        false,
        &mut state,
        || async { false },
    )
    .await
    .unwrap();
    assert_eq!(state.phase, Phase::Cleaned);
    assert!(texts(legacy.as_ref()).await.is_empty());
}

#[tokio::test]
async fn a_paused_cleanup_says_it_was_cleaning() {
    let store = MemoryStateStore::default();
    let engines = Engines {
        legacy: seeded(4).await,
        tree: Arc::new(ReferenceEngine::new()),
    };
    let mut state = copied(&store, &engines).await;
    cleanup(
        &store,
        &engines,
        &placement(),
        false,
        false,
        &mut state,
        || async { true },
    )
    .await
    .unwrap();
    assert_eq!(state.phase, Phase::Paused);
    assert!(state.cleaning, "a resume goes back to cleanup");
    assert_eq!(texts(engines.legacy.as_ref()).await.len(), 4);
}

#[tokio::test]
async fn a_shared_legacy_tree_is_forgotten_by_id_never_erased() {
    let store = MemoryStateStore::default();
    let legacy = Arc::new(NeverErase(ReferenceEngine::new()));
    for i in 0..5 {
        legacy
            .store(StoreItem::learning(
                format!("fact {i}"),
                LearningKind::Fact,
                0.5,
                MemoryMeta::default(),
            ))
            .await
            .unwrap();
    }
    let engines = Engines {
        legacy: legacy.clone(),
        tree: Arc::new(ReferenceEngine::new()),
    };
    let mut state = copied(&store, &engines).await;
    cleanup(
        &store,
        &engines,
        &placement(),
        true,
        false,
        &mut state,
        || async { false },
    )
    .await
    .unwrap();
    assert_eq!(state.phase, Phase::Cleaned);
    assert!(texts(legacy.as_ref()).await.is_empty());
}

/// An import that finished without its last batch confirmed listed may hold
/// items no export shows yet: cleanup must not erase a scope whole then, and
/// forgets only the moved items by id (an erase would panic here).
#[tokio::test]
async fn after_an_unconfirmed_import_moved_items_are_forgotten_by_id_never_erased() {
    let store = MemoryStateStore::default();
    let legacy = Arc::new(NeverErase(ReferenceEngine::new()));
    for i in 0..5 {
        legacy
            .store(StoreItem::learning(
                format!("fact {i}"),
                LearningKind::Fact,
                0.5,
                MemoryMeta::default(),
            ))
            .await
            .unwrap();
    }
    let engines = Engines {
        legacy: legacy.clone(),
        tree: Arc::new(ReferenceEngine::new()),
    };
    let mut state = copied(&store, &engines).await;
    cleanup(
        &store,
        &engines,
        &placement(),
        false,
        true,
        &mut state,
        || async { false },
    )
    .await
    .unwrap();
    assert_eq!(state.phase, Phase::Cleaned);
    assert!(texts(legacy.as_ref()).await.is_empty());
}

/// A legacy tree holding part of one chunked item: export names it in
/// `incomplete`, and its erase must never be called.
struct PartlyStored(ReferenceEngine);

#[async_trait]
impl MemoryEngine for PartlyStored {
    fn descriptor(&self) -> &EngineDescriptor {
        self.0.descriptor()
    }
    async fn health(&self) -> EngineHealth {
        self.0.health().await
    }
    async fn recall(&self, req: RecallRequest) -> Result<RecallAnswer> {
        self.0.recall(req).await
    }
    async fn fetch(&self, req: FetchRequest) -> Result<FetchPage> {
        self.0.fetch(req).await
    }
    async fn store(&self, item: StoreItem) -> Result<StoreReceipt> {
        self.0.store(item).await
    }
    async fn forget(&self, target: ForgetTarget) -> Result<ForgetReport> {
        self.0.forget(target).await
    }
    async fn list(&self, req: ListRequest) -> Result<ListPage> {
        self.0.list(req).await
    }
    async fn export(&self, req: ListRequest) -> Result<ExportPage> {
        let mut page = self.0.export(req).await?;
        page.incomplete.push(ItemId("partly-stored".into()));
        Ok(page)
    }
    async fn get(&self, req: GetRequest) -> Result<Vec<Hit>> {
        self.0.get(req).await
    }
    async fn erase(&self, _: EraseRequest) -> Result<EraseReport> {
        panic!("a legacy tree with a partly stored item is never erased scope-wide");
    }
}

#[tokio::test]
async fn a_partly_stored_legacy_item_blocks_the_scope_wide_erase() {
    let store = MemoryStateStore::default();
    let legacy = Arc::new(PartlyStored(ReferenceEngine::new()));
    for i in 0..5 {
        legacy
            .store(StoreItem::learning(
                format!("fact {i}"),
                LearningKind::Fact,
                0.5,
                MemoryMeta::default(),
            ))
            .await
            .unwrap();
    }
    let engines = Engines {
        legacy: legacy.clone(),
        tree: Arc::new(ReferenceEngine::new()),
    };
    let mut state = copied(&store, &engines).await;
    cleanup(
        &store,
        &engines,
        &placement(),
        false,
        false,
        &mut state,
        || async { false },
    )
    .await
    .unwrap();
    assert_eq!(state.phase, Phase::Cleaned);
    assert!(
        texts(legacy.as_ref()).await.is_empty(),
        "moved items forgotten by id"
    );
}
