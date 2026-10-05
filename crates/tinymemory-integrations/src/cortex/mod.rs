//! The CortexDB memory engine for TinyMemory v2.
//!
//! [`CortexEngine`] implements [`tinymemory_api::MemoryEngine`] over
//! CortexDB's append-only event log, on either of two wires:
//!
//! - **`cortexdb`** ([`CortexEngine::direct`]) — a CortexDB server's own
//!   `/v1/*` API with a bearer API key;
//! - **`tinyhumans`** ([`CortexEngine::tinyhumans`]) — CortexDB behind the
//!   TinyHumans backend's `/memory/*` routes, with the host's session JWT or
//!   API key resolved from a [`BearerSource`] on every request.
//!
//! Both declare [`tinymemory_api::FetchMode::Hybrid`] only: CortexDB's
//! recall body has no keyword/vector switch (see [`cortexdb_descriptor`]).
//!
//! Hosts normally build an engine through [`crate::registry::build_engine`] or
//! [`crate::config::MemoryConfig::build`] rather than naming
//! [`CortexEngine`]. Errors are the contract's [`tinymemory_api::Error`]; see
//! [`error_code`] and [`is_insufficient_credits`] for hosted failures.
//!
//! # Storage layout
//!
//! Items live in one scope per kind under the TinyMemory root:
//! `app:tinymemory/app:documents`, `app:tinymemory/app:conversations`,
//! `app:tinymemory/app:learnings`. A document or learning is one event; a
//! conversation is one event per turn. Each event's text is a JSON envelope
//! (`"v": 2`) carrying the item id ([`tinymemory_api::StoreItem::fingerprint`]),
//! kind, text and full metadata, and each event carries lookup labels (digests
//! of the item id and of the exact-match metadata fields) so reads can narrow
//! server-side before the full [`tinymemory_api::MetaFilter`] is applied
//! client-side. This module's `README.md` summarises the layout and every
//! engine behaviour it is shaped around; `docs/architecture/cortex.md`,
//! `cortex-wire.md` and `cortex-flows.md` give the full reference.
//!
//! # Example
//!
//! ```no_run
//! use std::sync::Arc;
//! use tinymemory_api::{MemoryEngine, MemoryMeta, SourceKind, StoreItem};
//! use tinymemory_integrations::cortex::{CortexCredential, CortexEngine, StaticBearer, CORTEX_API_ENDPOINT};
//!
//! # async fn demo() -> tinymemory_integrations::cortex::Result<()> {
//! let direct = CortexEngine::direct(CORTEX_API_ENDPOINT, CortexCredential::api_key("ctx_..."))?;
//! let hosted = CortexEngine::tinyhumans(
//!     tinymemory_integrations::cortex::TINYHUMANS_API_ENDPOINT,
//!     Arc::new(StaticBearer::new("tiny_live_...")),
//! )?;
//!
//! let meta = MemoryMeta::from_source(SourceKind::Agent, None);
//! let receipt = direct.store(StoreItem::document("Ownership moves values.", meta)).await?;
//! assert!(!receipt.replayed);
//! # let _ = hosted;
//! # Ok(())
//! # }
//! ```

mod credential;
mod descriptor;
mod engine;
mod envelope;
mod error;
mod log;
mod transport;

#[cfg(test)]
mod testing;

#[cfg(test)]
#[path = "conformance_tests.rs"]
mod conformance_tests;

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod lifecycle_tests;

pub use credential::{BearerSource, CortexCredential, StaticBearer};
pub use descriptor::{
    CORTEX_API_ENDPOINT, CORTEXDB_ENGINE_ID, CortexWire, TINYHUMANS_API_ENDPOINT,
    TINYHUMANS_ENGINE_ID, cortexdb_descriptor, tinyhumans_descriptor,
};
pub use engine::CortexEngine;
pub use error::{Error, Result, error_code, is_insufficient_credits};
