//! Credentials: a fixed API key or a per-request [`BearerSource`].
//!
//! Both wires authenticate with `Authorization: Bearer <token>`. Direct
//! CortexDB normally takes a fixed API key ([`CortexCredential::Static`]); the
//! TinyHumans backend takes the host's session JWT or `tiny_live_` API key,
//! which rotates, so it is consulted on **every request attempt**
//! ([`CortexCredential::Dynamic`]).
//!
//! Neither type ever prints its token: `Debug` is redacted, and the transport
//! marks the header it builds sensitive.

use std::{future::Future, sync::Arc};

use async_trait::async_trait;

use crate::cortex::error::Result;

/// A per-request source of bearer tokens.
///
/// A host that owns a rotating credential (a session JWT that refreshes, an
/// API key it reads from a keyring) hands the engine one of these, so a
/// refreshed token is used at once without rebuilding the engine.
///
/// Implementations must not log or otherwise print the token they return, and
/// should return an error (not an empty string) when no credential is
/// available. The engine reports either as [`crate::cortex::Error::Unauthorized`]
/// without sending a request, and never stores the value past the request.
#[async_trait]
pub trait BearerSource: Send + Sync {
    /// The bearer token to send on the next request.
    ///
    /// # Errors
    ///
    /// Fails when no credential is currently available (for example the host
    /// is signed out).
    async fn bearer(&self) -> Result<String>;
}

/// A [`BearerSource`] that resolves its token by calling a host-provided
/// callback for every request.
///
/// This is useful when the host owns credential refresh and wants a Cortex
/// engine to observe the latest token without being rebuilt. The callback's
/// errors are returned unchanged; the engine handles unavailable credentials
/// as unauthorized and does not send a request. Neither this type nor its
/// [`std::fmt::Debug`] output exposes the callback or any token.
pub struct CallbackBearerSource<F> {
    callback: F,
}

impl<F> CallbackBearerSource<F> {
    /// Creates a callback-backed source. The callback must return an owned,
    /// sendable future so each request can resolve independently.
    #[must_use]
    pub fn new<Fut>(callback: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<String>> + Send + 'static,
    {
        Self { callback }
    }
}

impl<F> std::fmt::Debug for CallbackBearerSource<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CallbackBearerSource(<callback>)")
    }
}

#[async_trait]
impl<F, Fut> BearerSource for CallbackBearerSource<F>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<String>> + Send + 'static,
{
    async fn bearer(&self) -> Result<String> {
        (self.callback)().await
    }
}

/// A fixed bearer token as a [`BearerSource`]. Its `Debug` never shows the
/// token.
#[derive(Clone)]
pub struct StaticBearer(String);

impl StaticBearer {
    /// Wraps a fixed token.
    #[must_use]
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }
}

impl std::fmt::Debug for StaticBearer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StaticBearer(<redacted>)")
    }
}

#[async_trait]
impl BearerSource for StaticBearer {
    async fn bearer(&self) -> Result<String> {
        Ok(self.0.clone())
    }
}

/// How an engine authenticates.
#[derive(Clone)]
pub enum CortexCredential {
    /// One fixed token, for example a CortexDB API key.
    Static(String),
    /// A token resolved from the source before every request attempt.
    Dynamic(Arc<dyn BearerSource>),
}

impl CortexCredential {
    /// A fixed token.
    #[must_use]
    pub fn api_key(key: impl Into<String>) -> Self {
        Self::Static(key.into())
    }

    /// The token to send on the next request.
    pub(crate) async fn resolve(&self) -> Result<String> {
        match self {
            Self::Static(token) => Ok(token.clone()),
            Self::Dynamic(source) => source.bearer().await,
        }
    }
}

impl std::fmt::Debug for CortexCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Static(_) => f.write_str("CortexCredential::Static(<redacted>)"),
            Self::Dynamic(_) => f.write_str("CortexCredential::Dynamic(<source>)"),
        }
    }
}

impl From<Arc<dyn BearerSource>> for CortexCredential {
    fn from(source: Arc<dyn BearerSource>) -> Self {
        Self::Dynamic(source)
    }
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
