//! Tests for the surrounding module.

use super::*;
use crate::tree::health::{FailureCode, PipelineFailure};
use tempfile::TempDir;
use tinymemory_api::host::test_support::TestHostConfig;

fn test_config() -> (TempDir, TestHostConfig) {
    crate::test_seams::init();
    let tmp = TempDir::new().unwrap();
    let mut cfg = TestHostConfig::default();
    cfg.workspace_dir = tmp.path().to_path_buf();
    (tmp, cfg)
}

/// Nothing parked ⇒ nothing to un-park. Must not error, and must not wake
/// the worker pool for no reason.
#[test]
fn requeue_after_provider_change_is_zero_on_an_empty_queue() {
    let (_tmp, cfg) = test_config();
    assert_eq!(requeue_failed_after_provider_change(&cfg).unwrap(), 0);
}

/// The #5324 case: jobs parked as `budget_exhausted` (unrecoverable, so
/// the periodic transient-requeue deliberately leaves them alone) must be
/// flipped back to `ready` when the user changes their embedding provider.
#[tokio::test]
async fn requeue_after_provider_change_unparks_budget_exhausted_jobs() {
    use crate::queue::store;
    use crate::queue::types::{FlushStalePayload, JobStatus, NewJob};

    let (_tmp, cfg) = test_config();
    let new_job = NewJob::flush_stale(&FlushStalePayload::default(), "2026-08-05", 3).unwrap();
    let id = store::enqueue(&cfg, &new_job)
        .unwrap()
        .expect("enqueue job");
    let job = store::get_job(&cfg, &id).unwrap().expect("job exists");

    // Park it exactly the way an exhausted managed budget does.
    let failure = PipelineFailure::new(FailureCode::BudgetExhausted);
    assert!(
        failure.is_unrecoverable(),
        "precondition: parked, not retried"
    );
    store::mark_failed_typed(&cfg, &job, "Insufficient budget", Some(&failure)).unwrap();
    assert_eq!(
        store::count_by_status(&cfg, JobStatus::Failed).unwrap(),
        1,
        "precondition: the job is parked"
    );
    assert_eq!(
        store::count_failed_unrecoverable(&cfg).unwrap(),
        1,
        "precondition: parked as unrecoverable, so periodic retry skips it"
    );

    assert_eq!(requeue_failed_after_provider_change(&cfg).unwrap(), 1);

    assert_eq!(
        store::count_by_status(&cfg, JobStatus::Ready).unwrap(),
        1,
        "the job must be retryable again after the user fixes their provider"
    );
    assert_eq!(store::count_by_status(&cfg, JobStatus::Failed).unwrap(), 0);
}

/// Idempotent: calling it again once the queue is drained of failures is a
/// no-op, so re-saving settings repeatedly cannot spam the worker pool.
#[tokio::test]
async fn requeue_after_provider_change_is_idempotent() {
    use crate::queue::store;
    use crate::queue::types::{FlushStalePayload, NewJob};

    let (_tmp, cfg) = test_config();
    let new_job = NewJob::flush_stale(&FlushStalePayload::default(), "2026-08-05", 3).unwrap();
    let id = store::enqueue(&cfg, &new_job)
        .unwrap()
        .expect("enqueue job");
    let job = store::get_job(&cfg, &id).unwrap().expect("job exists");
    store::mark_failed_typed(
        &cfg,
        &job,
        "Insufficient budget",
        Some(&PipelineFailure::new(FailureCode::BudgetExhausted)),
    )
    .unwrap();

    assert_eq!(requeue_failed_after_provider_change(&cfg).unwrap(), 1);
    assert_eq!(requeue_failed_after_provider_change(&cfg).unwrap(), 0);
}

/// CodeRabbit (#5324): a store failure must SURFACE as `Err`, not collapse
/// into a `0` that reads identically to "nothing to requeue" and makes a
/// still-parked queue look remediated.
#[test]
fn requeue_after_provider_change_surfaces_store_errors() {
    let tmp = TempDir::new().unwrap();
    // Point workspace_dir at a regular file, so the queue DB underneath it
    // cannot be opened (ENOTDIR). The failure must propagate to the caller.
    let as_file = tmp.path().join("workspace-is-a-file");
    std::fs::write(&as_file, b"not a directory").unwrap();
    let mut cfg = TestHostConfig::default();
    cfg.workspace_dir = as_file;

    let out = requeue_failed_after_provider_change(&cfg);
    assert!(
        out.is_err(),
        "a store failure must surface as Err, not a misleading Ok(0): {out:?}"
    );
}
