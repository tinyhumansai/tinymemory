//! Tests for the surrounding module.

use super::*;
use crate::store::trees::types::{TreeKind, TreeStatus};
use chrono::TimeZone;
use tempfile::TempDir;

fn cfg() -> (TempDir, TestHostConfig) {
    crate::test_seams::init();
    let tmp = TempDir::new().unwrap();
    let mut cfg = TestHostConfig::default();
    cfg.workspace_dir = tmp.path().to_path_buf();
    (tmp, cfg)
}

fn sample_tree(scope: &str) -> Tree {
    Tree {
        id: "source:abc".into(),
        kind: TreeKind::Source,
        scope: scope.into(),
        ask: None,
        root_id: None,
        max_level: 0,
        status: TreeStatus::Active,
        created_at: Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
        last_sealed_at: None,
    }
}

#[test]
fn writes_frontmatter_only_file() {
    let (_tmp, cfg) = cfg();
    let tree = sample_tree("gmail:acct-1");
    let path = write_source_file(&cfg, &tree).unwrap();
    assert!(
        path.ends_with("raw/gmail-acct-1/_source.md"),
        "{}",
        path.display()
    );
    let body = fs::read_to_string(&path).unwrap();
    // Bracketed by frontmatter delimiters with no body after.
    assert!(body.starts_with("---\n"));
    assert!(body.trim_end().ends_with("---"));
    assert!(body.contains("tree_id: source:abc") || body.contains("tree_id: \"source:abc\""));
    assert!(body.contains("kind: source"));
    assert!(body.contains("status: active"));
    assert!(body.contains("last_sealed_at: null"));
}

#[test]
fn rewrite_is_byte_identical_for_same_state() {
    let (_tmp, cfg) = cfg();
    let tree = sample_tree("slack:#eng");
    let path = write_source_file(&cfg, &tree).unwrap();
    let first = fs::read(&path).unwrap();
    write_source_file(&cfg, &tree).unwrap();
    let second = fs::read(&path).unwrap();
    assert_eq!(first, second);
}

#[test]
fn updates_last_sealed_at_on_rewrite() {
    let (_tmp, cfg) = cfg();
    let mut tree = sample_tree("slack:#eng");
    write_source_file(&cfg, &tree).unwrap();
    tree.last_sealed_at = Some(Utc.timestamp_millis_opt(1_700_000_500_000).unwrap());
    tree.max_level = 3;
    let path = write_source_file(&cfg, &tree).unwrap();
    let body = fs::read_to_string(&path).unwrap();
    assert!(body.contains("max_level: 3"));
    assert!(body.contains("last_sealed_at: 2023-11-14"), "{body}");
}

#[test]
fn quotes_scalars_with_colons() {
    let (_tmp, cfg) = cfg();
    let tree = sample_tree("gmail:user@example.com");
    let path = write_source_file(&cfg, &tree).unwrap();
    let body = fs::read_to_string(&path).unwrap();
    // scope contains ':' → must be quoted to round-trip through YAML.
    assert!(body.contains("scope: \"gmail:user@example.com\""), "{body}");
}
