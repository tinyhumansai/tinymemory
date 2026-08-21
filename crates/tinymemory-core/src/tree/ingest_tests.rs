//! Tests for direct pre-summarized tree ingestion and artifact persistence.

use super::*;
use crate::store::trees::store::{get_summary, insert_tree};
use crate::store::trees::{TreeKind, TreeStatus};
use chrono::{TimeZone, Utc};
use tempfile::TempDir;
use tinymemory_api::host::{test_support::TestHostConfig, MemoryHostConfig};

#[tokio::test]
async fn prebuilt_summary_persists_content_labels_and_index_row() {
    crate::test_seams::init();
    let tmp = TempDir::new().unwrap();
    let mut config = TestHostConfig::default();
    config.workspace_dir = tmp.path().join("workspace");
    let timestamp = Utc.with_ymd_and_hms(2024, 2, 3, 4, 0, 0).unwrap();
    let tree = Tree {
        id: "source:direct".into(),
        kind: TreeKind::Source,
        scope: "folder:notes".into(),
        ask: None,
        root_id: None,
        max_level: 0,
        status: TreeStatus::Active,
        created_at: timestamp,
        last_sealed_at: None,
    };
    insert_tree(&config, &tree).unwrap();
    let outcome = ingest_summary(
        &config,
        &tree,
        SummaryIngestInput {
            content: "A concise imported summary.".into(),
            token_count: 6,
            entities: vec!["entity:alice".into()],
            topics: vec!["launch".into()],
            time_range_start: timestamp,
            time_range_end: timestamp,
            score: 0.8,
            child_labels: vec!["child-a".into(), "child-b".into()],
            child_basenames: vec![Some("a.md".into()), None],
        },
    )
    .await
    .unwrap();
    assert!(!outcome.summary_id.is_empty());
    assert!(!outcome.content_path.is_empty());
    let stored = get_summary(&config, &outcome.summary_id).unwrap().unwrap();
    assert_eq!(stored.content, "A concise imported summary.");
    assert_eq!(stored.child_ids, vec!["child-a", "child-b"]);
    assert_eq!(stored.entities, vec!["entity:alice"]);
    assert!(config
        .memory_tree_content_root()
        .join(&outcome.content_path)
        .exists());
}
