//! Product config discovery and locking around the source registry's CRUD.
//!
//! The registry itself moved to `tinymemory-sources` (#18 §B4); this layer adds
//! the host's config path and the lock that serialises writes to it.

use std::sync::OnceLock;

use crate::config_loader as config_rpc;
use crate::sources::types::{MemorySourceEntry, SourceKind};

pub use tinymemory_sources::{
    memory_sync_defaults_for_toolkit, ComposioUpsertTarget, MemorySourcePatch,
};

static MEMORY_SOURCES_WRITE_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

pub(crate) async fn memory_sources_write_guard() -> tokio::sync::MutexGuard<'static, ()> {
    MEMORY_SOURCES_WRITE_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

async fn registry() -> Result<tinymemory_sources::registry::SourceRegistry, String> {
    let config = config_rpc::load_config_with_timeout().await?;
    Ok(registry_in(&*config))
}

fn registry_in(config: &crate::Config) -> tinymemory_sources::registry::SourceRegistry {
    tinymemory_sources::registry::SourceRegistry::new(config.config_path().clone())
}

pub async fn list_sources() -> Result<Vec<MemorySourceEntry>, String> {
    registry().await?.list().map_err(|error| error.to_string())
}

pub async fn list_enabled_by_kind(kind: SourceKind) -> Result<Vec<MemorySourceEntry>, String> {
    registry()
        .await?
        .list_enabled_by_kind(kind)
        .map_err(|error| error.to_string())
}

pub async fn get_source(id: &str) -> Result<Option<MemorySourceEntry>, String> {
    registry().await?.get(id).map_err(|error| error.to_string())
}

/// [`get_source`] against an **explicit** config rather than the process-global
/// one.
///
/// `registry` resolves its config path through
/// `config_rpc::load_config_with_timeout`, i.e. from the process environment.
/// That is right for RPC handlers, which serve the active user, and wrong for
/// the embedded memory driver
/// (`crate::driver::embedded`), which is bound to one
/// workspace and holds a `Config` re-anchored to it. Reading the global path
/// there would let a driver bound to workspace B answer with workspace A's
/// sources — the cross-workspace leak the workspace-keyed binding map exists to
/// prevent.
///
/// Synchronous because the registry read itself is; only the config lookup in
/// `registry` was ever async.
pub fn get_source_in(
    config: &crate::Config,
    id: &str,
) -> Result<Option<MemorySourceEntry>, String> {
    registry_in(config)
        .get(id)
        .map_err(|error| error.to_string())
}

pub async fn add_source(entry: MemorySourceEntry) -> Result<MemorySourceEntry, String> {
    let _guard = memory_sources_write_guard().await;
    log::debug!("[memory_sources] crate add kind={}", entry.kind.as_str());
    registry()
        .await?
        .add(entry)
        .map_err(|error| error.to_string())
}

pub async fn update_source(
    id: &str,
    patch: MemorySourcePatch,
) -> Result<MemorySourceEntry, String> {
    let _guard = memory_sources_write_guard().await;
    log::debug!("[memory_sources] crate update id_len={}", id.len());
    registry()
        .await?
        .update(id, patch)
        .map_err(|error| error.to_string())
}

pub async fn remove_source(id: &str) -> Result<bool, String> {
    let _guard = memory_sources_write_guard().await;
    registry()
        .await?
        .remove(id)
        .map_err(|error| error.to_string())
}

pub async fn remove_composio_source_by_connection_id(connection_id: &str) -> Result<usize, String> {
    let _guard = memory_sources_write_guard().await;
    registry()
        .await?
        .remove_composio_source_by_connection_id(connection_id)
        .map_err(|error| error.to_string())
}

pub async fn upsert_composio_source(
    toolkit: &str,
    connection_id: &str,
    label: &str,
) -> Result<MemorySourceEntry, String> {
    let _guard = memory_sources_write_guard().await;
    registry()
        .await?
        .upsert_composio_source(toolkit, connection_id, label)
        .map_err(|error| error.to_string())
}

pub async fn upsert_composio_sources_batch(
    targets: &[ComposioUpsertTarget],
) -> Result<u32, String> {
    let _guard = memory_sources_write_guard().await;
    registry()
        .await?
        .upsert_composio_sources_batch(targets)
        .map_err(|error| error.to_string())
}

pub async fn apply_all_in() -> Result<Vec<MemorySourceEntry>, String> {
    let _guard = memory_sources_write_guard().await;
    registry()
        .await?
        .apply_all_in()
        .map_err(|error| error.to_string())
}

/// Apply conservative per-kind cap defaults to a new source entry.
///
/// Only fills fields that are still `None` — never overwrites a
/// caller-supplied value. This mirrors the retroactive migration logic in
/// `reconcile::apply_composio_source_caps_migration` so the same defaults
/// are applied consistently at creation time and during migration.
pub fn apply_kind_defaults(entry: &mut MemorySourceEntry) {
    match entry.kind {
        SourceKind::GithubRepo => {
            if entry.max_prs.is_none() {
                entry.max_prs = Some(10);
            }
            if entry.max_issues.is_none() {
                entry.max_issues = Some(10);
            }
            if entry.max_commits.is_none() {
                entry.max_commits = Some(50);
            }
        }
        SourceKind::RssFeed => {
            if entry.max_items.is_none() {
                entry.max_items = Some(20);
            }
        }
        SourceKind::TwitterQuery if entry.since_days.is_none() => {
            entry.since_days = Some(7);
        }
        // Folder / WebPage / Composio: no defaults to apply here.
        // Composio defaults are set at upsert time in registry::upsert_composio_source.
        _ => {}
    }
}

/// Decode the source registry a host config carries.
///
/// The registry crosses the host seam as JSON: [`MemorySourceEntry`] is defined
/// by the engine crate, and `tinymemory-api` must not depend on it — that would
/// drag SQLite into the dependency-light contract crate. So the host hands the
/// registry over serialized and this is where it becomes typed again.
///
/// A malformed or absent registry yields an empty list rather than an error.
/// Every caller is a background loop deciding what to sync, and "nothing is
/// registered" is the fail-closed answer there; propagating would take the loop
/// down over one bad row.
#[must_use]
pub fn decode_memory_sources(config: &crate::Config) -> Vec<MemorySourceEntry> {
    match config.memory_sources_json() {
        Ok(value) => serde_json::from_value(value).unwrap_or_else(|e| {
            log::warn!("[memory_sources:registry] could not decode memory sources: {e:#}");
            Vec::new()
        }),
        Err(e) => {
            log::warn!("[memory_sources:registry] could not read memory sources: {e:#}");
            Vec::new()
        }
    }
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
