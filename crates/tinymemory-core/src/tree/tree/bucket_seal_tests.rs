//! Tests for deterministic leaf buffering and cascade adapter paths.

use super::*;
use crate::store::trees::store::{get_buffer, insert_tree};
use crate::store::trees::{TreeKind, TreeStatus};
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

fn tree() -> Tree {
    Tree {
        id: "tree-1".into(),
        kind: TreeKind::Source,
        scope: "source-1".into(),
        ask: None,
        root_id: None,
        max_level: 0,
        status: TreeStatus::Active,
        created_at: Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        last_sealed_at: None,
    }
}

#[test]
fn direct_and_deferred_leaf_appends_update_level_zero_buffer() {
    let (_tmp, config) = config();
    let tree = tree();
    insert_tree(&config, &tree).unwrap();
    let timestamp = Utc.with_ymd_and_hms(2024, 1, 2, 3, 0, 0).unwrap();
    append_to_buffer(&config, &tree.id, 0, "chunk-1", 12, timestamp).unwrap();
    let buffer = get_buffer(&config, &tree.id, 0).unwrap();
    assert_eq!(buffer.item_ids, vec!["chunk-1"]);
    assert_eq!(buffer.token_sum, 12);

    let leaf = LeafRef {
        chunk_id: "chunk-2".into(),
        token_count: 8,
        timestamp,
        content: "content".into(),
        entities: vec!["entity:alice".into()],
        topics: vec!["launch".into()],
        score: 0.5,
    };
    assert!(!append_leaf_deferred(&config, &tree, &leaf).unwrap());
    let buffer = get_buffer(&config, &tree.id, 0).unwrap();
    assert_eq!(buffer.item_ids, vec!["chunk-1", "chunk-2"]);
    assert_eq!(buffer.token_sum, 20);
}
