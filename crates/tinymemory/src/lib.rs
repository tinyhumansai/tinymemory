//! TinyMemory: recall, fetch and store over pluggable memory engines.
//!
//! The facade a host depends on. It re-exports the contract
//! ([`MemoryEngine`], [`StoreItem`], [`MetaFilter`], ...), registers the
//! engines this build can construct ([`list_engines`]), and builds one from
//! configuration ([`MemoryConfig`], [`build_engine`]). Every other crate of
//! the workspace is reachable through a feature named after it:
//!
//! | Feature | Module | What it adds |
//! | --- | --- | --- |
//! | `documents` | `documents` | format sniffing and conversion to markdown |
//! | `documents-office` | `documents` | `OfficeConverter`: PDF, DOCX, PPTX and XLSX to markdown |
//! | `sources` / `sources-network` | `sources` | source readers emitting `StoreItem`s |
//! | `safety` | `safety` | secret and PII scrubbing before `store` |
//! | `context` | `context` | the `context.md` compiler |
//! | `import` / `legacy-import` | `import` | the legacy v1 workspace reader |
//! | `conformance` | `conformance` | the behavioural suite and reference engine |
//! | `full` | | all of the above |
//!
//! With no feature the facade is the contract, the registry and the CortexDB
//! engines.
//!
//! # Example
//!
//! ```
//! use tinymemory::{EngineCredential, MemoryConfig, list_engines};
//!
//! let ids: Vec<&str> = list_engines().iter().map(|d| d.id).collect();
//! assert_eq!(ids, ["cortexdb", "tinyhumans"]);
//!
//! let config: MemoryConfig = serde_json::from_str(r#"{ "engine": "cortexdb" }"#)?;
//! let engine = config.build(EngineCredential::Static("cortex-api-key".into()))?;
//! assert_eq!(engine.descriptor().id, "cortexdb");
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub mod config;
pub mod registry;

pub use config::{DEFAULT_ENGINE, EngineSettings, MemoryConfig};
pub use registry::{EngineCredential, build_engine, list_engines};
pub use tinymemory_api::*;
pub use tinymemory_cortex::{BearerSource, StaticBearer};

/// The contract crate, by name.
pub use tinymemory_api as api;
/// The CortexDB engines (`cortexdb`, `tinyhumans`).
pub use tinymemory_cortex as cortex;

/// Format sniffing and conversion to markdown; `documents::OfficeConverter`
/// (PDF, DOCX, PPTX, XLSX) needs `documents-office` as well.
#[cfg(feature = "documents")]
pub use tinymemory_documents as documents;

/// Source readers that emit `StoreItem`s.
#[cfg(feature = "sources")]
pub use tinymemory_sources as sources;

/// Secret and PII scrubbing applied before `store`.
#[cfg(feature = "safety")]
pub use tinymemory_safety as safety;

/// The `context.md` compiler.
#[cfg(feature = "context")]
pub use tinymemory_context as context;

/// The legacy v1 workspace reader.
#[cfg(feature = "import")]
pub use tinymemory_import as import;

/// The behavioural suite and reference engine.
#[cfg(feature = "conformance")]
pub use tinymemory_conformance as conformance;
