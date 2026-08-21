//! Tests for the surrounding module.

use super::*;
use crate::queue::store::{claim_next, count_by_status, DEFAULT_LOCK_DURATION_MS};
use crate::queue::types::{FlushStalePayload, JobKind, JobStatus};
use tempfile::TempDir;

fn test_config() -> (TempDir, TestHostConfig) {
    crate::test_seams::init();
    let tmp = TempDir::new().unwrap();
    let mut cfg = TestHostConfig::default();
    cfg.workspace_dir = tmp.path().to_path_buf();
    cfg.memory_tree.embedding_endpoint = None;
    cfg.memory_tree.embedding_model = None;
    cfg.memory_tree.embedding_strict = false;
    (tmp, cfg)
}

#[test]
fn enqueue_flush_stale_enqueues_at_most_one_job_per_current_block() {
    let (_tmp, cfg) = test_config();
    enqueue_flush_stale(&cfg);
    enqueue_flush_stale(&cfg);

    assert_eq!(
        count_by_status(&cfg, JobStatus::Ready).unwrap(),
        1,
        "second enqueue in same 3h block should be dedupe-suppressed"
    );

    let claimed = claim_next(&cfg, DEFAULT_LOCK_DURATION_MS).unwrap().unwrap();
    assert_eq!(claimed.kind, JobKind::FlushStale);
    let payload: FlushStalePayload = serde_json::from_str(&claimed.payload_json).unwrap();
    assert_eq!(payload.max_age_secs, None);
}
