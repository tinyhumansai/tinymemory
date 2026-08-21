//! Catalog of every kind of data the memory_store persists.
//!
//! Every stored object falls into exactly one of these kinds. The enum is the
//! authoritative answer to "what can memory_store store?" and is used by:
//! - The retrieval facade, to fan out a query to the right backends.
//! - The vector/obsidian compatibility traits, to dispatch by kind.
//! - Agent tools, to surface a kind filter to LLM callers.
//!
//! Adding a new storage kind = adding a variant here, an impl of the
//! `VectorEmbeddable` / `ObsidianRepresentable` traits
//! ([`crate::store::traits`]), and a delegation in
//! [`crate::store::retrieval`].

use serde::{Deserialize, Serialize};

/// Every persisted data shape in memory_store, named once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    /// On-disk raw markdown file (the content store). One file per
    /// canonicalized source chunk OR per summary node. Source of truth
    /// for all content bodies.
    Raw,
    /// SQLite chunk row — metadata + tags + raw-md pointer + lifecycle.
    /// Bodies live in `Raw`; the chunk row is the index entry.
    Chunk,
    /// Canonical entity row in `mem_tree_entity_index` — every entity
    /// occurrence per tree node. The substrate `memory_graph` derives
    /// co-occurrence edges from.
    Entity,
    /// Sealed summary tree node — Source, Global, or Topic flavor.
    Tree,
    /// Dense vector embedding row in the local vector DB.
    Vector,
    /// Key-value record (global or namespace-scoped). Lives in the
    /// `kv_global` / `kv_namespace` tables.
    Kv,
    /// Address-book contact (`people::Person`) routed through the contacts
    /// facade.
    Contact,
}

impl MemoryKind {
    /// Snake-case discriminant used in RPC payloads, logs, and tool args.
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryKind::Raw => "raw",
            MemoryKind::Chunk => "chunk",
            MemoryKind::Entity => "entity",
            MemoryKind::Tree => "tree",
            MemoryKind::Vector => "vector",
            MemoryKind::Kv => "kv",
            MemoryKind::Contact => "contact",
        }
    }

    /// Every variant, in stable declaration order. Useful for fan-out
    /// retrieval and for surfacing the kind catalog to LLM tools.
    pub const ALL: &'static [MemoryKind] = &[
        MemoryKind::Raw,
        MemoryKind::Chunk,
        MemoryKind::Entity,
        MemoryKind::Tree,
        MemoryKind::Vector,
        MemoryKind::Kv,
        MemoryKind::Contact,
    ];
}

/// Per-kind canonical Rust type aliases — one stop to find "what struct
/// represents a Tree row?", "what struct represents a Contact?", etc.
/// Aliases (not re-exports) so the documentation lives here and the
/// source-of-truth types stay in their owning modules.
pub mod types {
    pub use crate::people::types::Person as Contact;
    pub use crate::store::chunks::types::Chunk;
    pub use crate::store::entities::EntityHit as Entity;
    pub use crate::store::trees::{SummaryNode as TreeNode, Tree, TreeKind};
    pub use crate::store::types::MemoryKvRecord as Kv;
}

#[cfg(test)]
#[path = "kinds_tests.rs"]
mod tests;
