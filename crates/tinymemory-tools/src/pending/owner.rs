//! Runtime-neutral owner lifetimes and erasure barriers.

use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::{Arc, LazyLock, Mutex, Weak};

use async_lock::{RwLock, RwLockReadGuardArc, RwLockWriteGuardArc};

use super::types::{OwnedFenceToken, WriteGate};

static GLOBAL_OWNERS: LazyLock<OwnerRegistry<String>> = LazyLock::new(OwnerRegistry::<String>::new);

/// Weak registry shared by the queues belonging to an application host.
///
/// The registry prunes dead entries on every lookup. Callers hold an owner
/// only while a mutation, drain, or erasure needs its gate and barrier.
#[derive(Debug)]
pub struct OwnerRegistry<K = String> {
    owners: Mutex<HashMap<K, Weak<OwnerState>>>,
}

impl<K: Eq + Hash> Default for OwnerRegistry<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Eq + Hash> OwnerRegistry<K> {
    /// Creates an empty owner registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            owners: Mutex::new(HashMap::new()),
        }
    }

    /// Returns the shared owner state for `key`, creating a fresh epoch when
    /// no live operation currently holds that owner.
    #[must_use]
    pub fn get(&self, key: K) -> Arc<OwnerState> {
        let mut owners = self
            .owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        owners.retain(|_, owner| owner.strong_count() > 0);
        if let Some(owner) = owners.get(&key).and_then(Weak::upgrade) {
            return owner;
        }
        let owner = Arc::new(OwnerState::new());
        owners.insert(key, Arc::downgrade(&owner));
        owner
    }
}

impl OwnerRegistry<String> {
    /// Returns the process-wide registry for hosts whose tools share one
    /// application lifetime and use string owner keys.
    #[must_use]
    pub fn global() -> &'static Self {
        &GLOBAL_OWNERS
    }
}

/// One owner's write gate and mutation barrier.
#[derive(Debug)]
pub struct OwnerState {
    gate: Arc<WriteGate>,
    barrier: Arc<RwLock<()>>,
}

impl OwnerState {
    fn new() -> Self {
        Self {
            gate: Arc::new(WriteGate::new(uuid::Uuid::new_v4().to_string())),
            barrier: Arc::new(RwLock::new(())),
        }
    }

    /// The gate used to stamp queued writes and stop stale drains.
    #[must_use]
    pub fn gate(&self) -> &WriteGate {
        &self.gate
    }

    /// Captures the current epoch for automatic work before it can wait in a
    /// host's prefetch or scheduling queue.
    #[must_use]
    pub fn automatic_mutation(self: &Arc<Self>) -> AutomaticMutation {
        AutomaticMutation {
            owner: self.clone(),
            generation: self.gate.generation(),
        }
    }

    /// Starts an erasure fence and waits for mutations already in flight.
    ///
    /// The returned fence keeps new mutations behind the owner barrier until
    /// it is dropped. Concurrent fences advance the same gate and serialize
    /// on this barrier.
    pub async fn begin_fence(self: &Arc<Self>) -> OwnerFence {
        let token = self.gate.begin_fence_owned();
        let generation = token.generation();
        let guard = self.barrier.clone().write_arc().await;
        OwnerFence {
            owner: self.clone(),
            token: Some(token),
            generation,
            guard: Some(guard),
        }
    }
}

/// Automatic work admitted at an owner's current erasure generation.
#[derive(Debug)]
pub struct AutomaticMutation {
    owner: Arc<OwnerState>,
    generation: u64,
}

impl AutomaticMutation {
    /// Runs `work` while holding the shared owner barrier.
    ///
    /// Returns `None` when an erasure began after admission and retired this
    /// work before it acquired the barrier.
    pub async fn run<F: Future>(self, work: F) -> Option<F::Output> {
        if self.owner.gate.generation() != self.generation {
            return None;
        }
        let _guard: RwLockReadGuardArc<()> = self.owner.barrier.clone().read_arc().await;
        if self.owner.gate.generation() != self.generation {
            return None;
        }
        Some(work.await)
    }
}

/// An erasure fence held across the host's explicit engine deletion.
#[must_use = "dropping the fence lets later owner mutations proceed"]
#[derive(Debug)]
pub struct OwnerFence {
    owner: Arc<OwnerState>,
    token: Option<OwnedFenceToken>,
    generation: u64,
    guard: Option<RwLockWriteGuardArc<()>>,
}

impl OwnerFence {
    /// The owner gate whose previous generation this fence retired.
    #[must_use]
    pub fn gate(&self) -> &WriteGate {
        &self.owner.gate
    }

    /// The generation opened by this fence.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

impl Drop for OwnerFence {
    fn drop(&mut self) {
        // Lift the gate before releasing the barrier. Mutations admitted in
        // this small interval still wait for the write guard to be released.
        drop(self.token.take());
        drop(self.guard.take());
    }
}

#[cfg(test)]
#[path = "owner_tests.rs"]
mod tests;
