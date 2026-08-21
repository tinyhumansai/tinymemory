//! Tests for stale-buffer and force-flush host adapters.

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

#[tokio::test]
async fn empty_flushes_are_noops_and_missing_tree_is_named() {
    let (_tmp, config) = config();
    assert_eq!(
        flush_stale_buffers(&config, Duration::zero(), &LabelStrategy::Empty)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        flush_stale_buffers_default(&config, &LabelStrategy::UnionFromChildren)
            .await
            .unwrap(),
        0
    );
    let error = force_flush_tree(&config, "missing", None, &LabelStrategy::Empty)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("no tree with id missing"));
}
