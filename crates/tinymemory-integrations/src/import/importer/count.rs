//! Opening and sizing the legacy store.

use std::path::Path;

use crate::import::{Error, LegacyWorkspace};

/// Item counts a legacy store would import.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImportCounts {
    /// Documents, and every memory-tree chunk source.
    pub documents: u64,
    /// Conversations.
    pub conversations: u64,
    /// Learnings: distilled facts, profile facets, extracted events, turn
    /// lessons, graph relations, and the goals and persona files.
    pub learnings: u64,
}

impl ImportCounts {
    /// Every item.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.documents + self.conversations + self.learnings
    }
}

/// Opens the legacy store for every scan, count, import and retry, skipping
/// connector (Composio) syncs so the scan counts match the run.
///
/// # Errors
///
/// As [`LegacyWorkspace::open`].
pub fn open_legacy(dir: &Path) -> Result<LegacyWorkspace, Error> {
    log::debug!(
        "[memory:import] skipping connector syncs at {}",
        dir.display()
    );
    LegacyWorkspace::open(dir).map(|workspace| workspace.skip_connector_syncs(true))
}

/// Counts what a legacy store at `dir` holds, or `None` when there is no v1
/// store there. Sized by the importer's own counts, one query per section
/// rather than a pass over every item, and exactly what the import then
/// yields. Blocking (SQLite, and memory-tree chunk files).
#[must_use]
pub fn count_legacy(dir: &Path) -> Option<ImportCounts> {
    let counts = open_legacy(dir)
        .and_then(|workspace| workspace.counts())
        .map_err(|error| log::debug!("[memory:import] no legacy store: {error}"))
        .ok()?;
    // Memory-tree chunk sources count as documents: most are, and a chat one
    // is a conversation the import still stores.
    Some(ImportCounts {
        documents: counts.documents + counts.chunks,
        conversations: counts.conversations,
        learnings: counts.learnings
            + counts.profile
            + counts.events
            + counts.lessons
            + counts.relations
            + counts.files,
    })
}
