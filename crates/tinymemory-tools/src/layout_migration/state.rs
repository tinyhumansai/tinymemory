//! The migration's progress, and the port the host persists it through.
//!
//! The host saves the state whole after every page, so a crash leaves the
//! previous state or the new one, never a torn one ([`MigrationStateStore`]).

use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};

use super::error::Result;

/// Where the migration is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Not started.
    #[default]
    Idle,
    /// Copying the legacy tree, page by page.
    Copying,
    /// Stopped where it was: paused by the scheduler, out of credits, or the
    /// engine unreachable. Resumes from the cursor.
    Paused,
    /// Every item it could copy is copied and verified.
    Copied,
    /// Removing the legacy copies of what was moved.
    Cleaning,
    /// The legacy tree holds nothing that was moved; what is left there (if
    /// anything) is in `failures` or `incomplete`.
    Cleaned,
}

/// An item that could not be copied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    /// Its id in the legacy tree.
    pub id: String,
    /// Why (an error code, never content).
    pub reason: String,
}

/// The migration's progress.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MigrationState {
    /// Where it is.
    pub phase: Phase,
    /// The legacy export cursor of the next page; `None` before the first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// Items copied and confirmed readable in the per-user tree.
    pub copied: u64,
    /// Of those, items the per-user tree already held (a resumed page).
    pub replayed: u64,
    /// Items that could not be copied.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<Failure>,
    /// Items the legacy tree holds only part of (a chunked document missing
    /// a piece).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub incomplete: Vec<String>,
    /// Why it is paused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Cleanup has started, so a pause resumes it, not the copy.
    #[serde(default)]
    pub cleaning: bool,
    /// Reads and writes moved to the per-user tree.
    #[serde(default)]
    pub switched: bool,
    /// The catch-up copy after the switch is done.
    #[serde(default)]
    pub caught_up: bool,
    /// The user agreed to take a legacy tree other accounts may share.
    #[serde(default)]
    pub takeover: bool,
    /// Extra catch-up passes made because the import ended with its last
    /// batch not confirmed listed (see `job::run`).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub rechecks: u32,
    /// Those passes are over (one moved nothing new, or the most were made).
    /// The import's flag stays set, so cleanup keeps forgetting by id.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rechecked: bool,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// Where the host keeps the migration's state.
///
/// The state is saved after every page, so an implementation must write it
/// whole and atomically (a temporary file and a rename, or a transaction).
pub trait MigrationStateStore: Send + Sync {
    /// The saved state, or a fresh one when none was saved.
    ///
    /// # Errors
    ///
    /// A state that cannot be read or parsed: the migration must not start
    /// over and copy everything again on a damaged one.
    fn load(&self) -> Result<MigrationState>;

    /// Saves `state` whole.
    ///
    /// # Errors
    ///
    /// When the state cannot be written.
    fn save(&self, state: &MigrationState) -> Result<()>;
}

/// A [`MigrationStateStore`] that keeps the state in memory.
#[derive(Debug, Default)]
pub struct MemoryStateStore(Mutex<MigrationState>);

impl MigrationStateStore for MemoryStateStore {
    fn load(&self) -> Result<MigrationState> {
        Ok(self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone())
    }

    fn save(&self, state: &MigrationState) -> Result<()> {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = state.clone();
        Ok(())
    }
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;
