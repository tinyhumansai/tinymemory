//! Tests for the surrounding module.

use super::*;

/// A folder source, the shape the prefix and status tests both start from.
fn folder_entry(id: &str) -> MemorySourceEntry {
    MemorySourceEntry {
        id: id.into(),
        kind: SourceKind::Folder,
        label: "x".into(),
        enabled: true,
        toolkit: None,
        connection_id: None,
        path: Some("/tmp".into()),
        glob: None,
        url: None,
        branch: None,
        paths: Vec::new(),
        query: None,
        since_days: None,
        max_items: None,
        max_commits: None,
        max_issues: None,
        max_prs: None,
        selector: None,
        max_tokens_per_sync: None,
        max_cost_per_sync_usd: None,
        sync_depth_days: None,
    }
}

#[test]
fn source_id_prefix_dispatch() {
    let mut entry = folder_entry("src_abc");
    assert_eq!(source_id_prefix(&entry), "mem_src:src_abc:%");

    // A Composio source is matched on its connection, not just its
    // toolkit: a second Gmail account must not count the first's chunks.
    entry.kind = SourceKind::Composio;
    entry.toolkit = Some("gmail".into());
    entry.connection_id = Some("conn-1".into());
    assert_eq!(source_id_prefix(&entry), "gmail:conn-1:%");

    entry.connection_id = None;
    assert_eq!(source_id_prefix(&entry), "gmail:%");

    entry.toolkit = None;
    assert_eq!(source_id_prefix(&entry), "__no_toolkit__:%");
}

/// A chunk under `source_id`, with a deterministic id the test can address.
fn chunk(id: &str, source_id: &str) -> crate::store::chunks::types::Chunk {
    use crate::store::chunks::types::{Chunk, Metadata, SourceKind as ChunkSourceKind};

    let at = chrono::Utc::now();
    Chunk {
        id: id.into(),
        content: "content".into(),
        metadata: Metadata::point_in_time(ChunkSourceKind::Document, source_id, "owner", at),
        token_count: 1,
        seq_in_source: 0,
        created_at: at,
        partial_message: false,
    }
}

/// The status query counted pending as `embedding IS NULL` over
/// `mem_tree_chunks`. That column is a legacy migration artefact nothing
/// writes, so every chunk read as pending and a healthy source reported
/// `chunks_pending == chunks_synced` forever.
///
/// Pending is "not resolved", and a chunk resolves by carrying an
/// embedding, by being dropped, or by being recorded as skipped for
/// re-embedding. Only the first of these four is genuinely still in
/// flight.
#[tokio::test]
async fn pending_counts_unresolved_chunks_not_the_dead_embedding_column() {
    use tinymemory_api::host::test_support::TestHostConfig;
    use tinymemory_api::host::MemoryHostConfig;

    crate::test_seams::init();
    let workspace = tempfile::tempdir().expect("workspace");
    let mut host = TestHostConfig::default();
    host.workspace_dir = workspace.path().join("workspace");
    let config = host.to_arc();

    let source = folder_entry("src_status");
    let chunks = [
        chunk("chunk-embedded", "mem_src:src_status:item-1"),
        chunk("chunk-pending", "mem_src:src_status:item-2"),
        chunk("chunk-dropped", "mem_src:src_status:item-3"),
        chunk("chunk-skipped", "mem_src:src_status:item-4"),
    ];
    crate::store::chunks::store::upsert_chunks(&*config, &chunks).expect("upsert chunks");

    crate::store::chunks::store::set_chunk_embedding(&*config, "chunk-embedded", &[0.1, 0.2])
        .expect("set embedding");
    crate::store::chunks::store::set_chunk_lifecycle_status(
        &*config,
        "chunk-dropped",
        crate::store::chunks::store::CHUNK_STATUS_DROPPED,
    )
    .expect("set lifecycle status");
    crate::store::chunks::store::mark_chunk_reembed_skipped(
        &*config,
        "chunk-skipped",
        "test-signature",
        "too long",
    )
    .expect("mark reembed skipped");

    // Guard against a vacuous test: the legacy column must still be NULL
    // for every row, so a pending count of 1 is attributable to the new
    // predicate rather than to the old one happening to agree.
    let legacy_nulls: i64 = crate::store::chunks::store::with_connection(&*config, |conn| {
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM mem_tree_chunks \
             WHERE embedding IS NULL AND source_id LIKE 'mem_src:src_status:%'",
            [],
            |row| row.get(0),
        )?)
    })
    .expect("count legacy nulls");
    assert_eq!(
        legacy_nulls, 4,
        "nothing writes the legacy column, so counting it would report all four pending"
    );

    let status = source_status(&*config, &source)
        .await
        .expect("source status");
    assert_eq!(status.chunks_synced, 4);
    assert_eq!(
        status.chunks_pending, 1,
        "only the chunk with no embedding, no drop and no skip is still in flight"
    );
    assert!(status.last_chunk_at_ms.is_some());
}

/// A source with no chunks reports zeroes rather than failing on the
/// `NULL` a `SUM` over no rows produces.
#[tokio::test]
async fn a_source_with_no_chunks_reports_zeroes() {
    use tinymemory_api::host::test_support::TestHostConfig;
    use tinymemory_api::host::MemoryHostConfig;

    crate::test_seams::init();
    let workspace = tempfile::tempdir().expect("workspace");
    let mut host = TestHostConfig::default();
    host.workspace_dir = workspace.path().join("workspace");
    let config = host.to_arc();

    let status = source_status(&*config, &folder_entry("src_empty"))
        .await
        .expect("source status");
    assert_eq!(status.chunks_synced, 0);
    assert_eq!(status.chunks_pending, 0);
    assert_eq!(status.last_chunk_at_ms, None);
    assert_eq!(status.freshness, FreshnessLabel::Idle);
}
