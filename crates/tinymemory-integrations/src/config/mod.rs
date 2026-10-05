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
//! tenancy = "org:acme/user:alice"   # or "single_user"
//! ```
//!
//! `engines` is optional, an engine with no entry uses its defaults, and a
//! blank or absent `endpoint` means the engine's default endpoint. Unknown
//! fields are ignored when reading.
//!
//! `tenancy` has no default. The `cortexdb` engine refuses to build without
//! one, so a deployment that forgot to say whose memory a shared key holds
//! fails at startup instead of writing every user into one scope tree (see
//! [`crate::cortex::CortexTenancy`]). The `tinyhumans` engine refuses one:
//! its backend pins the tenant.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tinymemory_api::{MemoryEngine, Result};

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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineSettings {
    /// The engine's base URL; `None` uses the engine's default endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Fixed headers sent on every request, such as the host's product
    /// attribution (`x-sdk-name`). Never a credential: the transport refuses
    /// `Authorization` and the other headers it sets itself.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// Whose memory the engine holds: `single_user` or a tenant scope such
    /// as `org:acme/user:alice`. Required by `cortexdb`, refused by
    /// `tinyhumans` (see the module docs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenancy: Option<crate::cortex::CortexTenancy>,
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
