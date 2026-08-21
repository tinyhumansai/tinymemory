//! Worker pool: drives the crate queue engine (W4 flip). Each `run_once`
//! delegates claim → dispatch → settle to `crate::engine::backend::queue::run_once`
//! via [`crate::engine::HostQueueDelegates`]; the legacy host
//! `handlers` engine that used to own dispatch was deleted at the flip.
//!
//! Concurrency control for LLM-bound work is delegated to
//! [`crate::scheduler_gate`] — its global single-slot
//! semaphore (`LlmPermit`) is the one source of truth across this
//! worker, voice cleanup, autocomplete, triage, and reflection. The
//! worker itself just calls `wait_for_capacity()`; non-LLM jobs
//! (`AppendBuffer`, `FlushStale`) run without acquiring a permit.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::Result;
use tokio::sync::Notify;

// Test-only: the tests below build a `TestHostConfig` and call
// `MemoryHostConfig` methods on it directly. Production code in this module
// goes through `crate::Config`, so neither name is needed in a non-test build.
#[cfg(test)]
use tinymemory_api::host::{test_support::TestHostConfig, MemoryHostConfig};

use crate::Config;
// W4 flip: `run_once` now delegates claim/dispatch/settle to the crate, so the
// legacy `handlers`, per-job settle (`mark_*`/`scrub_for_log`), and claim
// helpers are gone from this module. Only startup lock recovery + the loop's
// storage-degraded signalling remain host.
use crate::queue::store::{recover_stale_locks, release_running_locks};
use crate::tree::health::{clear_storage_degraded, mark_storage_degraded, FailureCode};

/// Number of concurrent job-worker tasks. Each worker claims one job
/// at a time via `claim_next` (atomic UPDATE under SQLite WAL with
/// `locked_until_ms` + status='running'), so multiple workers
/// parallelize independent jobs without double-claim risk.
///
/// On cloud backends, LLM-bound jobs drop the global LLM permit
/// after claim (see `run_once`) so all 4 workers can run cloud
/// extract/summarise calls in parallel.
///
/// On local backends, the single global LLM slot still serialises
/// Ollama calls for laptop-RAM safety. Note that `wait_for_capacity`
/// is acquired **before** `claim_next`, so non-LLM jobs (AppendBuffer,
/// FlushStale, TopicRoute) also block on the gate when an LLM job
/// holds the permit — they only run in parallel with each other while
/// no LLM job is in flight. Bumping `WORKER_COUNT` therefore helps
/// throughput most when local LLM calls are sparse.
const WORKER_COUNT: usize = 4;
const POLL_INTERVAL: Duration = Duration::from_secs(5);

static WORKER_NOTIFY: OnceLock<Arc<Notify>> = OnceLock::new();
static STARTED: std::sync::Once = std::sync::Once::new();

/// Process-wide latch so a `SQLITE_CORRUPT` flood is reported to Sentry **once**,
/// not on every poll from every worker. Set on the first malformed-image
/// detection; cleared after a recovery attempt settles (quarantine+rebuild or a
/// quick_check that now passes) so a genuinely-new, later corruption can still
/// page once. Without this, 4 workers polling a wedged DB re-page ~1/sec
/// (Sentry TAURI-RUST-E93: 1,633 events in ~17 min from one host).
static CORRUPT_REPORTED: AtomicBool = AtomicBool::new(false);

/// Process-wide latch so a persistent host-filesystem failure (EIO/ENOSPC/
/// EROFS on the memory_tree dir/DB path) is reported to Sentry **once**, not on
/// every poll from every worker. Set on the first host-I/O failure; cleared on
/// the next successful claim (storage recovered) so a genuinely-new, later
/// failure can still page once. Without this, 4 workers re-polling a dead disk
/// flood the dashboard (Sentry CORE-RUST-19J: ~10k events in ~50 min from one
/// Raspberry Pi with a failing SD card).
static STORAGE_IO_REPORTED: AtomicBool = AtomicBool::new(false);

/// Notify any idle workers so they re-poll immediately instead of waiting
/// out `POLL_INTERVAL`. Cheap no-op before [`start`] has run.
pub fn wake_workers() {
    if let Some(notify) = WORKER_NOTIFY.get() {
        notify.notify_waiters();
    }
}

/// Start the worker pool + daily scheduler. Takes the full `Config` so
/// each spawned task sees the user's actual settings (LLM endpoints,
/// embedder model, timeouts) — not `TestHostConfig::default()`. Without this,
/// workers fall back to inert/regex-only behavior regardless of what's
/// in `config.toml`, defeating the entire async pipeline.
///
/// Idempotent (`Once`-guarded) so repeat calls during bootstrap are
/// safe no-ops after the first.
pub fn start(config: Arc<Config>) {
    STARTED.call_once(|| {
        let notify = WORKER_NOTIFY
            .get_or_init(|| Arc::new(Notify::new()))
            .clone();
        if let Err(err) = recover_stale_locks(&*config) {
            log::warn!("[memory::jobs] recover_stale_locks failed at startup: {err:#}");
        }

        // Release in-flight locks on graceful shutdown so a clean restart
        // re-claims the work immediately instead of waiting out the lease
        // (which surfaced as a stale-lock recovery warn on every launch).
        // Hard kills still fall back to lease-expiry recovery at startup
        // (bug-report-2026-05-26 I2).
        let shutdown_cfg = config.to_arc();
        crate::shutdown::register(move || {
            // NOTE: `shutdown::register` is bound `F: Fn() -> Fut`, so this
            // closure may be invoked more than once; each call must hand the
            // returned future its own owned `Config`. Moving `shutdown_cfg`
            // in directly is `E0507` (cannot move out of an `Fn` closure), so
            // the per-call clone is required, not redundant.
            let cfg = shutdown_cfg.clone();
            async move {
                match release_running_locks(&*cfg) {
                    Ok(n) if n > 0 => {
                        log::info!(
                            "[memory::jobs] released {n} in-flight job lock(s) on graceful shutdown"
                        );
                    }
                    Ok(_) => {}
                    Err(err) => {
                        log::warn!(
                            "[memory::jobs] failed to release job locks on shutdown: {err:#}"
                        );
                    }
                }
            }
        });

        for idx in 0..WORKER_COUNT {
            let notify = notify.clone();
            let cfg = config.to_arc();
            tokio::spawn(async move {
                loop {
                    match run_once(&*cfg).await {
                        Ok(processed) => {
                            // A successful claim proves the memory_tree DB
                            // opened, so the host filesystem is healthy again.
                            // Clear any prior host-I/O degradation so the status
                            // banner self-heals and a genuinely-new later failure
                            // can page once more. Guarded on the latch swap so the
                            // clear + info log fire only on the recovery edge, not
                            // every poll.
                            if STORAGE_IO_REPORTED.swap(false, Ordering::Relaxed) {
                                clear_storage_degraded();
                                log::info!(
                                    "[memory::jobs] worker {idx} storage recovered; \
                                     cleared host-I/O degraded flag"
                                );
                            }
                            if processed {
                                continue;
                            }
                            tokio::select! {
                                _ = notify.notified() => {}
                                _ = tokio::time::sleep(POLL_INTERVAL) => {}
                            }
                        }
                        Err(err) => {
                            // SQLite `BUSY` / `LOCKED` is transient write-lock
                            // contention (multiple workers + the scheduler +
                            // ingest producers all write the same DB). The
                            // configured `busy_timeout` already retries
                            // inside rusqlite; if we still see it here, the
                            // right answer is to back off and re-poll — not
                            // to page Sentry. The next loop iteration will
                            // try `claim_next` again and almost always
                            // succeed. See OPENHUMAN-TAURI-BP.
                            if is_sqlite_busy(&err) {
                                log::warn!(
                                    "[memory::jobs] worker {idx} hit SQLite busy/locked, \
                                     backing off 1s: {err:#}"
                                );
                                tokio::time::sleep(Duration::from_secs(1)).await;
                            } else if is_sqlite_io_transient(&err) {
                                // I/O errors (IOERR_TRUNCATE 1546, the `-shm` family
                                // 4618/4874/5386, IN_PAGE 8714, CANTOPEN 14) or circuit
                                // breaker open — transient
                                // filesystem / WAL condition. Back off 30 s and let the
                                // connection cache try a fresh open on next poll. These
                                // are NOT reported to Sentry (they are transient and were
                                // flooding ~19K events/4 days, see #2206).
                                log::warn!(
                                    "[memory::jobs] worker {idx} hit transient I/O error, \
                                     backing off 30s: {err:#}"
                                );
                                tokio::time::sleep(Duration::from_secs(30)).await;
                            } else if is_sqlite_disk_full(&err) {
                                // SQLITE_FULL (code 13): the host disk is full.
                                // A claim UPDATE cannot succeed until the user
                                // frees space — this is persistent, not
                                // transient, so re-polling every second and
                                // paging Sentry on each failure floods the
                                // dashboard (TAURI-RUST-4R8: ~95k events, one
                                // user) for a condition only the user can
                                // clear. Back off long and stay silent; the
                                // `ready` rows resume when space returns and
                                // `notify` still wakes us on new enqueues.
                                log::warn!(
                                    "[memory::jobs] worker {idx} hit SQLITE_FULL (disk full), \
                                     backing off 300s without reporting: {err:#}"
                                );
                                tokio::time::sleep(Duration::from_secs(300)).await;
                            } else if is_sqlite_corrupt(&err) {
                                // SQLITE_CORRUPT (code 11): the on-disk mem_tree
                                // image is malformed. Unlike busy/io-transient/
                                // disk-full, this NEVER clears on its own — the
                                // claim UPDATE fails forever, so re-polling every
                                // second and paging Sentry each time turns one
                                // unrecoverable file into a flood (TAURI-RUST-E93:
                                // 1,633 events in ~17 min, one host). Report once,
                                // drive quarantine+rebuild recovery (factored into
                                // `recover_corrupt_db_once` so it is unit-testable
                                // without spinning the live loop), then back off
                                // long so a failed recovery never re-floods.
                                // `notify` still wakes us on new enqueues once the
                                // rebuild succeeds.
                                recover_corrupt_db_once(idx, &err, &*cfg);
                                tokio::time::sleep(Duration::from_secs(300)).await;
                            } else if is_host_io_error(&err) {
                                // Persistent host-filesystem failure (EIO 5 /
                                // ENOSPC 28 / EROFS 30) creating or opening the
                                // memory_tree dir/DB — e.g. a failing or
                                // disconnected SD card, or a volume the kernel
                                // remounted read-only. Like SQLITE_FULL/CORRUPT
                                // this is a persistent host condition only the
                                // user can clear (reseat/replace/free storage),
                                // so re-polling every second and paging Sentry on
                                // each failure floods the dashboard (CORE-RUST-19J:
                                // ~10k events in ~50 min from one Raspberry Pi).
                                //
                                // Resolution, not just suppression: mark the
                                // memory_tree degraded with `StorageUnavailable`
                                // so the status panel shows the user an actionable
                                // "check your disk" banner — they own the only
                                // lever. Report ONCE (process-wide latch) for dev
                                // telemetry, then back off 300s and stay silent.
                                // Jobs stay `ready` and resume when storage
                                // returns; the degraded flag + latch clear on the
                                // next successful claim.
                                mark_storage_degraded(FailureCode::StorageUnavailable);
                                if !STORAGE_IO_REPORTED.swap(true, Ordering::Relaxed) {
                                    crate::observability::report_error(
                                        &err,
                                        "memory",
                                        "tree_jobs_worker_host_io",
                                        &[("worker_idx", &idx.to_string())],
                                    );
                                }
                                log::warn!(
                                    "[memory::jobs] worker {idx} hit host filesystem I/O error \
                                     (EIO/ENOSPC/EROFS — failing or read-only storage), \
                                     backing off 300s: {err:#}"
                                );
                                tokio::time::sleep(Duration::from_secs(300)).await;
                            } else {
                                crate::observability::report_error(
                                    &err,
                                    "memory",
                                    "tree_jobs_worker",
                                    &[("worker_idx", &idx.to_string())],
                                );
                                tokio::time::sleep(Duration::from_secs(1)).await;
                            }
                        }
                    }
                }
            });
        }

        super::scheduler::start(config);
    });
}

/// Claim and run a single job. Returns `true` when work was processed,
/// `false` when no eligible row was available.
pub async fn run_once(config: &Config) -> Result<bool> {
    // Cooperative throttle BEFORE claiming, so memory queue work still yields to
    // voice/autocomplete/triage under load (Throttled/Paused modes), exactly as
    // the legacy pool did. Held across the single crate step below; returns
    // immediately in Aggressive/Normal so idle desktops pay zero cost.
    let _gate_permit = crate::scheduler_gate::wait_for_capacity().await;

    // W4 flip: TinyCortex now owns claim → dispatch → settle. `queue::run_once`
    // claims one `mem_tree_jobs` row (the same table host producers enqueue
    // into — identical schema, parity P4) and runs it through the crate's
    // `handle_job` + `HostQueueDelegates`, which bridge each heavy step
    // (score/admit, buffer push, seal, seal-document, re-embed) back to the host
    // `memory_tree`/score/embed engine, then settles the row itself. The crate's
    // single-slot LLM gate serialises llm-bound jobs; the legacy per-job
    // local/cloud permit routing and the extract-batch coalescing are
    // intentionally dropped here (perf, not correctness — W4 follow-up).
    let mc = crate::engine::memory_config_from(config, config.workspace_dir().clone());
    let delegates = crate::engine::HostQueueDelegates::new(config.to_arc());
    crate::engine::backend::queue::run_once(&mc, &delegates).await
}

/// Classify whether an error is a transient I/O failure that should be
/// silently backed off without a Sentry report (#2206).
///
/// Covers:
/// - `SQLITE_IOERR_TRUNCATE` (1546): WAL truncation failed — usually a
///   transient filesystem hiccup.
/// - WAL `-shm` family — `SHMOPEN` (4618, the macOS cold-start failure),
///   `SHMSIZE` (4874), `SHMMAP` (5386): shared-memory side-file temporarily
///   unavailable. (4874 is SHMSIZE, not SHMMAP — the real SHMMAP is 5386.)
/// - `SQLITE_IOERR_IN_PAGE` (8714): mmap-page I/O fault.
/// - `SQLITE_CANTOPEN` / `CannotOpen` (14): DB file temporarily inaccessible.
/// - Text fallback: circuit breaker message, or rusqlite phrases that don't
///   downcast cleanly after multiple `.context()` layers.
fn is_sqlite_io_transient(err: &anyhow::Error) -> bool {
    if let Some(rusqlite::Error::SqliteFailure(f, _)) = err.downcast_ref::<rusqlite::Error>() {
        // 14 CANTOPEN, 1546 TRUNCATE, 4618 SHMOPEN, 4874 SHMSIZE, 5386 SHMMAP,
        // 8714 IN_PAGE — the WAL `-shm` cold-start family (4874 is SHMSIZE, not
        // SHMMAP; the real SHMMAP is 5386).
        if matches!(f.extended_code, 14 | 1546 | 4618 | 4874 | 5386 | 8714) {
            return true;
        }
        if f.code == rusqlite::ErrorCode::CannotOpen {
            return true;
        }
    }
    // Text fallback for errors wrapped under `.context()` layers or
    // emitted as plain `anyhow!` strings (e.g. circuit breaker message).
    let msg = format!("{err:#}").to_ascii_lowercase();
    msg.contains("circuit breaker open")
        || msg.contains("disk i/o error")
        || msg.contains("unable to open database file")
        || msg.contains("xshmmap")
        || msg.contains("truncate file")
}

/// Classify whether an error from `run_once` is a transient SQLite
/// write-lock contention (`SQLITE_BUSY` or `SQLITE_LOCKED`).
///
/// The configured `busy_timeout` already absorbs short waits inside
/// rusqlite; this helper catches the residual case where the busy
/// handler exhausts and the error bubbles up. Treated as a soft signal:
/// the worker logs a warning and re-polls on the next loop iteration
/// rather than escalating to Sentry.
fn is_sqlite_busy(err: &anyhow::Error) -> bool {
    if let Some(rusqlite::Error::SqliteFailure(sqlite_err, _)) =
        err.downcast_ref::<rusqlite::Error>()
    {
        return matches!(
            sqlite_err.code,
            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
        );
    }
    // Fallback for chained/wrapped errors: the rusqlite `Error` may sit
    // a few `context()` layers deep. anyhow's alternate `Display`
    // joins every cause with ": ", so the SQLite-rendered text is
    // searchable in the flattened chain. Match the two well-known
    // phrases SQLite emits for these codes.
    let msg = format!("{err:#}").to_ascii_lowercase();
    msg.contains("database is locked") || msg.contains("database table is locked")
}

/// Classify whether an error from `claim_next` is a `SQLITE_FULL` disk-full
/// condition (primary code `DiskFull`, extended 13).
///
/// Unlike `SQLITE_BUSY`/`LOCKED` or the transient I/O family, a full disk is a
/// **persistent** host condition: the claim `UPDATE` cannot succeed until the
/// user frees space. Re-polling every second and paging Sentry on each failure
/// turns one unrecoverable condition into a flood (Sentry TAURI-RUST-4R8:
/// ~95k events from a single user). The worker backs off long and stays
/// silent; the rows stay `ready` and resume when space returns.
///
/// Matching on the `DiskFull` error code is rusqlite-version-stable. The text
/// fallback covers the case where the error was flattened to a plain `anyhow!`
/// string across `.context()` layers — rusqlite renders `SQLITE_FULL` as
/// `"database or disk is full: Error code 13: Insertion failed because
/// database is full"`, so anchor on either canonical fragment.
fn is_sqlite_disk_full(err: &anyhow::Error) -> bool {
    if let Some(rusqlite::Error::SqliteFailure(sqlite_err, _)) =
        err.downcast_ref::<rusqlite::Error>()
    {
        if sqlite_err.code == rusqlite::ErrorCode::DiskFull {
            return true;
        }
    }
    let msg = format!("{err:#}").to_ascii_lowercase();
    msg.contains("database or disk is full")
        || msg.contains("insertion failed because database is full")
}

/// Classify whether an error from `claim_next` is a `SQLITE_CORRUPT` malformed-
/// image condition (primary code `DatabaseCorrupt`, code 11) or the closely-
/// related `NotADatabase` (code 26 — the header itself is unreadable).
///
/// Unlike `SQLITE_BUSY`/`LOCKED`, the transient I/O family, or `SQLITE_FULL`,
/// a malformed image is **persistent on-disk damage**: the claim `UPDATE` can
/// never succeed, so re-polling every second and paging Sentry on each failure
/// turns one corrupt file into an infinite flood (Sentry TAURI-RUST-E93:
/// ~1.6k events in ~17 min from a single host). The worker reports once, drives
/// a quarantine+rebuild recovery (`recover_corrupt_db`), and backs off long.
///
/// Matching on the error code is rusqlite-version-stable. The text fallback
/// covers the case where the rusqlite error was flattened to a plain `anyhow!`
/// string across `.context()` layers — SQLite renders these as "database disk
/// image is malformed" (code 11) and "file is not a database" (code 26).
fn is_sqlite_corrupt(err: &anyhow::Error) -> bool {
    if let Some(rusqlite::Error::SqliteFailure(sqlite_err, _)) =
        err.downcast_ref::<rusqlite::Error>()
    {
        if matches!(
            sqlite_err.code,
            rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase
        ) {
            return true;
        }
    }
    let msg = format!("{err:#}").to_ascii_lowercase();
    msg.contains("database disk image is malformed") || msg.contains("file is not a database")
}

/// Classify whether an error is a **persistent host-filesystem failure** —
/// `std::fs::create_dir_all` / file open returning an OS-level I/O error on the
/// memory_tree path. Matches the three persistent, user-only-fixable POSIX
/// codes:
/// - **EIO `5`** — the block device can't service I/O (failing/disconnected SD
///   card or USB drive). This is the CORE-RUST-19J signal:
///   `"Failed to create memory_tree dir: …: Input/output error (os error 5)"`.
/// - **ENOSPC `28`** — no space left at the *filesystem* layer (distinct from
///   `SQLITE_FULL`, which is the SQLite-write code handled separately).
/// - **EROFS `30`** — read-only filesystem; Linux remounts a failing SD card
///   read-only, so this is the common next stage of the same Pi failure.
///
/// Unlike the SQLite busy/transient family, these are persistent host
/// conditions the app has no lever to fix: re-polling every second and paging
/// Sentry on each failure turns one dead disk into a flood. The worker backs
/// off long, surfaces a `StorageUnavailable` degradation to the user, and stays
/// silent after a single report.
///
/// Matching on `raw_os_error()` is platform-stable. The text fallback covers
/// the case where the `io::Error` was flattened to a plain `anyhow!` string
/// across `.context()` layers (anyhow renders `"… (os error N)"`); it anchors
/// on the unambiguous os-error number, not the loose phrase, so a network
/// "input/output" string can't false-positive. A non-OS error has
/// `raw_os_error() == None` and matches neither path.
fn is_host_io_error(err: &anyhow::Error) -> bool {
    if let Some(io_err) = err.downcast_ref::<std::io::Error>() {
        if matches!(io_err.raw_os_error(), Some(5) | Some(28) | Some(30)) {
            return true;
        }
    }
    let msg = format!("{err:#}").to_ascii_lowercase();
    msg.contains("(os error 5)") || msg.contains("(os error 28)") || msg.contains("(os error 30)")
}

/// Handle a confirmed `SQLITE_CORRUPT` failure from the worker loop: report it
/// to Sentry **once** (process-wide [`CORRUPT_REPORTED`] latch, not per-poll
/// across the workers) and drive the quarantine+rebuild recovery in
/// [`recover_corrupt_db`](crate::store::chunks::store::recover_corrupt_db).
///
/// Factored out of [`start`]'s error arm so the report-once + recovery decision
/// logic is unit-testable without spinning the live worker loop. The caller
/// applies the long backoff after this returns.
fn recover_corrupt_db_once(idx: usize, err: &anyhow::Error, config: &Config) {
    if !CORRUPT_REPORTED.swap(true, Ordering::Relaxed) {
        crate::observability::report_error(
            err,
            "memory",
            "tree_jobs_worker_corrupt",
            &[("worker_idx", &idx.to_string())],
        );
    }
    log::error!(
        "[memory::jobs] worker {idx} hit SQLITE_CORRUPT (malformed DB image), \
         attempting quarantine + rebuild recovery: {err:#}"
    );
    match crate::store::chunks::store::recover_corrupt_db(config) {
        Ok(true) => {
            log::warn!(
                "[memory::jobs] worker {idx} quarantined corrupt mem_tree DB and rebuilt \
                 empty schema; queue will resume"
            );
            // Recovery settled — allow a future, genuinely-new corruption to
            // page once.
            CORRUPT_REPORTED.store(false, Ordering::Relaxed);
        }
        Ok(false) => {
            log::info!(
                "[memory::jobs] worker {idx} corruption recovery: quick_check now passes, \
                 no quarantine needed"
            );
            CORRUPT_REPORTED.store(false, Ordering::Relaxed);
        }
        Err(rec_err) => {
            log::error!(
                "[memory::jobs] worker {idx} corruption recovery FAILED, retrying after \
                 backoff: {rec_err:#}"
            );
        }
    }
}

#[cfg(test)]
#[path = "worker_tests.rs"]
mod tests;
