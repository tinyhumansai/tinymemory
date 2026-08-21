//! Sync-status vocabulary (#18 §B1).
//!
//! Owned here rather than re-exported from the engine. These are the shapes
//! the host's status RPC speaks; today the only *producer* is the engine's
//! SQLite-backed `list_sync_statuses`, which OpenHuman still calls directly
//! (its own containment debt, tracked in its `direct_engine_refs` allowlist).
//! When §B1's orchestrator move gives core a producer, it fills these types;
//! the serde shape matches the engine's copy field for field.

use serde::{Deserialize, Serialize};

/// How fresh a provider's sync is, judged from its newest chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FreshnessLabel {
    /// Newest chunk is under 30 seconds old.
    Active,
    /// Newest chunk is under 5 minutes old.
    Recent,
    /// Anything older, or nothing synced yet.
    Idle,
}

impl FreshnessLabel {
    /// Label for a provider whose newest chunk landed at
    /// `last_chunk_at_ms`, judged at `now_ms`.
    pub fn from_age_ms(last_chunk_at_ms: Option<i64>, now_ms: i64) -> Self {
        match last_chunk_at_ms {
            None => Self::Idle,
            Some(timestamp) => match now_ms.saturating_sub(timestamp) {
                age if age <= 30_000 => Self::Active,
                age if age <= 5 * 60_000 => Self::Recent,
                _ => Self::Idle,
            },
        }
    }
}

/// One provider's sync progress, as the status RPC reports it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemorySyncStatus {
    pub provider: String,
    pub chunks_synced: u64,
    pub chunks_pending: u64,
    pub batch_total: u64,
    pub batch_processed: u64,
    pub last_chunk_at_ms: Option<i64>,
    pub freshness: FreshnessLabel,
}

#[cfg(test)]
#[path = "sync_status_tests.rs"]
mod tests;
