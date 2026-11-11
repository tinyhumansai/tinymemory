//! A mandatory-on-use decorator that scrubs each stored item before forwarding.

use std::sync::Arc;

use async_trait::async_trait;
use tinymemory_api::{
    BeliefsRequest, ConsolidateReceipt, ConsolidateRequest, EngineDescriptor, EngineHealth,
    EraseReport, EraseRequest, ExplorePage, ExploreRequest, ExportPage, FetchPage, FetchRequest,
    ForgetReport, ForgetTarget, GetRequest, Hit, ItemId, ListPage, ListRequest, MemoryEngine,
    Reach, RecallAnswer, RecallRequest, Result, StoreItem, StoreReceipt, WriteOptions,
};

use crate::safety::{Policy, scrub_item_with};

/// A [`MemoryEngine`] decorator that scrubs every single and bulk write.
///
/// Reads and non-write operations are forwarded unchanged. The wrapper
/// explicitly forwards all current trait methods, including methods with
/// default implementations such as [`MemoryEngine::forget_within`], so those
/// defaults cannot bypass the inner engine's semantics.
pub struct ScrubbingEngine {
    inner: Arc<dyn MemoryEngine>,
    policy: Policy,
}

impl std::fmt::Debug for ScrubbingEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScrubbingEngine")
            .field("engine", &self.inner.descriptor().id)
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

impl ScrubbingEngine {
    /// Wraps `inner` so every write is scrubbed under `policy`.
    #[must_use]
    pub fn wrap(inner: Arc<dyn MemoryEngine>, policy: Policy) -> Arc<dyn MemoryEngine> {
        Arc::new(Self { inner, policy })
    }

    fn scrub(&self, item: StoreItem) -> StoreItem {
        scrub_item_with(item, self.policy).value
    }

    fn scrub_many(&self, items: Vec<StoreItem>) -> Vec<StoreItem> {
        items.into_iter().map(|item| self.scrub(item)).collect()
    }
}

#[async_trait]
impl MemoryEngine for ScrubbingEngine {
    fn descriptor(&self) -> &EngineDescriptor {
        self.inner.descriptor()
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
        self.inner.store(self.scrub(item)).await
    }

    async fn store_with(&self, item: StoreItem, options: WriteOptions) -> Result<StoreReceipt> {
        self.inner.store_with(self.scrub(item), options).await
    }

    async fn store_many(&self, items: Vec<StoreItem>) -> Result<Vec<StoreReceipt>> {
        self.inner.store_many(self.scrub_many(items)).await
    }

    async fn store_many_with(
        &self,
        items: Vec<StoreItem>,
        options: WriteOptions,
    ) -> Result<Vec<StoreReceipt>> {
        self.inner
            .store_many_with(self.scrub_many(items), options)
            .await
    }

    async fn forget(&self, target: ForgetTarget) -> Result<ForgetReport> {
        self.inner.forget(target).await
    }

    async fn forget_within(&self, ids: Vec<ItemId>, reach: Reach) -> Result<ForgetReport> {
        self.inner.forget_within(ids, reach).await
    }

    async fn list(&self, req: ListRequest) -> Result<ListPage> {
        self.inner.list(req).await
    }

    async fn list_preview(&self, req: ListRequest) -> Result<ListPage> {
        self.inner.list_preview(req).await
    }

    async fn export(&self, req: ListRequest) -> Result<ExportPage> {
        self.inner.export(req).await
    }

    async fn erase(&self, req: EraseRequest) -> Result<EraseReport> {
        self.inner.erase(req).await
    }

    async fn explore(&self, req: ExploreRequest) -> Result<ExplorePage> {
        self.inner.explore(req).await
    }

    async fn get(&self, req: GetRequest) -> Result<Vec<Hit>> {
        self.inner.get(req).await
    }

    async fn consolidate(&self, req: ConsolidateRequest) -> Result<ConsolidateReceipt> {
        self.inner.consolidate(req).await
    }

    async fn beliefs(&self, req: BeliefsRequest) -> Result<Vec<Hit>> {
        self.inner.beliefs(req).await
    }
}
