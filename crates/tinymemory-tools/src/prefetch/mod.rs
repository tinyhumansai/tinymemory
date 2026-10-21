//! Best-effort, keyed background refresh of a finished result.
//!
//! A turn never waits for memory. It reads the last *completed* result for its
//! key ([`Prefetcher::completed`]) and queues a refresh
//! ([`Prefetcher::enqueue`]) that a bounded worker runs off the turn; the
//! next turn reads what that refresh produced. Results live in memory for a
//! time-to-live and are never persisted.
//!
//! The mechanics are generic over the key `K` (who and what the result is
//! for) and the result `V` (a context pack): per-key serial work, bounded
//! entries, bounded pending work per key, a bounded worker pool shared across
//! prefetchers, a timeout per refresh, and invalidation that rejects results
//! of reads already in flight. The crate links no executor: the host supplies
//! a [`PrefetchRuntime`] that spawns tasks and sleeps.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use futures::future::{BoxFuture, Either, select};

mod turn_cache;

pub use turn_cache::{TurnCache, TurnCacheRegistry};

/// The executor a [`Prefetcher`] runs its workers on.
pub trait PrefetchRuntime: Send + Sync {
    /// Runs `task` in the background, with whatever scope the host needs
    /// (identity, config) carried over.
    fn spawn(&self, task: BoxFuture<'static, ()>);
    /// A future that completes after `duration`.
    fn sleep(&self, duration: Duration) -> BoxFuture<'static, ()>;
}

/// Bounds on a [`Prefetcher`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// How long a completed result is served.
    pub ttl: Duration,
    /// How long one refresh may run.
    pub work_timeout: Duration,
    /// Keys kept at once; the least recently touched idle key is evicted.
    pub max_entries: usize,
    /// Refreshes queued behind a running one, per key.
    pub max_pending: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            ttl: Duration::from_secs(60),
            work_timeout: Duration::from_secs(120),
            max_entries: 64,
            max_pending: 16,
        }
    }
}

/// Concurrent refreshes allowed across every prefetcher sharing the pool.
#[derive(Debug, Clone)]
pub struct WorkerPool {
    free: Arc<AtomicUsize>,
}

impl WorkerPool {
    /// A pool of `size` workers.
    #[must_use]
    pub fn new(size: usize) -> Self {
        Self {
            free: Arc::new(AtomicUsize::new(size)),
        }
    }

    fn try_acquire(&self) -> Option<Permit> {
        self.free
            .try_update(Ordering::AcqRel, Ordering::Acquire, |free| {
                free.checked_sub(1)
            })
            .ok()
            .map(|_| Permit {
                free: self.free.clone(),
            })
    }

    /// Workers not running a refresh.
    #[must_use]
    pub fn available(&self) -> usize {
        self.free.load(Ordering::Acquire)
    }
}

impl Default for WorkerPool {
    fn default() -> Self {
        Self::new(8)
    }
}

struct Permit {
    free: Arc<AtomicUsize>,
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.free.fetch_add(1, Ordering::AcqRel);
    }
}

/// A refresh: resolves to the new result, or `None` for no result.
pub type Work<V> = BoxFuture<'static, Option<V>>;

struct Entry<V> {
    ticket: u64,
    revision: u64,
    pack: Option<(Instant, V)>,
    pending: VecDeque<Work<V>>,
    active: bool,
    touched: Instant,
}

struct State<K, V> {
    entries: HashMap<K, Entry<V>>,
    next_ticket: u64,
}

type Accept<V> = dyn Fn(&V) -> bool + Send + Sync;

struct Inner<K, V> {
    runtime: Arc<dyn PrefetchRuntime>,
    limits: Limits,
    workers: WorkerPool,
    accept: Box<Accept<V>>,
    state: Mutex<State<K, V>>,
}

impl<K, V> Inner<K, V> {
    fn lock(&self) -> MutexGuard<'_, State<K, V>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A keyed cache of completed results, refreshed in the background.
pub struct Prefetcher<K, V> {
    inner: Arc<Inner<K, V>>,
}

impl<K, V> std::fmt::Debug for Prefetcher<K, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Prefetcher").finish_non_exhaustive()
    }
}

impl<K, V> Clone for Prefetcher<K, V> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<K, V> Prefetcher<K, V>
where
    K: Clone + Hash + Eq + Send + Sync + 'static,
    V: Clone + Send + 'static,
{
    /// A prefetcher with default [`Limits`], its own worker pool and no
    /// result filter.
    pub fn new(runtime: Arc<dyn PrefetchRuntime>) -> Self {
        Self {
            inner: Arc::new(Inner {
                runtime,
                limits: Limits::default(),
                workers: WorkerPool::default(),
                accept: Box::new(|_| true),
                state: Mutex::new(State {
                    entries: HashMap::new(),
                    next_ticket: 0,
                }),
            }),
        }
    }

    /// Applies `change` while this handle is the only one; a clone already
    /// shares the settings, so the change is skipped (and logged).
    fn rebuild(mut self, change: impl FnOnce(&mut Inner<K, V>)) -> Self {
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => change(inner),
            None => log::warn!("[memory:prefetch] configure a Prefetcher before cloning it"),
        }
        self
    }

    /// Replaces the bounds.
    #[must_use]
    pub fn with_limits(self, limits: Limits) -> Self {
        self.rebuild(|inner| inner.limits = limits)
    }

    /// Draws workers from a pool shared with other prefetchers.
    #[must_use]
    pub fn with_workers(self, workers: WorkerPool) -> Self {
        self.rebuild(|inner| inner.workers = workers)
    }

    /// Keeps only results `accept` approves (for example, bounded in size).
    #[must_use]
    pub fn with_filter(self, accept: impl Fn(&V) -> bool + Send + Sync + 'static) -> Self {
        self.rebuild(|inner| inner.accept = Box::new(accept))
    }

    /// The last completed result for `key`, if it is within its time-to-live.
    #[must_use]
    pub fn completed(&self, key: &K) -> Option<V> {
        let mut state = self.inner.lock();
        let entry = state.entries.get_mut(key)?;
        entry.touched = Instant::now();
        if entry
            .pack
            .as_ref()
            .is_some_and(|(at, _)| at.elapsed() >= self.inner.limits.ttl)
        {
            entry.pack = None;
        }
        entry.pack.as_ref().map(|(_, pack)| pack.clone())
    }

    /// Whether no refresh for `key` is running (or the key is unknown).
    #[must_use]
    pub fn is_idle(&self, key: &K) -> bool {
        self.inner
            .lock()
            .entries
            .get(key)
            .is_none_or(|entry| !entry.active)
    }

    /// Discards completed results of keys `matches` selects. Pending work is
    /// kept, but a refresh already in flight cannot publish its result.
    pub fn invalidate_where(&self, matches: impl Fn(&K) -> bool) {
        self.inner
            .lock()
            .entries
            .iter_mut()
            .filter(|(key, _)| matches(key))
            .for_each(|(_, entry)| {
                entry.pack = None;
                entry.revision = entry.revision.wrapping_add(1);
            });
    }

    /// Drops every key. Refreshes in flight cannot recreate entries they no
    /// longer own.
    pub fn invalidate_all(&self) {
        self.inner.lock().entries.clear();
    }

    /// Queues `work` for `key`. Work for one key runs in order, one at a
    /// time. Skipped, with a log line and no waiting task, when the entry
    /// table, the key's queue or the worker pool is full.
    pub fn enqueue(&self, key: K, work: Work<V>) {
        let inner = &self.inner;
        let mut state = inner.lock();
        if !state.entries.contains_key(&key) {
            if state.entries.len() >= inner.limits.max_entries {
                let oldest = state
                    .entries
                    .iter()
                    .filter(|(_, entry)| !entry.active)
                    .min_by_key(|(_, entry)| entry.touched)
                    .map(|(key, _)| key.clone());
                let Some(oldest) = oldest else {
                    log::warn!("[memory:prefetch] cache busy; automatic refresh skipped");
                    return;
                };
                state.entries.remove(&oldest);
            }
            state.next_ticket = state.next_ticket.wrapping_add(1);
            let ticket = state.next_ticket;
            state.entries.insert(
                key.clone(),
                Entry {
                    ticket,
                    revision: 0,
                    pack: None,
                    pending: VecDeque::new(),
                    active: false,
                    touched: Instant::now(),
                },
            );
        }
        let Some(entry) = state.entries.get_mut(&key) else {
            return;
        };
        if entry.pending.len() >= inner.limits.max_pending {
            log::warn!("[memory:prefetch] queue full; automatic refresh skipped");
            return;
        }
        let permit = if entry.active {
            None
        } else {
            let Some(permit) = inner.workers.try_acquire() else {
                log::warn!("[memory:prefetch] workers busy; automatic refresh skipped");
                return;
            };
            Some(permit)
        };
        entry.pending.push_back(work);
        entry.touched = Instant::now();
        let ticket = entry.ticket;
        log::debug!(
            "[memory:prefetch] refresh queued ticket={ticket} pending={}",
            entry.pending.len()
        );
        if entry.active {
            return;
        }
        entry.active = true;
        drop(state);
        let cleanup = Worker {
            inner: inner.clone(),
            key: key.clone(),
            ticket,
            armed: true,
        };
        let inner = inner.clone();
        inner.runtime.clone().spawn(Box::pin(async move {
            let _permit = permit;
            let mut cleanup = cleanup;
            drain(&inner, &key, ticket, &mut cleanup).await;
        }));
    }
}

async fn drain<K, V>(inner: &Arc<Inner<K, V>>, key: &K, ticket: u64, cleanup: &mut Worker<K, V>)
where
    K: Clone + Hash + Eq + Send + Sync + 'static,
    V: Clone + Send + 'static,
{
    loop {
        let (work, revision) = {
            let mut state = inner.lock();
            let Some(entry) = state
                .entries
                .get_mut(key)
                .filter(|entry| entry.ticket == ticket)
            else {
                return;
            };
            let Some(work) = entry.pending.pop_front() else {
                entry.active = false;
                cleanup.armed = false;
                return;
            };
            (work, entry.revision)
        };
        let started = Instant::now();
        let timer = inner.runtime.sleep(inner.limits.work_timeout);
        let pack = match select(work, timer).await {
            Either::Left((pack, _)) => pack.filter(|pack| (inner.accept)(pack)),
            Either::Right(_) => {
                log::warn!("[memory:prefetch] automatic refresh timed out ticket={ticket}");
                None
            }
        };
        log::debug!(
            "[memory:prefetch] refresh completed ticket={ticket} completed={} elapsed_ms={}",
            pack.is_some(),
            started.elapsed().as_millis()
        );
        let mut state = inner.lock();
        if let Some(entry) = state
            .entries
            .get_mut(key)
            .filter(|entry| entry.ticket == ticket && entry.revision == revision)
        {
            entry.pack = pack.map(|pack| (Instant::now(), pack));
        }
    }
}

/// Cancellation, runtime shutdown and panics must release the active flag as
/// well as the worker permit; a later turn can then retry normally.
struct Worker<K: Clone + Hash + Eq, V> {
    inner: Arc<Inner<K, V>>,
    key: K,
    ticket: u64,
    armed: bool,
}

impl<K: Clone + Hash + Eq, V> Drop for Worker<K, V> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = state
            .entries
            .get_mut(&self.key)
            .filter(|entry| entry.ticket == self.ticket)
        {
            entry.active = false;
            entry.pending.clear();
        }
    }
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
