//! Process-global registry of [`ComposioProvider`] implementations.
//!
//! There is exactly one provider per toolkit slug — the trait is not
//! a fan-out fan-in dispatch, it is a 1:1 mapping. This keeps trigger
//! routing simple (`HashMap::get(toolkit)` → call) and avoids the
//! "which subscriber wins" ambiguity that would come with multiple
//! providers per toolkit.
//!
//! The registry is initialised once at startup via
//! [`init_default_providers`] and is intentionally write-rare: tests
//! can register additional providers ad-hoc, but the production path
//! only writes during the startup hook.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

use super::ComposioProvider;

/// Reference-counted handle to a registered provider.
pub type ProviderArc = Arc<dyn ComposioProvider>;

/// Backing storage for the global registry.
///
/// `RwLock<HashMap<…>>` is fine here — registration happens at
/// startup and lookups are very fast (no contention in steady state).
type Registry = RwLock<HashMap<String, ProviderArc>>;

static REGISTRY: OnceLock<Registry> = OnceLock::new();

fn registry() -> &'static Registry {
    REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Register or replace a provider for its toolkit slug.
///
/// Idempotent — re-registering the same toolkit overwrites the
/// previous entry, which is what tests rely on for setup/teardown.
pub fn register_provider(provider: ProviderArc) {
    let slug = provider.toolkit_slug().to_string();
    if slug.is_empty() {
        tracing::warn!("[composio:registry] refusing to register provider with empty slug");
        return;
    }
    let mut guard = registry()
        .write()
        .expect("composio provider registry poisoned");
    let was_present = guard.insert(slug.clone(), provider).is_some();
    if was_present {
        tracing::debug!(toolkit = %slug, "[composio:registry] replaced existing provider");
    } else {
        tracing::info!(toolkit = %slug, "[composio:registry] provider registered");
    }
}

/// Look up the provider for a toolkit slug, if one is registered.
pub fn get_provider(toolkit: &str) -> Option<ProviderArc> {
    let key = toolkit.trim();
    if key.is_empty() {
        return None;
    }
    let guard = registry()
        .read()
        .expect("composio provider registry poisoned");
    guard.get(key).cloned()
}

/// Snapshot of every registered provider, in unspecified order. Used
/// by the periodic sync scheduler to walk every toolkit.
pub fn all_providers() -> Vec<ProviderArc> {
    let guard = registry()
        .read()
        .expect("composio provider registry poisoned");
    guard.values().cloned().collect()
}

/// Register the built-in providers shipped with the core. Called once
/// from `start_channels` / `bootstrap_core_runtime` startup paths.
///
/// Idempotent: re-running just re-registers (no-op in practice).
pub fn init_default_providers() {
    register_provider(Arc::new(super::clickup::ClickUpProvider::new()));
    register_provider(Arc::new(super::github::GitHubProvider::new()));
    register_provider(Arc::new(super::gmail::GmailProvider::new()));
    register_provider(Arc::new(super::linear::LinearProvider::new()));
    register_provider(Arc::new(super::notion::NotionProvider::new()));
    register_provider(Arc::new(super::slack::SlackProvider::new()));
    tracing::info!(
        count = all_providers().len(),
        "[composio:registry] default providers initialised"
    );
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
