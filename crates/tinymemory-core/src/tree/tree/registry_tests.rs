//! Tests for the surrounding module.

use super::*;
use tempfile::TempDir;

fn test_config() -> (TempDir, TestHostConfig) {
    crate::test_seams::init();
    let tmp = TempDir::new().unwrap();
    let mut cfg = TestHostConfig::default();
    cfg.workspace_dir = tmp.path().to_path_buf();
    (tmp, cfg)
}

#[test]
fn get_or_create_is_idempotent_on_scope() {
    let (_tmp, cfg) = test_config();
    let first = get_or_create_tree(&cfg, TreeKind::Source, "slack:#eng").unwrap();
    let second = get_or_create_tree(&cfg, TreeKind::Source, "slack:#eng").unwrap();
    assert_eq!(first.id, second.id);
    assert_eq!(first.kind, TreeKind::Source);
    assert_eq!(first.status, TreeStatus::Active);
}

#[test]
fn different_scopes_yield_different_trees() {
    let (_tmp, cfg) = test_config();
    let a = get_or_create_tree(&cfg, TreeKind::Source, "slack:#eng").unwrap();
    let b = get_or_create_tree(&cfg, TreeKind::Source, "gmail:user@example.com").unwrap();
    assert_ne!(a.id, b.id);
    assert_ne!(a.scope, b.scope);
}

#[test]
fn different_kinds_same_scope_yield_different_trees() {
    let (_tmp, cfg) = test_config();
    let source = get_or_create_tree(&cfg, TreeKind::Source, "shared:scope").unwrap();
    let topic = get_or_create_tree(&cfg, TreeKind::Topic, "shared:scope").unwrap();
    assert_ne!(source.id, topic.id);
    assert_eq!(source.kind, TreeKind::Source);
    assert_eq!(topic.kind, TreeKind::Topic);
}

#[test]
fn global_tree_is_singleton() {
    let (_tmp, cfg) = test_config();
    let first = get_or_create_tree(&cfg, TreeKind::Global, "global").unwrap();
    let second = get_or_create_tree(&cfg, TreeKind::Global, "global").unwrap();
    assert_eq!(first.id, second.id);
    assert_eq!(first.kind, TreeKind::Global);
}

#[test]
fn tree_id_has_expected_prefix() {
    let source_id = new_tree_id(TreeKind::Source);
    assert!(source_id.starts_with("source:"));
    let topic_id = new_tree_id(TreeKind::Topic);
    assert!(topic_id.starts_with("topic:"));
    let global_id = new_tree_id(TreeKind::Global);
    assert!(global_id.starts_with("global:"));

    let sum_id = new_summary_id(3);
    assert!(sum_id.starts_with("summary:"));
    assert!(sum_id.contains(":L3-"), "expected level suffix in {sum_id}");
}

#[test]
fn summary_id_format_is_lexicographically_chronological() {
    let earlier_ms: u64 = 1_700_000_000_000;
    let later_ms: u64 = 1_700_000_000_001;
    let earlier = format!("summary:{:013}:L1-{:08x}", earlier_ms, u32::MAX);
    let later = format!("summary:{:013}:L9-{:08x}", later_ms, 0u32);
    assert!(
        earlier < later,
        "expected {earlier} < {later} (ms must outrank level + tail)"
    );

    let live = new_summary_id(2);
    assert!(live.starts_with("summary:"), "live: {live}");
    let rest = &live["summary:".len()..];
    let ms_part = rest.split(':').next().expect("ms segment");
    assert_eq!(ms_part.len(), 13, "ms must be 13 digits in {live}");
    assert!(
        ms_part.chars().all(|c| c.is_ascii_digit()),
        "ms must be all digits in {live}"
    );
}

#[test]
fn get_or_create_recovers_from_unique_race() {
    let (_tmp, cfg) = test_config();
    let pre_existing = Tree {
        id: "source:preexisting".into(),
        kind: TreeKind::Source,
        scope: "slack:#eng".into(),
        ask: None,
        root_id: None,
        max_level: 0,
        status: TreeStatus::Active,
        created_at: Utc::now(),
        last_sealed_at: None,
    };
    store::insert_tree(&cfg, &pre_existing).unwrap();

    let got = get_or_create_tree(&cfg, TreeKind::Source, "slack:#eng").unwrap();
    assert_eq!(got.id, "source:preexisting");

    let dup = Tree {
        id: "source:would-collide".into(),
        ..pre_existing.clone()
    };
    let err = store::insert_tree(&cfg, &dup).unwrap_err();
    assert!(
        is_unique_violation(&err),
        "expected UNIQUE violation, got: {err:#}"
    );
}
