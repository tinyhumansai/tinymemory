//! Binds configured and host-provided engines behind one cached, safe surface.
//!
//! [`crate::registry::binder::EngineBinder`] owns the strong per-fingerprint
//! cache, explicit invalidation and a replaceable host-engine slot. Hosts resolve their own config,
//! credentials and policy, then pass the resulting [`EngineSettings`],
//! [`EngineCredential`] and [`crate::safety::Policy`] here. Every path returned by the binder
//! scrubs every write and times every engine operation.
//!
//! The caller fingerprint identifies a credential source, not a token. Static
//! credentials are hashed into the cache key directly; for a dynamic
//! [`crate::cortex::BearerSource`], the host fingerprint must identify the
//! source (for example, the config or keyring slot), while the source itself
//! continues to resolve rotating tokens on each request.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, RwLock};

use sha2::{Digest, Sha256};
use tinymemory_api::{Error, MemoryEngine, Result};

use crate::config::EngineSettings;
use crate::safety::{Policy, ScrubbingEngine, TimedEngine};

use super::{EngineCredential, build_engine, list_engines};

pub use crate::cortex::{CORTEXDB_ENGINE_ID, TINYHUMANS_ENGINE_ID};

/// A host callback for supplying an engine endpoint when settings leave it
/// blank. It is called with the selected engine id; a returned blank endpoint
/// is ignored and the registry's default is used.
pub type EndpointSource = dyn Fn(&str) -> Option<String> + Send + Sync;

/// The endpoint assigned to a host-installed or transient external engine.
pub const HOST_ENGINE_ENDPOINT: &str = "host://engine";

/// A ready engine together with the id and endpoint safe for host status.
#[derive(Clone)]
pub struct BoundEngine {
    /// The engine. Writes pass through scrubbing, and all operations are timed.
    pub engine: Arc<dyn MemoryEngine>,
    /// The stable engine id.
    pub id: String,
    /// The effective endpoint, or a host-owned descriptive URI.
    pub endpoint: String,
}

impl std::fmt::Debug for BoundEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundEngine")
            .field("id", &self.id)
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

/// Whether a memory engine is available, preserving safe configuration
/// context when binding is off.
#[derive(Debug, Clone)]
pub enum Binding {
    /// A guarded engine is ready.
    On(BoundEngine),
    /// Binding is unavailable. The reason is suitable for a host UI and never
    /// includes a credential.
    Off {
        /// The selected engine id, when one was provided.
        engine: Option<String>,
        /// The configured or resolved endpoint, when known.
        endpoint: Option<String>,
        /// Why the engine could not be bound.
        reason: String,
    },
}

impl Binding {
    /// Returns the bound engine, or an [`Error::Off`] with the binding reason.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Off`] when this is [`Binding::Off`].
    pub fn engine(self) -> Result<BoundEngine> {
        match self {
            Self::On(engine) => Ok(engine),
            Self::Off { reason, .. } => Err(Error::Off(reason)),
        }
    }

    /// Whether a usable engine is bound.
    #[must_use]
    pub fn is_on(&self) -> bool {
        matches!(self, Self::On(_))
    }
}

/// Per-host engine construction and safety binding.
///
/// Cache entries are retained strongly until [`EngineBinder::invalidate`]
/// clears them. This preserves the constructed HTTP client when callers drop a
/// temporary [`Binding`]. Endpoint/settings changes, credential identity,
/// static credential values, and scrub policies all produce distinct entries.
/// There is no automatic eviction: a host with high configuration churn
/// should invalidate periodically or when its configuration is retired. The
/// cache is per binder (and therefore per host), not process-global.
pub struct EngineBinder {
    endpoint_source: Arc<EndpointSource>,
    cache: Mutex<HashMap<[u8; 32], BoundEngine>>,
    host_engine: RwLock<Option<BoundEngine>>,
}

impl std::fmt::Debug for EngineBinder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let cache_entries = self
            .cache
            .lock()
            .map_or_else(|error| error.into_inner().len(), |cache| cache.len());
        let host_engine_installed = self
            .host_engine
            .read()
            .map_or_else(|error| error.into_inner().is_some(), |host| host.is_some());
        f.debug_struct("EngineBinder")
            .field("cache_entries", &cache_entries)
            .field("host_engine_installed", &host_engine_installed)
            .finish_non_exhaustive()
    }
}

impl EngineBinder {
    /// Creates a binder using `endpoint_source` only when the configured
    /// endpoint is absent or blank.
    #[must_use]
    pub fn new(endpoint_source: impl Fn(&str) -> Option<String> + Send + Sync + 'static) -> Self {
        Self {
            endpoint_source: Arc::new(endpoint_source),
            cache: Mutex::new(HashMap::new()),
            host_engine: RwLock::new(None),
        }
    }

    /// Binds an engine, preferring the installed host engine when present.
    ///
    /// The host engine keeps the scrub policy supplied when it was installed;
    /// the `policy` argument applies only to a configured engine. Hosts that
    /// must ignore the process slot (for example in SaaS/profile mode) use
    /// [`EngineBinder::bind_configured`] instead.
    #[must_use]
    pub fn bind(
        &self,
        id: &str,
        settings: EngineSettings,
        credential_fingerprint: &str,
        credential: EngineCredential,
        policy: Policy,
    ) -> Binding {
        if let Some(host) = self.host_engine() {
            return Binding::On(host);
        }
        self.bind_configured(id, settings, credential_fingerprint, credential, policy)
    }

    /// Binds a configured engine without consulting the host-engine slot.
    ///
    /// This is useful when host policy deliberately ignores a process-wide
    /// engine, while leaving that slot installed for other callers.
    #[must_use]
    pub fn bind_configured(
        &self,
        id: &str,
        mut settings: EngineSettings,
        credential_fingerprint: &str,
        credential: EngineCredential,
        policy: Policy,
    ) -> Binding {
        let configured_endpoint = settings
            .endpoint
            .as_deref()
            .map(str::trim)
            .filter(|endpoint| !endpoint.is_empty())
            .map(str::to_string);
        let supplied_endpoint = configured_endpoint.or_else(|| {
            (self.endpoint_source)(id)
                .map(|endpoint| endpoint.trim().to_string())
                .filter(|endpoint| !endpoint.is_empty())
        });
        let descriptor = list_engines()
            .into_iter()
            .find(|descriptor| descriptor.id == id);
        let known_engine = descriptor.is_some();
        let default_endpoint = descriptor
            .and_then(|descriptor| descriptor.default_endpoint)
            .map(str::to_string);
        let endpoint = supplied_endpoint.or(default_endpoint);
        settings.endpoint = endpoint.clone();

        let key = cache_key(id, &settings, credential_fingerprint, &credential, policy);
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(bound) = cache.get(&key) {
            return Binding::On(bound.clone());
        }

        match build_engine(id, &settings, credential) {
            Ok(engine) => {
                let endpoint = endpoint.unwrap_or_default();
                let bound = guarded(engine, id, endpoint, policy);
                cache.insert(key, bound.clone());
                Binding::On(bound)
            }
            Err(error) => Binding::Off {
                engine: known_engine.then(|| id.to_string()),
                endpoint: if known_engine { endpoint } else { None },
                reason: error.to_string(),
            },
        }
    }

    /// Binds a transient engine without installing it in the process slot.
    ///
    /// Hosts use this for a tenant- or profile-specific engine whose lifetime
    /// is limited to the current binding. It is protected by both decorators
    /// just like configured and installed engines. It is not inserted into
    /// the configured-engine cache; dropping the returned binding releases its
    /// wrapper and engine once no other references remain.
    #[must_use]
    pub fn bind_external(
        &self,
        engine: Arc<dyn MemoryEngine>,
        endpoint: impl Into<String>,
        policy: Policy,
    ) -> BoundEngine {
        let id = engine.descriptor().id.to_string();
        guarded(engine, &id, endpoint.into(), policy)
    }

    /// Installs or replaces the process engine. Its policy is fixed for that
    /// installed engine, even if a later [`EngineBinder::bind`] call supplies a
    /// different configured-engine policy.
    pub fn install_host_engine(&self, engine: Arc<dyn MemoryEngine>, policy: Policy) {
        let id = engine.descriptor().id.to_string();
        let bound = guarded(engine, &id, HOST_ENGINE_ENDPOINT.to_string(), policy);
        *self
            .host_engine
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(bound);
    }

    /// Removes the installed host engine. Returns whether one was present.
    pub fn clear_host_engine(&self) -> bool {
        self.host_engine
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .is_some()
    }

    /// Drops every configured-engine cache entry, releasing strong engine
    /// references when callers also drop their bindings. Hosts should call
    /// this when settings are retired and periodically under high churn. An
    /// installed host engine is a separate slot and remains installed.
    pub fn invalidate(&self) {
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    /// Applies the host's scoped-layout settings and returns a discriminator
    /// for callers to include in their credential fingerprint.
    ///
    /// `root` is the person's root (usually `org:<id>`). Direct CortexDB
    /// receives that scope and its owning actor; the TinyHumans wire uses the
    /// backend-pinned tenant root. `legacy_read` keeps the previous
    /// `user:<id>` root readable and erasable while the host migrates data.
    #[must_use]
    pub fn rooted(
        settings: EngineSettings,
        root: Option<&str>,
        tenant: bool,
        legacy_read: bool,
    ) -> (EngineSettings, String) {
        let Some(root) = root else {
            return (settings, "|root=legacy".to_string());
        };
        let actor = actor_of_root(root);
        let retired = legacy_read.then(|| actor.clone());
        let fingerprint = format!(
            "|root={root}|tenant={tenant}|retired={}",
            retired.as_deref().unwrap_or("-")
        );
        let settings = if tenant {
            EngineSettings {
                tenant_root: true,
                retired_scope_root: retired,
                ..settings
            }
        } else {
            EngineSettings {
                scope_root: Some(root.to_string()),
                scope_owner: Some(actor),
                retired_scope_root: retired,
                ..settings
            }
        };
        (settings, fingerprint)
    }

    /// Returns a clone of the currently installed host binding, if any.
    ///
    /// This reads only the host slot. It does not construct or cache an
    /// engine, change the slot, or add another safety decorator. The returned
    /// binding retains its engine `Arc` if the slot is later replaced or
    /// cleared.
    #[must_use]
    pub fn host_engine(&self) -> Option<BoundEngine> {
        self.host_engine
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl Default for EngineBinder {
    fn default() -> Self {
        Self::new(|_| None)
    }
}

fn guarded(
    engine: Arc<dyn MemoryEngine>,
    id: &str,
    endpoint: String,
    policy: Policy,
) -> BoundEngine {
    BoundEngine {
        engine: TimedEngine::wrap(ScrubbingEngine::wrap(engine, policy)),
        id: id.to_string(),
        endpoint,
    }
}

fn actor_of_root(root: &str) -> String {
    match root.strip_prefix("org:") {
        Some(id) => format!("user:{id}"),
        None => root.to_string(),
    }
}

fn cache_key(
    id: &str,
    settings: &EngineSettings,
    credential_fingerprint: &str,
    credential: &EngineCredential,
    policy: Policy,
) -> [u8; 32] {
    let mut hasher = Sha256Hasher(Sha256::new());
    match credential {
        EngineCredential::None => {
            (
                id,
                settings,
                credential_fingerprint,
                0_u8,
                None::<&str>,
                policy,
            )
                .hash(&mut hasher);
        }
        EngineCredential::Static(token) => {
            (
                id,
                settings,
                credential_fingerprint,
                1_u8,
                Some(token.as_str()),
                policy,
            )
                .hash(&mut hasher);
        }
        EngineCredential::Dynamic(_) => {
            (
                id,
                settings,
                credential_fingerprint,
                2_u8,
                None::<&str>,
                policy,
            )
                .hash(&mut hasher);
        }
    }
    hasher.0.finalize().into()
}

struct Sha256Hasher(Sha256);

impl Hasher for Sha256Hasher {
    fn finish(&self) -> u64 {
        let digest = self.0.clone().finalize();
        let mut prefix = [0_u8; 8];
        prefix.copy_from_slice(&digest[..8]);
        u64::from_be_bytes(prefix)
    }

    fn write(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
