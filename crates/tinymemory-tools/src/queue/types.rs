//! The queue's persisted shapes and the host storage port.

use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::background::BackgroundJob;

/// Most finished runs kept for the UI.
pub const MAX_HISTORY: usize = 50;

/// Attempts before a failing job is dropped.
pub const MAX_ATTEMPTS: u32 = 5;

/// One queued job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueuedJob {
    /// Stable id: a digest of the job and its root, so a duplicate merges.
    pub id: String,
    /// The layout root it runs under.
    pub root: String,
    /// The work.
    pub job: BackgroundJob,
    /// When it was first queued.
    pub queued_at: DateTime<Utc>,
    /// Failed runs so far.
    #[serde(default)]
    pub attempts: u32,
    /// The last failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// One finished run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobRun {
    /// The job's id.
    pub id: String,
    /// `build_beliefs` or `ingest_brain`.
    pub job: String,
    /// The layout root.
    pub root: String,
    /// When it ran.
    pub ran_at: DateTime<Utc>,
    /// `done`, `started`, `scheduled`, `skipped` or `failed`.
    pub outcome: String,
    /// Why it was skipped or failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Beliefs built, when the engine says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub built: Option<usize>,
    /// Items stored by an ingest.
    #[serde(default)]
    pub stored: usize,
}

/// The persisted queue.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QueueState {
    /// Jobs waiting to run.
    #[serde(default)]
    pub pending: Vec<QueuedJob>,
    /// The latest finished runs, newest first.
    #[serde(default)]
    pub history: Vec<JobRun>,
}

/// Why a queue run was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QueueError {
    /// [`Selection::One`] named a job that is not queued.
    #[error("no queued job `{0}`")]
    NoSuchJob(String),
}

/// Which jobs a run takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// Those at least the build delay old.
    Due,
    /// Every pending job, now.
    All,
    /// One job by id, now.
    One(String),
}

impl Selection {
    /// Selects every pending job for a missing or blank id, or the named job
    /// while preserving the bytes of a nonblank id.
    #[must_use]
    pub fn from_id(id: Option<String>) -> Self {
        id.filter(|id| !id.trim().is_empty())
            .map_or(Self::All, Self::One)
    }
}

/// Where the host keeps the queue (a file, a document, a stream).
///
/// Synchronous, because the queue is small and read-modify-written under its
/// own lock. A missing or unreadable queue loads as empty; a failed save is
/// logged and the in-memory run carries on.
pub trait QueueStore: Send + Sync {
    /// The saved queue, empty when there is none.
    fn load(&self) -> QueueState;
    /// Saves the queue.
    ///
    /// # Errors
    /// A message the queue logs; it never aborts a run.
    fn save(&self, state: &QueueState) -> Result<(), String>;
}

/// A [`QueueStore`] that keeps the queue in memory.
#[derive(Debug, Default)]
pub struct MemoryQueueStore(Mutex<QueueState>);

impl QueueStore for MemoryQueueStore {
    fn load(&self) -> QueueState {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn save(&self, state: &QueueState) -> Result<(), String> {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = state.clone();
        Ok(())
    }
}
