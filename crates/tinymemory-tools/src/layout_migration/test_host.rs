//! A host over two in-memory engines, for the migration's tests.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tinymemory_api::conformance::ReferenceEngine;
use tinymemory_api::{
    LearningKind, ListRequest, MemoryEngine, MemoryMeta, MetaFilter, Namespace, StoreItem,
};

use super::copy::Engines;
use super::error::Result;
use super::job::{LayoutHost, LegacyClaim};
use super::map::{FlowPlacement, Placement};
use super::state::{MemoryStateStore, MigrationStateStore};
use crate::layout::MemoryLayout;

/// A shared-tree claim kept in memory: `other` is whether another account
/// holds it, `taken` whether this one took it.
#[derive(Clone, Default)]
pub(crate) struct FakeClaim {
    pub(crate) other: Arc<AtomicBool>,
    pub(crate) taken: Arc<AtomicBool>,
}

impl LegacyClaim for FakeClaim {
    fn held_by_other(&self) -> Result<bool> {
        Ok(self.other.load(Ordering::SeqCst))
    }

    fn take(&self) -> Result<bool> {
        if self.other.load(Ordering::SeqCst) {
            return Ok(false);
        }
        self.taken.store(true, Ordering::SeqCst);
        Ok(true)
    }
}

/// Two in-memory engines and the switches a real host keeps.
pub(crate) struct FakeHost {
    pub(crate) legacy: Arc<ReferenceEngine>,
    pub(crate) tree: Arc<ReferenceEngine>,
    pub(crate) switched: AtomicBool,
    pub(crate) free: AtomicBool,
    /// The claim when the legacy tree is shared.
    pub(crate) claim: Option<FakeClaim>,
    /// Where the progress is saved.
    pub(crate) store: MemoryStateStore,
    /// The last import ended with its last batch not confirmed listed.
    pub(crate) unconfirmed: AtomicBool,
    /// Asks for free_now: after this many, moving stops being free.
    pub(crate) free_for: AtomicUsize,
    /// Written to the legacy tree as the switch happens (a turn racing it).
    pub(crate) racing_write: Mutex<Option<StoreItem>>,
}

impl FakeHost {
    pub(crate) async fn with(learnings: usize) -> Self {
        let legacy = Arc::new(ReferenceEngine::new());
        for i in 0..learnings {
            legacy.store(fact(&format!("fact {i}"))).await.unwrap();
        }
        Self {
            legacy,
            tree: Arc::new(ReferenceEngine::new()),
            switched: AtomicBool::new(false),
            free: AtomicBool::new(true),
            claim: None,
            store: MemoryStateStore::default(),
            unconfirmed: AtomicBool::new(false),
            free_for: AtomicUsize::new(usize::MAX),
            racing_write: Mutex::new(None),
        }
    }
}

pub(crate) fn fact(text: &str) -> StoreItem {
    StoreItem::learning(text, LearningKind::Fact, 0.5, MemoryMeta::default())
}

pub(crate) async fn count(engine: &dyn MemoryEngine) -> usize {
    let mut total = 0;
    let mut cursor = None;
    loop {
        let mut request = ListRequest::new(MetaFilter::default(), 100);
        request.cursor = cursor;
        let page = engine.list(request).await.unwrap();
        total += page.items.len();
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return total,
        }
    }
}

#[async_trait::async_trait]
impl LayoutHost for FakeHost {
    fn store(&self) -> &dyn MigrationStateStore {
        &self.store
    }

    async fn engines(&self) -> Result<Engines> {
        Ok(Engines {
            legacy: self.legacy.clone(),
            tree: self.tree.clone(),
        })
    }

    fn placement(&self) -> Result<Placement> {
        Ok(Placement {
            layout: MemoryLayout::new(Namespace::ROOT)?,
            chat_node: "ws:main".parse().unwrap(),
            flows: FlowPlacement::WithRoot,
            split_github_by_repo: false,
        })
    }

    fn is_switched(&self) -> bool {
        self.switched.load(Ordering::SeqCst)
    }

    async fn switch(&self) -> Result<()> {
        let racing = self.racing_write.lock().unwrap().take();
        if let Some(item) = racing {
            self.legacy.store(item).await.unwrap();
        }
        self.switched.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn free_now(&self) -> bool {
        let left = self.free_for.fetch_sub(1, Ordering::SeqCst);
        self.free.load(Ordering::SeqCst) && left > 0
    }

    async fn legacy_claim(&self) -> Result<Option<Box<dyn LegacyClaim>>> {
        Ok(self
            .claim
            .clone()
            .map(|claim| Box::new(claim) as Box<dyn LegacyClaim>))
    }

    fn listed_unconfirmed(&self) -> bool {
        self.unconfirmed.load(Ordering::SeqCst)
    }

    async fn wait_for_listing(&self) {
        // Yields, never sleeps: the tests run without a clock.
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
    }
}

/// An engine that refuses every operation with one error, the way the hosted
/// engine refuses a whole account (no credits, a rejected key, an outage).
/// It describes itself as the reference engine.
pub(crate) struct RefusingEngine {
    inner: ReferenceEngine,
    error: tinymemory_api::Error,
}

impl RefusingEngine {
    /// The hosted engine's refusal for an exhausted credit balance (HTTP 402).
    pub(crate) fn out_of_credits() -> Self {
        Self::with(tinymemory_api::Error::InsufficientCredits(
            "[USER_INSUFFICIENT_CREDITS] memory API fetch on api.example: the account has \
             insufficient credits (HTTP 402)"
                .into(),
        ))
    }

    /// Refuses every operation with `error`.
    pub(crate) fn with(error: tinymemory_api::Error) -> Self {
        Self {
            inner: ReferenceEngine::new(),
            error,
        }
    }
}

#[async_trait::async_trait]
impl MemoryEngine for RefusingEngine {
    fn descriptor(&self) -> &tinymemory_api::EngineDescriptor {
        self.inner.descriptor()
    }

    async fn health(&self) -> tinymemory_api::EngineHealth {
        self.inner.health().await
    }

    async fn recall(
        &self,
        _req: tinymemory_api::RecallRequest,
    ) -> tinymemory_api::Result<tinymemory_api::RecallAnswer> {
        Err(self.error.clone())
    }

    async fn fetch(
        &self,
        _req: tinymemory_api::FetchRequest,
    ) -> tinymemory_api::Result<tinymemory_api::FetchPage> {
        Err(self.error.clone())
    }

    async fn store(
        &self,
        _item: tinymemory_api::StoreItem,
    ) -> tinymemory_api::Result<tinymemory_api::StoreReceipt> {
        Err(self.error.clone())
    }

    async fn forget(
        &self,
        _target: tinymemory_api::ForgetTarget,
    ) -> tinymemory_api::Result<tinymemory_api::ForgetReport> {
        Err(self.error.clone())
    }

    async fn list(&self, _req: ListRequest) -> tinymemory_api::Result<tinymemory_api::ListPage> {
        Err(self.error.clone())
    }

    async fn export(
        &self,
        _req: ListRequest,
    ) -> tinymemory_api::Result<tinymemory_api::ExportPage> {
        Err(self.error.clone())
    }

    async fn consolidate(
        &self,
        _req: tinymemory_api::ConsolidateRequest,
    ) -> tinymemory_api::Result<tinymemory_api::ConsolidateReceipt> {
        Err(self.error.clone())
    }
}
