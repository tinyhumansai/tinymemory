//! [`EvidenceRef`] — a pointer to the thing a learned fact was learned from.
//!
//! Moved here from the host's `agent::learning::candidate` because it is
//! persisted *in the memory store*: `store::namespace_store::profile` writes it
//! into profile rows, and the Composio provider-profile sync reads it back. Two
//! structurally identical enums either side of the seam would round-trip
//! through serde and silently diverge on the first added variant.
//!
//! Inert serde data; the contract crate's dependency-light guarantee is
//! unaffected. **Its serde form is persisted**, so the `#[serde(tag = "type")]`
//! representation and every variant name are a compatibility surface.

use serde::{Deserialize, Serialize};

/// A typed pointer back into the memory substrate from which a candidate was
/// derived. Used for provenance tracking, citation, and the `evidence_ids`
/// column in `user_profile_facets` (Phase 3+).
///
/// Serialised with a `"type"` discriminator in snake_case so the JSON is
/// human-readable: `{"type":"episodic","episodic_id":42}`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EvidenceRef {
    /// A single row in `episodic_log`.
    Episodic {
        /// Row id in `episodic_log`.
        episodic_id: i64,
    },
    /// A contiguous window of rows in `episodic_log`.
    EpisodicWindow {
        /// First row id in the window, inclusive.
        from_id: i64,
        /// Last row id in the window, inclusive.
        to_id: i64,
    },
    /// A row in the tree-source summary table.
    SourceSummary {
        /// Row id in the tree-source summary table.
        summary_id: String,
    },
    /// A node in `tree_topic`.
    TreeTopic {
        /// Node id in `tree_topic`.
        topic_id: String,
    },
    /// A chunk in `vector_chunks` associated with a document source.
    DocumentChunk {
        /// The document source the chunk belongs to.
        source_id: String,
        /// Row id in `vector_chunks`.
        chunk_id: String,
    },
    /// A specific message in an email source.
    EmailMessage {
        /// The email source the message arrived in.
        source_id: String,
        /// Provider-assigned message id.
        message_id: String,
    },
    /// A field value from a connected provider (Composio toolkit).
    Provider {
        /// Composio toolkit slug the value came from.
        toolkit: String,
        /// The connection the value was read through.
        connection_id: String,
        /// Field name within the provider's payload.
        field: String,
    },
    /// A tool call record within an episodic entry.
    ToolCall {
        /// The tool that was called.
        tool_name: String,
        /// The episodic row the call was recorded in.
        episodic_id: i64,
    },
    /// A per-window weight from `tree_source`.
    TreeSourceWeight {
        /// The `tree_source` window the weight belongs to.
        window_label: String,
    },
}

#[cfg(test)]
#[path = "evidence_tests.rs"]
mod tests;
