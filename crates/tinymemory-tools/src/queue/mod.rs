//! The background job queue: the slow memory work TinyMemory hands back.
//!
//! `AgentMemory` and `Brain` return [`BackgroundJob`] values (belief builds,
//! deferred ingests) instead of running them. A [`JobQueue`] holds them,
//! de-duplicated (two builds of one scope are one build), persisted through a
//! host [`QueueStore`] so a restart loses none, and runs them against an
//! engine once they are old enough ([`Selection::Due`]) — a belief build reads
//! the facts the engine extracts from the writes, and that extraction lags
//! behind them.
//!
//! An engine that cannot consolidate answers [`JobOutcome::Skipped`], so the
//! same queue runs on every engine; a hosted engine builds on its own
//! schedule ([`JobOutcome::Scheduled`]). A failing job is kept with its error
//! and retried up to [`MAX_ATTEMPTS`] times; an account-wide refusal, including
//! a hosted plan refusal, never counts as an attempt.
//!
//! The crate links no executor and does no I/O: the host supplies the
//! [`QueueStore`] and decides when [`JobQueue::run`] is called.

mod types;

use std::sync::Arc;

use chrono::{Duration, Utc};
use futures::lock::Mutex;
use sha2::{Digest, Sha256};
use tinymemory_api::{MemoryEngine, Namespace};

use crate::background::{BackgroundJob, BackgroundRunner, JobOutcome};
use crate::layout::MemoryLayout;
use crate::layout_migration::Error as EngineFailure;

pub use types::{
    JobRun, MAX_ATTEMPTS, MAX_HISTORY, MemoryQueueStore, QueueError, QueueState, QueueStore,
    QueuedJob, Selection,
};

/// A persisted, de-duplicating queue of [`BackgroundJob`]s.
pub struct JobQueue<S: QueueStore> {
    store: S,
    /// Serialises read-modify-writes. Held across a run, so two runs never
    /// take the same job.
    lock: Mutex<()>,
}

/// The stable id of `job` under `root`: a digest, so a duplicate merges.
#[must_use]
pub fn job_id(root: &str, job: &BackgroundJob) -> String {
    let json = serde_json::to_vec(job).unwrap_or_default();
    let mut digest = Sha256::new();
    digest.update(root.as_bytes());
    digest.update([0]);
    digest.update(&json);
    digest
        .finalize()
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn queued(root: &str, job: BackgroundJob) -> QueuedJob {
    QueuedJob {
        id: job_id(root, &job),
        root: root.to_string(),
        job,
        queued_at: Utc::now(),
        attempts: 0,
        last_error: None,
    }
}

impl<S: QueueStore> std::fmt::Debug for JobQueue<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobQueue").finish_non_exhaustive()
    }
}

impl<S: QueueStore> JobQueue<S> {
    /// A queue persisted in `store`.
    pub fn new(store: S) -> Self {
        Self {
            store,
            lock: Mutex::new(()),
        }
    }

    fn write(&self, state: &QueueState) {
        if let Err(error) = self.store.save(state) {
            log::warn!("[memory:jobs] writing the job queue failed: {error}");
        }
    }

    /// Queues `jobs` under `root`; a job already queued is kept once, at its
    /// first queue time.
    pub async fn enqueue(&self, root: &Namespace, jobs: Vec<BackgroundJob>) {
        if jobs.is_empty() {
            return;
        }
        let _guard = self.lock.lock().await;
        let mut state = self.store.load();
        let root = root.to_string();
        let mut added = 0;
        for job in jobs {
            let entry = queued(&root, job);
            if state.pending.iter().any(|known| known.id == entry.id) {
                continue;
            }
            log::debug!(
                "[memory:jobs] queued id={} job={} root={root}",
                entry.id,
                entry.job.name()
            );
            state.pending.push(entry);
            added += 1;
        }
        if added > 0 {
            self.write(&state);
        }
    }

    /// Whether jobs are waiting. A cheap, lock-free read for a host deciding
    /// which queues are worth a run.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        !self.store.load().pending.is_empty()
    }

    /// The queue as it stands.
    pub async fn snapshot(&self) -> QueueState {
        let _guard = self.lock.lock().await;
        self.store.load()
    }

    /// Runs the selected jobs on `engine`. `build_delay_secs` is how old a job
    /// must be to be [`Selection::Due`]. Returns the runs it recorded. A job
    /// that fails is kept with its error and retried on a later run, up to
    /// [`MAX_ATTEMPTS`]; an account-wide refusal does not count as one. The
    /// run that gives up says so in its reason.
    ///
    /// # Errors
    /// [`QueueError::NoSuchJob`] when [`Selection::One`] names a job that is
    /// not queued; the queue is left as it was.
    pub async fn run(
        &self,
        engine: Arc<dyn MemoryEngine>,
        selection: Selection,
        build_delay_secs: u64,
    ) -> Result<Vec<JobRun>, QueueError> {
        let _guard = self.lock.lock().await;
        let mut state = self.store.load();
        let cutoff = Utc::now() - Duration::seconds(i64::try_from(build_delay_secs).unwrap_or(0));
        let (take, keep): (Vec<QueuedJob>, Vec<QueuedJob>) =
            state.pending.drain(..).partition(|entry| match &selection {
                Selection::Due => entry.queued_at <= cutoff,
                Selection::All => true,
                Selection::One(id) => &entry.id == id,
            });
        state.pending = keep;
        if let Selection::One(id) = &selection
            && take.is_empty()
        {
            return Err(QueueError::NoSuchJob(id.clone()));
        }
        let mut runs = Vec::new();
        for mut entry in take {
            let layout = entry
                .root
                .parse::<Namespace>()
                .ok()
                .and_then(|root| MemoryLayout::new(root).ok())
                .unwrap_or_default();
            let runner = BackgroundRunner::new(engine.clone(), layout);
            let name = entry.job.name().to_string();
            let mut record = JobRun {
                id: entry.id.clone(),
                job: name.clone(),
                root: entry.root.clone(),
                ran_at: Utc::now(),
                outcome: String::new(),
                reason: None,
                built: None,
                stored: 0,
            };
            match runner.run(entry.job.clone()).await {
                Ok(report) => {
                    record.outcome = match &report.outcome {
                        JobOutcome::Done => "done",
                        JobOutcome::Started => "started",
                        JobOutcome::Scheduled => "scheduled",
                        JobOutcome::Skipped { .. } => "skipped",
                    }
                    .to_string();
                    if let JobOutcome::Skipped { reason } = &report.outcome {
                        record.reason = Some(reason.clone());
                    }
                    record.built = report
                        .consolidation
                        .as_ref()
                        .and_then(|receipt| receipt.built);
                    record.stored = report.stored.len();
                    for follow_up in report.follow_ups {
                        let next = queued(&entry.root, follow_up);
                        if !state.pending.iter().any(|known| known.id == next.id) {
                            state.pending.push(next);
                        }
                    }
                    log::info!(
                        "[memory:jobs] ran id={} job={name} outcome={} built={:?}",
                        entry.id,
                        record.outcome,
                        record.built
                    );
                }
                Err(error) => {
                    // A typed refusal of the whole account (memory off, no
                    // credits, or a rejected credential) says nothing about
                    // the job, so it keeps its attempts and waits: dropping
                    // it would leave a permanent gap in the beliefs.
                    let account_wide = EngineFailure::from(error.clone()).is_account_wide();
                    if !account_wide {
                        entry.attempts += 1;
                    }
                    entry.last_error = Some(error.to_string());
                    record.outcome = "failed".to_string();
                    log::warn!(
                        "[memory:jobs] run failed id={} job={name} attempts={} account_wide={account_wide}: {error}",
                        entry.id,
                        entry.attempts
                    );
                    if entry.attempts < MAX_ATTEMPTS {
                        record.reason = Some(error.to_string());
                        state.pending.push(entry);
                    } else {
                        record.reason = Some(format!(
                            "dropped after {MAX_ATTEMPTS} failed attempts: {error}"
                        ));
                    }
                }
            }
            runs.push(record);
        }
        let mut history = runs.clone();
        history.reverse();
        history.append(&mut state.history);
        history.truncate(MAX_HISTORY);
        state.history = history;
        self.write(&state);
        Ok(runs)
    }
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
