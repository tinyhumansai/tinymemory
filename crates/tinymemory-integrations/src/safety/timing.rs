//! Safe, one-line timing diagnostics for every memory-engine operation.

use std::future::Future;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use tinymemory_api::{
    BeliefsRequest, ConsolidateReceipt, ConsolidateRequest, EngineDescriptor, EngineHealth,
    EraseReport, EraseRequest, ExplorePage, ExploreRequest, ExportPage, FetchPage, FetchRequest,
    ForgetReport, ForgetTarget, GetRequest, Hit, ListPage, ListRequest, MemoryEngine, Reach,
    RecallAnswer, RecallRequest, Result, StoreItem, StoreReceipt, WaitFor, WriteOptions,
};

/// A [`MemoryEngine`] decorator that logs one content-free timing line per
/// operation.
///
/// Failure outcomes use [`tinymemory_api::Error::code`] and never include an
/// error message. `health` reports only `ok`, `degraded`, or `down`; its reason
/// is deliberately omitted because it is supplied by an engine.
pub struct TimedEngine {
    inner: Arc<dyn MemoryEngine>,
    id: String,
}

impl std::fmt::Debug for TimedEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TimedEngine")
            .field("engine", &self.id)
            .finish_non_exhaustive()
    }
}

impl TimedEngine {
    /// Wraps `inner` with timing diagnostics.
    #[must_use]
    pub fn wrap(inner: Arc<dyn MemoryEngine>) -> Arc<dyn MemoryEngine> {
        let id = inner.descriptor().id.to_string();
        Arc::new(Self { inner, id })
    }

    async fn timed<T>(
        &self,
        operation: &'static str,
        call: impl Future<Output = Result<T>>,
        count: impl FnOnce(&T) -> usize,
    ) -> Result<T> {
        let started = Instant::now();
        let result = call.await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let outcome = timing_outcome(&result);
        let count = result.as_ref().map_or(0, count);
        log::debug!(
            "[memory:timing] op={operation} engine={} elapsed_ms={elapsed_ms} outcome={outcome} count={count}",
            self.id
        );
        result
    }
}

/// The safe timing outcome for a result: `ok` or the error's stable code.
pub(crate) fn timing_outcome<T>(result: &Result<T>) -> &'static str {
    match result {
        Ok(_) => "ok",
        Err(error) => error.code(),
    }
}

#[async_trait]
impl MemoryEngine for TimedEngine {
    fn descriptor(&self) -> &EngineDescriptor {
        self.inner.descriptor()
    }

    async fn health(&self) -> EngineHealth {
        let started = Instant::now();
        let health = self.inner.health().await;
        let outcome = match &health {
            EngineHealth::Ok => "ok",
            EngineHealth::Degraded(_) => "degraded",
            EngineHealth::Down(_) => "down",
        };
        log::debug!(
            "[memory:timing] op=health engine={} elapsed_ms={} outcome={outcome}",
            self.id,
            started.elapsed().as_millis() as u64
        );
        health
    }

    async fn recall(&self, req: RecallRequest) -> Result<RecallAnswer> {
        self.timed("recall", self.inner.recall(req), |answer| {
            answer.citations.len()
        })
        .await
    }

    async fn fetch(&self, req: FetchRequest) -> Result<FetchPage> {
        self.timed("fetch", self.inner.fetch(req), |page| page.hits.len())
            .await
    }

    async fn store(&self, item: StoreItem) -> Result<StoreReceipt> {
        self.timed("store", self.inner.store(item), |_| 1).await
    }

    async fn store_with(&self, item: StoreItem, options: WriteOptions) -> Result<StoreReceipt> {
        let operation = match options.wait {
            WaitFor::Accepted => "store_accepted",
            WaitFor::Visible => "store",
        };
        self.timed(operation, self.inner.store_with(item, options), |_| 1)
            .await
    }

    async fn store_many(&self, items: Vec<StoreItem>) -> Result<Vec<StoreReceipt>> {
        self.timed("store_many", self.inner.store_many(items), Vec::len)
            .await
    }

    async fn store_many_with(
        &self,
        items: Vec<StoreItem>,
        options: WriteOptions,
    ) -> Result<Vec<StoreReceipt>> {
        let operation = match options.wait {
            WaitFor::Accepted => "store_many_accepted",
            WaitFor::Visible => "store_many",
        };
        self.timed(
            operation,
            self.inner.store_many_with(items, options),
            Vec::len,
        )
        .await
    }

    async fn forget(&self, target: ForgetTarget) -> Result<ForgetReport> {
        self.timed("forget", self.inner.forget(target), |report| {
            report.forgotten
        })
        .await
    }

    async fn forget_within(
        &self,
        ids: Vec<tinymemory_api::ItemId>,
        reach: Reach,
    ) -> Result<ForgetReport> {
        self.timed(
            "forget_within",
            self.inner.forget_within(ids, reach),
            |report| report.forgotten,
        )
        .await
    }

    async fn list(&self, req: ListRequest) -> Result<ListPage> {
        self.timed("list", self.inner.list(req), |page| page.items.len())
            .await
    }

    async fn list_preview(&self, req: ListRequest) -> Result<ListPage> {
        self.timed("list_preview", self.inner.list_preview(req), |page| {
            page.items.len()
        })
        .await
    }

    async fn export(&self, req: ListRequest) -> Result<ExportPage> {
        self.timed("export", self.inner.export(req), |page| page.items.len())
            .await
    }

    async fn erase(&self, req: EraseRequest) -> Result<EraseReport> {
        self.timed("erase", self.inner.erase(req), |report| {
            report.erased_scopes
        })
        .await
    }

    async fn explore(&self, req: ExploreRequest) -> Result<ExplorePage> {
        self.timed("explore", self.inner.explore(req), |page| {
            page.buckets.len()
        })
        .await
    }

    async fn get(&self, req: GetRequest) -> Result<Vec<Hit>> {
        self.timed("get", self.inner.get(req), Vec::len).await
    }

    async fn consolidate(&self, req: ConsolidateRequest) -> Result<ConsolidateReceipt> {
        self.timed("consolidate", self.inner.consolidate(req), |receipt| {
            receipt.jobs.len()
        })
        .await
    }

    async fn beliefs(&self, req: BeliefsRequest) -> Result<Vec<Hit>> {
        self.timed("beliefs", self.inner.beliefs(req), Vec::len)
            .await
    }
}
