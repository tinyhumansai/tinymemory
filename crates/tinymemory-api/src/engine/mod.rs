//! The [`MemoryEngine`] trait and the [`EngineDescriptor`] that advertises
//! what an engine offers.

use async_trait::async_trait;
use serde::Serialize;

use crate::consolidate::{BeliefsRequest, ConsolidateReceipt, ConsolidateRequest, Consolidation};
use crate::error::{Error, Result};
use crate::explore::{
    ExplorePage, ExploreRequest, GetRequest, explore_by_listing, forget_within_by_get,
    get_by_listing,
};
use crate::item::ItemId;
use crate::item::{StoreItem, StoreReceipt};
use crate::namespace::Reach;
use crate::query::{
    EraseReport, EraseRequest, ExportPage, FetchMode, FetchPage, FetchRequest, ForgetReport,
    ForgetTarget, Hit, ListPage, ListRequest, RecallAnswer, RecallRequest,
};
use crate::write::WriteOptions;

/// A memory engine: recall, fetch, store, forget and list over typed items.
///
/// Every method validates its request first (the `validate` methods on the
/// request types) so engines refuse malformed calls identically. A
/// [`FetchMode`] not listed in [`EngineDescriptor::fetch_modes`] fails with
/// [`Error::Unsupported`].
#[async_trait]
pub trait MemoryEngine: Send + Sync {
    /// What this engine is and offers.
    fn descriptor(&self) -> &EngineDescriptor;

    /// Whether the engine can serve right now.
    async fn health(&self) -> EngineHealth;

    /// Answers a question from stored items, or, in
    /// [`crate::RecallMode::Evidence`], returns the evidence for the caller
    /// to answer from without writing an answer.
    ///
    /// # Errors
    ///
    /// Invalid requests, and the engine's own failures.
    async fn recall(&self, req: RecallRequest) -> Result<RecallAnswer>;

    /// Retrieves raw items matching a query.
    ///
    /// # Errors
    ///
    /// Invalid requests, [`Error::Unsupported`] for an undeclared mode, and
    /// the engine's own failures.
    async fn fetch(&self, req: FetchRequest) -> Result<FetchPage>;

    /// Stores one item. Storing an identical item again is a replay.
    ///
    /// # Errors
    ///
    /// Invalid items, and the engine's own failures.
    async fn store(&self, item: StoreItem) -> Result<StoreReceipt>;

    /// Stores one item, returning as soon as `options` allows (see
    /// [`crate::write`]). With [`crate::WaitFor::Visible`] this is exactly
    /// [`MemoryEngine::store`]; with [`crate::WaitFor::Accepted`] an engine
    /// may return once the item is durably accepted, before it is readable.
    ///
    /// The default serves every option as `store`, which is always correct.
    ///
    /// # Errors
    ///
    /// As [`MemoryEngine::store`].
    async fn store_with(&self, item: StoreItem, options: WriteOptions) -> Result<StoreReceipt> {
        let _ = options;
        self.store(item).await
    }

    /// Stores several items, in order: bulk ingestion (imports, backfills,
    /// source syncs).
    ///
    /// As with [`MemoryEngine::store`], every item is readable through
    /// [`MemoryEngine::list`], [`MemoryEngine::get`] and
    /// [`MemoryEngine::forget`] when the call returns; ranked
    /// [`MemoryEngine::fetch`] and [`MemoryEngine::recall`] may lag a moment
    /// behind for all but the last, which is what lets an engine skip a
    /// per-item wait. Receipts come back in item order. On an error the items
    /// before the failing one are stored; storing them again is a replay.
    ///
    /// The default stores one item at a time.
    ///
    /// # Errors
    ///
    /// No items or more than [`MAX_STORE_MANY`], an invalid item, and the
    /// engine's own failures.
    async fn store_many(&self, items: Vec<StoreItem>) -> Result<Vec<StoreReceipt>> {
        validate_many(&items)?;
        let mut receipts = Vec::with_capacity(items.len());
        for item in items {
            receipts.push(self.store(item).await?);
        }
        Ok(receipts)
    }

    /// Stores several items, in order, returning as soon as `options` allows
    /// (see [`crate::write`]). With [`crate::WaitFor::Visible`] this is
    /// exactly [`MemoryEngine::store_many`]; with [`crate::WaitFor::Accepted`]
    /// an engine may return once every item is durably accepted, before they
    /// are readable: a bulk import that reads nothing back until it ends.
    ///
    /// The default serves every option as `store_many`, which is always
    /// correct.
    ///
    /// # Errors
    ///
    /// As [`MemoryEngine::store_many`].
    async fn store_many_with(
        &self,
        items: Vec<StoreItem>,
        options: WriteOptions,
    ) -> Result<Vec<StoreReceipt>> {
        let _ = options;
        self.store_many(items).await
    }

    /// Removes items by id or by a non-empty filter.
    ///
    /// # Errors
    ///
    /// An empty target, and the engine's own failures.
    async fn forget(&self, target: ForgetTarget) -> Result<ForgetReport>;

    /// Removes the items `ids` name that lie within `reach`, and looks for
    /// them nowhere else: unlike [`ForgetTarget::Ids`], which finds an id
    /// wherever it lives, an id outside the reach is left alone as if it
    /// named nothing, and the engine reads no node the reach does not admit
    /// while looking. A host confining a caller to its own subtree forgets
    /// by id this way, so the lookup never touches another tree.
    ///
    /// The default reads the ids back with [`MemoryEngine::get`] and the
    /// reach, then forgets what came back ([`forget_within_by_get`]); an
    /// engine whose `forget` by id searches beyond the reach overrides it.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRequest`] for no ids or a blank one, and the engine's
    /// own failures.
    async fn forget_within(&self, ids: Vec<ItemId>, reach: Reach) -> Result<ForgetReport> {
        forget_within_by_get(self, ids, reach).await
    }

    /// Pages through stored items.
    ///
    /// # Errors
    ///
    /// Invalid requests, and the engine's own failures.
    async fn list(&self, req: ListRequest) -> Result<ListPage>;

    /// Pages through stored items like [`MemoryEngine::list`], for a view
    /// that shows a snippet of each and reads one whole with
    /// [`MemoryEngine::get`] when it is opened. The items, their order, ids,
    /// metadata and cursors are the listing's; only a hit's text may be the
    /// start of the item (a conversation's first turn, a chunked document's
    /// first piece), which spares an engine that stores those as several
    /// events from assembling each one.
    ///
    /// The default is [`MemoryEngine::list`], whose text is always whole.
    ///
    /// # Errors
    ///
    /// Invalid requests, and the engine's own failures.
    async fn list_preview(&self, req: ListRequest) -> Result<ListPage> {
        self.list(req).await
    }

    /// Pages through stored items like [`MemoryEngine::list`], handing each
    /// back whole as the [`StoreItem`] it was stored as, for moving memory:
    /// to another engine, or to another node (a new namespace is a new item,
    /// see [`StoreItem::fingerprint`]). A listing's [`Hit`] cannot do that:
    /// its text is the rendered form, which drops a document's mime, a
    /// conversation's turn times and tool calls, and a learning's kind and
    /// evidence.
    ///
    /// Items the engine holds only part of are named in
    /// [`ExportPage::incomplete`] instead of being dropped.
    ///
    /// Like every filtered read, an export with no reach, or a subtree
    /// reach, does not enter a `service:` sandbox below its node
    /// ([`crate::Reach::admits`]): a host exporting everything also exports
    /// each service node with a reach at it.
    ///
    /// The default refuses: an engine that can hand items back whole
    /// overrides it.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] when the engine cannot export, invalid
    /// requests, and the engine's own failures.
    async fn export(&self, req: ListRequest) -> Result<ExportPage> {
        req.validate()?;
        Err(Error::Unsupported(format!(
            "engine `{}` does not export items",
            self.descriptor().id
        )))
    }

    /// Erases what `req` names, for good: every item of its kinds at its
    /// nodes is deleted, not hidden, and storing one of them again stores it
    /// anew. For deleting a source, a workflow, a workspace or an account;
    /// [`MemoryEngine::forget`] removes items by id or filter.
    ///
    /// The default refuses: an engine that can erase overrides it. A caller
    /// that is refused falls back to [`MemoryEngine::forget`].
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] when the engine (or its credential) cannot
    /// erase, invalid requests (see [`EraseRequest::validate`]), and the
    /// engine's own failures.
    async fn erase(&self, req: EraseRequest) -> Result<EraseReport> {
        req.validate()?;
        Err(Error::Unsupported(format!(
            "engine `{}` does not erase",
            self.descriptor().id
        )))
    }

    /// Groups the items a filter admits by one [`crate::Facet`] and counts
    /// each value, for explorers (see [`crate::explore`]).
    ///
    /// The default pages through [`MemoryEngine::list`]
    /// ([`explore_by_listing`]); an engine that can aggregate server-side
    /// overrides it.
    ///
    /// # Errors
    ///
    /// Invalid requests, and the engine's own failures.
    async fn explore(&self, req: ExploreRequest) -> Result<ExplorePage> {
        explore_by_listing(self, req).await
    }

    /// Reads items whole by id, in the order named; an id that names nothing
    /// is left out.
    ///
    /// The default pages through [`MemoryEngine::list`]
    /// ([`get_by_listing`]); an engine that can look an id up directly
    /// overrides it.
    ///
    /// # Errors
    ///
    /// Invalid requests, and the engine's own failures.
    async fn get(&self, req: GetRequest) -> Result<Vec<Hit>> {
        get_by_listing(self, req).await
    }

    /// Asks the engine to distil what `req` covers into beliefs, returning
    /// once the job is taken, not done (see [`crate::consolidate`]). What it
    /// builds surfaces through ordinary reads.
    ///
    /// The default refuses: an engine declaring
    /// [`Consolidation::OnDemand`], [`Consolidation::Scheduled`] or
    /// [`Consolidation::Automatic`] overrides it.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] when the engine does not consolidate, invalid
    /// requests, and the engine's own failures.
    async fn consolidate(&self, req: ConsolidateRequest) -> Result<ConsolidateReceipt> {
        req.validate()?;
        Err(Error::Unsupported(format!(
            "engine `{}` does not consolidate memory",
            self.descriptor().id
        )))
    }

    /// The beliefs the engine built and keeps apart from its stored items,
    /// within `req.reach`: ranked for `req.query`, or most confident and
    /// then newest without one (see [`crate::consolidate`]).
    ///
    /// Each is a [`crate::ItemKind::Learning`] hit tagged
    /// [`crate::BELIEF_TAG`], at the node its sources live at. It is not a
    /// stored item: it cannot be listed, fetched or forgotten by id.
    ///
    /// The default holds none: an engine whose beliefs are ordinary
    /// learning items, or that builds none, keeps it.
    ///
    /// # Errors
    ///
    /// Invalid requests, and the engine's own failures.
    async fn beliefs(&self, req: BeliefsRequest) -> Result<Vec<Hit>> {
        req.validate()?;
        Ok(Vec::new())
    }
}

/// Most items one [`MemoryEngine::store_many`] call may take.
pub const MAX_STORE_MANY: usize = 100;

/// Checks a [`MemoryEngine::store_many`] batch: `1..=`[`MAX_STORE_MANY`]
/// items, each valid. Engines overriding `store_many` call it first.
///
/// # Errors
///
/// [`Error::InvalidRequest`] for an empty or oversized batch, and the first
/// invalid item's error.
pub fn validate_many(items: &[StoreItem]) -> Result<()> {
    if items.is_empty() || items.len() > MAX_STORE_MANY {
        return Err(Error::InvalidRequest(format!(
            "store_many takes between 1 and {MAX_STORE_MANY} items"
        )));
    }
    items.iter().try_for_each(StoreItem::validate)
}

/// What an engine is and offers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EngineDescriptor {
    /// Stable id used in configuration (`cortexdb`, `tinyhumans`).
    pub id: &'static str,
    /// Human-readable name.
    pub label: &'static str,
    /// One-sentence description.
    pub description: &'static str,
    /// Whether a third party runs the engine.
    pub hosted: bool,
    /// Whether configuration must name an endpoint.
    pub needs_endpoint: bool,
    /// Whether configuration must supply a credential.
    pub needs_key: bool,
    /// The endpoint used when configuration names none.
    pub default_endpoint: Option<&'static str>,
    /// The fetch modes the engine serves.
    pub fetch_modes: Vec<FetchMode>,
    /// How the engine turns raw memory into beliefs (see
    /// [`MemoryEngine::consolidate`]).
    pub consolidation: Consolidation,
}

/// Version of the TinyMemory engine contract reported in subsystem status.
pub const CONTRACT_VERSION: &str = "2.0";

/// Health state suitable for reporting to a host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineStatusHealth {
    /// The engine is serving normally.
    Ready,
    /// The engine is serving with reduced health.
    Degraded,
    /// The configured engine is not serving.
    Down,
    /// No usable engine is configured.
    Off,
}

/// Engine health and contract capabilities for a host subsystem status row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EngineStatus {
    /// Health state of the engine.
    pub health: EngineStatusHealth,
    /// Reason for a degraded, down, or off state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Operations the TinyMemory v2 contract provides.
    pub capabilities: Vec<&'static str>,
    /// TinyMemory contract version.
    pub contract_version: &'static str,
}

impl EngineStatus {
    /// Builds a status row from an engine descriptor and its current health.
    #[must_use]
    pub fn from_engine(descriptor: &EngineDescriptor, health: EngineHealth) -> Self {
        let (health, reason) = match health {
            EngineHealth::Ok => (EngineStatusHealth::Ready, None),
            EngineHealth::Degraded(reason) => (EngineStatusHealth::Degraded, Some(reason)),
            EngineHealth::Down(reason) => (EngineStatusHealth::Down, Some(reason)),
        };
        Self {
            health,
            reason,
            capabilities: operations_for(descriptor),
            contract_version: CONTRACT_VERSION,
        }
    }

    /// Builds the status row for a configuration with no usable engine.
    #[must_use]
    pub fn off(reason: impl Into<String>) -> Self {
        Self {
            health: EngineStatusHealth::Off,
            reason: Some(reason.into()),
            capabilities: Vec::new(),
            contract_version: CONTRACT_VERSION,
        }
    }
}

/// Operations every v2 engine serves.
fn operations_for(_descriptor: &EngineDescriptor) -> Vec<&'static str> {
    vec!["recall", "fetch", "store", "forget", "list"]
}

impl EngineDescriptor {
    /// Whether the engine serves `mode`.
    #[must_use]
    pub fn supports(&self, mode: FetchMode) -> bool {
        self.fetch_modes.contains(&mode)
    }

    /// Refuses a mode the engine does not serve.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] naming the mode and the engine.
    pub fn ensure_mode(&self, mode: FetchMode) -> Result<()> {
        if self.supports(mode) {
            Ok(())
        } else {
            Err(Error::Unsupported(format!(
                "engine `{}` does not offer {} fetch",
                self.id,
                mode.as_str()
            )))
        }
    }
}

/// Whether an engine can serve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", content = "reason", rename_all = "snake_case")]
pub enum EngineHealth {
    /// Serving.
    Ok,
    /// Serving, impaired (rate limited, partially available).
    Degraded(String),
    /// Not serving.
    Down(String),
}

impl EngineHealth {
    /// Whether the engine is serving at all.
    #[must_use]
    pub fn is_serving(&self) -> bool {
        !matches!(self, Self::Down(_))
    }
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
