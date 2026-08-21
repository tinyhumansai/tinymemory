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

#[tokio::test]
async fn drain_until_idle_is_noop_when_queue_is_empty() {
    let (_tmp, cfg) = test_config();
    drain_until_idle(&cfg).await.unwrap();
}
