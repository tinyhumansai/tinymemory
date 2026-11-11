//! TinyMemory integrations: everything that connects the core contract
//! ([`tinymemory_api`]) to the outside world.
//!
//! Each integration is a module behind a feature of (nearly) the same name:
//!
//! | Module | Feature | What it does |
//! | --- | --- | --- |
//! | [`cortex`], [`registry`], [`config`] | `cortex` (default) | The CortexDB engine, cached guarded bindings, and configuration |
//! | `documents` | `documents`, `documents-office` | Format sniffing and conversion to markdown, emitting `StoreItem::Document` |
//! | `brain` | `brain` | Converting files into the brain documents `tinymemory_tools::Brain` ingests, by source type |
//! | `sources` | `sources`, `sources-network` | Readers turning folders, files, links, GitHub, RSS, and conversations into `StoreItem`s |
//! | `safety` | `safety` | Secret and PII scrubbing plus safe [`safety::ScrubbingEngine`] and [`safety::TimedEngine`] decorators |
//! | `import` | `legacy-import` | Migrating a legacy v1 (embedded TinyCortex) workspace into any engine |
//!
//! A typical write path is source → documents → safety → engine; the
//! agent-facing tools and `context.md` live in `tinymemory-tools`.
//!
//! # Example
//!
//! ```no_run
//! # #[cfg(feature = "cortex")]
//! # async fn demo() -> tinymemory_integrations::Result<()> {
//! use std::sync::Arc;
//! use tinymemory_api::{FetchMode, FetchRequest, MemoryMeta, SourceKind, StoreItem};
//! use tinymemory_integrations::{
//!     EngineBinder, EngineCredential, MemoryConfig, cortex::StaticBearer, safety::Policy,
//! };
//!
//! let config = MemoryConfig::default(); // engine = "tinyhumans"
//! let engine = EngineBinder::default()
//!     .bind_configured(
//!         &config.engine,
//!         config.settings(),
//!         "host-session-credential-slot",
//!         EngineCredential::Dynamic(Arc::new(StaticBearer::new("tiny_live_..."))),
//!         Policy::default(),
//!     )
//!     .engine()?
//!     .engine;
//!
//! let meta = MemoryMeta::from_source(SourceKind::Folder, Some("notes".into()));
//! engine.store(StoreItem::document("Ownership moves values.", meta)).await?;
//! let page = engine.fetch(FetchRequest::new("ownership", FetchMode::Hybrid, 5)).await?;
//! # let _ = page;
//! # Ok(())
//! # }
//! ```

pub mod error;

#[cfg(feature = "cortex")]
pub mod config;
#[cfg(feature = "cortex")]
pub mod cortex;
#[cfg(feature = "cortex")]
pub mod registry;

#[cfg(feature = "brain")]
pub mod brain;
#[cfg(feature = "documents")]
pub mod documents;
#[cfg(feature = "legacy-import")]
pub mod import;
#[cfg(feature = "safety")]
pub mod safety;
#[cfg(feature = "sources")]
pub mod sources;

pub use error::{Error, Result};

#[cfg(feature = "cortex")]
pub use config::{DEFAULT_ENGINE, EngineSettings, MemoryConfig};
#[cfg(feature = "cortex")]
pub use cortex::{BearerSource, StaticBearer};
#[cfg(feature = "cortex")]
pub use registry::binder::{
    Binding, BoundEngine, CORTEXDB_ENGINE_ID, EndpointSource, EngineBinder, HOST_ENGINE_ENDPOINT,
    TINYHUMANS_ENGINE_ID,
};
#[cfg(feature = "cortex")]
pub use registry::{EngineCredential, build_engine, list_engines};
#[cfg(feature = "safety")]
pub use safety::{ScrubbingEngine, TimedEngine};
