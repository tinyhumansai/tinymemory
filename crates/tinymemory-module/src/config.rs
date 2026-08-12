//! What the host tells this module at load time.
//!
//! # Why configuration and not a constructor argument
//!
//! A module is `dlopen`ed; there is no Rust call to pass a struct to. `TinyBus`
//! carries borrowed JSON in the host vtable and the SDK copies and deserializes
//! it during initialization, which is what [`ModuleConfig`] is deserialized
//! from. The host supplies it with `ModuleHost::set_config` before the load.
//!
//! # What is deliberately absent: credentials
//!
//! [`ModuleConfig`] has no API key, no session token, and no cloud-provider
//! credential list, and that is the central decision of this module rather than
//! an omission.
//!
//! The engine needs embeddings to recall anything, and embedding means calling
//! an inference provider that wants a key. Handing the key over would also hand
//! over the host's routing, cost accounting and BYOK policy, all of which live
//! host-side. So the key stays where it is and the *compute* is what crosses:
//! the module asks the host to embed, over the bus. See [`crate::embedding`].
//!
//! This is the same split the `tinywallet` module makes with a signing key, for
//! the same reason. As there, a loaded module shares this address space, so the
//! split is not a hard isolation boundary and is not claimed as one — it is a
//! refusal to widen what crosses a boundary that already exists.
//!
//! # `MemoryConfig` travels whole
//!
//! The engine's own configuration is `tinymemory_api::host::MemoryConfig`,
//! which is already `Serialize`/`Deserialize` with `#[serde(default)]`. It is
//! embedded verbatim rather than re-declared field by field, so a field added
//! upstream reaches the engine without an edit here and cannot silently drift
//! from the host's copy of the same struct.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tinymemory_api::host::{
    EmbeddingRouteConfig, LocalAiConfig, MemoryConfig, MemoryTreeConfig, SchedulerGateConfig,
    StorageProviderConfig,
};

/// Everything this module needs to bring up a memory engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ModuleConfig {
    /// Where the engine keeps its store.
    ///
    /// The host owns this path: it is inside the host's workspace, the host has
    /// already applied whatever path policy it has, and the module does not
    /// second-guess it. An empty path is refused at setup rather than silently
    /// resolved against the process working directory, which would put a user's
    /// memory store somewhere nobody would look for it.
    pub workspace_dir: PathBuf,

    /// The engine's own configuration, passed through unchanged.
    pub memory: MemoryConfig,

    /// Summary-tree and language-model settings used by tree operations.
    pub memory_tree: MemoryTreeConfig,

    /// Background-work admission policy used by bounded maintenance steps.
    pub scheduler_gate: SchedulerGateConfig,

    /// Local model selection. Credentials remain host-side; this contains only
    /// routing and model identifiers.
    pub local_ai: LocalAiConfig,

    /// Resolved route for the embedding workload.
    pub embeddings_provider: Option<String>,

    /// Resolved route for memory summarisation/extraction.
    pub memory_provider: Option<String>,

    /// Default chat model identifier used for module-side summarisation.
    pub default_model: Option<String>,

    /// Sampling temperature for module-side background language tasks.
    pub default_temperature: f64,

    /// Optional output language for summaries and extracted artifacts.
    pub output_language: Option<String>,

    /// Serialized memory-source registry used by snapshot operations.
    #[serde(default = "empty_array")]
    pub memory_sources: serde_json::Value,

    /// Per-workload embedding routes, as the host resolved them.
    pub embedding_routes: Vec<EmbeddingRouteConfig>,

    /// Storage-provider selection, when the host has one configured.
    pub storage_provider: Option<StorageProviderConfig>,

    /// The Ollama base URL the host would use.
    ///
    /// Carried as data because `EmbeddingHost::ollama_base_url` is a synchronous
    /// getter and cannot make a bus call. It is only ever reported, never
    /// dialled: every embed goes through the host, so this module opens no
    /// network connection of its own.
    pub ollama_base_url: String,

    /// The host's default managed-cloud embedding model id.
    pub cloud_embedding_model: String,

    /// The dimensionality [`Self::cloud_embedding_model`] emits.
    pub cloud_embedding_dimensions: usize,

    /// Models the host will accept an explicit output dimensionality for.
    ///
    /// A list rather than a bus call because
    /// `EmbeddingHost::model_supports_dimensions` is synchronous. Absent from
    /// the list means "does not support it", which is the safe direction: the
    /// engine then omits the parameter instead of writing a batch the provider
    /// rejects halfway through.
    pub models_supporting_dimensions: Vec<String>,

    /// Driver id to advertise.
    ///
    /// Defaults to the tinycortex driver id, since that is the engine this
    /// module carries. Overridable so a host can bind two builds of the same
    /// engine distinguishably, but it must stay stable across restarts and must
    /// never embed a URL or a token — it appears in status output and audit
    /// events.
    pub driver_id: String,
}

impl Default for ModuleConfig {
    fn default() -> Self {
        Self {
            workspace_dir: PathBuf::new(),
            memory: MemoryConfig::default(),
            memory_tree: MemoryTreeConfig::default(),
            scheduler_gate: SchedulerGateConfig::default(),
            local_ai: LocalAiConfig::default(),
            embeddings_provider: None,
            memory_provider: None,
            default_model: None,
            default_temperature: 0.0,
            output_language: None,
            memory_sources: empty_array(),
            embedding_routes: Vec::new(),
            storage_provider: None,
            ollama_base_url: String::new(),
            cloud_embedding_model: String::new(),
            cloud_embedding_dimensions: 0,
            models_supporting_dimensions: Vec::new(),
            driver_id: tinymemory::registry::TINYCORTEX_DRIVER_ID.to_string(),
        }
    }
}

fn empty_array() -> serde_json::Value {
    serde_json::Value::Array(Vec::new())
}

impl ModuleConfig {
    /// Reject a configuration that cannot bring up a store.
    ///
    /// Only `workspace_dir` is checked. Everything else has a defensible
    /// default: an absent embedding model means the host answers with whatever
    /// it routes to, and an empty route list means the engine's own defaults
    /// apply. A missing workspace has no defensible default.
    ///
    /// # Errors
    ///
    /// A message naming the field, never its value — a path can identify a
    /// user, and module errors must not carry absolute paths.
    pub fn validate(&self) -> Result<(), String> {
        if self.workspace_dir.as_os_str().is_empty() {
            return Err("workspace_dir must be set".to_string());
        }
        Ok(())
    }

    /// Drop any credential that rode in on the embedded `MemoryConfig`.
    ///
    /// # Why this exists
    ///
    /// [`Self::memory`] is `tinymemory_api::host::MemoryConfig` carried verbatim,
    /// which is the right call for every other field — but it contains
    /// `agentmemory_secret`, a bearer token for a *remote* memory backend. So the
    /// module's "no credentials" property is not a property of
    /// [`ModuleConfig`]'s own field list after all; it has to be enforced, and
    /// this is where.
    ///
    /// Found by reading `MemoryConfig` field by field while debugging something
    /// unrelated. The structural test over this struct's own keys did not catch
    /// it, because the key is one level down — which is the general lesson:
    /// "carried verbatim" means credentials are carried verbatim too.
    ///
    /// # Why strip rather than refuse
    ///
    /// This module serves the local `tinycortex` engine. A remote-backend token
    /// is not something it can use, so refusing the whole load would turn an
    /// irrelevant leftover config field into a hard failure for a host whose
    /// memory would otherwise work. Stripping is silent to the engine and
    /// removes the token from this address space.
    ///
    /// A host that genuinely wants a remote memory backend should bind that
    /// driver directly rather than through this module, which is why the warning
    /// says so.
    ///
    /// Returns whether anything was removed, so the caller can log it once.
    pub fn strip_host_credentials(&mut self) -> bool {
        if self.memory.agentmemory_secret.take().is_some() {
            return true;
        }
        false
    }
}

#[cfg(test)]
#[path = "config_test.rs"]
mod test;
