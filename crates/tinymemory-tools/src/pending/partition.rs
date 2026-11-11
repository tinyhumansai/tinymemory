//! Weakly cached async locks that serialize drains per durable partition.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, LazyLock, Mutex, Weak};

use async_lock::Mutex as AsyncMutex;

use super::{Entry, Limits, PendingWrites};
use crate::list_store::ListStore;

static GLOBAL_LOCKS: LazyLock<PartitionLocks<String>> =
    LazyLock::new(PartitionLocks::<String>::new);

/// Weak registry of drain locks, with one independent lock per storage key.
///
/// A host normally keys this registry by the path or identifier of each
/// durable queue. Locks disappear as soon as the last drain drops its handle;
/// dead keys are pruned on the next lookup.
#[derive(Debug)]
pub struct PartitionLocks<K = String> {
    locks: Mutex<HashMap<K, Weak<AsyncMutex<()>>>>,
}

/// Weak registry that shares one pending queue for each durable partition.
///
/// The last active host reference releases the in-memory queue. A later lookup
/// creates a new queue from the host's store factory, which reloads the same
/// durable partition.
pub struct PendingPartitions<S: ListStore<Entry>, K = String> {
    queues: Mutex<HashMap<K, Weak<PendingWrites<S>>>>,
    limits: Limits,
}

impl<S: ListStore<Entry>, K> std::fmt::Debug for PendingPartitions<S, K>
where
    S: ListStore<Entry>,
    K: Eq + Hash,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingPartitions")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl<S: ListStore<Entry>, K: Eq + Hash> PendingPartitions<S, K> {
    /// Creates a queue registry using `limits` for every partition.
    #[must_use]
    pub fn new(limits: Limits) -> Self {
        Self {
            queues: Mutex::new(HashMap::new()),
            limits,
        }
    }

    /// Returns the shared queue for `key`, constructing its store only when
    /// no live queue for that partition exists.
    ///
    /// The store factory runs while the registry is locked and should only
    /// construct a handle; actual I/O remains inside the host's `ListStore`.
    #[must_use]
    pub fn get_or_create(&self, key: K, create_store: impl FnOnce() -> S) -> Arc<PendingWrites<S>> {
        let mut queues = self
            .queues
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queues.retain(|_, queue| queue.strong_count() > 0);
        if let Some(queue) = queues.get(&key).and_then(Weak::upgrade) {
            return queue;
        }
        let queue = Arc::new(PendingWrites::new(create_store(), self.limits));
        queues.insert(key, Arc::downgrade(&queue));
        queue
    }
}

impl<K: Eq + Hash> Default for PartitionLocks<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Eq + Hash> PartitionLocks<K> {
    /// Creates an empty registry of partition drain locks.
    #[must_use]
    pub fn new() -> Self {
        Self {
            locks: Mutex::new(HashMap::new()),
        }
    }

    /// Gets the shared drain lock for one durable partition.
    #[must_use]
    pub fn for_partition(&self, key: K) -> Arc<AsyncMutex<()>> {
        let mut locks = self
            .locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(AsyncMutex::new(()));
        locks.insert(key, Arc::downgrade(&lock));
        lock
    }
}

impl PartitionLocks<String> {
    /// Returns the process-wide partition lock registry for string keys.
    #[must_use]
    pub fn global() -> &'static Self {
        &GLOBAL_LOCKS
    }
}

#[cfg(test)]
#[path = "partition_tests.rs"]
mod tests;
