//! The **host seam** — everything `tinymemory-core` needs from the application
//! that embeds it, expressed as object-safe traits plus the plain serde config
//! structs the memory subsystem owns.
//!
//! # Why this module exists
//!
//! `tinymemory-core` holds the substance of a memory subsystem: the store, the
//! summary tree, the sync pipelines, ingestion, recall. Per the repository
//! README's split, the *host* keeps the RPC surface, the agent tools, the
//! security policy, the schedulers, the event bus, and config loading. That
//! split only works if the core can name what it needs from the host without
//! naming the host itself — which is what these traits are.
//!
//! # The three seams
//!
//! - [`MemoryHostConfig`] — the host's configuration, read through accessor
//!   methods rather than public fields. `tinymemory_core::Config` is the type
//!   alias `dyn MemoryHostConfig`, so code moved out of the host keeps writing
//!   `config: &Config` and the host's concrete `Config` unsize-coerces at every
//!   call site.
//! - [`EmbeddingProvider`] — text → vector. The core never builds one; the host
//!   resolves provider credentials, rate limits and routing and hands an
//!   `Arc<dyn EmbeddingProvider>` down.
//! - [`MemoryEventSink`] — the handful of domain events the memory subsystem
//!   publishes. The host implements it by publishing its own event enum onto
//!   its own bus; the core never learns that enum exists.
//!
//! # Config *sections* live here, config *loading* does not
//!
//! [`MemoryConfig`], [`MemoryTreeConfig`], [`MemorySubsystemConfig`] and friends
//! moved here from the host because the core reads their fields directly and a
//! trait accessor per field would be absurd. They are inert serde/`schemars`
//! data with no behaviour, and **their serde representation is persisted in
//! users' `config.toml`** — field names, defaults, and `#[serde(...)]`
//! attributes are a compatibility surface, not an implementation detail.
//!
//! Sections that are *not* memory-owned but that the core still reads
//! ([`LocalAiConfig`], [`cloud_providers`]) are here for the same mechanical
//! reason. They are the seam's rough edge: the honest fix is to move embedding
//! *construction* back into the host, at which point the core stops reading
//! them and they can go home.

pub mod cloud_providers;
pub mod local_ai;
pub mod scheduler_gate;
pub mod scheduler_gate_decide;
pub mod storage_memory;
pub mod subsystems;

mod config;
mod embedding_host;
mod embeddings;
mod error_reporter;
mod events;
mod nlp;
mod routes;
mod usage;

#[cfg(feature = "test-support")]
pub mod test_support;

pub use cloud_providers::{
    endpoint_host, generate_provider_id, is_slug_reserved, migrate_legacy_fields, AuthStyle,
    CloudProviderCreds, CloudProviderType,
};
pub use config::{ComposioMode, MemoryHostConfig, COMPOSIO_MODE_BACKEND, COMPOSIO_MODE_DIRECT};
pub use embedding_host::EmbeddingHost;
pub use embeddings::{format_embedding_signature, EmbeddingProvider, NoopEmbedding};
pub use error_reporter::ErrorReporter;
pub use events::{
    EmbeddingHealthReason, MemoryEvent, MemoryEventSink, NoopEventSink, SyncTrigger,
    LOCAL_MODEL_UNAVAILABLE_KIND, MEMORY_USER_ERROR_SOURCE, STORE_CORRUPT_KIND,
};
pub use local_ai::{LocalAiConfig, LocalAiUsage};
pub use nlp::{SpacyEntity, SpacyResponse};
pub use routes::EmbeddingRouteConfig;
pub use scheduler_gate::{PauseReason, Policy, SchedulerGateConfig, SchedulerGateMode};
pub use scheduler_gate_decide::{decide, Signals};
pub use storage_memory::{
    LlmBackend, MemoryConfig, MemoryTreeConfig, StorageConfig, StorageProviderConfig,
    StorageProviderSection, DEFAULT_CLOUD_LLM_MODEL,
};
pub use subsystems::{
    MemoryDriverConfig, MemoryHooksConfig, MemorySubsystemConfig, SubsystemsConfig,
};
pub use tinymemory_bus::evidence::EvidenceRef;
pub use usage::UsageInfo;

/// Effective default global memory-sync cadence (seconds) used when
/// [`MemoryHostConfig::memory_sync_interval_secs`] is `None` — i.e. the user has
/// not explicitly picked a schedule. 24h, matching the "Sync every 24h" preset
/// surfaced in the Memory Sources UI.
pub const DEFAULT_MEMORY_SYNC_INTERVAL_SECS: u64 = 86_400;
