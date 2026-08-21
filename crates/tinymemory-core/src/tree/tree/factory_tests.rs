//! Tests for the surrounding module.

use super::*;
use tempfile::TempDir;
use tinymemory_api::host::test_support::TestHostConfig;

fn config() -> (TempDir, TestHostConfig) {
    crate::test_seams::init();
    let tmp = TempDir::new().unwrap();
    let mut config = TestHostConfig::default();
    config.workspace_dir = tmp.path().join("workspace");
    (tmp, config)
}

#[test]
fn source_factory_uses_source_kind_and_full_scope() {
    let f = TreeFactory::source("slack:#eng");
    assert_eq!(f.kind(), TreeKind::Source);
    assert_eq!(f.scope(), "slack:#eng");
    assert_eq!(f.summary_tree_kind(), SummaryTreeKind::Source);
}

#[test]
fn global_uses_global_scope_and_kind() {
    let global = TreeFactory::global();
    assert_eq!(global.kind(), TreeKind::Global);
    assert_eq!(global.scope(), GLOBAL_SCOPE);
}

#[test]
fn source_scope_slug_preserves_non_gmail_prefix() {
    let f = TreeFactory::source("slack:#eng");
    assert_eq!(f.scope_slug(), "slack-eng");
}

#[test]
fn source_scope_slug_strips_gmail_prefix_only() {
    let f = TreeFactory::source("gmail:alice@example.com|bob@example.com");
    assert_eq!(f.scope_slug(), "alice-example-com-bob-example-com");
}

#[test]
fn topic_scope_slug_keeps_canonical_prefix() {
    let f = TreeFactory::topic("email:alice@example.com");
    assert_eq!(f.scope_slug(), "email-alice-example-com");
    assert_eq!(f.summary_tree_kind(), SummaryTreeKind::Topic);
}

#[test]
fn from_tree_profiles_and_summary_kinds_match_every_factory() {
    let (_tmp, config) = config();
    let source = TreeFactory::source("source");
    let topic = TreeFactory::topic("topic");
    let global = TreeFactory::global();
    assert_ne!(source.profile(), topic.profile());
    assert_ne!(topic.profile(), global.profile());
    assert_eq!(global.summary_tree_kind(), SummaryTreeKind::Global);
    assert!(matches!(
        source.label_strategy(&config),
        LabelStrategy::ExtractFromContent(_)
    ));
    assert!(matches!(
        topic.label_strategy(&config),
        LabelStrategy::Empty
    ));
    assert!(matches!(
        global.label_strategy(&config),
        LabelStrategy::Empty
    ));

    let stored = source.get_or_create(&config).unwrap();
    let reconstructed = TreeFactory::from_tree(&stored);
    assert_eq!(reconstructed.kind(), stored.kind);
    assert_eq!(reconstructed.scope(), stored.scope);
    assert_eq!(reconstructed.profile(), source.profile());
}

#[test]
fn get_or_create_and_archive_update_persisted_tree() {
    let (_tmp, config) = config();
    let factory = TreeFactory::source("source-to-archive");
    let tree = factory.get_or_create(&config).unwrap();
    factory.archive(&config).unwrap();
    let archived = crate::store::trees::store::get_tree(&config, &tree.id)
        .unwrap()
        .unwrap();
    assert_eq!(archived.status, crate::store::trees::TreeStatus::Archived);
}
