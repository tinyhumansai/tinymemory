//! Business logic for memory diff — thin host async wrappers over
//! `crate::engine::backend::diff::DiffEngine` (W7).
//!
//! The snapshot/diff/checkpoint/ledger engine is the crate's; the git ledger it
//! writes lives at the same `<workspace>/memory_diff/repo` path with the same
//! libgit2 layout, so existing ledgers keep working byte-for-byte. `DiffEngine`
//! is synchronous and generic over a chunk-source seam, so each op here builds
//! the host [`ChunkStoreItemSource`] (which reads the authoritative
//! `mem_tree_chunks`) and drives the engine inside `spawn_blocking`, preserving
//! the host's `async` + `Result<_, String>` signatures, the `DomainEvent`
//! publishes, and the tracing that RPC/tools/sync/subconscious callers expect.

#[cfg(test)]
use tinymemory_api::host::test_support::TestHostConfig;

use crate::sources::types::MemorySourceEntry;
use crate::Config;

use crate::engine::backend::diff::{DiffEngine, SourceDescriptor};

use super::source::ChunkStoreItemSource;
use crate::engine::backend::diff::types::*;

/// A crate [`SourceDescriptor`] from a host source entry.
fn descriptor(source: &MemorySourceEntry) -> SourceDescriptor {
    SourceDescriptor::new(
        source.id.clone(),
        source.kind.as_str().to_string(),
        source.label.clone(),
    )
}

/// Take a snapshot of the current chunk-store state for a source.
///
/// Reads from `mem_tree_chunks` (already-ingested data) via the item-source
/// seam, groups by item, and commits one blob per item to the git ledger.
/// Returns the new [`Snapshot`] whose `id` is the commit SHA.
pub async fn take_snapshot(
    source: &MemorySourceEntry,
    config: &Config,
    trigger: SnapshotTrigger,
) -> Result<Snapshot, String> {
    let workspace_dir = config.workspace_dir().clone();
    let config_clone = config.to_arc();
    let source_owned = source.clone();
    let desc = descriptor(source);

    let snapshot = tokio::task::spawn_blocking(move || -> anyhow::Result<Snapshot> {
        let items = ChunkStoreItemSource::single(config_clone, &source_owned);
        let engine = DiffEngine::new(workspace_dir, items);
        engine.take_snapshot(&desc, trigger)
    })
    .await
    .map_err(|e| format!("snapshot join error: {e}"))?
    .map_err(|e: anyhow::Error| format!("take_snapshot: {e:#}"))?;

    tracing::debug!(
        snapshot_id = %snapshot.id,
        source_id = %source.id,
        items = snapshot.item_count,
        trigger = %snapshot.trigger.as_str(),
        "[memory_diff] snapshot taken"
    );

    crate::events::publish(crate::events::MemoryEvent::DiffSnapshotTaken {
        snapshot_id: snapshot.id.clone(),
        source_id: source.id.clone(),
        source_kind: source.kind.as_str().to_string(),
        item_count: snapshot.item_count as usize,
        trigger: snapshot.trigger.as_str().to_string(),
    });

    Ok(snapshot)
}

/// Auto-snapshot hook called from `sync_source()` after a successful sync.
pub async fn auto_snapshot_after_sync(
    source: &MemorySourceEntry,
    config: &Config,
) -> Result<Snapshot, String> {
    take_snapshot(source, config, SnapshotTrigger::Auto).await
}

/// List snapshots, newest first — for one source when `source_id` is `Some`,
/// across every source otherwise.
///
/// Lifted verbatim out of `super::rpc::list_snapshots_rpc`, which had the
/// only copy of this query and returned it wrapped in an `RpcOutcome`. The
/// embedded memory driver's `MemoryDiff::snapshots` needs the same read without
/// the RPC envelope, and a second `Ledger::open` call site would make this
/// module no longer the only place that knows the ledger layout.
///
/// An unknown `source_id` yields an empty vector rather than an error — the
/// ledger has no source registry to check against.
pub async fn list_snapshots(
    config: &Config,
    source_id: Option<&str>,
    limit: u32,
) -> Result<Vec<Snapshot>, String> {
    let workspace_dir = config.workspace_dir().clone();
    let source_id = source_id.map(str::to_string);

    tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Snapshot>> {
        let ledger = crate::engine::backend::diff::Ledger::open(&workspace_dir)?;
        ledger.list_snapshots(source_id.as_deref(), limit)
    })
    .await
    .map_err(|e| format!("list_snapshots join: {e}"))?
    .map_err(|e: anyhow::Error| format!("list_snapshots: {e:#}"))
}

/// Compute the diff between two snapshots of the same source.
pub async fn compute_diff(
    config: &Config,
    from_snapshot_id: Option<&str>,
    to_snapshot_id: &str,
    include_text_diff: bool,
) -> Result<DiffResult, String> {
    let workspace_dir = config.workspace_dir().clone();
    let config_clone = config.to_arc();
    let to_id = to_snapshot_id.to_string();
    let from_id = from_snapshot_id.map(|s| s.to_string());

    tokio::task::spawn_blocking(move || -> anyhow::Result<DiffResult> {
        let engine = DiffEngine::new(workspace_dir, ChunkStoreItemSource::read_only(config_clone));
        engine.compute_diff(from_id.as_deref(), &to_id, include_text_diff)
    })
    .await
    .map_err(|e| format!("diff join: {e}"))?
    .map_err(|e: anyhow::Error| format!("compute_diff: {e:#}"))
}

/// Diff current state (latest snapshot) vs previous snapshot for a source.
pub async fn diff_since_last(
    source: &MemorySourceEntry,
    config: &Config,
    include_text_diff: bool,
) -> Result<DiffResult, String> {
    let workspace_dir = config.workspace_dir().clone();
    let config_clone = config.to_arc();
    let source_id = source.id.clone();

    tokio::task::spawn_blocking(move || -> anyhow::Result<DiffResult> {
        let engine = DiffEngine::new(workspace_dir, ChunkStoreItemSource::read_only(config_clone));
        engine.diff_since_last(&source_id, include_text_diff)
    })
    .await
    .map_err(|e| format!("diff_since_last join: {e}"))?
    .map_err(|e: anyhow::Error| format!("diff_since_last: {e:#}"))
}

/// Diff a source's latest snapshot against its read marker — i.e. everything
/// that changed since the agent last *read* this source's diff.
///
/// When `commit` is true, the read marker (a git ref) is advanced to the head
/// snapshot after the diff is computed, so a subsequent call returns only newer
/// changes. This is the turn-to-turn primitive: read the world delta, then
/// acknowledge it as consumed.
pub async fn diff_since_read(
    source: &MemorySourceEntry,
    config: &Config,
    include_text_diff: bool,
    commit: bool,
) -> Result<DiffResult, String> {
    let workspace_dir = config.workspace_dir().clone();
    let config_clone = config.to_arc();
    let source_id = source.id.clone();

    let diff = tokio::task::spawn_blocking(move || -> anyhow::Result<DiffResult> {
        let engine = DiffEngine::new(workspace_dir, ChunkStoreItemSource::read_only(config_clone));
        engine.diff_since_read(&source_id, include_text_diff, commit)
    })
    .await
    .map_err(|e| format!("diff_since_read join: {e}"))?
    .map_err(|e: anyhow::Error| format!("diff_since_read: {e:#}"))?;

    if commit {
        tracing::debug!(
            source_id = %source.id,
            snapshot_id = %diff.to_snapshot_id,
            added = diff.summary.added,
            modified = diff.summary.modified,
            removed = diff.summary.removed,
            "[memory_diff] read marker committed"
        );
    }

    Ok(diff)
}

/// Commit a read marker for one or more sources, advancing each to its
/// current head snapshot. When `source_ids` is `None`, marks all enabled
/// sources that have at least one snapshot. Returns the number of markers set.
pub async fn mark_read(config: &Config, source_ids: Option<Vec<String>>) -> Result<u64, String> {
    let target_ids: Vec<String> = match source_ids {
        Some(ids) => ids,
        None => crate::sources::registry::list_sources()
            .await
            .map_err(|e| format!("list sources: {e}"))?
            .into_iter()
            .filter(|s| s.enabled)
            .map(|s| s.id)
            .collect(),
    };

    let workspace_dir = config.workspace_dir().clone();
    let config_clone = config.to_arc();
    let ids_for_blocking = target_ids.clone();

    let (marked, snapshot_ids) =
        tokio::task::spawn_blocking(move || -> anyhow::Result<(u64, Vec<String>)> {
            let engine =
                DiffEngine::new(workspace_dir, ChunkStoreItemSource::read_only(config_clone));
            // Gather the head snapshot ids that will be marked, for the event
            // payload (the crate `mark_read` returns only a count).
            let mut snapshot_ids = Vec::new();
            for sid in &ids_for_blocking {
                if let Some(head) = engine.list_snapshots(Some(sid), 1)?.into_iter().next() {
                    snapshot_ids.push(head.id);
                }
            }
            let marked = engine.mark_read(&ids_for_blocking)?;
            Ok((marked, snapshot_ids))
        })
        .await
        .map_err(|e| format!("mark_read join: {e}"))?
        .map_err(|e: anyhow::Error| format!("mark_read: {e:#}"))?;

    tracing::debug!(
        sources = marked,
        "[memory_diff] mark_read committed read markers"
    );

    crate::events::publish(crate::events::MemoryEvent::DiffMarkedRead {
        source_ids: target_ids,
        snapshot_ids,
    });

    Ok(marked)
}

/// Create a checkpoint (git tag at HEAD) grouping the latest snapshot per
/// enabled source. Sources lacking a snapshot are baselined first.
pub async fn create_checkpoint(label: &str, config: &Config) -> Result<Checkpoint, String> {
    let sources = crate::sources::registry::list_sources()
        .await
        .map_err(|e| format!("list sources: {e}"))?;
    let enabled: Vec<MemorySourceEntry> = sources.into_iter().filter(|s| s.enabled).collect();

    let workspace_dir = config.workspace_dir().clone();
    let config_clone = config.to_arc();
    let label_owned = label.to_string();

    let checkpoint = tokio::task::spawn_blocking(move || -> anyhow::Result<Checkpoint> {
        let descriptors: Vec<SourceDescriptor> = enabled.iter().map(descriptor).collect();
        let items = ChunkStoreItemSource::for_sources(config_clone, &enabled);
        let engine = DiffEngine::new(workspace_dir, items);
        engine.create_checkpoint(&label_owned, &descriptors)
    })
    .await
    .map_err(|e| format!("checkpoint persist join: {e}"))?
    .map_err(|e: anyhow::Error| format!("create_checkpoint: {e:#}"))?;

    tracing::debug!(
        checkpoint_id = %checkpoint.id,
        snapshots = checkpoint.snapshot_ids.len(),
        "[memory_diff] checkpoint created"
    );

    Ok(checkpoint)
}

/// Compute a cross-source diff: everything that changed since a checkpoint.
pub async fn diff_since_checkpoint(
    checkpoint_id: &str,
    config: &Config,
    include_text_diff: bool,
) -> Result<CrossSourceDiff, String> {
    let workspace_dir = config.workspace_dir().clone();
    let config_clone = config.to_arc();
    let ckpt_id = checkpoint_id.to_string();

    tokio::task::spawn_blocking(move || -> anyhow::Result<CrossSourceDiff> {
        let engine = DiffEngine::new(workspace_dir, ChunkStoreItemSource::read_only(config_clone));
        engine.diff_since_checkpoint(&ckpt_id, include_text_diff)
    })
    .await
    .map_err(|e| format!("diff_since_checkpoint join: {e}"))?
    .map_err(|e: anyhow::Error| format!("diff_since_checkpoint: {e:#}"))
}

/// Delete checkpoint tags older than `older_than_days`.
///
/// Snapshot commits are retained — git history *is* the ledger, and git's
/// delta compression keeps it compact — so cleanup only prunes named baselines.
/// Returns the number of checkpoints deleted.
pub async fn cleanup(config: &Config, older_than_days: u32) -> Result<u64, String> {
    let workspace_dir = config.workspace_dir().clone();
    let config_clone = config.to_arc();

    tokio::task::spawn_blocking(move || -> anyhow::Result<u64> {
        let engine = DiffEngine::new(workspace_dir, ChunkStoreItemSource::read_only(config_clone));
        engine.cleanup(older_than_days)
    })
    .await
    .map_err(|e| format!("cleanup join: {e}"))?
    .map_err(|e: anyhow::Error| format!("cleanup: {e:#}"))
}

#[cfg(test)]
#[path = "ops_tests.rs"]
mod tests;
