//! Import a legacy (v1, embedded TinyCortex) workspace into TinyMemory v2.
//!
//! [`LegacyWorkspace::open`] detects a v1 store (`<path>/memory/memory.db`
//! with the `memory_docs`, `episodic_log` and `user_profile` tables, a
//! `<path>/memory_tree/chunks.db`, or both) and refuses anything else with
//! [`Error::NotLegacy`]. [`LegacyWorkspace::counts`] sizes it cheaply, and
//! [`LegacyWorkspace::items`]
//! then streams every importable record as a [`StoreItem`], read straight off
//! disk with SQLite opened read-only — the engine that wrote the store is not
//! linked:
//!
//! | Legacy record | v2 item |
//! | --- | --- |
//! | `memory_docs` rows in document namespaces | `Document` |
//! | `memory_tree/chunks.db` sources (optional) | `Document`, or `Conversation` for `chat` |
//! | `episodic_log` threads | `Conversation` |
//! | `memory_docs` rows in `learning:*` and `global` | `Learning` |
//! | `user_profile` facets | `Learning(Preference)` |
//! | `event_log` events | `Learning` |
//! | `episodic_log` turn lessons | `Learning(Other)` |
//! | `graph_global` / `graph_namespace` relations | `Learning(Fact)` |
//! | `MEMORY_GOALS.md`, `persona/directives.md` | `Learning(Other)` |
//!
//! Every item's `meta.source` is `SourceKind::Import` with a section-scoped
//! legacy id (`memory_docs:<document_id>`, `episodic_log:<session_id>`,
//! `user_profile:<facet_id>`, `mem_tree_chunks:<kind>:<id>`, `event_log:<event_id>`,
//! `episodic_log:lesson:<id>`, `graph_global:<rowid>`, `graph_namespace:<rowid>`,
//! `file:<path>`), and
//! `meta.workspace` is the workspace path. A `memory_docs` row v1 marked as
//! synced from an external service also carries [`EXTERNAL_SYNC_TAG`]. The
//! module's `README.md` details every mapping decision.
//!
//! Import is resumable: each [`ImportedItem`] carries the [`Checkpoint`] to
//! persist once its item is stored, and [`LegacyWorkspace::items_from`]
//! continues after it. [`migrate`] (and [`migrate_with`], which reports each
//! committed checkpoint) drives the whole copy into a
//! [`tinymemory_api::MemoryEngine`] in `store_many` batches, and an engine
//! failure carries the checkpoint to resume from.
//!
//! # Example
//!
//! ```
//! use tinymemory_integrations::import::{Checkpoint, LegacyWorkspace};
//! # let dir = tempfile::tempdir()?;
//! # std::fs::create_dir_all(dir.path().join("memory"))?;
//! # let db = rusqlite::Connection::open(dir.path().join("memory/memory.db"))?;
//! # db.execute_batch(
//! #     "CREATE TABLE memory_docs (document_id TEXT PRIMARY KEY, namespace TEXT NOT NULL,
//! #        title TEXT NOT NULL, content TEXT NOT NULL, tags_json TEXT NOT NULL,
//! #        metadata_json TEXT NOT NULL, updated_at REAL NOT NULL);
//! #      CREATE TABLE episodic_log (id INTEGER PRIMARY KEY, session_id TEXT NOT NULL,
//! #        timestamp REAL NOT NULL, role TEXT NOT NULL, content TEXT NOT NULL);
//! #      CREATE TABLE user_profile (facet_id TEXT PRIMARY KEY, facet_type TEXT NOT NULL,
//! #        key TEXT NOT NULL, value TEXT NOT NULL, confidence REAL NOT NULL,
//! #        last_seen_at REAL NOT NULL);
//! #      INSERT INTO memory_docs VALUES
//! #        ('d1', 'document_notes', 'Plan', 'Ship v2.', '[]', '{}', 1700000000.0);
//! #      INSERT INTO user_profile VALUES ('f1', 'preference', 'tone', 'terse', 0.9, 1700000000.0);",
//! # )?;
//! # drop(db);
//! # let path = dir.path();
//! let workspace = LegacyWorkspace::open(path)?;
//!
//! let mut saved = Checkpoint::default();
//! for imported in workspace.items_from(&saved) {
//!     let imported = imported?;
//!     // engine.store(imported.item).await?;
//!     saved = imported.checkpoint; // persist it
//! }
//! assert_eq!(saved.documents.as_deref(), Some("d1"));
//! assert_eq!(saved.profile.as_deref(), Some("f1"));
//!
//! // A later run resumes after the last stored item: nothing is left.
//! assert_eq!(workspace.items_from(&saved).count(), 0);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod checkpoint;
mod convert;
mod counts;
mod error;
mod importer;
mod items;
mod migrate;
mod sections;
mod workspace;

pub use checkpoint::{Checkpoint, ChunkCursor, ImportedItem};
pub use counts::LegacyCounts;
pub use error::{Error, Result};
pub use importer::{
    Begun, CHECKPOINT_EVERY, CheckpointStore, Consent, Describe, FailedItem, ImportCounts,
    ImportError, ImportFile, ImportPhase, ImportState, Importer, MemoryCheckpointStore, Pacer,
    PauseCheck, ResumePlan, RetryPolicy, STORE_BATCH, count_legacy, is_insufficient_credits,
    open_legacy, record_resume_failure, skips_item,
};
pub use items::{DEFAULT_PAGE_SIZE, Items};
pub use migrate::{MigrationReport, migrate, migrate_with};
pub use sections::EXTERNAL_SYNC_TAG;
pub use workspace::LegacyWorkspace;

/// Re-exported so a host names the same item type the importer yields.
pub use tinymemory_api::StoreItem;
