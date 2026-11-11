//! Syncing sources into memory: the loop a host drives over its source
//! registry.
//!
//! A host keeps its sources (where they are configured, how often they sync,
//! what the last run left) and implements [`SourceRegistry`] over that. This
//! module owns the rest of the loop:
//!
//! - [`Syncer`] claims a source so a second request does not start a parallel
//!   run ([`Syncer::start`]), picks the sources whose schedule has elapsed
//!   ([`Syncer::due`]), and records how a run ended ([`Syncer::finish`]).
//! - [`entry_for`] turns a [`RegisteredSource`] into the reader's
//!   [`MemorySourceEntry`], filling the per-kind read caps.
//! - [`read_source`] reads it through the readers (`sources-network`).
//! - [`store_all`] stores the placed items on an engine in batches of
//!   [`STORE_CHUNK`], falling back to item by item when a batch is refused,
//!   and reports which nodes it touched so the host can queue belief builds.
//!
//! Where an item is filed (which brain node), the document converter, the
//! belief-build jobs and the executor that runs a claimed sync stay with the
//! host: the module links no executor and holds no process-wide state.

use std::collections::{BTreeSet, HashSet};
use std::sync::{Arc, Mutex, PoisonError};

use chrono::{DateTime, Duration, Utc};
use tinymemory_api::{
    ConsolidateRequest, Consolidation, ItemKind, MAX_STORE_MANY, MemoryEngine, Namespace, Reach,
    Result as ApiResult, StoreItem,
};
use tinymemory_tools::BackgroundJob;

use super::types::{MemorySourceEntry, SourceKind};

/// Fewest minutes between scheduled syncs of one source.
pub const MIN_SCHEDULE_MINS: u32 = 15;

/// How many items go to the engine in one request. The contract caps a batch
/// at [`MAX_STORE_MANY`]; sitting at the cap is what makes the request count
/// `ceil(N / 100)` instead of `N`.
pub const STORE_CHUNK: usize = MAX_STORE_MANY;

/// Why a sync failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SyncError {
    /// The source is malformed or unknown.
    #[error("invalid source: {0}")]
    Invalid(String),
    /// The source could not be read.
    #[error("reading the source failed: {0}")]
    Read(String),
    /// The engine refused or failed.
    #[error(transparent)]
    Engine(#[from] tinymemory_api::Error),
}

impl SyncError {
    /// Whether the failure is about the account, not the item: out of
    /// credits, not in the plan, signed out, or the engine unreachable. Retrying siblings
    /// would fail the same way, so a sync stops on it.
    #[must_use]
    pub fn is_account_wide(&self) -> bool {
        match self {
            Self::Engine(error) => error.is_account_wide(),
            _ => false,
        }
    }
}

/// A source as the host's registry holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredSource {
    /// Stable unique id.
    pub id: String,
    /// What reads it.
    pub kind: SourceKind,
    /// A path for `Folder` and `File`, a URL for the rest.
    pub target: String,
    /// Human-readable label.
    pub label: String,
    /// Minutes between scheduled syncs; `None` syncs only on request.
    pub schedule_mins: Option<u32>,
}

/// Where a source's sync stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SyncStatus {
    /// Not running; the last run (if any) succeeded.
    #[default]
    Idle,
    /// A run is going.
    Syncing,
    /// The last run failed.
    Error,
}

/// What the registry remembers of a source's last runs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SyncState {
    /// Where the sync stands.
    pub status: SyncStatus,
    /// When the last run ended.
    pub last_sync_at: Option<DateTime<Utc>>,
}

/// A change to a source's remembered state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncUpdate {
    /// A run was claimed and is starting.
    Started,
    /// A run stored `items` and ended at `at`. Every item is listed by
    /// then; ranked recall can trail by an index refresh (see [`store_all`]).
    Finished {
        /// Items stored.
        items: u64,
        /// When it ended.
        at: DateTime<Utc>,
    },
    /// A run failed at `at`; `error` is for display.
    Failed {
        /// The failure, as the host should show it.
        error: String,
        /// When it ended.
        at: DateTime<Utc>,
    },
}

/// The host's source registry: what is configured and what each sync left.
pub trait SourceRegistry: Send + Sync {
    /// Every configured source.
    fn sources(&self) -> Vec<RegisteredSource>;

    /// What the registry remembers of source `id`.
    fn state(&self, id: &str) -> Option<SyncState>;

    /// Records a change to source `id`'s state.
    fn record(&self, id: &str, update: SyncUpdate);
}

/// Whether a source with `schedule_mins` is due at `now`, given its `state`.
/// A syncing source is never due; one that never ran is; otherwise the
/// schedule (at least [`MIN_SCHEDULE_MINS`]) must have elapsed.
#[must_use]
pub fn is_due(schedule_mins: Option<u32>, state: Option<&SyncState>, now: DateTime<Utc>) -> bool {
    let Some(mins) = schedule_mins else {
        return false;
    };
    match state {
        Some(state) if state.status == SyncStatus::Syncing => false,
        Some(state) => state.last_sync_at.is_none_or(|at| {
            now.signed_duration_since(at)
                >= Duration::minutes(i64::from(mins.max(MIN_SCHEDULE_MINS)))
        }),
        None => true,
    }
}

/// A claimed source: the right to run its sync. Dropping it releases the
/// claim, so a run that panics or is cancelled does not wedge the source.
#[derive(Debug)]
pub struct Claim {
    source: RegisteredSource,
    running: Arc<Mutex<HashSet<String>>>,
}

impl Claim {
    /// The claimed source.
    #[must_use]
    pub fn source(&self) -> &RegisteredSource {
        &self.source
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        self.running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.source.id);
    }
}

/// The sync loop over a [`SourceRegistry`]. Cheap to clone; clones share the
/// set of running syncs.
#[derive(Clone)]
pub struct Syncer {
    registry: Arc<dyn SourceRegistry>,
    running: Arc<Mutex<HashSet<String>>>,
}

impl std::fmt::Debug for Syncer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Syncer").finish_non_exhaustive()
    }
}

impl Syncer {
    /// A loop over `registry`.
    #[must_use]
    pub fn new(registry: Arc<dyn SourceRegistry>) -> Self {
        Self {
            registry,
            running: Arc::default(),
        }
    }

    /// Claims source `id`, or every source, that is not already syncing, and
    /// marks each [`SyncUpdate::Started`]. The host runs each [`Claim`] (read,
    /// place, [`store_all`]) and passes it to [`Syncer::finish`].
    ///
    /// # Errors
    ///
    /// [`SyncError::Invalid`] when `id` names no source.
    pub fn start(&self, id: Option<&str>) -> Result<Vec<Claim>, SyncError> {
        let sources = self.registry.sources();
        let targets: Vec<RegisteredSource> = match id {
            Some(id) => vec![
                sources
                    .into_iter()
                    .find(|source| source.id == id)
                    .ok_or_else(|| SyncError::Invalid(format!("no source `{id}`")))?,
            ],
            None => sources,
        };
        let mut claims = Vec::new();
        for source in targets {
            let claimed = self
                .running
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(source.id.clone());
            if !claimed {
                log::debug!("[memory:sources] {} already syncing", source.id);
                continue;
            }
            self.registry.record(&source.id, SyncUpdate::Started);
            claims.push(Claim {
                source,
                running: Arc::clone(&self.running),
            });
        }
        Ok(claims)
    }

    /// The ids of the sources whose schedule has elapsed at `now`.
    #[must_use]
    pub fn due(&self, now: DateTime<Utc>) -> Vec<String> {
        self.registry
            .sources()
            .into_iter()
            .filter(|source| {
                is_due(
                    source.schedule_mins,
                    self.registry.state(&source.id).as_ref(),
                    now,
                )
            })
            .map(|source| source.id)
            .collect()
    }

    /// Records how a claimed run ended and releases the claim.
    pub fn finish(&self, claim: Claim, result: &Result<u64, SyncError>) {
        let at = Utc::now();
        let id = claim.source.id.clone();
        match result {
            Ok(items) => {
                self.registry
                    .record(&id, SyncUpdate::Finished { items: *items, at });
                log::info!("[memory:sources] sync finished id={id} items={items}");
            }
            Err(error) => {
                self.registry.record(
                    &id,
                    SyncUpdate::Failed {
                        error: error.to_string(),
                        at,
                    },
                );
                log::warn!("[memory:sources] sync failed id={id}");
            }
        }
        drop(claim);
    }
}

/// The reader's entry for `source`: the path or URL by kind, the per-kind
/// read caps filled in, and validated.
///
/// A GitHub repo reads at most 10 PRs, 10 issues and 50 commits, a feed at
/// most 20 items, unless the entry sets its own.
///
/// # Errors
///
/// [`SyncError::Invalid`] when the entry fails validation.
pub fn entry_for(source: &RegisteredSource) -> Result<MemorySourceEntry, SyncError> {
    let mut entry =
        MemorySourceEntry::new(source.id.clone(), source.kind.clone(), source.label.clone());
    match source.kind {
        SourceKind::Folder | SourceKind::File => entry.path = Some(source.target.clone()),
        _ => entry.url = Some(source.target.clone()),
    }
    apply_kind_defaults(&mut entry);
    entry
        .validate()
        .map_err(|error| SyncError::Invalid(error.to_string()))?;
    Ok(entry)
}

fn apply_kind_defaults(entry: &mut MemorySourceEntry) {
    match entry.kind {
        SourceKind::GithubRepo => {
            entry.max_prs.get_or_insert(10);
            entry.max_issues.get_or_insert(10);
            entry.max_commits.get_or_insert(50);
        }
        SourceKind::RssFeed => {
            entry.max_items.get_or_insert(20);
        }
        _ => {}
    }
}

/// Reads `source` through its reader, anchored on `workspace`, converting
/// files with `converter`. Per-item failures are logged and skipped.
///
/// # Errors
///
/// [`SyncError::Invalid`] for a malformed source, [`SyncError::Read`] when the
/// listing fails.
#[cfg(feature = "sources-network")]
pub async fn read_source(
    source: &RegisteredSource,
    workspace: &std::path::Path,
    converter: &dyn crate::documents::DocumentConverter,
) -> Result<super::items::Collected, SyncError> {
    let entry = entry_for(source)?;
    let reader = super::readers::reader_for_request(&entry.kind);
    let collected = super::items::collect_items(reader.as_ref(), &entry, workspace, converter)
        .await
        .map_err(|error| SyncError::Read(error.to_string()))?;
    if !collected.skipped.is_empty() {
        log::debug!(
            "[memory:sources] id={} skipped={} some items could not be read",
            source.id,
            collected.skipped.len()
        );
    }
    Ok(collected)
}

/// What [`store_all`] stored.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Stored {
    /// Items stored.
    pub stored: u64,
    /// Items the engine refused one by one.
    pub failed: u64,
    /// The nodes that received at least one item.
    pub touched: BTreeSet<Namespace>,
}

/// What [`store_placed`] stored and the host can schedule afterward.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Placed {
    /// Store result, including nodes that accepted items.
    pub stored: Stored,
    /// One exact-node belief build for each node touched by an on-demand
    /// engine; empty for other consolidation modes.
    pub jobs: Vec<BackgroundJob>,
}

/// Places each item with the host's callback, stores it, and returns belief
/// jobs for accepted nodes when the engine consolidates on demand. The host
/// owns the callback and decides how to enqueue returned jobs.
///
/// # Errors
///
/// Placement failures and the errors from [`store_all`].
pub async fn store_placed(
    engine: &dyn MemoryEngine,
    source_id: &str,
    items: Vec<StoreItem>,
    mut placement: impl FnMut(&StoreItem) -> ApiResult<Namespace>,
) -> Result<Placed, SyncError> {
    let mut placed = Vec::with_capacity(items.len());
    for item in items {
        let node = placement(&item).map_err(|error| SyncError::Invalid(error.to_string()))?;
        placed.push((
            node.clone(),
            tinymemory_tools::layout::placement::file_into(node, item),
        ));
    }
    let stored = store_all(engine, source_id, placed).await?;
    let jobs = if engine.descriptor().consolidation == Consolidation::OnDemand {
        stored
            .touched
            .iter()
            .cloned()
            .map(|node| BackgroundJob::BuildBeliefs {
                request: ConsolidateRequest::new(Reach::exact(node)).kinds([ItemKind::Document]),
            })
            .collect()
    } else {
        Vec::new()
    };
    Ok(Placed { stored, jobs })
}

/// Stores items already placed at their node on `engine`, [`STORE_CHUNK`] at
/// a time. A refused batch is retried item by item, so one invalid item costs
/// only itself; an account-wide refusal stops the sync.
///
/// On return every stored item is listed, but not necessarily recallable:
/// the Cortex engines' `store_many` waits for ranked recall only on each
/// batch's final event, so earlier items of a batch can show in listings
/// before ranked recall finds them, until the engine's index refreshes. That
/// is the price of waiting once per batch instead of once per item, the same
/// price a batched import pays. Items stored one by one after a refused batch
/// are each recall-awaited. See tinyhumansai/openhuman#7474.
///
/// # Errors
///
/// An account-wide failure, or the last item error when nothing was stored.
pub async fn store_all(
    engine: &dyn MemoryEngine,
    source_id: &str,
    mut placed: Vec<(Namespace, StoreItem)>,
) -> Result<Stored, SyncError> {
    let mut out = Stored::default();
    let mut last_error = None;
    while !placed.is_empty() {
        let take = placed.len().min(STORE_CHUNK);
        let chunk: Vec<_> = placed.drain(..take).collect();
        // Cloned because a rejected batch hands nothing back, and the
        // item-by-item retry needs the items.
        let batch: Vec<StoreItem> = chunk.iter().map(|(_, item)| item.clone()).collect();
        match engine.store_many(batch).await {
            Ok(receipts) => {
                out.stored += receipts.len() as u64;
                out.touched.extend(chunk.into_iter().map(|(node, _)| node));
            }
            Err(error) if error.is_account_wide() => return Err(error.into()),
            Err(error) => {
                log::debug!(
                    "[memory:sources] id={source_id} batch of {} failed ({error}); retrying item by item",
                    chunk.len()
                );
                for (node, item) in chunk {
                    match engine.store(item).await {
                        Ok(_) => {
                            out.stored += 1;
                            out.touched.insert(node);
                        }
                        Err(error) if error.is_account_wide() => {
                            return Err(error.into());
                        }
                        Err(error) => {
                            log::debug!("[memory:sources] id={source_id} item store failed");
                            out.failed += 1;
                            last_error = Some(error);
                        }
                    }
                }
            }
        }
    }
    match last_error {
        Some(error) if out.stored == 0 && out.failed > 0 => Err(error.into()),
        _ => Ok(out),
    }
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
