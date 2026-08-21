//! [`ShutdownHost`] — registering work that must run before the process exits.
//!
//! The ingest-queue workers hold database leases while a job runs. On a clean
//! shutdown they release them, so the next launch re-claims the work
//! immediately instead of waiting out the lease — which otherwise surfaces as a
//! stale-lock recovery warning on every start. A hard kill still falls back to
//! lease expiry.
//!
//! Ordering shutdown across every subsystem is the host's job, so the core
//! hands it a hook rather than owning a lifecycle of its own.
//!
//! # Unwired means the hook never runs
//!
//! Which is the hard-kill path, and already handled: leases expire and startup
//! recovery reclaims them. So registering without a host installed logs and
//! moves on rather than failing.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use parking_lot::RwLock;

/// A hook the host awaits during shutdown. Boxed because the host stores a
/// heterogeneous list of them.
///
/// `Fn`, not `FnOnce`: the host may invoke it more than once, so each call must
/// own whatever state it needs.
pub type ShutdownHook =
    Box<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync + 'static>;

/// Accepts shutdown hooks from the core.
pub trait ShutdownHost: Send + Sync + std::fmt::Debug {
    /// Register `hook` to be awaited during shutdown.
    fn register(&self, hook: ShutdownHook);
}

static HOST: RwLock<Option<Arc<dyn ShutdownHost>>> = RwLock::new(None);

/// Install the host's shutdown registry. Called once during startup wiring.
pub fn set_shutdown_host(host: Arc<dyn ShutdownHost>) {
    *HOST.write() = Some(host);
}

/// Remove any installed host. For tests.
pub fn clear_shutdown_host() {
    *HOST.write() = None;
}

/// The installed shutdown host, or `None` when nothing has been wired up.
#[must_use]
pub fn shutdown_host() -> Option<Arc<dyn ShutdownHost>> {
    HOST.read().clone()
}

/// Register a hook to run before the process exits.
///
/// A no-op beyond logging when no host is installed — see the module docs.
pub fn register<F, Fut>(hook: F)
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let host = shutdown_host();
    match host {
        Some(host) => host.register(Box::new(move || Box::pin(hook()))),
        None => log::debug!(
            "[memory:shutdown] hook dropped — no shutdown host installed; \
             leases will be reclaimed by expiry at next startup instead"
        ),
    }
}
