//! Tests for the surrounding module.

use super::*;
use crate::store::trees::types::TreeKind;
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
    let first = get_or_create_source_tree(&cfg, "slack:#eng").unwrap();
    let second = get_or_create_source_tree(&cfg, "slack:#eng").unwrap();
    assert_eq!(first.id, second.id);
    assert_eq!(first.kind, TreeKind::Source);
}

#[test]
fn different_scopes_yield_different_trees() {
    let (_tmp, cfg) = test_config();
    let a = get_or_create_source_tree(&cfg, "slack:#eng").unwrap();
    let b = get_or_create_source_tree(&cfg, "gmail:user@example.com").unwrap();
    assert_ne!(a.id, b.id);
}

#[test]
fn writes_source_file_on_create() {
    let (_tmp, cfg) = test_config();
    let tree = get_or_create_source_tree(&cfg, "gmail:user@example.com").unwrap();
    let path = file::source_file_path(&cfg, &tree.scope);
    assert!(path.exists(), "expected _source.md at {}", path.display());
}
