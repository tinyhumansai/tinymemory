//! [`MemoryHostConfig`] — the memory subsystem's view of the host's config.
//!
//! # Why a trait and not a struct
//!
//! The host's `Config` is one giant serde struct covering voice, channels,
//! sandboxing, inference routing, the agent harness — the lot. The memory
//! subsystem reads about two dozen of its fields. Moving the whole struct into
//! this crate would drag the host's entire configuration vocabulary into a
//! contract crate that is meant to stay dependency-light; leaving it behind and
//! passing individual values would mean rewriting every function signature in
//! the extracted code.
//!
//! A trait threads the needle. `tinymemory_core::Config` is the alias
//! `dyn MemoryHostConfig`, so a function that took `config: &Config` before the
//! extraction still takes `config: &Config` after it, and the host's concrete
//! `Config` unsize-coerces at the call site with no edit at all. Only the field
//! *accesses* inside the extracted code change, from `config.workspace_dir` to
//! `config.workspace_dir()`.
//!
//! # Accessor shapes are chosen for zero churn, not for elegance
//!
//! Several accessors return `&PathBuf` / `&Vec<T>` where `&Path` / `&[T]` would
//! be the idiomatic choice. That is deliberate: the extracted code calls
//! `.clone()` on these values in dozens of places, and `&Path`/`&[T]` would
//! silently resolve `.clone()` to the *reference*'s `Clone` impl and fail at the
//! use site with a confusing type error. Returning the owning type keeps every
//! one of those sites compiling unchanged.
//!
//! # Mutation
//!
//! Three methods take `&mut self`. They exist because the extracted code owns
//! two write paths the host does not: the composio source-caps migration and
//! the CLI's env-override re-application. Everything else is read-only.

use std::path::PathBuf;

use super::cloud_providers::CloudProviderCreds;
use super::local_ai::LocalAiConfig;
use super::scheduler_gate::SchedulerGateConfig;
use super::storage_memory::{MemoryConfig, MemoryTreeConfig};

/// Composio routing mode: proxied through the host's cloud backend.
pub const COMPOSIO_MODE_BACKEND: &str = "backend";
/// Composio routing mode: BYO API key, calling `backend.composio.dev` directly.
pub const COMPOSIO_MODE_DIRECT: &str = "direct";

/// The subset of a host's Composio configuration the memory sync pipelines read.
///
/// Passed by value rather than by reference because the host's own
/// `ComposioConfig` carries fields (toolkit triage opt-outs, the enabled flag)
/// that have nothing to do with memory, and because borrowing it would pin the
/// host's type into this contract.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ComposioMode {
    /// [`COMPOSIO_MODE_BACKEND`] or [`COMPOSIO_MODE_DIRECT`].
    pub mode: String,
    /// The Composio entity the host authenticates as.
    pub entity_id: String,
    /// Direct-mode API key, when the user hand-wrote one into `config.toml`.
    /// The keychain-backed value takes precedence and is resolved host-side.
    pub api_key: Option<String>,
    /// Whether the LLM triage turn is switched off for all triggers.
    pub triage_disabled: bool,
}

impl std::fmt::Debug for ComposioMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComposioMode")
            .field("mode", &self.mode)
            .field("entity_id", &self.entity_id)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("triage_disabled", &self.triage_disabled)
            .finish()
    }
}

impl ComposioMode {
    /// True when the host routes Composio calls directly rather than through
    /// its cloud backend.
    #[must_use]
    pub fn is_direct(&self) -> bool {
        self.mode.eq_ignore_ascii_case(COMPOSIO_MODE_DIRECT)
    }
}

/// The host's configuration, as the memory subsystem sees it.
///
/// Implemented by the embedding application for its own root config type. See
/// the module docs for why this is a trait and why the accessor return types
/// are shaped the way they are.
#[async_trait::async_trait]
pub trait MemoryHostConfig: Send + Sync + std::fmt::Debug {
    // ── Paths ───────────────────────────────────────────────────────────────

    /// Root of the host's internal per-user state. Every memory database,
    /// summary-tree directory and queue file is resolved beneath this.
    fn workspace_dir(&self) -> &PathBuf;

    /// Absolute path of the `config.toml` this config was loaded from.
    fn config_path(&self) -> &PathBuf;

    /// Where chunk `.md` files are written. Either the explicit
    /// `memory_tree.content_dir` or `<workspace_dir>/memory_tree/content`.
    fn memory_tree_content_root(&self) -> PathBuf;

    // ── Memory-owned sections ───────────────────────────────────────────────

    /// The `[memory]` block — backend selection, embedding provider/model/dims,
    /// relevance floor, SQLite timeouts.
    fn memory(&self) -> &MemoryConfig;

    /// The `[memory_tree]` block — summary-tree embedder, extractor and
    /// summariser wiring.
    fn memory_tree(&self) -> &MemoryTreeConfig;

    /// The `[scheduler_gate]` block — when background LLM-bound work may run.
    fn scheduler_gate(&self) -> &SchedulerGateConfig;

    // ── Host-owned sections the memory subsystem still reads ────────────────
    //
    // These are the seam's rough edge (see the module docs on `host`): they are
    // read only to *construct* embedding providers, which is work that belongs
    // in the host. Moving the embedding factory back out of the core would let
    // all four of these accessors go away.

    /// The `[local_ai]` block — whether a local runtime is enabled and which
    /// model it serves.
    fn local_ai(&self) -> &LocalAiConfig;

    /// Configured cloud LLM/embedding backends, keyed by user-chosen slug.
    fn cloud_providers(&self) -> &Vec<CloudProviderCreds>;

    /// `provider:model` routing string for the embeddings workload, if pinned.
    fn embeddings_provider(&self) -> Option<&str>;

    /// `provider:model` routing string for the memory workload, if pinned.
    fn memory_provider(&self) -> Option<&str>;

    /// The memory **engine** this host selects, when its configuration names
    /// one — `tinycortex`, `supermemory`, `mem0`, `cognee`, `null`.
    ///
    /// Deliberately distinct from [`Self::memory_provider`], which despite the
    /// name is a `provider:model` routing string for the memory *workload* —
    /// which language model does summarisation and entity extraction. That is a
    /// different axis from which store the memory lives in, and conflating them
    /// would let a model change repoint a company's storage.
    ///
    /// `None` means "the host's default", which the host resolves rather than
    /// this trait: the driver registry admits a reserved embedded id with no
    /// configuration entry precisely so an unconfigured host still binds
    /// something instead of failing to start.
    ///
    /// Defaulted so adding it breaks no existing implementation.
    fn memory_driver(&self) -> Option<&str> {
        None
    }

    /// The local model id for a workload, when that workload is routed to
    /// Ollama (`"ollama:<model>"`). `None` for cloud or unset workloads.
    ///
    /// This is the single source of truth for "is this workload local?" —
    /// callers must not consult the deprecated `local_ai.usage.*` booleans or
    /// `memory_tree.llm_backend`.
    fn workload_local_model(&self, workload: &str) -> Option<String>;

    // ── Scalars ─────────────────────────────────────────────────────────────

    /// The concrete config behind this trait object, for host code that needs
    /// its own type back.
    ///
    /// The seam deliberately hands the core a `dyn MemoryHostConfig`, and that
    /// is the right shape for everything the core does. But a *host*
    /// implementation of one of the behavioural seams — chat-model routing,
    /// Composio mode dispatch — is handed the same trait object and has to get
    /// its own `Config` back: routing reads BYOK fallbacks, per-role routes and
    /// credentials, none of which are on this trait and none of which should
    /// be.
    ///
    /// Implementations return `self`. A host downcasts and, on failure, falls
    /// back to whatever it was configured with — a failure means the config is
    /// somebody else's type (a test double), not that something is wrong.
    fn as_any(&self) -> &dyn std::any::Any;

    /// An owned, shareable handle to this config.
    ///
    /// `tinymemory_core::Config` is the *unsized* `dyn MemoryHostConfig`, which
    /// makes `&Config` free at every call site — the host's concrete `Config`
    /// unsize-coerces with no edit. The cost is that a borrow cannot be turned
    /// into an owned value: background loops that outlive their caller, structs
    /// that hold a config, and `spawn_blocking` bodies all need one.
    ///
    /// This is that escape hatch. Implementations return
    /// `Arc::new(self.clone())`; callers that only read should keep taking
    /// `&Config` rather than reaching for this.
    fn to_arc(&self) -> std::sync::Arc<dyn MemoryHostConfig>;

    /// Backend base URL, used to recognise first-party endpoints.
    fn api_url(&self) -> Option<&str>;

    /// The backend API URL this host actually talks to, with the host's own
    /// environment and default resolution already applied.
    ///
    /// Distinct from [`Self::api_url`], which is the raw configured value —
    /// resolution (env override, staging/prod default, trailing-slash
    /// normalisation) is host logic and must not be re-derived here.
    fn effective_backend_api_url(&self) -> String;

    /// The current backend session bearer, or `None` when signed out.
    ///
    /// Read through the trait rather than from a config field because the host
    /// keeps it in its credential store, not in `config.toml`.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the credential store cannot be read — distinct from
    /// `Ok(None)`, which means "read fine, not signed in".
    fn session_token(&self) -> Result<Option<String>, String>;

    /// Default chat model id.
    fn default_model(&self) -> Option<&str>;

    /// Default sampling temperature for background LLM calls.
    fn default_temperature(&self) -> f64;

    /// Optional language for background LLM artifacts — tree summaries,
    /// extraction reasons, learning reflections. `None` keeps the default.
    fn output_language(&self) -> Option<&str>;

    /// Global memory-sync cadence in seconds. `None` means "no explicit choice"
    /// and callers fall back to [`super::DEFAULT_MEMORY_SYNC_INTERVAL_SECS`];
    /// `Some(0)` means manual-only.
    fn memory_sync_interval_secs(&self) -> Option<u64>;

    /// Whether the user has finished onboarding. Background ingestion holds off
    /// until they have.
    fn onboarding_completed(&self) -> bool;

    /// Whether at-rest secret encryption is switched on for this workspace.
    fn secrets_encrypt(&self) -> bool;

    /// Composio routing mode + credentials, as the sync pipelines need them.
    fn composio(&self) -> ComposioMode;

    // ── Memory sources ──────────────────────────────────────────────────────
    //
    // Serde-mediated on purpose. `MemorySourceEntry` is defined by the *engine*
    // crate (`tinycortex`), which this contract crate must not depend on — it
    // would drag SQLite in and break the dependency-light guarantee this crate
    // exists to hold. JSON is the narrowest waist that keeps the type where it
    // belongs.

    /// The persisted `[[memory_sources]]` registry, as JSON.
    ///
    /// # Errors
    /// Propagates a serialization failure from the host's own entry type.
    fn memory_sources_json(&self) -> anyhow::Result<serde_json::Value>;

    /// Replace the persisted `[[memory_sources]]` registry. Does not save to
    /// disk — call [`Self::save`] afterwards.
    ///
    /// # Errors
    /// Returns an error when `value` does not deserialize into the host's entry
    /// type, in which case the registry is left untouched.
    fn set_memory_sources_json(&mut self, value: serde_json::Value) -> anyhow::Result<()>;

    // ── Migration bookkeeping ───────────────────────────────────────────────

    /// Version of the composio source-capabilities migration already applied.
    fn composio_source_caps_migration_version(&self) -> u32;

    /// Record that the composio source-capabilities migration has run.
    fn set_composio_source_caps_migration_version(&mut self, version: u32);

    // ── Lifecycle ───────────────────────────────────────────────────────────

    /// Re-apply the host's environment-variable overlay over this config.
    /// Used by the CLI entry points, which build a config before the host's
    /// normal load path has run.
    fn apply_env_overrides(&mut self);

    /// Persist this config back to [`Self::config_path`] atomically.
    ///
    /// # Errors
    /// Propagates the host's own write/serialize failure.
    async fn save(&self) -> anyhow::Result<()>;
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
