//! Tests for the host-config adapters around the markdown time-tree store.

use super::*;
use crate::engine::backend::tree::runtime::{
    derive_parent_id, estimate_tokens, level_from_node_id, NodeLevel,
};
use chrono::TimeZone;
use tempfile::TempDir;
use tinymemory_api::host::test_support::TestHostConfig;

fn config(tmp: &TempDir) -> TestHostConfig {
    crate::test_seams::init();
    let mut config = TestHostConfig::default();
    config.workspace_dir = tmp.path().join("workspace");
    config
}

fn node(namespace: &str, node_id: &str, summary: &str) -> TreeNode {
    let now = Utc.with_ymd_and_hms(2024, 3, 15, 14, 0, 0).unwrap();
    TreeNode {
        node_id: node_id.into(),
        namespace: namespace.into(),
        level: level_from_node_id(node_id),
        parent_id: derive_parent_id(node_id),
        summary: summary.into(),
        token_count: estimate_tokens(summary),
        child_count: 0,
        created_at: now,
        updated_at: now,
        metadata: None,
    }
}

#[test]
fn node_paths_round_trip_and_tree_queries_are_scoped() {
    let tmp = TempDir::new().unwrap();
    let config = config(&tmp);
    assert!(tree_dir(&config, "team").ends_with("tree"));
    assert!(buffer_dir(&config, "team").ends_with("buffer"));
    assert!(node_file_path(&config, "team", "root").ends_with("root.md"));
    assert!(read_node(&config, "team", "root").unwrap().is_none());

    for (id, summary) in [
        ("root", "all time"),
        ("2024", "year"),
        ("2024/03", "month"),
        ("2024/03/15", "day"),
        ("2024/03/15/14", "hour"),
    ] {
        write_node(&config, &node("team", id, summary)).unwrap();
    }
    let read = read_node(&config, "team", "2024/03/15/14")
        .unwrap()
        .unwrap();
    assert_eq!(read.level, NodeLevel::Hour);
    assert_eq!(read.summary, "hour");
    let children = read_children(&config, "team", "2024/03/15").unwrap();
    assert_eq!(children.len(), 1);
    let ancestors = read_ancestors(&config, "team", "2024/03/15/14").unwrap();
    assert_eq!(ancestors.len(), 4);
    assert_eq!(ancestors.last().unwrap().node_id, "root");
    assert_eq!(count_nodes(&config, "team").unwrap(), 5);
    let status = get_tree_status(&config, "team").unwrap();
    assert_eq!(status.total_nodes, 5);
    assert_eq!(status.depth, 5);

    write_node(&config, &node("other", "root", "other summary")).unwrap();
    assert_eq!(
        list_namespaces_with_root(&config).unwrap(),
        vec!["other", "team"]
    );
    let roots = collect_root_summaries_with_caps(&config.workspace_dir, 4, 100);
    assert_eq!(roots.len(), 2);
    assert_eq!(delete_tree(&config, "team").unwrap(), 5);
    assert_eq!(count_nodes(&config, "team").unwrap(), 0);
}

#[test]
fn buffer_read_delete_and_drain_preserve_content() {
    let tmp = TempDir::new().unwrap();
    let config = config(&tmp);
    let first = Utc.with_ymd_and_hms(2024, 3, 15, 10, 0, 0).unwrap();
    let second = Utc.with_ymd_and_hms(2024, 3, 15, 11, 0, 0).unwrap();
    let first_path = buffer_write(
        &config,
        "team",
        "---\nuser content",
        &first,
        Some(&serde_json::json!({"source": "test"})),
    )
    .unwrap();
    buffer_write(&config, "team", "second", &second, None).unwrap();
    let rows = buffer_read(&config, "team").unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].1, "---\nuser content");
    let filename = first_path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    buffer_delete(&config, "team", &[filename]).unwrap();
    let drained = buffer_drain(&config, "team").unwrap();
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].1, "second");
    assert!(buffer_read(&config, "team").unwrap().is_empty());
}

#[test]
fn validation_and_markdown_parsing_fail_closed() {
    for valid in ["root", "2024", "2024/03", "2024/03/15", "2024/03/15/14"] {
        validate_node_id(valid).unwrap();
    }
    for invalid in ["../etc", "2024/13", "2024/03/32", "2024/03/15/24"] {
        assert!(validate_node_id(invalid).is_err());
    }
    assert!(validate_namespace("team:mail").is_ok());
    assert!(validate_namespace("../escape").is_err());
    let parsed = parse_node_markdown_pub(
        "---\nnode_id: \"root\"\nlevel: root\ntoken_count: 2\n---\n\nSummary.",
        "team",
        "root",
    )
    .unwrap();
    assert_eq!(parsed.summary, "Summary.");
    assert_eq!(parsed.created_at, DateTime::<Utc>::UNIX_EPOCH);
}
