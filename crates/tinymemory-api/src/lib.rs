//! The TinyMemory v2 contract.
//!
//! A host needs three things from memory:
//!
//! - **Recall** — a question in, a synthesised answer with citations out
//!   ([`MemoryEngine::recall`]).
//! - **Fetch** — raw keyword, vector or hybrid retrieval over stored items,
//!   filtered by metadata ([`MemoryEngine::fetch`]).
//! - **Store** — ingest a document, a conversation or a learning, each with
//!   typed [`MemoryMeta`] ([`MemoryEngine::store`]).
//!
//! plus [`MemoryEngine::list`] and [`MemoryEngine::forget`] to page through
//! and remove what was stored, and [`MemoryEngine::explore`] and
//! [`MemoryEngine::get`] for explorers: counts of stored items per metadata
//! [`Facet`], and items read whole by id ([`explore`]).
//!
//! A live agent turn stores with [`MemoryEngine::store_with`] and
//! [`WaitFor::Accepted`] so it never waits on indexing, and a host asks an
//! engine to distil beliefs off the hot path with
//! [`MemoryEngine::consolidate`] ([`consolidate`]).
//!
//! Every item lives at one [`Namespace`] node (the root, an agent, a team, a
//! nested sub-agent); a [`Reach`] in the filter says which nodes a read sees
//! ([`namespace`]).
//! An engine advertises what it offers through its
//! [`EngineDescriptor`]; a fetch mode it does not list fails with
//! [`Error::Unsupported`].
//!
//! [`ProvenancePolicy`] classifies recalled external text using host-supplied
//! connector prefixes; [`wrap_untrusted_for_agent`] renders escaped provenance
//! markers for prompt composition ([`provenance`]).
//!
//! This crate performs no I/O. Engines live in `tinymemory-integrations`
//! (the CortexDB engine, and the registry that builds one from
//! configuration).
//!
//! # Example
//!
//! ```
//! use tinymemory_api::{ItemKind, MemoryMeta, MetaFilter, SourceKind, StoreItem};
//!
//! let mut meta = MemoryMeta::from_source(SourceKind::Folder, Some("notes".into()));
//! meta.file_path = Some("/notes/rust/ownership.md".into());
//! let item = StoreItem::document("Ownership moves values.", meta);
//! item.validate()?;
//!
//! let filter = MetaFilter {
//!     file_path: Some("/notes/rust".into()),
//!     ..MetaFilter::kinds([ItemKind::Document])
//! };
//! assert!(filter.matches(item.kind(), item.meta()));
//! # Ok::<(), tinymemory_api::Error>(())
//! ```

#[cfg(feature = "conformance")]
pub mod conformance;
pub mod consolidate;
pub mod engine;
pub mod error;
pub mod explore;
pub mod item;
pub mod meta;
pub mod namespace;
pub mod provenance;
pub mod query;
/// Pure, local identification-risk vocabulary, classification and redaction.
pub mod safety;
pub mod write;

pub use consolidate::{
    BELIEF_TAG, BeliefsRequest, ConsolidateReceipt, ConsolidateRequest, ConsolidateStatus,
    Consolidation,
};
pub use engine::{
    CONTRACT_VERSION, EngineDescriptor, EngineHealth, EngineStatus, EngineStatusHealth,
    MAX_STORE_MANY, MemoryEngine, validate_many,
};
pub use error::{Error, Result};
pub use explore::{
    ExplorePage, ExploreRequest, Facet, FacetBucket, GetRequest, MAX_BUCKETS, MAX_GET_IDS,
    MAX_SCAN_LIMIT, explore_page_of, forget_within_by_get, forget_within_ids,
};
pub use item::{DocumentBody, ItemId, ItemKind, LearningKind, Role, StoreItem, StoreReceipt, Turn};
pub use meta::{
    MemoryMeta, MetaFilter, ObservedActor, SourceKind, SourceRef, ToolCallRef, TurnRange,
};
pub use namespace::{Namespace, Reach, Segment, SegmentKind};
pub use provenance::{ProvenancePolicy, wrap_untrusted_for_agent};
pub use query::{
    Citation, EraseReport, EraseRequest, ExportPage, Exported, FetchMode, FetchPage, FetchRequest,
    ForgetReport, ForgetTarget, Hit, ListPage, ListRequest, RecallAnswer, RecallMode,
    RecallRequest, ThreadWindow, TimeHint,
};
pub use write::{WaitFor, WriteOptions};

/// Re-exported so engines and hosts name the same `async_trait` and `chrono`
/// the contract was compiled with.
pub use async_trait::async_trait;
pub use chrono;
