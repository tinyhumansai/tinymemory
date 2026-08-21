//! The process-global [`EmbeddingHost`], and the accessors the extracted code
//! calls in place of the host's `inference::embeddings` factory functions.
//!
//! Mirrors [`crate::events`]'s shape — see that module for why provider
//! construction is reached through a global rather than threaded as a
//! parameter.
//!
//! # Unwired is an error here, unlike the event sink
//!
//! [`crate::events::publish`] drops events when no sink is installed, because
//! the work being announced already happened. Embedding construction is the
//! opposite: silently returning "no embedder" would write vectors into the
//! wrong space or downgrade a semantic query to lexical-only, and neither
//! failure is visible until a later search returns the wrong answer. So the
//! accessors here fail loudly, and callers propagate.

use std::sync::Arc;

use parking_lot::RwLock;

pub use tinymemory_api::host::EmbeddingHost;

static HOST: RwLock<Option<Arc<dyn EmbeddingHost>>> = RwLock::new(None);

/// The message every accessor fails with before a host wires itself up.
const NOT_INSTALLED: &str =
    "no EmbeddingHost installed — the host must call memory::embedding_host::set_embedding_host \
     during startup wiring, before any memory work begins";

/// Install the host's embedding factory. Called once during startup wiring.
/// Calling it again replaces it, which is what test harnesses want between
/// cases.
pub fn set_embedding_host(host: Arc<dyn EmbeddingHost>) {
    *HOST.write() = Some(host);
}

/// Remove any installed host. For tests.
pub fn clear_embedding_host() {
    *HOST.write() = None;
}

/// The installed host, or `None` when nothing has been wired up.
#[must_use]
pub fn embedding_host() -> Option<Arc<dyn EmbeddingHost>> {
    HOST.read().clone()
}

/// The installed host.
///
/// # Errors
///
/// Returns `Err` when no host has been installed.
pub fn require_embedding_host() -> Result<Arc<dyn EmbeddingHost>, String> {
    embedding_host().ok_or_else(|| NOT_INSTALLED.to_string())
}

/// The host's default embedding provider — the managed cloud embedder.
///
/// # Errors
///
/// Returns `Err` when no [`EmbeddingHost`] has been installed.
pub fn default_embedding_provider(
) -> Result<Arc<dyn tinymemory_api::host::EmbeddingProvider>, String> {
    Ok(require_embedding_host()?.default_embedding_provider())
}

/// Serialises tests that mutate embedding-related process environment.
///
/// The host has its own guard over the same variables (`inference::local::
/// inference_test_guard`). They are deliberately *different* locks: each crate's
/// tests link into their own binary and therefore their own process, so a shared
/// lock would buy nothing and would mean the contract crate owning a mutex for
/// the host's benefit.
pub fn embedding_test_guard() -> std::sync::MutexGuard<'static, ()> {
    static GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());
    GUARD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
#[path = "embedding_host_test_support.rs"]
mod test_support;
#[cfg(test)]
pub(crate) use test_support::TestEmbeddingHost;
