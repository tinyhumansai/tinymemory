//! A durable, bounded FIFO of memory writes an agent tool asked for.
//!
//! Enqueue never waits on the engine: a validated, scrubbed write is made
//! durable in the host's [`ListStore`] and acknowledged as *queued*, and a
//! drain applies it later, oldest first. A write that fails stays at the head
//! with a stable, content-free status, and everything behind it waits, so
//! order is preserved across a retry.
//!
//! Adjacent retries of one write coalesce, never across a forget/relearn and
//! never while an erasure is fencing the owner. A [`WriteGate`] carries that
//! erasure state: [`WriteGate::begin_fence`] retires every write admitted
//! before it ([`PendingWrites::clear_stale`]) and holds new work back until
//! the [`FenceToken`] drops.
//!
//! The crate links no executor and does no I/O. The host supplies the
//! store, the [`WriteExecutor`] that runs one write (and wraps it in its own
//! timeout), and decides when [`PendingWrites::drain`] is called.
//!
//! Hosts may share owner generations and erasure barriers through
//! [`OwnerRegistry`]. Automatic mutations capture an epoch before scheduling;
//! [`AutomaticMutation::run`] drops work made stale by a later fence. A
//! [`OwnerFence`] holds exclusive access across the host's explicit erase.
//! [`PendingPartitions`] shares one weakly cached queue per durable partition,
//! and [`PartitionLocks`] supplies weakly cached drain locks keyed by each
//! partition, leaving storage paths and persistence to the host.

mod owner;
mod partition;
mod types;

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::lock::Mutex as AsyncMutex;
use sha2::{Digest, Sha256};

use crate::list_store::{ListStore, StoreError};

pub use owner::{AutomaticMutation, OwnerFence, OwnerRegistry, OwnerState};
pub use partition::{PartitionLocks, PendingPartitions};
pub use types::{
    Ack, Binding, Entry, Epoch, FenceToken, Limits, PendingError, WriteExecutor, WriteGate,
    WriteOperation,
};

static TICKETS: AtomicU64 = AtomicU64::new(0);

fn new_ticket() -> String {
    let mut digest = Sha256::new();
    digest.update(TICKETS.fetch_add(1, Ordering::SeqCst).to_le_bytes());
    digest.update(
        chrono::Utc::now()
            .timestamp_nanos_opt()
            .unwrap_or_default()
            .to_le_bytes(),
    );
    digest.update(std::process::id().to_le_bytes());
    digest
        .finalize()
        .iter()
        .take(12)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A durable, bounded FIFO of [`WriteOperation`]s in one store.
pub struct PendingWrites<S: ListStore<Entry>> {
    store: S,
    limits: Limits,
    /// Serialises read-modify-writes of the store.
    lock: Mutex<()>,
    /// Serialises drains, so two triggers never run one write twice.
    draining: AsyncMutex<()>,
}

impl<S: ListStore<Entry>> std::fmt::Debug for PendingWrites<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingWrites")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl<S: ListStore<Entry>> PendingWrites<S> {
    /// A queue persisted in `store`, bounded by `limits`.
    #[must_use]
    pub fn new(store: S, limits: Limits) -> Self {
        Self {
            store,
            limits,
            lock: Mutex::new(()),
            draining: AsyncMutex::new(()),
        }
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, ()> {
        self.lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn read(&self) -> Result<Vec<Entry>, PendingError> {
        let entries = self.store.load().map_err(|_| PendingError::Unavailable)?;
        if entries.len() > self.limits.max_entries {
            return Err(PendingError::Unavailable);
        }
        Ok(entries)
    }

    fn write(&self, entries: &[Entry]) -> Result<(), PendingError> {
        let bytes = serde_json::to_vec(entries).map_err(|_| PendingError::Unavailable)?;
        if bytes.len() > self.limits.max_bytes {
            return Err(PendingError::Unavailable);
        }
        self.store
            .save(entries)
            .map_err(|StoreError(_)| PendingError::Unavailable)
    }

    /// Makes `operation` durable. The acknowledgement means only that it is
    /// in the local outbox, not accepted by the engine.
    ///
    /// `gate` supplies the epoch and whether an erasure is fencing;
    /// `binding` is what a drain checks the active owner against.
    ///
    /// # Errors
    /// [`PendingError::Full`] at the entry bound, [`PendingError::Unavailable`]
    /// when the store cannot be read or written within the byte bound.
    pub fn enqueue(
        &self,
        operation: WriteOperation,
        scope_root: tinymemory_api::Namespace,
        binding: Binding,
        gate: &WriteGate,
    ) -> Result<Ack, PendingError> {
        let ack = Ack::of(&operation);
        let _guard = self.locked();
        let mut entries = self.read()?;
        let (fencing, epoch) = gate.admission();
        if !fencing
            && entries
                .last()
                .is_some_and(|last| last.binding == binding && last.operation.coalesces(&operation))
        {
            log::debug!("[tinymemory:pending] adjacent retry coalesced");
            return Ok(ack);
        }
        if entries.len() >= self.limits.max_entries {
            log::debug!("[tinymemory:pending] queue full; nothing queued");
            return Err(PendingError::Full);
        }
        entries.push(Entry {
            ticket: new_ticket(),
            binding,
            scope_root,
            epoch: Some(epoch),
            operation,
            status: None,
        });
        self.write(&entries)?;
        Ok(ack)
    }

    /// Every write still owed, oldest first.
    ///
    /// # Errors
    /// [`PendingError::Unavailable`] when the store cannot be read.
    pub fn entries(&self) -> Result<Vec<Entry>, PendingError> {
        let _guard = self.locked();
        self.read()
    }

    /// Whether anything is queued. An unreadable store counts as empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries()
            .map(|entries| entries.is_empty())
            .unwrap_or(true)
    }

    /// Drops every write admitted before `generation` of `process` (or with
    /// no epoch), durably. Returns how many went.
    ///
    /// # Errors
    /// [`PendingError::Unavailable`] when the store cannot be read or written.
    pub fn clear_stale(&self, process: &str, generation: u64) -> Result<usize, PendingError> {
        let _guard = self.locked();
        let mut entries = self.read()?;
        let before = entries.len();
        entries.retain(|entry| {
            !entry
                .epoch
                .as_ref()
                .is_none_or(|epoch| epoch.process != process || epoch.generation < generation)
        });
        let removed = before - entries.len();
        if removed > 0 {
            self.write(&entries)?;
        }
        Ok(removed)
    }

    /// Settles the head write: removed on `None`, kept with `status` on
    /// `Some`. `false` when the head is no longer `ticket`.
    fn settle(&self, ticket: &str, status: Option<String>) -> bool {
        let _guard = self.locked();
        let Ok(mut entries) = self.read() else {
            return false;
        };
        if entries.first().is_none_or(|first| first.ticket != ticket) {
            return false;
        }
        match status {
            Some(code) => entries[0].status = Some(code),
            None => {
                entries.remove(0);
            }
        }
        self.write(&entries).is_ok()
    }

    /// Runs queued writes oldest first through `executor`, settling each that
    /// succeeds. A failure keeps that write (with its status) and every later
    /// one for the next drain. Stops when `gate`'s generation moves from
    /// `generation`, or when the head's binding fails `accepts` (a different
    /// owner or layout is active).
    ///
    /// Bounded by the entry limit even if a producer keeps adding. Returns
    /// how many writes settled.
    pub async fn drain<E: WriteExecutor>(
        &self,
        executor: &E,
        gate: &WriteGate,
        generation: u64,
        accepts: impl Fn(&Entry) -> bool + Send,
        mut on_settled: impl FnMut(&Entry) + Send,
    ) -> usize {
        let _draining = self.draining.lock().await;
        let mut settled = 0;
        for _ in 0..self.limits.max_entries {
            if gate.generation() != generation {
                break;
            }
            let head = {
                let _guard = self.locked();
                self.read()
                    .ok()
                    .and_then(|entries| entries.into_iter().next())
            };
            let Some(entry) = head else { break };
            if gate.generation() != generation || !accepts(&entry) {
                break;
            }
            let status = executor.run(&entry.operation).await.err();
            let saved = self.settle(&entry.ticket, status.clone());
            if let Some(code) = status {
                log::debug!(
                    "[tinymemory:pending] write retained code={code} ticket={}",
                    entry.ticket
                );
                break;
            }
            if !saved {
                break;
            }
            settled += 1;
            on_settled(&entry);
        }
        settled
    }
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
