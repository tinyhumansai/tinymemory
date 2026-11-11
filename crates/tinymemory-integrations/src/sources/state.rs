//! Source sync state types, without a storage implementation.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Where a source's latest sync stands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceStatus {
    /// Not running, with no current error.
    #[default]
    Idle,
    /// A sync is in progress.
    Syncing,
    /// The latest sync failed.
    Error,
}

/// What the last sync of one source left behind.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceState {
    /// When the last sync finished.
    #[serde(default)]
    pub last_sync_at: Option<DateTime<Utc>>,
    /// Current status.
    #[serde(default)]
    pub status: SourceStatus,
    /// Why the last sync failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Items stored by the last successful sync.
    #[serde(default)]
    pub items: u64,
}

/// Resets stale `syncing` entries after a host process restart.
///
/// A `syncing` status cannot have an active run after process startup. Other
/// status fields and values are retained.
pub fn reset_interrupted(all: &mut BTreeMap<String, SourceState>) {
    for state in all.values_mut() {
        if state.status == SourceStatus::Syncing {
            state.status = SourceStatus::Idle;
        }
    }
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;
