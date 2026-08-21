//! Tests for the surrounding module.

use super::*;
use crate::engine::backend::diff::{Ledger, SnapshotMeta};

fn test_config() -> TestHostConfig {
    crate::test_seams::init();
    let dir = tempfile::tempdir().unwrap();
    let mut config = TestHostConfig::default();
    config.workspace_dir = dir.path().to_path_buf();
    // Leak the tempdir so the path stays valid for the test's lifetime.
    std::mem::forget(dir);
    config
}

fn folder_source(id: &str) -> MemorySourceEntry {
    MemorySourceEntry {
        id: id.into(),
        kind: crate::sources::types::SourceKind::Folder,
        label: "Docs".into(),
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

/// Seed a snapshot directly through the (crate) ledger, bypassing the chunk
/// store — exercises the host async wrappers over real ledger state.
fn seed(config: &Config, source_id: &str, taken_at_ms: i64, items: &[(&str, &str)]) -> Snapshot {
    let ledger = Ledger::open(config.workspace_dir()).unwrap();
    let items: Vec<(String, String)> = items
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    ledger
        .commit_snapshot(
            &SnapshotMeta {
                source_id: source_id.to_string(),
                source_kind: "folder".to_string(),
                label: "Docs".to_string(),
                trigger: SnapshotTrigger::Auto,
            },
            &items,
            taken_at_ms,
        )
        .unwrap()
}

#[tokio::test]
async fn compute_diff_detects_added_modified_removed() {
    let config = test_config();
    let from = seed(
        &config,
        "src_a",
        1000,
        &[("a", "alpha"), ("b", "beta"), ("c", "gamma")],
    );
    let to = seed(
        &config,
        "src_a",
        2000,
        &[("a", "alpha"), ("b", "beta v2"), ("d", "delta")],
    );

    let diff = compute_diff(&config, Some(&from.id), &to.id, false)
        .await
        .unwrap();

    assert_eq!(diff.summary.added, 1, "d added");
    assert_eq!(diff.summary.modified, 1, "b modified");
    assert_eq!(diff.summary.removed, 1, "c removed");
    assert_eq!(diff.summary.unchanged, 1, "a unchanged");

    let kind_of = |id: &str| {
        diff.changes
            .iter()
            .find(|c| c.item_id == id)
            .map(|c| c.kind.clone())
    };
    assert_eq!(kind_of("d"), Some(ChangeKind::Added));
    assert_eq!(kind_of("b"), Some(ChangeKind::Modified));
    assert_eq!(kind_of("c"), Some(ChangeKind::Removed));
    assert_eq!(kind_of("a"), None, "unchanged items are not in changes");
}

#[tokio::test]
async fn compute_diff_against_none_marks_all_added() {
    let config = test_config();
    let to = seed(&config, "src_a", 1000, &[("a", "x")]);
    let diff = compute_diff(&config, None, &to.id, false).await.unwrap();
    assert_eq!(diff.summary.added, 1);
    assert_eq!(diff.from_snapshot_id, None);
}

#[tokio::test]
async fn auto_snapshot_and_listing_round_trip_empty_source_state() {
    let config = test_config();
    let source = folder_source("src_empty");

    let snapshot = auto_snapshot_after_sync(&source, &config).await.unwrap();
    assert_eq!(snapshot.source_id, source.id);
    assert_eq!(snapshot.trigger, SnapshotTrigger::Auto);
    assert_eq!(snapshot.item_count, 0);

    let for_source = list_snapshots(&config, Some("src_empty"), 10)
        .await
        .unwrap();
    assert_eq!(for_source.len(), 1);
    assert_eq!(for_source[0].id, snapshot.id);
    assert_eq!(list_snapshots(&config, None, 10).await.unwrap().len(), 1);
    assert!(list_snapshots(&config, Some("unknown"), 10)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn cleanup_removes_old_checkpoint_tags_but_keeps_snapshots() {
    let config = test_config();
    let snapshot = seed(&config, "src_cleanup", 1_000, &[("a", "alpha")]);
    let ledger = Ledger::open(&config.workspace_dir).unwrap();
    ledger
        .create_checkpoint("ckpt_old", "old", std::slice::from_ref(&snapshot.id), 1_500)
        .unwrap();
    assert_eq!(ledger.list_checkpoints(10).unwrap().len(), 1);
    drop(ledger);

    assert_eq!(cleanup(&config, 0).await.unwrap(), 1);
    assert_eq!(
        list_snapshots(&config, Some("src_cleanup"), 10)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn compute_diff_rejects_cross_source() {
    let config = test_config();
    let from = seed(&config, "src_a", 1000, &[("a", "x")]);
    let to = seed(&config, "src_b", 2000, &[("b", "y")]);
    let err = compute_diff(&config, Some(&from.id), &to.id, false)
        .await
        .unwrap_err();
    assert!(err.contains("cross-source"), "got: {err}");
}

#[tokio::test]
async fn compute_diff_text_diff_only_when_requested() {
    let config = test_config();
    let from = seed(&config, "src_a", 1000, &[("a", "line one\nline two\n")]);
    let to = seed(
        &config,
        "src_a",
        2000,
        &[("a", "line one\nline TWO changed\n")],
    );

    let without = compute_diff(&config, Some(&from.id), &to.id, false)
        .await
        .unwrap();
    assert!(without.changes[0].text_diff.is_none());

    let with = compute_diff(&config, Some(&from.id), &to.id, true)
        .await
        .unwrap();
    let td = with.changes[0]
        .text_diff
        .as_ref()
        .expect("text diff present");
    assert!(td.contains("line TWO changed"), "got: {td}");
}

#[tokio::test]
async fn diff_since_last_handles_zero_one_two_snapshots() {
    let config = test_config();
    let source = folder_source("src_a");

    // 0 snapshots → error
    assert!(diff_since_last(&source, &config, false).await.is_err());

    // 1 snapshot → everything added (diff vs None)
    seed(&config, "src_a", 1000, &[("a", "x")]);
    let one = diff_since_last(&source, &config, false).await.unwrap();
    assert_eq!(one.summary.added, 1);

    // 2 snapshots → diff latest vs previous
    seed(&config, "src_a", 2000, &[("a", "x"), ("b", "y")]);
    let two = diff_since_last(&source, &config, false).await.unwrap();
    assert_eq!(two.summary.added, 1, "b is new in s2");
    assert_eq!(two.summary.unchanged, 1, "a unchanged");
}

#[tokio::test]
async fn diff_since_read_commits_marker_and_returns_only_new_changes() {
    let config = test_config();
    let source = folder_source("src_a");

    seed(&config, "src_a", 1000, &[("a", "x")]);

    // First read: no marker → full diff (a added), and commit advances marker.
    let first = diff_since_read(&source, &config, false, true)
        .await
        .unwrap();
    assert_eq!(first.summary.added, 1);

    // Second read with no new snapshot: marker == head → nothing changed.
    let second = diff_since_read(&source, &config, false, true)
        .await
        .unwrap();
    assert_eq!(second.summary.added, 0);
    assert_eq!(second.summary.modified, 0);
    assert_eq!(second.summary.removed, 0);
    assert!(second.changes.is_empty());

    // New snapshot then read: only the delta since the marker shows.
    seed(&config, "src_a", 2000, &[("a", "x"), ("b", "y")]);
    let third = diff_since_read(&source, &config, false, true)
        .await
        .unwrap();
    assert_eq!(third.summary.added, 1, "only b is new since last read");
    assert_eq!(third.summary.unchanged, 1);
}

#[tokio::test]
async fn diff_since_read_without_commit_does_not_advance_marker() {
    let config = test_config();
    let source = folder_source("src_a");
    seed(&config, "src_a", 1000, &[("a", "x")]);

    // Preview (commit=false) twice → both show the full diff.
    let a = diff_since_read(&source, &config, false, false)
        .await
        .unwrap();
    let b = diff_since_read(&source, &config, false, false)
        .await
        .unwrap();
    assert_eq!(a.summary.added, 1);
    assert_eq!(b.summary.added, 1, "marker was not advanced");
}

#[tokio::test]
async fn mark_read_advances_marker_for_explicit_sources() {
    let config = test_config();
    let source = folder_source("src_a");
    seed(&config, "src_a", 1000, &[("a", "x")]);

    let marked = mark_read(&config, Some(vec!["src_a".to_string()]))
        .await
        .unwrap();
    assert_eq!(marked, 1);

    // After marking, a read shows no changes (marker already at head).
    let diff = diff_since_read(&source, &config, false, false)
        .await
        .unwrap();
    assert_eq!(diff.summary.added, 0);
    assert!(diff.changes.is_empty());
}

#[tokio::test]
async fn diff_since_checkpoint_aggregates_across_sources() {
    let config = test_config();
    // Baseline snapshots for two sources, grouped into a checkpoint.
    let a1 = seed(&config, "src_a", 1000, &[("a", "x")]);
    let b1 = seed(&config, "src_b", 1000, &[("b", "y")]);
    {
        let ledger = Ledger::open(&config.workspace_dir).unwrap();
        ledger
            .create_checkpoint("ckpt_1", "base", &[a1.id.clone(), b1.id.clone()], 1500)
            .unwrap();
    }

    // src_a gets a new head with a modification; src_b unchanged.
    seed(&config, "src_a", 2000, &[("a", "x v2")]);

    let cross = diff_since_checkpoint("ckpt_1", &config, false)
        .await
        .unwrap();
    assert_eq!(cross.summary.modified, 1, "src_a 'a' modified");
    assert_eq!(
        cross.per_source.len(),
        1,
        "only src_a changed; unchanged src_b is skipped"
    );
    assert_eq!(cross.per_source[0].source_id, "src_a");
}
