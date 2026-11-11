//! Source readers for TinyMemory: turn a folder, a file, a web page, a GitHub
//! repository, an RSS feed or a local conversation
//! into [`StoreItem`](tinymemory_api::StoreItem)s.
//!
//! - **Configuration** — reader entries ([`MemorySourceEntry`]) and the
//!   host's persisted source types ([`MemorySourceConfig`], keyed by
//!   [`MemorySourceKind`]). [`SourceSpec`] normalizes targets and checks
//!   duplicate, schedule and namespace rules; the host owns persistence and
//!   legacy migration.
//! - **Readers** — [`readers::SourceReader`] lists a source's items and reads
//!   one. Local readers (folder, file, conversation) are always compiled; the
//!   network readers (GitHub, RSS, web page) and `fetch` sit behind the
//!   `sources-network` feature; RSS and web pages fetch through `fetch`,
//!   behind its one SSRF guard (`fetch::ssrf`).
//! - **Items** — [`items`] maps reader output to `StoreItem`s with
//!   [`MemoryMeta`](tinymemory_api::MemoryMeta) filled per kind; every text
//!   body is converted to markdown through [`crate::documents`].
//!
//! Scheduling, credentials and egress budgets stay with the host: this module
//! reads when asked. Sync-state storage and enqueueing placed-item belief jobs
//! are host ports; [`state`] and [`sync`] provide the shared types and logic.
//!
//! # Example
//!
//! ```
//! use tinymemory_api::{SourceKind as ApiKind, StoreItem};
//! use tinymemory_integrations::documents::ConverterChain;
//! use tinymemory_integrations::sources::{items, readers, MemorySourceEntry, SourceKind};
//!
//! # let runtime = tokio::runtime::Builder::new_current_thread().build()?;
//! # runtime.block_on(async {
//! let workspace = tempfile::tempdir()?;
//! std::fs::create_dir(workspace.path().join("notes"))?;
//! std::fs::write(workspace.path().join("notes/plan.md"), "# Plan\n\nShip v2.")?;
//! std::fs::write(workspace.path().join("notes/build.rs"), "fn main() {}\n")?;
//!
//! let mut entry = MemorySourceEntry::new("src_notes", SourceKind::Folder, "Notes");
//! entry.path = Some("notes".into());
//! let reader = readers::reader_for(&entry.kind).expect("folders are local");
//!
//! let converter = ConverterChain::default();
//! let mut collected =
//!     items::collect_items(reader.as_ref(), &entry, workspace.path(), &converter).await?;
//! collected.items.sort_by_key(|item| item.meta().file_path.clone());
//!
//! let rust = &collected.items[0];
//! assert_eq!(rust.meta().source.kind, ApiKind::Folder);
//! assert_eq!(rust.meta().source.id.as_deref(), Some("src_notes"));
//! assert_eq!(rust.meta().language.as_deref(), Some("rust"));
//! let StoreItem::Document { title, .. } = &collected.items[1] else {
//!     unreachable!("folder sources produce documents");
//! };
//! assert_eq!(title.as_deref(), Some("Plan"));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! # })?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Feature flags
//!
//! - `sources` — everything above except the network pieces. Implies
//!   `documents`; it includes source validation and placement support but no
//!   HTTP client.
//! - `sources-network` — the GitHub, RSS and web-page readers, `fetch`,
//!   `readers::reader_for_request` and the SSRF guard. Without it, a host that
//!   only reads local sources links no HTTP stack.
//! - `schema` — JSON Schema derives for source configuration and reader types;
//!   downstream hosts enable it only when they generate schemas.

pub mod error;
#[cfg(feature = "sources-network")]
pub mod fetch;
pub mod items;
pub mod readers;
pub mod spec;
pub mod state;
pub mod sync;
pub mod types;

/// Largest file a folder or file source will read.
pub const FOLDER_FILE_SIZE_CAP_BYTES: u64 = 10 * 1024 * 1024;

pub use error::{Error, Result};
pub use items::{Collected, collect_items, content_item, conversation_item, file_item};
pub use spec::{SourceSpec, layout_of, layout_of_source};
pub use state::{SourceState, SourceStatus, reset_interrupted};
pub use types::{
    ContentType, MemorySourceConfig, MemorySourceEntry, MemorySourceKind, SourceContent,
    SourceItem, SourceKind,
};
