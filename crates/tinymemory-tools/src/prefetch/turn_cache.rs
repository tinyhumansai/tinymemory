//! A bounded cache of completed [`TurnPack`](crate::lifecycle::turn::TurnPack)s.
//!
//! [`TurnCache`] layers turn-pack policy on the existing [`super::Prefetcher`]:
//! it decorates cloned completed results for the next turn and rejects packs
//! whose serialized result and citations exceed 64 KiB. Queueing, work
//! deadlines, per-key ordering, worker limits, cancellation, and stale-result
//! rejection remain owned by `Prefetcher`.
//!
//! Hosts choose the key and retain ownership of its configuration fingerprint,
//! identity, thread, and runtime scope. A [`TurnCacheRegistry`] weakly tracks
//! per-context caches so a successful write can invalidate matching keys
//! across contexts without keeping those caches alive.

use std::hash::Hash;
use std::sync::{Arc, Mutex, PoisonError, Weak};

use crate::lifecycle::turn::TurnPack;

use super::{Limits, PrefetchRuntime, Prefetcher, Work, WorkerPool};

const MAX_PACK_BYTES: usize = 64 * 1024;
const CACHED_NOTICE: &str = "This is cached memory from a completed background lookup for an earlier turn. It may be incomplete or unrelated to the current question; use explicit memory recall or fetch when you need a current answer.\n\n";

/// A host-keyed cache of completed turn packs.
///
/// The cache returns the last completed pack immediately; it never awaits
/// queued work. Its executor is supplied by the host through
/// [`PrefetchRuntime`].
pub struct TurnCache<K> {
    prefetcher: Prefetcher<K, TurnPack>,
}

impl<K> std::fmt::Debug for TurnCache<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnCache").finish_non_exhaustive()
    }
}

impl<K> TurnCache<K>
where
    K: Clone + Hash + Eq + Send + Sync + 'static,
{
    /// Creates a turn cache with default [`Limits`], its own worker pool, and
    /// TinyMemory's completed-pack size filter.
    pub fn new(runtime: Arc<dyn PrefetchRuntime>) -> Self {
        Self {
            prefetcher: Prefetcher::new(runtime).with_filter(pack_fits),
        }
    }

    /// Replaces the cache bounds.
    ///
    /// Configure the cache before enqueuing work. As with [`Prefetcher`],
    /// settings are skipped and logged if its state has already been shared.
    #[must_use]
    pub fn with_limits(self, limits: Limits) -> Self {
        Self {
            prefetcher: self.prefetcher.with_limits(limits),
        }
    }

    /// Uses a worker pool shared with other caches.
    ///
    /// Configure the cache before enqueuing work. As with [`Prefetcher`],
    /// settings are skipped and logged if its state has already been shared.
    #[must_use]
    pub fn with_workers(self, workers: WorkerPool) -> Self {
        Self {
            prefetcher: self.prefetcher.with_workers(workers),
        }
    }

    /// Returns the last completed pack for `key`, with a notice for the model.
    ///
    /// The stored value is left untouched; every call decorates a fresh clone
    /// exactly once.
    #[must_use]
    pub fn completed(&self, key: &K) -> Option<TurnPack> {
        self.prefetcher.completed(key).map(cached_notice)
    }

    /// Whether no refresh is running or queued for `key`.
    ///
    /// Returns `true` for a missing key. Invalidation can remove an entry while
    /// its worker is still unwinding, so this observation is not a completion
    /// barrier for invalidated work.
    #[must_use]
    pub fn is_idle(&self, key: &K) -> bool {
        self.prefetcher.is_idle(key)
    }

    /// Queues owned background work for `key`.
    ///
    /// The existing prefetcher serializes work per key and applies its queue,
    /// worker, timeout, cancellation, and stale-result rules.
    pub fn enqueue(&self, key: K, work: Work<TurnPack>) {
        self.prefetcher.enqueue(key, work);
    }
}

/// Weakly tracks host-owned turn caches for cross-context invalidation.
pub struct TurnCacheRegistry<K> {
    caches: Mutex<Vec<Weak<TurnCache<K>>>>,
}

impl<K> Default for TurnCacheRegistry<K> {
    fn default() -> Self {
        Self {
            caches: Mutex::default(),
        }
    }
}

impl<K> std::fmt::Debug for TurnCacheRegistry<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let caches = self.caches.lock().unwrap_or_else(PoisonError::into_inner);
        f.debug_struct("TurnCacheRegistry")
            .field(
                "live_caches",
                &caches
                    .iter()
                    .filter(|cache| cache.strong_count() > 0)
                    .count(),
            )
            .finish()
    }
}

impl<K> TurnCacheRegistry<K>
where
    K: Clone + Hash + Eq + Send + Sync + 'static,
{
    /// Registers a host-owned cache without extending its lifetime.
    ///
    /// Re-registering the same cache is a no-op. Dead entries are pruned.
    pub fn register(&self, cache: &Arc<TurnCache<K>>) {
        let weak = Arc::downgrade(cache);
        let mut caches = self.caches.lock().unwrap_or_else(PoisonError::into_inner);
        caches.retain(|known| known.strong_count() > 0);
        if !caches.iter().any(|known| known.ptr_eq(&weak)) {
            caches.push(weak);
        }
    }

    /// Invalidates matching keys across all currently live caches.
    ///
    /// The predicate is called synchronously for each key and is not retained.
    /// The registry lock is released before cache invalidation begins. The
    /// underlying prefetcher keeps queued work, but rejects a result that was
    /// already in flight when its key was invalidated.
    pub fn invalidate_where(&self, matches: impl Fn(&K) -> bool) {
        for cache in self.live_caches() {
            cache.prefetcher.invalidate_where(&matches);
        }
    }

    /// Clears all currently live caches.
    ///
    /// The underlying prefetcher drops pending entries, and in-flight work
    /// cannot recreate entries removed by invalidation.
    pub fn invalidate_all(&self) {
        for cache in self.live_caches() {
            cache.prefetcher.invalidate_all();
        }
    }

    fn live_caches(&self) -> Vec<Arc<TurnCache<K>>> {
        let mut caches = self.caches.lock().unwrap_or_else(PoisonError::into_inner);
        caches.retain(|cache| cache.strong_count() > 0);
        caches.iter().filter_map(Weak::upgrade).collect()
    }
}

fn pack_fits(pack: &TurnPack) -> bool {
    serde_json::to_vec(pack)
        .ok()
        .zip(serde_json::to_vec(&pack.citations).ok())
        .is_some_and(|(pack, citations)| {
            pack.len().saturating_add(citations.len()) <= MAX_PACK_BYTES
        })
}

fn cached_notice(mut pack: TurnPack) -> TurnPack {
    pack.markdown.insert_str(0, CACHED_NOTICE);
    pack.tokens += crate::recall::estimate_tokens(CACHED_NOTICE);
    pack
}

#[cfg(test)]
#[path = "turn_cache_tests.rs"]
mod tests;
