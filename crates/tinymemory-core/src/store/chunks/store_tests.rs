//! Tests for chunk persistence, lifecycle, raw-ingest, and embedding adapters.

use super::*;
use crate::store::chunks::types::{chunk_id, Metadata, SourceRef};
use chrono::{TimeZone, Utc};
use tempfile::TempDir;
use tinymemory_api::host::test_support::TestHostConfig;

fn config() -> (TempDir, TestHostConfig) {
    crate::test_seams::init();
    let tmp = TempDir::new().unwrap();
    let mut config = TestHostConfig::default();
    config.workspace_dir = tmp.path().join("workspace");
    (tmp, config)
}

fn chunk(source_id: &str, sequence: u32, timestamp_ms: i64) -> Chunk {
    let timestamp = Utc.timestamp_millis_opt(timestamp_ms).unwrap();
    Chunk {
        id: chunk_id(SourceKind::Chat, source_id, sequence, "body"),
        content: format!("body {source_id} {sequence}"),
        metadata: Metadata {
            source_kind: SourceKind::Chat,
            source_id: source_id.into(),
            owner: "owner@example.com".into(),
            timestamp,
            time_range: (timestamp, timestamp),
            tags: vec!["memory_sources".into()],
            source_ref: Some(SourceRef::new(format!("chat://{source_id}/{sequence}"))),
            path_scope: None,
        },
        token_count: 4,
        seq_in_source: sequence,
        created_at: timestamp,
        partial_message: false,
    }
}

#[test]
fn chunks_round_trip_filter_and_lifecycle() {
    let (_tmp, config) = config();
    assert_eq!(upsert_chunks(&config, &[]).unwrap(), 0);
    let first = chunk("team:a", 0, 1_700_000_000_000);
    let mut second = chunk("team:b", 1, 1_700_000_001_000);
    second.metadata.source_kind = SourceKind::Email;
    upsert_chunks(&config, &[first.clone(), second.clone()]).unwrap();
    assert_eq!(count_chunks(&config).unwrap(), 2);
    assert_eq!(get_chunk(&config, &first.id).unwrap(), Some(first.clone()));
    assert!(get_chunk(&config, "missing").unwrap().is_none());
    let batch = get_chunks_batch(&config, &[first.id.clone(), "missing".into()]).unwrap();
    assert_eq!(batch.len(), 1);
    let listed = list_chunks(
        &config,
        &ListChunksQuery {
            source_kind: Some(SourceKind::Email),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(listed, vec![second.clone()]);
    assert_eq!(extraction_coverage(&config).unwrap(), 0.0);

    set_chunk_lifecycle_status(&config, &first.id, CHUNK_STATUS_ADMITTED).unwrap();
    set_chunk_lifecycle_status(&config, &second.id, CHUNK_STATUS_DROPPED).unwrap();
    assert_eq!(
        get_chunk_lifecycle_status(&config, &first.id)
            .unwrap()
            .as_deref(),
        Some(CHUNK_STATUS_ADMITTED)
    );
    assert_eq!(
        count_chunks_by_lifecycle_status(&config, CHUNK_STATUS_ADMITTED).unwrap(),
        1
    );
    update_chunk_content_sha256(&config, &first.id, "chunk-sha").unwrap();
    update_summary_content_sha256(&config, "missing-summary", "summary-sha").unwrap();
    assert_eq!(
        list_source_ids_with_prefix(&config, SourceKind::Chat, "team:").unwrap(),
        vec!["team:a"]
    );
}

#[test]
fn source_claim_raw_paths_and_deletes_are_idempotent() {
    let (_tmp, config) = config();
    let first = chunk("prefix:a", 0, 1_700_000_000_000);
    let second = chunk("prefix:b", 0, 1_700_000_001_000);
    upsert_chunks(&config, &[first.clone(), second.clone()]).unwrap();
    with_connection(&config, |connection| {
        let transaction = connection.unchecked_transaction()?;
        assert!(claim_source_ingest_tx(
            &transaction,
            SourceKind::Chat,
            "source",
            100
        )?);
        assert!(!claim_source_ingest_tx(
            &transaction,
            SourceKind::Chat,
            "source",
            101
        )?);
        set_chunk_lifecycle_status_tx(&transaction, &first.id, CHUNK_STATUS_SEALED)?;
        assert_eq!(
            get_chunk_lifecycle_status_tx(&transaction, &first.id)?.as_deref(),
            Some(CHUNK_STATUS_SEALED)
        );
        transaction.commit()?;
        Ok(())
    })
    .unwrap();
    assert!(is_source_ingested(&config, SourceKind::Chat, "source").unwrap());

    let paths = vec!["raw/mail/a".into(), "raw/mail/b".into()];
    assert_eq!(mark_raw_paths_ingested(&config, &paths).unwrap(), 2);
    assert!(filter_raw_paths_not_ingested(&config, &paths)
        .unwrap()
        .is_empty());
    assert_eq!(
        count_raw_paths_ingested_with_prefix(&config, "raw/mail/").unwrap(),
        2
    );
    assert_eq!(
        filter_raw_paths_not_ingested(&config, &["raw/mail/a".into(), "raw/mail/c".into()])
            .unwrap(),
        vec!["raw/mail/c"]
    );

    assert_eq!(
        delete_chunks_by_source(&config, SourceKind::Chat, "prefix:a").unwrap(),
        1
    );
    assert_eq!(
        delete_chunks_by_source_prefix(&config, SourceKind::Chat, "prefix:").unwrap(),
        1
    );
    assert_eq!(
        delete_chunks_by_owner(&config, SourceKind::Chat, "nobody").unwrap(),
        0
    );
    assert!(!delete_orphaned_source_tree(&config, SourceKind::Chat, "missing").unwrap());
}

#[test]
fn raw_archive_references_round_trip_through_direct_and_transaction_paths() {
    let (_tmp, config) = config();
    let first = chunk("raw:a", 0, 1_700_000_000_000);
    let second = chunk("raw:b", 0, 1_700_000_001_000);
    upsert_chunks(&config, &[first.clone(), second.clone()]).unwrap();
    let first_refs = vec![RawRef {
        path: "raw/mail/one.md".into(),
        start: 4,
        end: Some(12),
    }];
    set_chunk_raw_refs(&config, &first.id, &first_refs).unwrap();
    with_connection(&config, |connection| {
        let transaction = connection.unchecked_transaction()?;
        set_chunk_raw_refs_tx(
            &transaction,
            &second.id,
            &[RawRef {
                path: "raw/mail/two.md".into(),
                start: 0,
                end: Some(8),
            }],
        )?;
        transaction.commit()?;
        Ok(())
    })
    .unwrap();

    let stored = get_chunk_raw_refs(&config, &first.id)
        .unwrap()
        .expect("raw refs stored");
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].path, first_refs[0].path);
    assert_eq!(stored[0].start, first_refs[0].start);
    assert_eq!(stored[0].end, first_refs[0].end);
    assert!(get_chunk_raw_refs(&config, "missing").unwrap().is_none());
    let paths = list_chunk_raw_ref_paths_with_prefix(&config, "raw/mail/").unwrap();
    assert_eq!(paths.len(), 2);
    assert!(paths.contains("raw/mail/one.md"));
    assert!(paths.contains("raw/mail/two.md"));
    assert!(get_chunk_content_pointers(&config, &first.id)
        .unwrap()
        .is_none());
    assert!(get_chunk_content_path(&config, &first.id)
        .unwrap()
        .is_none());
    assert!(get_summary_content_pointers(&config, "missing")
        .unwrap()
        .is_none());
    assert!(list_summaries_with_content_path(&config)
        .unwrap()
        .is_empty());
}

#[test]
fn embeddings_are_signature_scoped_and_tombstones_clear() {
    let (_tmp, config) = config();
    let first = chunk("team:a", 0, 1_700_000_000_000);
    let second = chunk("team:b", 0, 1_700_000_001_000);
    upsert_chunks(&config, &[first.clone(), second.clone()]).unwrap();
    let active = tree_active_signature(&config);
    set_chunk_embedding(&config, &first.id, &[0.1, 0.2]).unwrap();
    set_chunk_embedding_for_signature(&config, &first.id, "custom@2", &[0.3, 0.4]).unwrap();
    assert_eq!(
        get_chunk_embedding(&config, &first.id).unwrap(),
        Some(vec![0.1, 0.2])
    );
    assert_eq!(
        get_chunk_embedding_for_signature(&config, &first.id, "custom@2").unwrap(),
        Some(vec![0.3, 0.4])
    );
    assert!(
        get_chunk_embedding_for_signature(&config, &first.id, "missing")
            .unwrap()
            .is_none()
    );
    let batch =
        get_chunk_embeddings_batch(&config, &[first.id.clone(), second.id.clone()]).unwrap();
    assert_eq!(batch.len(), 1);
    let custom = get_chunk_embeddings_for_signature_batch(
        &config,
        &[first.id.clone(), second.id.clone()],
        "custom@2",
    )
    .unwrap();
    assert_eq!(custom.len(), 1);

    mark_chunk_reembed_skipped(&config, &first.id, &active, "unreadable").unwrap();
    clear_chunk_reembed_skipped(&config, &first.id, &active).unwrap();
    mark_chunk_reembed_skipped(&config, &first.id, "custom@2", "unreadable").unwrap();
    mark_chunk_reembed_skipped(&config, &second.id, "custom@2", "unreadable").unwrap();
    assert_eq!(
        clear_reembed_skipped_for_signature(&config, "custom@2").unwrap(),
        2
    );

    with_connection(&config, |connection| {
        let transaction = connection.unchecked_transaction()?;
        set_chunk_embedding_for_signature_tx(&transaction, &second.id, "tx@1", &[0.9])?;
        transaction.commit()?;
        Ok(())
    })
    .unwrap();
    assert_eq!(
        get_chunk_embedding_for_signature(&config, &second.id, "tx@1").unwrap(),
        Some(vec![0.9])
    );
}
