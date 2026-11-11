//! A resumable import driver: [`Importer`] copies a legacy workspace into an
//! engine with checkpoints, retries and a failed-item list.
//!
//! [`migrate`](crate::import::migrate) is the plain copy loop. The `Importer`
//! is what a product runs on top of it: progress and the resume
//! [`Checkpoint`] persist through a host [`CheckpointStore`], a batch the
//! engine cannot take for a transient reason is retried with backoff
//! ([`RetryPolicy`], slept through the host's [`Pacer`]), an item the engine
//! refuses is skipped and kept with the reason so [`Importer::begin_retry`]
//! can store it again later, and a failure that is about the account rather
//! than an item (credits exhausted, signed out, engine unreachable) stops the
//! run without advancing the checkpoint past anything unstored.
//!
//! The host keeps what is its own: whether the user consented
//! ([`Consent`]), whether background work is paused ([`PauseCheck`]), which
//! runs are live, where the state file lives, and what to do when a run
//! ends. A run is two steps so the host can claim the workspace in between:
//!
//! 1. [`Importer::begin`] (or [`Importer::begin_retry`] /
//!    [`Importer::begin_retry_resume`]) checks consent, counts the legacy
//!    store, marks the state `Running` and returns the [`Begun`] run. It
//!    reads SQLite, so a host calls it from a blocking-capable context.
//! 2. [`Importer::run`] (or [`Importer::run_retry`]) drives the run to its
//!    end and persists the outcome.
//!
//! Reads of the legacy store happen between awaits, a batch at a time, so a
//! run's future is `Send` and holds no SQLite handle across a suspension.

mod count;
mod retry;
mod run;
mod state;

pub use count::{ImportCounts, count_legacy, open_legacy};
pub use run::{is_insufficient_credits, skips_item};
pub use state::{
    CheckpointStore, FailedItem, ImportFile, ImportPhase, ImportState, MemoryCheckpointStore,
};

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tinymemory_api::MemoryEngine;

/// Items stored between two checkpoint writes.
pub const CHECKPOINT_EVERY: u64 = 25;

/// Items per bulk store.
pub const STORE_BATCH: usize = 25;

/// Whether the user consented to the import uploading local memory to the
/// engine. Asked once, by [`Importer::begin`].
pub type Consent = Arc<dyn Fn() -> bool + Send + Sync>;

/// Whether background work is paused right now. An automatic resume is asked
/// before every batch it stores (and every item it retries); `true` stops the
/// run, leaving it `Running` with its checkpoint for a later tick.
pub type PauseCheck = Arc<dyn Fn() -> bool + Send + Sync>;

/// How an engine error reads in the persisted state and in a failed item's
/// reason. Defaults to the error's `Display`.
pub type Describe = Arc<dyn Fn(&tinymemory_api::Error) -> String + Send + Sync>;

/// The host's clock for backoff, so the driver links no async runtime.
#[async_trait]
pub trait Pacer: Send + Sync {
    /// Waits for `delay`.
    async fn sleep(&self, delay: Duration);
}

/// How long to wait before each retry of a batch the engine could not take
/// for a transient reason (an `Unavailable` error). After the last delay the
/// run stops and is left to resume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryPolicy {
    /// One delay per retry, in order.
    pub delays: Vec<Duration>,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            delays: vec![
                Duration::from_secs(2),
                Duration::from_secs(10),
                Duration::from_secs(30),
            ],
        }
    }
}

impl RetryPolicy {
    /// `retries` retries with no wait between them.
    #[must_use]
    pub fn immediate(retries: usize) -> Self {
        Self {
            delays: vec![Duration::ZERO; retries],
        }
    }
}

/// Why a run could not begin.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ImportError {
    /// The user has not consented to uploading local memory.
    #[error("importing uploads local memory to the selected engine; pass consent: true")]
    ConsentRequired,
    /// The workspace holds no v1 store.
    #[error("no v1 memory store to import")]
    NoLegacyStore,
    /// A retry was asked for with nothing to retry.
    #[error("no failed items to retry: the import has not finished or skipped nothing")]
    NothingToRetry,
}

/// What an interrupted import tick may resume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumePlan {
    /// No persisted state is currently eligible for an automatic resume.
    DoNotResume,
    /// Resume the legacy-store import from its saved checkpoint.
    ResumeImport,
    /// Resume retrying the persisted failed-item list.
    ResumeRetry,
}

/// A run that has begun: the state it marked `Running`, ready for
/// [`Importer::run`] or [`Importer::run_retry`].
#[derive(Debug)]
#[must_use = "a begun run does nothing until it is run"]
pub struct Begun {
    /// The state as persisted when the run began.
    pub state: ImportState,
    file: ImportFile,
}

/// Imports one legacy workspace into one engine.
pub struct Importer {
    pub(crate) workspace_dir: PathBuf,
    pub(crate) engine: Arc<dyn MemoryEngine>,
    pub(crate) store: Arc<dyn CheckpointStore>,
    pub(crate) pacer: Arc<dyn Pacer>,
    pub(crate) consent: Consent,
    pub(crate) paused: Option<PauseCheck>,
    pub(crate) policy: RetryPolicy,
    pub(crate) describe: Describe,
}

impl Importer {
    /// An importer of the legacy workspace at `workspace_dir` into `engine`,
    /// persisting through `store`.
    #[must_use]
    pub fn new(
        workspace_dir: impl Into<PathBuf>,
        engine: Arc<dyn MemoryEngine>,
        store: Arc<dyn CheckpointStore>,
        pacer: Arc<dyn Pacer>,
        consent: Consent,
    ) -> Self {
        Self {
            workspace_dir: workspace_dir.into(),
            engine,
            store,
            pacer,
            consent,
            paused: None,
            policy: RetryPolicy::default(),
            describe: Arc::new(|error| error.to_string()),
        }
    }

    /// Asks `paused` before every batch (and every retried item); `true`
    /// leaves the run `Running` to resume later. Without it a run goes to the
    /// end, as one the user started does.
    #[must_use]
    pub fn with_pause(mut self, paused: PauseCheck) -> Self {
        self.paused = Some(paused);
        self
    }

    /// Replaces the transient-failure backoff.
    #[must_use]
    pub fn with_retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Replaces how an engine error reads in state and failed-item reasons.
    #[must_use]
    pub fn with_describe(mut self, describe: Describe) -> Self {
        self.describe = describe;
        self
    }

    /// The pure automatic-resume decision for one saved file.
    ///
    /// The scheduler pause blocks every resume. A `Running` import resumes
    /// with consent already recorded; a credits-stopped import resumes only
    /// when billing allows it. A running retry keeps its retry mode, while a
    /// credits-gated `Error` resumes the import checkpoint.
    #[must_use]
    pub fn resume_plan(file: &ImportFile, billing_ok: bool, paused: bool) -> ResumePlan {
        if paused {
            return ResumePlan::DoNotResume;
        }
        match file.state.phase {
            ImportPhase::Running if file.retrying => ResumePlan::ResumeRetry,
            ImportPhase::Running => ResumePlan::ResumeImport,
            ImportPhase::Error if file.paused_for_credits && billing_ok => ResumePlan::ResumeImport,
            _ => ResumePlan::DoNotResume,
        }
    }
}

/// Records that an automatic resume could not start, without requiring an
/// engine or pacer to be constructed. The checkpoint and failed-item list
/// remain as saved.
///
/// An interrupted retry becomes `Done` so the user can press Retry again; an
/// interrupted import becomes `Error` so Resume can continue from its
/// checkpoint.
pub fn record_resume_failure(store: &dyn CheckpointStore, error: &str) {
    let mut file = store.load();
    if file.retrying {
        file.state.phase = ImportPhase::Done;
        file.state.error = Some(format!(
            "the retry could not resume: {error}; press Retry again"
        ));
        file.retrying = false;
    } else {
        file.state.phase = ImportPhase::Error;
        file.state.error = Some(format!("the import could not resume: {error}"));
    }
    file.paused_for_credits = false;
    if let Err(error) = store.save(&file) {
        log::warn!("[memory:import] writing import state failed: {error}");
    }
}

impl std::fmt::Debug for Importer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Importer")
            .field("workspace_dir", &self.workspace_dir)
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}
