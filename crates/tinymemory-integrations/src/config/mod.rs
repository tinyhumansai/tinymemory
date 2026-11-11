//! [`MemoryConfig`]: which engine a host uses and how each is reached.
//!
//! The config holds no credential. A host keeps its keys in its own secret
//! store and hands one to [`crate::registry::build_engine`] as an
//! [`crate::registry::EngineCredential`], so a config file can be shared or
//! logged.
//!
//! The same shape is read from TOML or JSON:
//!
//! ```toml
//! engine = "cortexdb"
//!
//! [engines.cortexdb]
//! endpoint = "https://cortex.example.com"
//! consolidation = "automatic"   # optional: this server builds beliefs itself
//! ```
//!
//! `engines` is optional, an engine with no entry uses its defaults, and a
//! blank or absent `endpoint` means the engine's default endpoint. Unknown
//! fields are ignored when reading.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tinymemory_api::{Consolidation, MemoryEngine, Result};

use crate::registry::{EngineCredential, build_engine};

/// The engine a fresh config selects.
pub const DEFAULT_ENGINE: &str = crate::cortex::TINYHUMANS_ENGINE_ID;

/// Which engine a host uses, and per-engine settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryConfig {
    /// The selected engine's id (see [`crate::registry::list_engines`]).
    pub engine: String,
    /// Settings per engine id. An engine with no entry uses its defaults.
    #[serde(default)]
    pub engines: BTreeMap<String, EngineSettings>,
}

impl Default for MemoryConfig {
    /// Selects [`DEFAULT_ENGINE`] with no per-engine settings.
    fn default() -> Self {
        Self {
            engine: DEFAULT_ENGINE.to_string(),
            engines: BTreeMap::new(),
        }
    }
}

impl MemoryConfig {
    /// The selected engine's settings, or the defaults when it has none.
    #[must_use]
    pub fn settings(&self) -> EngineSettings {
        self.engines.get(&self.engine).cloned().unwrap_or_default()
    }

    /// Builds the selected engine.
    ///
    /// # Errors
    ///
    /// As [`build_engine`].
    pub fn build(&self, credential: EngineCredential) -> Result<Arc<dyn MemoryEngine>> {
        build_engine(&self.engine, &self.settings(), credential)
    }
}

/// How one engine is reached.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EngineSettings {
    /// The engine's base URL; `None` uses the engine's default endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Fixed headers sent on every request, such as the host's product
    /// attribution (`x-sdk-name`). Never a credential: the transport refuses
    /// `Authorization` and the other headers it sets itself.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// How the engine consolidates, overriding its default: a `cortexdb`
    /// engine is `automatic` on CortexDB's managed API and `on_demand`
    /// anywhere else, and `tinyhumans` is always `scheduled`. Set
    /// `automatic` for a self-hosted CortexDB that runs its own layer
    /// scheduler.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consolidation: Option<Consolidation>,
    /// The scope every item is laid out below (layout v3), such as one
    /// person's `org:<id>`; unset keeps the legacy `app:tinymemory` tree
    /// (unless [`EngineSettings::tenant_root`]).
    /// Switching it moves nothing: memory under the other layout is no
    /// longer read until a host moves it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_root: Option<String>,
    /// The actor that owns [`EngineSettings::scope_root`] (`user:<id>`): a
    /// direct CortexDB engine registers the root with it before its first
    /// write. Ignored without a root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_owner: Option<String>,
    /// Layout v3 relative to the hosted tenant's own root, which the
    /// TinyHumans backend pins (`org:<id>`): no root segment is sent.
    /// `tinyhumans` engine only; overrides [`EngineSettings::scope_root`].
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tenant_root: bool,
    /// A root an earlier v3 layout wrote below (`user:<id>`), still read
    /// and forgotten, never written, until its memory has moved. Needs a
    /// v3 layout ([`EngineSettings::scope_root`] or
    /// [`EngineSettings::tenant_root`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retired_scope_root: Option<String>,
    /// Attribute events to who actually said or did them (CortexDB's
    /// `observed_actor`, with the memory's owner as `subject`): an assistant
    /// turn to its agent, a user turn or an item naming a
    /// [`tinymemory_api::ObservedActor`] to that person. Off by default, and off nothing on the wire changes.
    /// Only a direct `cortexdb` engine honours it; a write CortexDB refuses
    /// for it is sent again without it (see the cortex README).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub observed_actor: bool,
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
