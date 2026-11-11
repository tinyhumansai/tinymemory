//! Deletions that must reach the engine, whenever it is reachable.
//!
//! Deleting a chat thread or removing a source asks the engine to delete for
//! good. When the engine cannot be reached, or a deletion fails, it is
//! recorded in a [`DeletionQueue`] (durable through a host [`ListStore`]) and
//! [`DeletionQueue::drain`] runs it again — on the next sign-in, and on every
//! background tick — until it succeeds.
//!
//! A deletion that fails is retried with exponential backoff
//! ([`Backoff`]): a tick skips it until it is due again, while a forced
//! drain (a sign-in) runs everything now.
//!
//! An unreadable store is never overwritten. A deletion that cannot be saved
//! is held in memory, counted by [`DeletionQueue::pending`], and written into
//! the store once it can be read again.
//!
//! The crate links no executor and does no I/O: the host supplies the store
//! and the [`DeletionExecutor`] that does the forget.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use futures::lock::Mutex as AsyncMutex;
use serde::{Deserialize, Serialize};

use crate::list_store::ListStore;

/// One deletion still owed to the engine.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PendingDeletion {
    /// A deleted chat thread's conversation memory.
    Thread {
        /// The thread.
        thread_id: String,
    },
    /// Every item a removed memory source stored.
    Source {
        /// The source id.
        source_id: String,
    },
}

impl PendingDeletion {
    /// A short, content-free label for logs.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Thread { .. } => "thread",
            Self::Source { .. } => "source",
        }
    }
}

/// Exponential backoff between retries of one failing deletion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    /// The wait after the first failure.
    pub base: Duration,
    /// The longest wait.
    pub max: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            base: Duration::seconds(30),
            max: Duration::hours(1),
        }
    }
}

impl Backoff {
    /// The wait after `failures` consecutive failures (at least one).
    #[must_use]
    pub fn delay(&self, failures: u32) -> Duration {
        let shift = failures.saturating_sub(1).min(20);
        self.base
            .checked_mul(1 << shift)
            .map_or(self.max, |delay| delay.min(self.max))
    }
}

/// Runs one deletion against the engine.
#[async_trait]
pub trait DeletionExecutor: Send + Sync {
    /// Whether the engine can be reached at all. When not, a drain stops
    /// before running anything.
    async fn is_reachable(&self) -> bool;

    /// Runs `deletion`, returning how many items went.
    ///
    /// # Errors
    /// A stable error code; the deletion stays queued.
    async fn run(&self, deletion: &PendingDeletion) -> Result<usize, String>;
}

/// When a drain runs a deletion that is backing off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drain {
    /// Only deletions that are due (a background tick).
    Due,
    /// Everything now (a sign-in).
    Force,
}

#[derive(Debug, Clone, Copy)]
struct Retry {
    failures: u32,
    not_before: DateTime<Utc>,
}

/// A durable queue of [`PendingDeletion`]s.
pub struct DeletionQueue<S: ListStore<PendingDeletion>> {
    store: S,
    backoff: Backoff,
    /// Serialises read-modify-writes of the store, and holds the deletions
    /// not yet saved to it.
    ///
    /// ponytail: held deletions are lost if the process exits while the store
    /// stays unreadable; a host that sets a corrupt file aside recovers them on
    /// the next save.
    held: Mutex<Vec<PendingDeletion>>,
    /// Serialises drains, so two triggers never run one deletion twice.
    draining: AsyncMutex<()>,
    retries: Mutex<HashMap<PendingDeletion, Retry>>,
}

impl<S: ListStore<PendingDeletion>> std::fmt::Debug for DeletionQueue<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeletionQueue")
            .field("backoff", &self.backoff)
            .finish_non_exhaustive()
    }
}

impl<S: ListStore<PendingDeletion>> DeletionQueue<S> {
    /// A queue persisted in `store`.
    #[must_use]
    pub fn new(store: S, backoff: Backoff) -> Self {
        Self {
            store,
            backoff,
            held: Mutex::new(Vec::new()),
            draining: AsyncMutex::new(()),
            retries: Mutex::new(HashMap::new()),
        }
    }

    fn held(&self) -> std::sync::MutexGuard<'_, Vec<PendingDeletion>> {
        self.held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The stored deletions with every `held` one written back into the
    /// store, or `None` while the store cannot be read.
    fn read(&self, held: &mut Vec<PendingDeletion>) -> Option<Vec<PendingDeletion>> {
        let mut all = match self.store.load() {
            Ok(all) => all,
            Err(error) => {
                log::warn!("[tinymemory:deletion] pending deletions unreadable: {error}");
                return None;
            }
        };
        if !held.is_empty() {
            merge(&mut all, held.drain(..));
            self.write(&all, held);
        }
        Some(all)
    }

    /// Saves `all`; when that fails, holds it until the next read succeeds.
    ///
    /// Every list written holds every `held` deletion except one being
    /// settled, so a successful save leaves nothing to hold.
    fn write(&self, all: &[PendingDeletion], held: &mut Vec<PendingDeletion>) {
        match self.store.save(all) {
            Ok(()) => held.clear(),
            Err(error) => {
                log::warn!("[tinymemory:deletion] writing pending deletions failed: {error}");
                merge(held, all.iter().cloned());
            }
        }
    }

    /// Records `deletion` to run on the next drain. One already queued is not
    /// queued twice.
    pub fn enqueue(&self, deletion: PendingDeletion) {
        let mut held = self.held();
        let Some(mut all) = self.read(&mut held) else {
            log::warn!(
                "[tinymemory:deletion] {} deletion held until pending deletions are readable",
                deletion.label()
            );
            merge(&mut held, std::iter::once(deletion));
            return;
        };
        if all.contains(&deletion) {
            return;
        }
        log::info!(
            "[tinymemory:deletion] {} deletion queued until memory is reachable",
            deletion.label()
        );
        all.push(deletion);
        self.write(&all, &mut held);
    }

    /// Every deletion still owed, oldest first, including those held while
    /// the store is unreadable.
    #[must_use]
    pub fn pending(&self) -> Vec<PendingDeletion> {
        let mut held = self.held();
        let mut all = self.read(&mut held).unwrap_or_default();
        merge(&mut all, held.iter().cloned());
        all
    }

    fn settle(&self, deletion: &PendingDeletion) {
        let mut held = self.held();
        held.retain(|queued| queued != deletion);
        let Some(mut all) = self.read(&mut held) else {
            return;
        };
        let before = all.len();
        all.retain(|queued| queued != deletion);
        if all.len() != before {
            self.write(&all, &mut held);
        }
    }

    fn retries(&self) -> std::sync::MutexGuard<'_, HashMap<PendingDeletion, Retry>> {
        self.retries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Runs queued deletions, settling each that succeeds. Stops at once when
    /// the engine is unreachable, and keeps a failing deletion (backing off
    /// its next [`Drain::Due`] run). Returns how many settled.
    pub async fn drain<E: DeletionExecutor>(
        &self,
        executor: &E,
        mode: Drain,
        now: DateTime<Utc>,
    ) -> usize {
        let _draining = self.draining.lock().await;
        let queued = self.pending();
        if queued.is_empty() {
            return 0;
        }
        if !executor.is_reachable().await {
            log::debug!(
                "[tinymemory:deletion] memory off; {} pending deletions wait",
                queued.len()
            );
            return 0;
        }
        let mut settled = 0;
        for deletion in queued {
            if mode == Drain::Due
                && self
                    .retries()
                    .get(&deletion)
                    .is_some_and(|retry| retry.not_before > now)
            {
                log::debug!("[tinymemory:deletion] {} backing off", deletion.label());
                continue;
            }
            match executor.run(&deletion).await {
                Ok(forgotten) => {
                    self.settle(&deletion);
                    self.retries().remove(&deletion);
                    settled += 1;
                    log::info!(
                        "[tinymemory:deletion] {} deletion completed forgotten={forgotten}",
                        deletion.label()
                    );
                }
                Err(code) => {
                    let mut retries = self.retries();
                    let failures = retries.get(&deletion).map_or(0, |r| r.failures) + 1;
                    retries.insert(
                        deletion.clone(),
                        Retry {
                            failures,
                            not_before: now + self.backoff.delay(failures),
                        },
                    );
                    log::warn!(
                        "[tinymemory:deletion] {} deletion failed code={code} failures={failures}; kept",
                        deletion.label()
                    );
                }
            }
        }
        settled
    }
}

/// Appends each of `extra` not already in `all`.
fn merge(all: &mut Vec<PendingDeletion>, extra: impl IntoIterator<Item = PendingDeletion>) {
    for deletion in extra {
        if !all.contains(&deletion) {
            all.push(deletion);
        }
    }
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
