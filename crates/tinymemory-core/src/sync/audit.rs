//! The sync audit log: one JSON line per sync run (#18 §B1/§B2).
//!
//! Owned here rather than re-exported from the engine so the files under
//! `core/src/sync/` can account for a run without naming an engine. The file
//! itself is shared infrastructure:
//!
//! - **Path**: `<workspace>/memory_tree/sync_audit.jsonl` — fixed, because two
//!   writers append to it.
//! - **The engine writes it too.** Its rebuild pipeline appends entries with
//!   its own copy of this type. The `audit_line_format_is_pinned` test below
//!   holds this copy to the exact serialised form so the two writers cannot
//!   drift apart silently; if that test fails, the fix is a coordinated format
//!   change on both sides, never a local edit.

use std::io::Write;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

const AUDIT_DIR: &str = "memory_tree";
const AUDIT_FILENAME: &str = "sync_audit.jsonl";

/// One sync run, as the audit log records it.
///
/// Field names are the on-disk format. See the module doc before changing
/// anything here.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SyncAuditEntry {
    pub timestamp: DateTime<Utc>,
    pub source_id: String,
    pub source_kind: String,
    pub scope: String,
    pub items_fetched: u32,
    pub batches: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub estimated_cost_usd: f64,
    #[serde(default)]
    pub composio_actions_called: u32,
    #[serde(default)]
    pub composio_cost_usd: f64,
    #[serde(default)]
    pub actual_charged_usd: Option<f64>,
    pub duration_ms: u64,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl SyncAuditEntry {
    /// The run's cost as the audit views it: the real charge when the
    /// provider reported one, the estimate otherwise, plus Composio's own
    /// action cost.
    pub fn effective_cost_usd(&self) -> f64 {
        self.actual_charged_usd.unwrap_or(self.estimated_cost_usd) + self.composio_cost_usd
    }

    /// Alias for [`Self::effective_cost_usd`], kept because the periodic
    /// scheduler's budget accounting already speaks this name.
    pub fn combined_cost_usd(&self) -> f64 {
        self.effective_cost_usd()
    }
}

/// Estimated inference cost for a sync batch, in USD.
///
/// The engine prices identically from its copy; both are estimates the audit
/// records alongside the real charge when one is reported. Owned here with
/// the audit log because this is where the number lands.
pub fn estimate_cost_usd(input_tokens: u64, output_tokens: u64) -> f64 {
    input_tokens as f64 * 0.07 / 1_000_000.0 + output_tokens as f64 * 0.28 / 1_000_000.0
}

/// Append one entry to the audit log under `workspace`.
///
/// # Errors
///
/// Returns an error when the directory cannot be created or the file cannot
/// be opened or written.
pub fn append_audit_entry(workspace: &Path, entry: &SyncAuditEntry) -> anyhow::Result<()> {
    let directory = workspace.join(AUDIT_DIR);
    std::fs::create_dir_all(&directory)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(directory.join(AUDIT_FILENAME))?;
    // One buffer, one write. Two appenders share this file (the periodic loop
    // and the manual source sync), and a two-syscall append lets their lines
    // interleave; the reader would then skip both as malformed. A single
    // `write_all` on an O_APPEND handle lands the whole line atomically for
    // any plausible entry size.
    let mut line = serde_json::to_vec(entry)?;
    line.push(b'\n');
    file.write_all(&line)?;
    tracing::debug!(source_id = %entry.source_id, success = entry.success, "[memory_sync:audit] entry appended");
    Ok(())
}

/// Read the audit log under `workspace`, newest first.
///
/// A missing file is an empty log. Malformed lines are skipped with a
/// warning rather than failing the read: the log is append-only across
/// process crashes, so a torn final line must not hide the rest.
///
/// # Errors
///
/// Returns an error only when the file exists and cannot be read.
pub fn read_audit_log(workspace: &Path) -> anyhow::Result<Vec<SyncAuditEntry>> {
    let path = workspace.join(AUDIT_DIR).join(AUDIT_FILENAME);
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut entries: Vec<_> = content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| match serde_json::from_str(line) {
            Ok(entry) => Some(entry),
            Err(error) => {
                tracing::warn!(%error, "[memory_sync:audit] malformed audit line skipped");
                None
            }
        })
        .collect();
    entries.reverse();
    Ok(entries)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "audit_tests.rs"]
mod tests;
