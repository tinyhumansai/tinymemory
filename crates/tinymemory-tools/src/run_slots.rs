//! Runtime-neutral reservations for work that may run once per key.
//!
//! A host owns a [`RunSlots`] registry for as long as its workers may run. A
//! successful [`RunSlots::try_acquire`] returns a [`RunGuard`]; dropping that
//! guard releases the key, including when an executor rejects or cancels its
//! task.

use std::collections::HashSet;
use std::hash::Hash;
use std::sync::{Arc, Mutex, PoisonError};

/// A synchronous interlock around checking peer work and reserving this run.
///
/// Implementations hold the shared start lock while calling
/// `reserve`. The callback must not await: imports and migrations use it to
/// make their peer check plus slot reservation one atomic decision.
pub trait StartGate: Send + Sync {
    /// Calls `reserve` while holding the shared start gate.
    fn with_gate(&self, reserve: &mut dyn FnMut() -> bool) -> bool;
}

/// A start gate backed by the same standard mutex a host can share between
/// import and migration services.
impl StartGate for Mutex<()> {
    fn with_gate(&self, reserve: &mut dyn FnMut() -> bool) -> bool {
        let _guard = self.lock().unwrap_or_else(PoisonError::into_inner);
        reserve()
    }
}

/// A library-owned start gate shared by related background drivers.
///
/// Clones use the same critical section. Give them to drivers that must check
/// each other's activity before reserving their own [`RunSlots`]; keep those
/// registries separate so each driver's running status remains accurate.
#[derive(Clone, Default)]
pub struct RunStartGate {
    gate: Arc<Mutex<()>>,
}

impl RunStartGate {
    /// Creates a gate with a new shared critical section.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl StartGate for RunStartGate {
    fn with_gate(&self, reserve: &mut dyn FnMut() -> bool) -> bool {
        let _guard = self.gate.lock().unwrap_or_else(PoisonError::into_inner);
        reserve()
    }
}

impl std::fmt::Debug for RunStartGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunStartGate").finish_non_exhaustive()
    }
}

/// Shared keyed reservations for background runs.
pub struct RunSlots<K> {
    slots: Arc<Mutex<HashSet<K>>>,
}

impl<K> Clone for RunSlots<K> {
    fn clone(&self) -> Self {
        Self {
            slots: Arc::clone(&self.slots),
        }
    }
}

impl<K> Default for RunSlots<K> {
    fn default() -> Self {
        Self {
            slots: Arc::new(Mutex::new(HashSet::new())),
        }
    }
}

impl<K> RunSlots<K> {
    /// An empty shared registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl<K: Eq + Hash + Clone> RunSlots<K> {
    /// Whether a run currently holds `key`.
    #[must_use]
    pub fn is_running(&self, key: &K) -> bool {
        self.slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(key)
    }

    /// Reserves `key`, or returns `None` if another run already holds it.
    #[must_use]
    pub fn try_acquire(&self, key: K) -> Option<RunGuard<K>> {
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        if !slots.insert(key.clone()) {
            return None;
        }
        Some(RunGuard {
            slots: Arc::clone(&self.slots),
            key: Some(key),
        })
    }
}

/// An owned reservation. Dropping it makes its key available again.
#[must_use = "dropping the run guard releases its reservation"]
pub struct RunGuard<K: Eq + Hash> {
    slots: Arc<Mutex<HashSet<K>>>,
    key: Option<K>,
}

impl<K: Eq + Hash> Drop for RunGuard<K> {
    fn drop(&mut self) {
        if let Some(key) = self.key.take() {
            self.slots
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&key);
        }
    }
}

impl<K: std::fmt::Debug + Eq + Hash> std::fmt::Debug for RunGuard<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunGuard")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

impl<K: std::fmt::Debug> std::fmt::Debug for RunSlots<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunSlots").finish_non_exhaustive()
    }
}

#[cfg(test)]
#[path = "run_slots_tests.rs"]
mod tests;
