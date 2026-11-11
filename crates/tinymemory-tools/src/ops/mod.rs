//! Validated memory request shaping shared by hosts and the agent tools.
//!
//! [`MemoryOps`] applies an optional host [`Confinement`] consistently before
//! a request reaches an engine. It also owns the common read limits, learning
//! metadata merge, explorer path rules, and deletion fallback used by hosts.
//! It has no runtime dependency: asynchronous engine calls use
//! [`tinymemory_api::MemoryEngine`] directly, and pending deletions are handed
//! back through a host-provided enqueue callback.

use chrono::{DateTime, NaiveDate, Utc};
use tinymemory_api::explore::{MAX_BUCKETS as API_MAX_BUCKETS, MAX_GET_IDS};
use tinymemory_api::{
    EngineDescriptor, EraseRequest, Error, ExplorePage, ExploreRequest, Facet, FetchMode,
    FetchRequest, ForgetReport, ForgetTarget, GetRequest, ItemId, ItemKind, LearningKind, ListPage,
    ListRequest, MemoryEngine, MemoryMeta, MetaFilter, Namespace, Reach, RecallMode, RecallRequest,
    Result, StoreItem, ThreadWindow, TimeHint,
};

use crate::confine::Confinement;
use crate::deletion::PendingDeletion;

/// Default page size for recall, fetch and list requests.
pub const DEFAULT_LIMIT: usize = 10;

/// Largest page size for recall, fetch and list requests.
pub const MAX_LIMIT: usize = 100;

/// Default confidence for a learning without an explicit value.
pub const DEFAULT_LEARNING_CONFIDENCE: f32 = 0.8;

/// Default bucket count for an explorer request.
pub const DEFAULT_BUCKETS: usize = 50;

/// Largest bucket count accepted by the memory API.
pub const MAX_BUCKETS: usize = API_MAX_BUCKETS;

/// Most steps accepted in an explorer path.
pub const MAX_PATH_STEPS: usize = 16;

/// One local calendar date range, inclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateRange {
    /// First local day.
    pub from: NaiveDate,
    /// Last local day; omitted means `from`.
    pub to: Option<NaiveDate>,
}

/// Converts an optional local date range into the engine's ranking hint.
///
/// The dates remain local calendar dates; the zone is carried on the hint so
/// engines interpret it consistently. An omitted end date means one day.
///
/// # Errors
///
/// Returns [`Error::InvalidRequest`] for a reversed range or an invalid IANA
/// zone.
pub fn time_hint(range: Option<DateRange>, zone: Option<&str>) -> Result<Option<TimeHint>> {
    range
        .map(|range| {
            TimeHint::new(
                range.from,
                range.to.unwrap_or(range.from),
                zone.map(str::to_string),
            )
        })
        .transpose()
}

/// One facet selection in an explorer path.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PathStep {
    /// The facet to narrow.
    pub facet: Facet,
    /// The selected bucket value.
    pub value: String,
}

/// Inputs for building a recall request.
#[derive(Debug, Clone)]
pub struct RecallOptions {
    /// The question to answer.
    pub question: String,
    /// Additional metadata constraints.
    pub filter: Option<MetaFilter>,
    /// Maximum citations to return.
    pub limit: Option<usize>,
    /// Optional answer instructions.
    pub instructions: Option<String>,
    /// Local date range hint.
    pub refers_to: Option<TimeHint>,
    /// Current thread turns already present in the prompt.
    pub exclude_thread: Option<ThreadWindow>,
    /// Whether the engine answers or returns evidence only.
    pub mode: RecallMode,
}

/// Inputs for building a fetch request.
#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// The query text.
    pub query: String,
    /// Requested mode; absent uses the engine's first declared mode.
    pub mode: Option<FetchMode>,
    /// Additional metadata constraints.
    pub filter: Option<MetaFilter>,
    /// Maximum hits to return.
    pub limit: Option<usize>,
    /// Cursor from a previous page.
    pub cursor: Option<String>,
    /// Local date range hint.
    pub refers_to: Option<TimeHint>,
    /// Current thread turns already present in the prompt.
    pub exclude_thread: Option<ThreadWindow>,
}

/// Inputs for building a list request.
#[derive(Debug, Clone, Default)]
pub struct ListOptions {
    /// Additional metadata constraints.
    pub filter: Option<MetaFilter>,
    /// Explorer path to apply over the filter.
    pub path: Vec<PathStep>,
    /// Maximum items to return.
    pub limit: Option<usize>,
    /// Cursor from a previous page.
    pub cursor: Option<String>,
}

/// Inputs for building an explorer request.
#[derive(Debug, Clone)]
pub struct ExploreOptions {
    /// Facet to group by.
    pub facet: Facet,
    /// Breadcrumb path to apply before grouping.
    pub path: Vec<PathStep>,
    /// Additional metadata constraints.
    pub filter: Option<MetaFilter>,
    /// Maximum buckets; absent uses [`DEFAULT_BUCKETS`].
    pub limit: Option<usize>,
    /// Maximum items a listing-based engine scans.
    pub scan_limit: Option<usize>,
}

/// Inputs for building a learning item.
#[derive(Debug, Clone)]
pub struct LearningOptions {
    /// Learned text.
    pub text: String,
    /// Learning kind; absent means `fact`.
    pub kind: Option<LearningKind>,
    /// Confidence in `0..=1`; absent uses [`DEFAULT_LEARNING_CONFIDENCE`].
    pub confidence: Option<f32>,
    /// Caller-supplied metadata.
    pub meta: Option<MemoryMeta>,
    /// Host facts, which override caller metadata field by field.
    pub host_meta: Option<MemoryMeta>,
    /// Namespace used when both metadata sources leave it at the global root.
    pub default_namespace: Namespace,
    /// Clock value used when neither metadata source sets `observed_at`.
    pub now: DateTime<Utc>,
}

/// Host-owned request shaping that applies one confinement policy everywhere
/// or, when explicitly constructed as unconfined, preserves caller scope.
#[derive(Debug, Clone)]
pub struct MemoryOps {
    confinement: Option<Confinement>,
}

impl MemoryOps {
    /// Request shaping confined to the host-selected reach.
    #[must_use]
    pub fn new(confinement: Confinement) -> Self {
        Self {
            confinement: Some(confinement),
        }
    }

    /// Request shaping without host-selected namespace confinement.
    ///
    /// Caller-supplied reaches and namespaces are preserved. Shared request
    /// validation, metadata shaping, and limits still apply.
    #[must_use]
    pub fn unconfined() -> Self {
        Self { confinement: None }
    }

    /// The confinement this instance applies, or `None` when unconfined.
    #[must_use]
    pub fn confinement(&self) -> Option<&Confinement> {
        self.confinement.as_ref()
    }

    fn filter(&self, filter: Option<MetaFilter>) -> Result<MetaFilter> {
        match &self.confinement {
            Some(confinement) => confinement.filter(filter),
            None => Ok(filter.unwrap_or_default()),
        }
    }

    fn reach(&self, reach: Option<Reach>) -> Result<Option<Reach>> {
        match &self.confinement {
            Some(confinement) => Ok(Some(confinement.reach(reach)?)),
            None => Ok(reach),
        }
    }

    /// Builds and validates a recall request, applying configured
    /// confinement when present.
    ///
    /// # Errors
    ///
    /// Invalid request fields or, when confined, a refused out-of-reach
    /// filter.
    pub fn recall_request(&self, options: RecallOptions) -> Result<RecallRequest> {
        let request = RecallRequest {
            question: options.question,
            filter: self.filter(options.filter)?,
            limit: clamp_limit(options.limit),
            instructions: options.instructions,
            refers_to: options.refers_to,
            exclude_thread: options.exclude_thread,
            mode: options.mode,
        };
        request.validate()?;
        Ok(request)
    }

    /// Builds and validates a fetch request using the engine's advertised
    /// modes, applying configured confinement when present.
    ///
    /// An omitted mode selects the first mode the engine declares. An
    /// explicitly requested mode must be declared by that engine.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] when no fetch mode is available or the requested
    /// mode is unsupported, invalid request fields, or (when confined) a
    /// refused filter.
    pub fn fetch_request(
        &self,
        options: FetchOptions,
        descriptor: &EngineDescriptor,
    ) -> Result<FetchRequest> {
        let mode = match options.mode {
            Some(mode) => {
                descriptor.ensure_mode(mode)?;
                mode
            }
            None => *descriptor.fetch_modes.first().ok_or_else(|| {
                Error::Unsupported(format!("engine `{}` offers no fetch mode", descriptor.id))
            })?,
        };
        let request = FetchRequest {
            query: options.query,
            mode,
            filter: self.filter(options.filter)?,
            limit: clamp_limit(options.limit),
            cursor: options.cursor,
            beliefs: 0,
            max_scopes: None,
            refers_to: options.refers_to,
            exclude_thread: options.exclude_thread,
        };
        request.validate()?;
        Ok(request)
    }

    /// Builds and validates a list request after applying its explorer path.
    ///
    /// # Errors
    ///
    /// An invalid or overlong path, an invalid request, or (when confined) a
    /// refused out-of-reach filter.
    pub fn list_request(&self, options: ListOptions) -> Result<ListRequest> {
        let filter = narrowed(options.filter, &options.path)?;
        let request = ListRequest {
            filter: self.filter(Some(filter))?,
            limit: clamp_limit(options.limit),
            cursor: options.cursor,
        };
        request.validate()?;
        Ok(request)
    }

    /// Builds and validates an explorer request after applying its path.
    ///
    /// # Errors
    ///
    /// An invalid or overlong path, an invalid request, or (when confined) a
    /// refused out-of-reach filter.
    pub fn explore_request(&self, options: ExploreOptions) -> Result<ExploreRequest> {
        let filter = narrowed(options.filter, &options.path)?;
        let mut request =
            ExploreRequest::new(options.facet, options.limit.unwrap_or(DEFAULT_BUCKETS));
        request.filter = self.filter(Some(filter))?;
        if let Some(scan_limit) = options.scan_limit {
            request.scan_limit = scan_limit;
        }
        request.validate()?;
        Ok(request)
    }

    /// Builds and validates a get request using the configured confinement,
    /// or preserving the caller's optional reach when unconfined.
    ///
    /// # Errors
    ///
    /// Invalid ids or, when confined, a refused out-of-reach request.
    pub fn items_get_request(&self, ids: Vec<ItemId>, reach: Option<Reach>) -> Result<GetRequest> {
        let request = GetRequest {
            ids,
            reach: self.reach(reach)?,
        };
        request.validate()?;
        Ok(request)
    }

    /// Builds a learning item with host metadata and timestamp, applying
    /// confinement when configured and the default namespace for root metadata.
    ///
    /// # Errors
    ///
    /// Invalid confidence, text, item metadata or, when confined, a refused
    /// namespace.
    pub fn learning_item(&self, options: LearningOptions) -> Result<StoreItem> {
        let confidence = options.confidence.unwrap_or(DEFAULT_LEARNING_CONFIDENCE);
        if !(0.0..=1.0).contains(&confidence) {
            return Err(Error::InvalidRequest(
                "confidence must be between 0 and 1".to_string(),
            ));
        }

        let mut meta = options.meta.unwrap_or_default();
        if let Some(host) = options.host_meta {
            meta = meta.overlay(host);
        }
        if meta.observed_at.is_none() {
            meta.observed_at = Some(options.now);
        }
        let meta = match &self.confinement {
            Some(confinement) => confinement.learn_meta(Some(meta), &options.default_namespace)?,
            None => {
                if meta.namespace.is_root() {
                    meta.namespace = options.default_namespace;
                }
                meta
            }
        };
        let item = StoreItem::learning(
            options.text.trim(),
            options.kind.unwrap_or(LearningKind::Fact),
            confidence,
            meta,
        );
        item.validate()?;
        Ok(item)
    }

    /// Applies the configured write policy, if any, and validates a
    /// caller-built item before it reaches an engine.
    ///
    /// # Errors
    ///
    /// Invalid item metadata or, when confined, a refused namespace.
    pub fn store_item(&self, mut item: StoreItem) -> Result<StoreItem> {
        if let Some(confinement) = &self.confinement {
            confinement.item(&mut item)?;
        }
        item.validate()?;
        Ok(item)
    }

    /// Builds and validates a whole-tree erase request.
    ///
    /// Erasing the engine's whole tree crosses configured ordinary namespace
    /// confinement by design. The caller must require explicit consent and
    /// perform any host write fence before sending the returned request to the
    /// engine.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] unless `confirmed` is true.
    pub fn erase_all_request(&self, confirmed: bool) -> Result<EraseRequest> {
        if !confirmed {
            return Err(Error::InvalidRequest(
                "erasing all memory needs explicit confirmation".to_string(),
            ));
        }
        let mut request = EraseRequest::new(Reach::subtree(Namespace::ROOT));
        request.whole_tree = true;
        request.validate()?;
        Ok(request)
    }

    /// Lists items after validating and confining the request.
    ///
    /// # Errors
    ///
    /// Invalid paths or requests, refused reaches when confined, and engine
    /// errors.
    pub async fn list(&self, engine: &dyn MemoryEngine, options: ListOptions) -> Result<ListPage> {
        engine.list(self.list_request(options)?).await
    }

    /// Explores items after validating and confining the request.
    ///
    /// # Errors
    ///
    /// Invalid paths or requests, refused reaches when confined, and engine
    /// errors.
    pub async fn explore(
        &self,
        engine: &dyn MemoryEngine,
        options: ExploreOptions,
    ) -> Result<ExplorePage> {
        engine.explore(self.explore_request(options)?).await
    }

    /// Reads ids in the configured reach, or the caller-supplied optional
    /// reach when unconfined.
    ///
    /// # Errors
    ///
    /// Invalid ids, refused reaches when confined, and engine errors.
    pub async fn items_get(
        &self,
        engine: &dyn MemoryEngine,
        ids: Vec<ItemId>,
        reach: Option<Reach>,
    ) -> Result<Vec<tinymemory_api::Hit>> {
        engine.get(self.items_get_request(ids, reach)?).await
    }

    /// Forgets ids within the effective reach, or globally when unconfined
    /// and no reach is supplied.
    ///
    /// With an explicit reach, ids are first read within that reach and only
    /// those found are forgotten. In unconfined mode with no reach, the
    /// engine's global ID deletion semantics apply, including service
    /// namespaces.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] if no non-blank id is supplied, if
    /// the reach is refused when confined, or if the engine reports an error.
    pub async fn forget(
        &self,
        engine: &dyn MemoryEngine,
        ids: Vec<String>,
        reach: Option<Reach>,
    ) -> Result<ForgetReport> {
        let ids: Vec<ItemId> = ids
            .into_iter()
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty())
            .map(ItemId::new)
            .collect();
        if ids.is_empty() {
            return Err(Error::InvalidRequest(
                "forget needs at least one id".to_string(),
            ));
        }
        let reach = self.reach(reach)?;
        match reach {
            Some(reach) => {
                let ids = within_reach(engine, ids, reach.clone()).await?;
                if ids.is_empty() {
                    return Ok(ForgetReport::default());
                }
                engine.forget_within(ids, reach).await
            }
            None => engine.forget(ForgetTarget::Ids(ids)).await,
        }
    }

    /// Forgets every item matching a non-empty filter in one engine call.
    ///
    /// The configured confinement is applied to the filter while preserving
    /// its metadata clauses. This keeps filtered deletion unbounded: it does
    /// not page through matches or convert them to ids first.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for an empty filter or a refused
    /// out-of-reach filter, and returns engine errors unchanged.
    pub async fn forget_filter(
        &self,
        engine: &dyn MemoryEngine,
        filter: MetaFilter,
    ) -> Result<ForgetReport> {
        if filter.is_empty() {
            return Err(Error::InvalidRequest(
                "forget needs a non-empty filter".to_string(),
            ));
        }
        let filter = self.filter(Some(filter))?;
        engine.forget(ForgetTarget::Filter(filter)).await
    }

    /// Tries a pending deletion, queueing it once when memory is off or the
    /// engine fails. Off is a safe zero-count outcome; engine errors remain
    /// visible to the host after the deletion is queued.
    ///
    /// # Errors
    ///
    /// Returns the engine error after queueing when the engine is reachable
    /// but the deletion fails.
    pub async fn forget_or_enqueue<F>(
        &self,
        engine: Option<&dyn MemoryEngine>,
        deletion: PendingDeletion,
        enqueue: F,
    ) -> Result<usize>
    where
        F: FnOnce(PendingDeletion),
    {
        let Some(engine) = engine else {
            enqueue(deletion);
            return Ok(0);
        };
        match self.forget_pending(engine, &deletion).await {
            Ok(forgotten) => Ok(forgotten),
            Err(error) => {
                enqueue(deletion);
                Err(error)
            }
        }
    }

    /// Forgets a thread's conversation items, or queues the deletion when the
    /// engine is off or fails. Blank thread ids are a no-op. In unconfined
    /// mode the filter has no reach, so the engine's default ordinary
    /// root-subtree behavior applies and excludes service namespaces.
    ///
    /// # Errors
    ///
    /// Returns the engine error after queueing when the engine is reachable
    /// but the deletion fails.
    pub async fn forget_thread<F>(
        &self,
        engine: Option<&dyn MemoryEngine>,
        thread_id: &str,
        enqueue: F,
    ) -> Result<usize>
    where
        F: FnOnce(PendingDeletion),
    {
        if thread_id.trim().is_empty() {
            return Ok(0);
        }
        self.forget_or_enqueue(
            engine,
            PendingDeletion::Thread {
                thread_id: thread_id.to_string(),
            },
            enqueue,
        )
        .await
    }

    /// Forgets items belonging to a source, or queues the deletion when the
    /// engine is off or fails. In unconfined mode the filter has no reach, so
    /// the engine's default ordinary root-subtree behavior applies and
    /// excludes service namespaces.
    ///
    /// # Errors
    ///
    /// Returns the engine error after queueing when the engine is reachable
    /// but the deletion fails.
    pub async fn forget_source<F>(
        &self,
        engine: Option<&dyn MemoryEngine>,
        source_id: &str,
        enqueue: F,
    ) -> Result<usize>
    where
        F: FnOnce(PendingDeletion),
    {
        self.forget_or_enqueue(
            engine,
            PendingDeletion::Source {
                source_id: source_id.to_string(),
            },
            enqueue,
        )
        .await
    }
}

/// Clamps a caller's page size into `1..=MAX_LIMIT`.
#[must_use]
pub fn clamp_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

/// Applies each explorer step to `filter` in order.
///
/// # Errors
///
/// Returns [`Error::InvalidRequest`] for a path longer than
/// [`MAX_PATH_STEPS`] or a facet value the API cannot narrow to.
pub fn narrowed(filter: Option<MetaFilter>, path: &[PathStep]) -> Result<MetaFilter> {
    if path.len() > MAX_PATH_STEPS {
        return Err(Error::InvalidRequest(format!(
            "an explorer path takes at most {MAX_PATH_STEPS} steps"
        )));
    }
    let mut filter = filter.unwrap_or_default();
    for step in path {
        step.facet.narrow(&mut filter, &step.value)?;
    }
    Ok(filter)
}

/// Returns ids among `ids` which the engine finds within `reach`, chunked to
/// the contract's maximum get page size.
///
/// # Errors
///
/// Returns the first engine error.
pub async fn within_reach(
    engine: &dyn MemoryEngine,
    ids: Vec<ItemId>,
    reach: Reach,
) -> Result<Vec<ItemId>> {
    let mut kept = Vec::new();
    for chunk in ids.chunks(MAX_GET_IDS) {
        let found = engine
            .get(GetRequest {
                ids: chunk.to_vec(),
                reach: Some(reach.clone()),
            })
            .await?;
        kept.extend(found.into_iter().map(|hit| hit.id));
    }
    Ok(kept)
}

impl MemoryOps {
    async fn forget_pending(
        &self,
        engine: &dyn MemoryEngine,
        deletion: &PendingDeletion,
    ) -> Result<usize> {
        let filter = match deletion {
            PendingDeletion::Thread { thread_id } => MetaFilter {
                thread_id: Some(thread_id.clone()),
                ..MetaFilter::kinds([ItemKind::Conversation])
            },
            PendingDeletion::Source { source_id } => MetaFilter {
                source_id: Some(source_id.clone()),
                ..MetaFilter::default()
            },
        };
        Ok(self.forget_filter(engine, filter).await?.forgotten)
    }
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
