//! Tests for the surrounding module.

use super::*;
use crate::queue::store::{count_by_status, enqueue, get_job};
use crate::queue::types::{FlushStalePayload, JobKind, JobStatus, NewJob, ReembedBackfillPayload};
use crate::store::chunks::store::{
    tree_active_signature, upsert_chunks, upsert_staged_chunks_tx, with_connection,
};
use crate::store::chunks::types::{chunk_id, Chunk, Metadata, SourceKind, SourceRef};
use crate::store::content as content_store;
use chrono::{TimeZone, Utc};
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

/// Raw `rusqlite::Error::SqliteFailure` with the `DatabaseBusy` code
/// is what surfaces when the `busy_timeout` is exhausted on a write.
#[test]
fn is_sqlite_busy_matches_database_busy_code() {
    let raw = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error {
            code: rusqlite::ErrorCode::DatabaseBusy,
            extended_code: 5, // SQLITE_BUSY
        },
        Some("database is locked".into()),
    );
    let err = anyhow::Error::from(raw);
    assert!(is_sqlite_busy(&err));
}

/// `SQLITE_LOCKED` is the per-table flavour (e.g. shared cache); same
/// classification — transient, retry.
#[test]
fn is_sqlite_busy_matches_database_locked_code() {
    let raw = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error {
            code: rusqlite::ErrorCode::DatabaseLocked,
            extended_code: 6, // SQLITE_LOCKED
        },
        Some("database table is locked".into()),
    );
    let err = anyhow::Error::from(raw);
    assert!(is_sqlite_busy(&err));
}

/// When the rusqlite error is buried under `.context(...)` layers
/// (as happens when `with_connection` wraps the closure result),
/// the downcast still finds it. Regression guard: don't rely on
/// matching the top-level error type.
#[test]
fn is_sqlite_busy_matches_through_context_layers() {
    let raw = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error {
            code: rusqlite::ErrorCode::DatabaseBusy,
            extended_code: 5,
        },
        Some("database is locked".into()),
    );
    let wrapped: anyhow::Error = anyhow::Error::from(raw)
        .context("Failed to claim next mem_tree_jobs row")
        .context("with_connection closure failed");
    assert!(is_sqlite_busy(&wrapped));
}

/// Fallback text-match: if the rusqlite error has been re-rendered
/// into a plain `anyhow!` (no downcast available), the "database is
/// locked" phrase still triggers the busy classification.
#[test]
fn is_sqlite_busy_text_fallback() {
    let err = anyhow::anyhow!("Failed to claim next mem_tree_jobs row: database is locked");
    assert!(is_sqlite_busy(&err));
}

/// Non-busy SQLite failures (e.g. UNIQUE constraint) must NOT be
/// reclassified — those are real bugs worth reporting.
#[test]
fn is_sqlite_busy_does_not_match_constraint_violation() {
    let raw = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error {
            code: rusqlite::ErrorCode::ConstraintViolation,
            extended_code: 19,
        },
        Some("UNIQUE constraint failed: mem_tree_jobs.dedupe_key".into()),
    );
    let err = anyhow::Error::from(raw);
    assert!(!is_sqlite_busy(&err));
}

/// Generic non-SQLite errors must not be reclassified as busy.
#[test]
fn is_sqlite_busy_does_not_match_unrelated_errors() {
    let err = anyhow::anyhow!("upstream returned 500: internal server error");
    assert!(!is_sqlite_busy(&err));
}

// ── is_sqlite_io_transient tests (#2206) ─────────────────────────────

/// SQLITE_IOERR_TRUNCATE (extended code 1546) must be classified as
/// transient so the worker backs off without hitting Sentry.
#[test]
fn is_sqlite_io_transient_matches_ioerr_truncate() {
    let raw = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error {
            code: rusqlite::ErrorCode::SystemIoFailure,
            extended_code: 1546, // SQLITE_IOERR_TRUNCATE
        },
        Some("disk I/O error".into()),
    );
    assert!(is_sqlite_io_transient(&anyhow::Error::from(raw)));
}

/// The WAL `-shm` family must classify as transient via the NUMERIC arm
/// (the message deliberately avoids the text-fallback phrases). 4618
/// SHMOPEN is the macOS cold-start failure; 4874 is SHMSIZE; 5386 is the
/// real SHMMAP; 8714 is IN_PAGE.
#[test]
fn is_sqlite_io_transient_matches_shm_family() {
    for ext in [4618, 4874, 5386, 8714] {
        let raw = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::SystemIoFailure,
                extended_code: ext,
            },
            Some("sqlite extended io failure".into()),
        );
        assert!(
            is_sqlite_io_transient(&anyhow::Error::from(raw)),
            "extended_code {ext} must classify as transient (numeric arm)"
        );
    }
}

/// SQLITE_CANTOPEN (code CannotOpen, extended code 14) must be
/// classified as transient — temporary inability to open the file.
#[test]
fn is_sqlite_io_transient_matches_cantopen() {
    let raw = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error {
            code: rusqlite::ErrorCode::CannotOpen,
            extended_code: 14, // SQLITE_CANTOPEN
        },
        Some("unable to open database file".into()),
    );
    assert!(is_sqlite_io_transient(&anyhow::Error::from(raw)));
}

/// The circuit breaker error message produced by `get_or_init_connection`
/// must be classified as transient via the text fallback.
#[test]
fn is_sqlite_io_transient_text_fallback() {
    let err = anyhow::anyhow!("memory_tree_db circuit breaker open: too many init failures");
    assert!(is_sqlite_io_transient(&err));
}

/// UNIQUE constraint violation must NOT be reclassified as a transient
/// I/O error — those are genuine bugs.
#[test]
fn is_sqlite_io_transient_negative_constraint_violation() {
    let raw = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error {
            code: rusqlite::ErrorCode::ConstraintViolation,
            extended_code: 19,
        },
        Some("UNIQUE constraint failed: mem_tree_jobs.dedupe_key".into()),
    );
    assert!(!is_sqlite_io_transient(&anyhow::Error::from(raw)));
}

// ── is_sqlite_disk_full tests (#3909 / Sentry TAURI-RUST-4R8) ─────────

/// `SQLITE_FULL` (primary code `DiskFull`, extended 13) is the disk-full
/// signal from `claim_next`; it must classify so the worker backs off
/// long instead of paging Sentry every second.
#[test]
fn is_sqlite_disk_full_matches_disk_full_code() {
    let raw = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error {
            code: rusqlite::ErrorCode::DiskFull,
            extended_code: 13,
        },
        Some("database or disk is full".into()),
    );
    assert!(is_sqlite_disk_full(&anyhow::Error::from(raw)));
}

/// The rusqlite error sits a few `.context()` layers deep when it bubbles
/// out of `claim_next` → `with_connection`; the downcast must still find
/// the `DiskFull` code.
#[test]
fn is_sqlite_disk_full_matches_through_context_layers() {
    let raw = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error {
            code: rusqlite::ErrorCode::DiskFull,
            extended_code: 13,
        },
        Some("database or disk is full".into()),
    );
    let wrapped = anyhow::Error::from(raw)
        .context("Failed to claim next mem_tree_jobs row")
        .context("with_connection closure failed");
    assert!(is_sqlite_disk_full(&wrapped));
}

/// Text fallback: the exact flattened Sentry string (TAURI-RUST-4R8) is
/// classified even when no rusqlite error is available to downcast (the
/// canonical phrase is mid-string, not a suffix).
#[test]
fn is_sqlite_disk_full_text_fallback() {
    let err = anyhow::anyhow!(
        "Failed to claim next mem_tree_jobs row: database or disk is full: \
         Error code 13: Insertion failed because database is full"
    );
    assert!(is_sqlite_disk_full(&err));
}

/// Busy/locked, constraint violations, and unrelated errors must NOT be
/// swallowed as disk-full — those still warrant their own handling /
/// Sentry escalation.
#[test]
fn is_sqlite_disk_full_does_not_match_other_errors() {
    let busy = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error {
            code: rusqlite::ErrorCode::DatabaseBusy,
            extended_code: 5,
        },
        Some("database is locked".into()),
    );
    assert!(!is_sqlite_disk_full(&anyhow::Error::from(busy)));

    let constraint = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error {
            code: rusqlite::ErrorCode::ConstraintViolation,
            extended_code: 19,
        },
        Some("UNIQUE constraint failed: mem_tree_jobs.dedupe_key".into()),
    );
    assert!(!is_sqlite_disk_full(&anyhow::Error::from(constraint)));

    assert!(!is_sqlite_disk_full(&anyhow::anyhow!(
        "upstream returned 500: internal server error"
    )));
}

// ── is_sqlite_corrupt tests (#4048 / Sentry TAURI-RUST-E93) ──────────────

/// `SQLITE_CORRUPT` (primary code `DatabaseCorrupt`, code 11) is the
/// malformed-image signal from `claim_next`; it must classify so the worker
/// quarantines + rebuilds instead of paging Sentry every second.
#[test]
fn is_sqlite_corrupt_matches_database_corrupt_code() {
    let raw = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error {
            code: rusqlite::ErrorCode::DatabaseCorrupt,
            extended_code: 11,
        },
        Some("database disk image is malformed".into()),
    );
    assert!(is_sqlite_corrupt(&anyhow::Error::from(raw)));
}

/// `SQLITE_NOTADB` (code `NotADatabase`, 26 — header unreadable) is the
/// same broad on-disk-damage class and must classify too.
#[test]
fn is_sqlite_corrupt_matches_not_a_database_code() {
    let raw = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error {
            code: rusqlite::ErrorCode::NotADatabase,
            extended_code: 26,
        },
        Some("file is not a database".into()),
    );
    assert!(is_sqlite_corrupt(&anyhow::Error::from(raw)));
}

/// The rusqlite error sits a few `.context()` layers deep when it bubbles
/// out of `claim_next` → `with_connection`; the downcast must still find
/// the `DatabaseCorrupt` code.
#[test]
fn is_sqlite_corrupt_matches_through_context_layers() {
    let raw = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error {
            code: rusqlite::ErrorCode::DatabaseCorrupt,
            extended_code: 11,
        },
        Some("database disk image is malformed".into()),
    );
    let wrapped = anyhow::Error::from(raw)
        .context("Failed to claim next mem_tree_jobs row")
        .context("with_connection closure failed");
    assert!(is_sqlite_corrupt(&wrapped));
}

/// Text fallback: the exact flattened Sentry string (TAURI-RUST-E93) must
/// classify even when no rusqlite error is available to downcast.
#[test]
fn is_sqlite_corrupt_text_fallback() {
    let err = anyhow::anyhow!(
        "Failed to claim next mem_tree_jobs row: database disk image is malformed: \
         Error code 11: The database disk image is malformed"
    );
    assert!(is_sqlite_corrupt(&err));
}

/// Busy/locked, disk-full, constraint violations, and unrelated errors must
/// NOT be swallowed as corruption — quarantining on those would destroy a
/// perfectly good DB.
#[test]
fn is_sqlite_corrupt_does_not_match_other_errors() {
    let busy = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error {
            code: rusqlite::ErrorCode::DatabaseBusy,
            extended_code: 5,
        },
        Some("database is locked".into()),
    );
    assert!(!is_sqlite_corrupt(&anyhow::Error::from(busy)));

    let disk_full = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error {
            code: rusqlite::ErrorCode::DiskFull,
            extended_code: 13,
        },
        Some("database or disk is full".into()),
    );
    assert!(!is_sqlite_corrupt(&anyhow::Error::from(disk_full)));

    let constraint = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error {
            code: rusqlite::ErrorCode::ConstraintViolation,
            extended_code: 19,
        },
        Some("UNIQUE constraint failed: mem_tree_jobs.dedupe_key".into()),
    );
    assert!(!is_sqlite_corrupt(&anyhow::Error::from(constraint)));

    assert!(!is_sqlite_corrupt(&anyhow::anyhow!(
        "upstream returned 500: internal server error"
    )));
}

// ── is_host_io_error tests (CORE-RUST-19J) ───────────────────────────────

/// EIO (`os error 5`) is the CORE-RUST-19J signal: `create_dir_all` on a
/// failing/disconnected SD card. Must classify so the worker surfaces a
/// `StorageUnavailable` degradation + backs off long instead of paging
/// Sentry every second.
#[test]
fn is_host_io_error_matches_eio() {
    let err = anyhow::Error::from(std::io::Error::from_raw_os_error(5));
    assert!(is_host_io_error(&err));
}

/// ENOSPC (28, filesystem-level out-of-space on `create_dir`) and EROFS (30,
/// kernel-remounted-read-only — the common next stage of a dying SD card)
/// are the same persistent, user-only-fixable host condition.
#[test]
fn is_host_io_error_matches_enospc_and_erofs() {
    for code in [28, 30] {
        let err = anyhow::Error::from(std::io::Error::from_raw_os_error(code));
        assert!(
            is_host_io_error(&err),
            "os error {code} must classify as host I/O"
        );
    }
}

/// The production shape: the `io::Error` bubbles out of `open_and_init`
/// wrapped in `.with_context("Failed to create memory_tree dir: …")` then
/// the `with_connection` layer. The downcast must still find it through the
/// anyhow context chain (regression guard: don't rely on the top-level type).
#[test]
fn is_host_io_error_matches_through_context_layers() {
    let wrapped = anyhow::Error::from(std::io::Error::from_raw_os_error(5))
        .context(
            "Failed to create memory_tree dir: /home/x/.openhuman-workspace/workspace/memory_tree",
        )
        .context("with_connection closure failed");
    assert!(is_host_io_error(&wrapped));
}

/// Text fallback: when no `io::Error` is available to downcast (flattened to
/// a plain `anyhow!` string), the exact flattened CORE-RUST-19J message is
/// still classified via the os-error-number anchor.
#[test]
fn is_host_io_error_text_fallback() {
    let err = anyhow::anyhow!(
        "Failed to create memory_tree dir: /home/x/.openhuman-workspace/workspace/memory_tree: \
         Input/output error (os error 5)"
    );
    assert!(is_host_io_error(&err));
}

/// Permission-denied (13), not-found (2), a SQLite disk-full failure (its
/// own arm), and unrelated errors must NOT be swallowed as host I/O — those
/// are real bugs / handled elsewhere and must keep reporting.
#[test]
fn is_host_io_error_does_not_match_other_errors() {
    // EACCES — a genuine permission bug, not failing hardware.
    assert!(!is_host_io_error(&anyhow::Error::from(
        std::io::Error::from_raw_os_error(13)
    )));
    // ENOENT.
    assert!(!is_host_io_error(&anyhow::Error::from(
        std::io::Error::from_raw_os_error(2)
    )));
    // SQLITE_FULL stays in is_sqlite_disk_full's arm, not here.
    let disk_full = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error {
            code: rusqlite::ErrorCode::DiskFull,
            extended_code: 13,
        },
        Some("database or disk is full".into()),
    );
    assert!(!is_host_io_error(&anyhow::Error::from(disk_full)));
    // Unrelated.
    assert!(!is_host_io_error(&anyhow::anyhow!(
        "upstream returned 500: internal server error"
    )));
}

/// The worker's corruption arm must quarantine a malformed image and rebuild
/// an empty, queryable schema so the queue resumes — exercising the
/// report-once + recover path the live loop runs.
#[tokio::test]
async fn recover_corrupt_db_once_quarantines_and_rebuilds() {
    let (_tmp, cfg) = test_config();
    // Lay down a malformed `chunks.db` (garbage header) at the canonical path.
    let db_path = cfg.workspace_dir.join("memory_tree").join("chunks.db");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    std::fs::write(&db_path, b"not a sqlite database, just garbage bytes").unwrap();

    let err =
        anyhow::anyhow!("Failed to claim next mem_tree_jobs row: database disk image is malformed");
    recover_corrupt_db_once(0, &err, &cfg);

    // Corrupt bytes are preserved alongside (never silently dropped) ...
    let quarantined = std::fs::read_dir(db_path.parent().unwrap())
        .unwrap()
        .filter_map(|e| e.ok())
        .any(|e| {
            e.file_name()
                .to_string_lossy()
                .contains("chunks.db.corrupt-")
        });
    assert!(
        quarantined,
        "corrupt image must be quarantined, not deleted"
    );

    // ... and the rebuilt queue DB is healthy and empty.
    let processed = run_once(&cfg).await.unwrap();
    assert!(!processed, "rebuilt queue starts empty");
}

#[tokio::test]
async fn wake_workers_is_noop_before_start() {
    wake_workers();
}

#[tokio::test]
async fn run_once_returns_false_when_queue_is_empty() {
    let (_tmp, cfg) = test_config();
    let processed = run_once(&cfg).await.unwrap();
    assert!(!processed);
}

#[tokio::test]
async fn run_once_claims_and_completes_a_flush_stale_job() {
    let (_tmp, cfg) = test_config();
    let new_job = NewJob::flush_stale(&FlushStalePayload::default(), "2026-05-24", 3).unwrap();
    let id = enqueue(&cfg, &new_job).unwrap().expect("enqueue job");

    let processed = run_once(&cfg).await.unwrap();
    assert!(processed);

    let job = get_job(&cfg, &id).unwrap().expect("job should still exist");
    assert_eq!(job.kind.as_str(), "flush_stale");
    assert_eq!(job.status, JobStatus::Done);
    assert_eq!(count_by_status(&cfg, JobStatus::Done).unwrap(), 1);
    assert!(job.completed_at_ms.is_some());
    assert!(job.locked_until_ms.is_none());
}

#[tokio::test]
async fn run_once_reschedules_reembed_backfill_jobs_that_defer() {
    let (_tmp, mut cfg) = test_config();
    // Deliberate "none" opt-out → InertEmbedder (zero vectors, no network)
    // so the backfill has work and Defers; this test pins the worker's
    // defer-reschedule path, not embed quality.
    cfg.embeddings_provider = Some("none".to_string());
    let ts = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let chunk = Chunk {
        id: chunk_id(SourceKind::Chat, "slack:#eng", 0, "reembed-worker-seed"),
        content: "memory content about the phoenix migration project".into(),
        metadata: Metadata {
            source_kind: SourceKind::Chat,
            source_id: "slack:#eng".into(),
            owner: "alice".into(),
            timestamp: ts,
            time_range: (ts, ts),
            tags: vec![],
            source_ref: Some(SourceRef::new("slack://x")),
            path_scope: None,
        },
        token_count: 12,
        seq_in_source: 0,
        created_at: ts,
        partial_message: false,
    };
    upsert_chunks(&cfg, std::slice::from_ref(&chunk)).unwrap();
    let content_root = cfg.memory_tree_content_root();
    std::fs::create_dir_all(&content_root).unwrap();
    let staged = content_store::stage_chunks(&content_root, &[chunk]).unwrap();
    with_connection(&cfg, |conn| {
        let tx = conn.unchecked_transaction()?;
        upsert_staged_chunks_tx(&tx, &staged)?;
        tx.commit()?;
        Ok(())
    })
    .unwrap();

    let signature = tree_active_signature(&cfg);
    let new_job = NewJob::reembed_backfill(&ReembedBackfillPayload {
        signature: signature.clone(),
    })
    .unwrap();
    let id = enqueue(&cfg, &new_job)
        .unwrap()
        .expect("enqueue backfill job");

    // The TinyCortex LLM gate is process-global, so a parallel libtest can
    // briefly own its single permit. In that case `run_once` legitimately
    // defers this row for 50 ms with `llm concurrency gate busy` before the
    // re-embed handler is reached. Retry that transient gate deferral so
    // this test continues to pin the handler's own defer/reschedule path.
    let mut job = None;
    for _ in 0..20 {
        let processed = run_once(&cfg).await.unwrap();
        assert!(processed);
        let current = get_job(&cfg, &id).unwrap().expect("job should still exist");
        if current
            .last_error
            .as_deref()
            .is_some_and(|reason| reason.contains("re-embed backfill"))
        {
            job = Some(current);
            break;
        }
        assert_eq!(
            current.last_error.as_deref(),
            Some("llm concurrency gate busy"),
            "unexpected defer reason before re-embed handler"
        );
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
    let job = job.expect("re-embed handler should run after transient gate contention");
    assert_eq!(job.kind, JobKind::ReembedBackfill);
    assert_eq!(job.status, JobStatus::Ready);
    assert_eq!(
        job.attempts, 0,
        "defer should revert the claim attempt bump"
    );
    assert!(job.started_at_ms.is_none());
    assert!(job.locked_until_ms.is_none());
    assert!(job.completed_at_ms.is_none());
    assert!(
        job.available_at_ms > Utc::now().timestamp_millis(),
        "deferred job should be rescheduled into the future"
    );
    let defer_reason = job.last_error.as_deref().unwrap_or("");
    assert!(
        defer_reason.contains("re-embed backfill")
            || defer_reason.contains("llm concurrency gate busy"),
        "defer reason should identify the backfill or the shared gate: {defer_reason:?}"
    );
    assert_eq!(count_by_status(&cfg, JobStatus::Ready).unwrap(), 1);
}
