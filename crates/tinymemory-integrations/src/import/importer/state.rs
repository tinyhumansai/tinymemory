//! The persisted state of an import and the port it persists through.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tinymemory_api::StoreItem;

use crate::import::Checkpoint;

/// Where an import stands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportPhase {
    /// Never started.
    #[default]
    Idle,
    /// Running.
    Running,
    /// Finished.
    Done,
    /// Stopped on an error; starting again resumes.
    Error,
}

/// Progress a host shows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportState {
    /// Phase.
    pub phase: ImportPhase,
    /// Items stored so far.
    pub imported: u64,
    /// Items found by the scan.
    pub total: u64,
    /// Why it stopped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Items the engine refused (malformed, too large), kept so they can be
    /// retried.
    #[serde(default)]
    pub failed: u64,
}

/// One legacy item the engine refused, kept with the reason so a user can see
/// it and retry it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailedItem {
    /// The item's legacy id (`memory_docs:<id>`, `episodic_log:<session>`, ...).
    pub id: String,
    /// Why the engine refused it.
    pub reason: String,
}

/// The legacy id the importer gave `item` (its `source.id`).
pub(crate) fn legacy_id(item: &StoreItem) -> String {
    item.meta().source.id.clone().unwrap_or_default()
}

/// Everything an import persists. The JSON shape is the one the product has
/// always written, so an existing state file keeps loading.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportFile {
    /// Progress.
    #[serde(default)]
    pub state: ImportState,
    /// The run stopped because the account ran out of credits; a background
    /// job resumes it once automatic runs are allowed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub paused_for_credits: bool,
    /// Items the engine refused, for a retry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed: Vec<FailedItem>,
    /// The `Running` run is a retry of `failed`, not the import itself: an
    /// interrupted one resumes as a retry.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub retrying: bool,
    /// Where to resume reading the legacy store.
    #[serde(default)]
    pub checkpoint: Checkpoint,
    /// The import ended with its last batch not confirmed listed (the final
    /// wait gave up).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub listed_unconfirmed: bool,
}

/// Where a host keeps the [`ImportFile`].
///
/// `save` must be atomic (staged, synced, renamed): a crash mid-write has to
/// leave the previous state, never a torn file that reads back as a fresh
/// start and re-sends everything.
pub trait CheckpointStore: Send + Sync {
    /// The persisted file, or the default when there is none or it cannot be
    /// read.
    fn load(&self) -> ImportFile;

    /// Persists `file`.
    ///
    /// # Errors
    ///
    /// A message the importer logs; the run goes on and persists again at the
    /// next checkpoint.
    fn save(&self, file: &ImportFile) -> Result<(), String>;
}

/// A [`CheckpointStore`] held in memory.
#[derive(Debug, Default)]
pub struct MemoryCheckpointStore {
    file: Mutex<ImportFile>,
}

impl MemoryCheckpointStore {
    /// A store holding `file`.
    #[must_use]
    pub fn new(file: ImportFile) -> Self {
        Self {
            file: Mutex::new(file),
        }
    }
}

impl CheckpointStore for MemoryCheckpointStore {
    fn load(&self) -> ImportFile {
        self.file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn save(&self, file: &ImportFile) -> Result<(), String> {
        *self
            .file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = file.clone();
        Ok(())
    }
}
