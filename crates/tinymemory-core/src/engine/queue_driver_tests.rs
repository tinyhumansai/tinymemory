//! Tests for the surrounding module.

// Engine/queue types (`QueueDelegates`, `MemoryConfig`, the payload types,
// `async_trait`) come through `super::*` from the module-level imports.
use super::*;

fn sqlite_failure(code: rusqlite::ErrorCode, extended: i32, msg: &str) -> anyhow::Error {
    anyhow::Error::from(rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error {
            code,
            extended_code: extended,
        },
        Some(msg.into()),
    ))
}

#[test]
fn busy_backs_off_one_second_silently() {
    let a = classify_worker_error(&sqlite_failure(
        rusqlite::ErrorCode::DatabaseBusy,
        5,
        "database is locked",
    ));
    assert_eq!(a.backoff, Duration::from_secs(1));
    assert_eq!(a.report, WorkerReport::Silent);
    assert!(!a.mark_degraded && !a.recover_corrupt);
}

#[test]
fn transient_io_backs_off_thirty_seconds_silently() {
    let a = classify_worker_error(&sqlite_failure(
        rusqlite::ErrorCode::SystemIoFailure,
        1546,
        "disk I/O error",
    ));
    assert_eq!(a.backoff, Duration::from_secs(30));
    assert_eq!(a.report, WorkerReport::Silent);
}

#[test]
fn disk_full_backs_off_long_and_silent() {
    let a = classify_worker_error(&sqlite_failure(
        rusqlite::ErrorCode::DiskFull,
        13,
        "database or disk is full",
    ));
    assert_eq!(a.backoff, Duration::from_secs(300));
    assert_eq!(a.report, WorkerReport::Silent);
    assert!(!a.mark_degraded && !a.recover_corrupt);
}

#[test]
fn corrupt_drives_recovery_not_a_direct_page() {
    let a = classify_worker_error(&sqlite_failure(
        rusqlite::ErrorCode::DatabaseCorrupt,
        11,
        "database disk image is malformed",
    ));
    assert_eq!(a.backoff, Duration::from_secs(300));
    assert!(a.recover_corrupt, "corrupt must drive quarantine+rebuild");
    assert_eq!(
        a.report,
        WorkerReport::Silent,
        "recovery owns the report-once latch"
    );
    assert!(!a.mark_degraded);
}

#[test]
fn host_io_marks_degraded_and_reports_once() {
    let a = classify_worker_error(&anyhow::Error::from(std::io::Error::from_raw_os_error(5)));
    assert_eq!(a.backoff, Duration::from_secs(300));
    assert!(
        a.mark_degraded,
        "host-FS failure must flip storage-degraded"
    );
    assert_eq!(a.report, WorkerReport::Once("tree_jobs_worker_host_io"));
    assert!(!a.recover_corrupt);
}

#[test]
fn unknown_error_reports_every_time_short_backoff() {
    let a = classify_worker_error(&anyhow::anyhow!("upstream returned 500"));
    assert_eq!(a.backoff, Duration::from_secs(1));
    assert_eq!(a.report, WorkerReport::Always("tree_jobs_worker"));
    assert!(!a.mark_degraded && !a.recover_corrupt);
}

/// A minimal host-side [`QueueDelegates`] — proves the host can satisfy the
/// crate trait (all delegate arg/return types resolve) and that the host can
/// drive `queue::run_once` end-to-end. The real engine bridge lands with the
/// W4 delegates brick; this no-op stands in so the driver integration is
/// exercised now.
struct NoopDelegates;

#[async_trait]
impl QueueDelegates for NoopDelegates {
    async fn extract_chunk(
        &self,
        _config: &MemoryConfig,
        _chunk_id: &str,
    ) -> anyhow::Result<Option<ExtractDecision>> {
        Ok(None)
    }
    async fn append_node(
        &self,
        _config: &MemoryConfig,
        _node: &NodeRef,
        _target: &AppendTarget,
    ) -> anyhow::Result<Option<AppendDecision>> {
        Ok(None)
    }
    async fn seal_level(
        &self,
        _config: &MemoryConfig,
        _payload: &SealPayload,
    ) -> anyhow::Result<Option<SealPayload>> {
        Ok(None)
    }
    async fn list_stale_buffers(
        &self,
        _config: &MemoryConfig,
        _max_age_secs: i64,
    ) -> anyhow::Result<Vec<StaleBuffer>> {
        Ok(Vec::new())
    }
    async fn seal_document(
        &self,
        _config: &MemoryConfig,
        _payload: &SealDocumentPayload,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn reembed_batch(
        &self,
        _config: &MemoryConfig,
        _signature: &str,
    ) -> anyhow::Result<ReembedProgress> {
        Ok(ReembedProgress::Covered)
    }
    fn active_signature(&self, _config: &MemoryConfig) -> String {
        "provider=inert;model=none;dims=0".to_string()
    }
    fn has_uncovered_reembed_work(
        &self,
        _config: &MemoryConfig,
        _signature: &str,
    ) -> anyhow::Result<bool> {
        Ok(false)
    }
}

/// End-to-end smoke: the host can drive the crate queue. An empty workspace
/// queue → `run_once` claims nothing → `Ok(false)`, and initialising the
/// chunk DB along the way does not error.
#[tokio::test]
async fn host_drives_run_once_on_empty_queue() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mc = MemoryConfig::new(tmp.path());
    let processed = tinycortex::memory::queue::run_once(&mc, &NoopDelegates)
        .await
        .expect("run_once on empty queue");
    assert!(!processed, "empty queue processes nothing");
}

fn host_delegates_on_tempdir() -> (tempfile::TempDir, HostQueueDelegates) {
    crate::test_seams::init();
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut config = tinymemory_api::host::test_support::TestHostConfig::default();
    config.workspace_dir = tmp.path().to_path_buf();
    (
        tmp,
        HostQueueDelegates::new(std::sync::Arc::new(config) as std::sync::Arc<crate::Config>),
    )
}

/// The self-contained `HostQueueDelegates` methods bind to the real host
/// engine and run on a fresh workspace: the signature is non-empty, and an
/// empty workspace reports no uncovered re-embed work and no stale buffers.
#[tokio::test]
async fn host_delegates_selfcontained_methods_bind_and_run() {
    let (tmp, d) = host_delegates_on_tempdir();
    let mc = MemoryConfig::new(tmp.path());

    let sig = d.active_signature(&mc);
    assert!(!sig.is_empty(), "active signature should be non-empty");

    assert!(
        !d.has_uncovered_reembed_work(&mc, &sig)
            .expect("coverage probe"),
        "a fresh workspace has no uncovered re-embed work"
    );

    assert!(
        d.list_stale_buffers(&mc, 3600)
            .await
            .expect("list stale buffers")
            .is_empty(),
        "a fresh workspace has no stale buffers"
    );
}

/// `extract_chunk` / `append_node` are ported: on a missing chunk row they
/// are a no-op (`Ok(None)`), matching the legacy handlers' "row vanished
/// between enqueue and claim" path.
#[tokio::test]
async fn host_delegates_extract_and_append_missing_chunk_are_noop() {
    let (tmp, d) = host_delegates_on_tempdir();
    let mc = MemoryConfig::new(tmp.path());
    assert!(d
        .extract_chunk(&mc, "nonexistent")
        .await
        .expect("extract_chunk")
        .is_none());
    assert!(d
        .append_node(
            &mc,
            &NodeRef::Leaf {
                chunk_id: "nonexistent".into()
            },
            &AppendTarget::Source {
                source_id: "s".into()
            },
        )
        .await
        .expect("append_node")
        .is_none());
}

/// `reembed_batch` is ported: a job signature that differs from the config's
/// active embedding signature is superseded (`StaleSignature`), exactly as
/// the legacy `handle_reembed_backfill` finished a stale chain — and this
/// path returns before touching the worklist SQL.
#[tokio::test]
async fn host_delegates_reembed_batch_supersedes_stale_signature() {
    let (tmp, d) = host_delegates_on_tempdir();
    let mc = MemoryConfig::new(tmp.path());
    let progress = d
        .reembed_batch(&mc, "provider=stale-does-not-match;model=old;dims=1")
        .await
        .expect("reembed_batch stale path");
    assert!(matches!(progress, ReembedProgress::StaleSignature));
}

/// The ported seal methods handle empty/missing state without error: an
/// empty document version is a no-op, and sealing a level of a tree that
/// doesn't exist yields no parent to cascade.
#[tokio::test]
async fn host_delegates_seal_methods_handle_empty_state() {
    let (tmp, d) = host_delegates_on_tempdir();
    let mc = MemoryConfig::new(tmp.path());

    d.seal_document(
        &mc,
        &SealDocumentPayload {
            tree_scope: "notion:conn".into(),
            doc_id: "notion:conn:page".into(),
            version_ms: Some(1),
            chunk_ids: vec![],
        },
    )
    .await
    .expect("seal_document on an empty version is a no-op");

    let parent = d
        .seal_level(
            &mc,
            &SealPayload {
                tree_id: "nonexistent-tree".into(),
                level: 0,
                force_now_ms: None,
            },
        )
        .await
        .expect("seal_level on a missing tree");
    assert!(parent.is_none(), "missing tree has no parent to cascade");
}
