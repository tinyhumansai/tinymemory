//! TinyMemory — the engine-neutral memory layer.
//!
//! A host that embeds TinyMemory performs every memory operation through one
//! contract, and picks which engine answers it by configuration rather than by
//! recompiling. TinyCortex is the default embedded engine; a second engine
//! (`supermemory`, `mem0`, a self-hosted HTTP backend) implements the same
//! traits and binds in its place without the host learning anything new.
//!
//! ## What is here
//!
//! - **The contract** — [`tinymemory_api`], re-exported wholesale below, so a
//!   host takes one dependency and `tinymemory::provider::MemoryProvider` and
//!   `tinymemory_api::provider::MemoryProvider` are the same type. It is
//!   deliberately dependency-light: depending on the contract never drags in
//!   SQLite, git2, reqwest, or an async runtime.
//! - **[`mandatory`]** — the three mandatory capability families, composed
//!   once over the [`traits::Memory`] storage trait, so every backend that
//!   implements it inherits a correct `store` / `list` / `recall` / export
//!   rather than re-deriving the same four subtleties.
//! - **[`registry`]** — driver admission. Which driver ids exist, what class
//!   each binds as, and the fail-closed rule for out-of-process drivers.
//! - **[`sections`]** — typed surfaces for the sections the namespace
//!   convention names: conversations, learnings, documents, and a
//!   section-aware recall. Composes the mandatory families only, so it works
//!   on every driver.
//! - **Engine adapters** — one crate per engine under `crates/`, each
//!   implementing [`provider::MemoryProvider`] over a concrete engine, and
//!   each selected by the feature named after it.
//! - **Subsystems** — [`core`], [`sync`], [`sources`] and [`conformance`],
//!   re-exported behind features of the same name so a host takes one
//!   dependency on this crate and states what it wants.
//!
//! ## What is deliberately *not* here
//!
//! Policy. A host that binds a memory driver is responsible for tier
//! enforcement, scope predicates, taint stamping, redaction, egress checks, and
//! audit — and it must apply them in a decorator it owns, on the path every
//! caller takes. Pushing any of that into the engine layer would mean a driver
//! could be swapped for one that does not enforce it, which is the whole reason
//! the policy layer exists.
//!
//! Also not here: the host's RPC surface, its agent tools, its credential
//! storage, and its schedulers. Those are what makes a host a host; an engine
//! that learned about them could no longer be replaced by a different engine.
//!
//! ## Binding, end to end
//!
//! ```no_run
//! use tinymemory::null::NullMemoryProvider;
//! use tinymemory::provider::MemoryProvider;
//! use tinymemory::registry::{ConfigLabels, DriverClass, DriverRegistry};
//! use std::sync::Arc;
//!
//! let registry = DriverRegistry::builtin();
//! let provider: Arc<dyn MemoryProvider> =
//!     match registry.admit("tinycortex", None, ConfigLabels::default()) {
//!         Ok(admitted) => match admitted.class {
//!             // The host constructs the engine adapter it compiled in.
//!             DriverClass::Embedded => unimplemented!("bind the engine adapter"),
//!             _ => Arc::new(NullMemoryProvider::new()),
//!         },
//!         // Refusal is not failure: stay bound, loudly.
//!         Err(fallback) => {
//!             eprintln!("{fallback}");
//!             Arc::new(NullMemoryProvider::new())
//!         }
//!     };
//!
//! // Ask once, at bind time, and cache: filtering an RPC surface from a set
//! // that can change underneath it is worse than not filtering at all.
//! let capabilities = provider.capabilities();
//! ```

/// The mandatory-family composition, re-exported from the contract crate.
///
/// The module itself moved to `tinymemory-api` so the adapters can reach it
/// without depending on this crate — which is what lets this crate depend on
/// *them* and declare the per-engine features (#18 §D1). Re-exported rather
/// than relocated silently: `tinymemory::mandatory::MemoryTraitProvider` is a
/// path downstream code already uses.
pub use tinymemory_api::mandatory;

/// The bundled TinyCortex embedded engine, when the `tinycortex` feature is on.
///
/// Re-exported so a host selects an engine by feature rather than by taking a
/// second dependency: `tinymemory = { features = ["tinycortex"] }` is the whole
/// wiring, and `tinymemory::tinycortex::provider(backend)` binds it (#18 §D1).
#[cfg(feature = "tinycortex")]
pub use tinymemory_tinycortex as tinycortex;

/// The HTTP engines, when any remote-engine feature is on.
///
/// One module for all remote engines because they share one adapter crate — enabling
/// two of them costs one dependency, not two. The per-engine features still
/// exist so a host states which it actually uses, and so a future split can
/// happen without changing how hosts ask for them.
#[cfg(any(
    feature = "supermemory",
    feature = "mem0",
    feature = "cognee",
    feature = "cortex",
    feature = "agentmemory",
    feature = "livingbrain"
))]
pub use tinymemory_remote as remote;

/// The engine-neutral memory subsystem, when the `core` feature is on.
///
/// The store, summary tree, sync pipelines, ingestion and recall. This is the
/// heaviest thing the workspace offers — it links a bundled SQLite and the
/// embedded engine — which is why it is a feature rather than a dependency
/// every consumer of the contract pays for.
#[cfg(feature = "core")]
pub use tinymemory_core as core;

/// The Composio payload normalisers, when the `sync` feature is on.
///
/// Pure `Value -> Value` transforms with no engine behind them, which is the
/// point of the crate: a host binding a driver that is not TinyCortex can still
/// run them.
#[cfg(feature = "sync")]
pub use tinymemory_sync as sync;

/// The memory-source contracts and readers, when the `sources` feature is on.
///
/// The readers that fetch over the network — GitHub, RSS, web pages — sit
/// behind `sources-network` on top of this, so a host that only reads local
/// folders links no HTTP stack.
#[cfg(feature = "sources")]
pub use tinymemory_sources as sources;

/// Document and URL intake — [`documents::DocumentIntake`]: sniff a format,
/// convert it to markdown, and write it into whichever engine is bound.
///
/// Behind the `documents` feature; the URL half needs `documents-network`,
/// which links an HTTP stack. Deliberately *not* part of the mandatory
/// composition: a host that stores only what its agent produces converts
/// nothing and should link none of this.
#[cfg(feature = "documents")]
pub use tinymemory_documents as documents;

/// The behavioural conformance suite, when the `conformance` feature is on.
///
/// Every driver admitted by [`registry`] must pass it. Exposed here so a host
/// can hold a `MemoryProvider` of its own to the same contract the bundled
/// adapters are held to, without taking a second dependency.
#[cfg(feature = "conformance")]
pub use tinymemory_conformance as conformance;

pub mod registry;
pub mod routing;
pub use routing::MemoryApi;

// Typed surfaces for the sections the namespace convention names —
// conversations, learnings, documents — plus a section-aware recall. Documented
// by its own `//!` docs; an outer doc comment here as well would merge the two
// and resolve the module's intra-doc links in this file's scope instead.
pub mod sections;

// The contract, re-exported wholesale. Listed module by module rather than as a
// glob so the crate's own surface is visible in one place and rustdoc links
// resolve — and so adding a module to the contract is a deliberate act here too.
pub use tinymemory_api::{
    capabilities, chunks, error, goals, health, namespace, null, operations, provider, recall,
    tool_memory, traits, tree, types,
};
pub use tinymemory_api::{is_compatible, CONTRACT_VERSION};

/// The contract crate itself, for callers that want to name it explicitly.
pub use tinymemory_api as api;
