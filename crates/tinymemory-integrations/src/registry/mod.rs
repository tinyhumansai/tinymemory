//! The engine registry: [`list_engines`] and [`build_engine`].
//!
//! Two engines are registered, both served by [`crate::cortex`]:
//!
//! | Id | Engine | Endpoint | Credential |
//! | --- | --- | --- | --- |
//! | `cortexdb` | CortexDB's own `/v1/*` API | defaults to the managed API | API key |
//! | `tinyhumans` | CortexDB behind the TinyHumans backend `/memory/*` | defaults to `api.tinyhumans.ai` | session JWT or `tiny_live_` key, usually dynamic |
//!
//! [`build_engine`] refuses an unknown id, a missing required endpoint or
//! credential, a credentialed cleartext endpoint that is not loopback, and a
//! `cortexdb` engine with no tenancy (or a `tinyhumans` one with one), all as
//! [`Error::Config`]. Messages never carry the credential.

use std::sync::Arc;

use crate::cortex::{
    BearerSource, CORTEXDB_ENGINE_ID, CortexCredential, CortexEngine, CortexWire,
    TINYHUMANS_ENGINE_ID,
};
use tinymemory_api::{EngineDescriptor, Error, MemoryEngine, Result};

use crate::config::EngineSettings;

/// How an engine authenticates.
#[derive(Clone, Default)]
pub enum EngineCredential {
    /// No credential, for an engine that needs none.
    #[default]
    None,
    /// One fixed token, for example an API key.
    Static(String),
    /// A token resolved before every request, so a refreshed session is used
    /// at once.
    Dynamic(Arc<dyn BearerSource>),
}

impl std::fmt::Debug for EngineCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::None => "EngineCredential::None",
            Self::Static(_) => "EngineCredential::Static(<redacted>)",
            Self::Dynamic(_) => "EngineCredential::Dynamic(<source>)",
        })
    }
}

/// Every engine this build can construct.
#[must_use]
pub fn list_engines() -> Vec<EngineDescriptor> {
    vec![
        crate::cortex::cortexdb_descriptor(),
        crate::cortex::tinyhumans_descriptor(),
    ]
}

/// Builds the engine `id` from `settings` and `credential`.
///
/// An unset or blank endpoint falls back to the engine's default. Every
/// registered engine is credentialed, so a credential is always required;
/// the endpoint's own checks (an HTTP(S) URL, and no cleartext off loopback)
/// are [`CortexEngine::new`]'s.
///
/// # Errors
///
/// [`Error::Config`] for an unknown id, a missing endpoint or credential, an
/// endpoint that is not an HTTP(S) URL, a credentialed cleartext
/// (`http://`) endpoint that is not loopback, a `cortexdb` engine without a
/// tenancy, or a `tinyhumans` engine with one.
pub fn build_engine(
    id: &str,
    settings: &EngineSettings,
    credential: EngineCredential,
) -> Result<Arc<dyn MemoryEngine>> {
    let wire = match id {
        CORTEXDB_ENGINE_ID => CortexWire::Direct,
        TINYHUMANS_ENGINE_ID => CortexWire::TinyHumans,
        _ => return Err(Error::Config(format!("unknown memory engine `{id}`"))),
    };
    let endpoint = settings
        .endpoint
        .as_deref()
        .map(str::trim)
        .filter(|endpoint| !endpoint.is_empty())
        .or(wire.descriptor().default_endpoint)
        .ok_or_else(|| Error::Config(format!("memory engine `{id}` needs an endpoint")))?;
    let credential = match credential {
        EngineCredential::Static(token) if !token.trim().is_empty() => {
            CortexCredential::Static(token)
        }
        EngineCredential::Dynamic(source) => CortexCredential::Dynamic(source),
        EngineCredential::Static(_) | EngineCredential::None => {
            return Err(Error::Config(format!(
                "memory engine `{id}` needs a credential"
            )));
        }
    };
    let engine = CortexEngine::new(wire, endpoint, credential, settings.tenancy.clone())?
        .with_default_headers(&settings.headers)?;
    Ok(Arc::new(engine))
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
