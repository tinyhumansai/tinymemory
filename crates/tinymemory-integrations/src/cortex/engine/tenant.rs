//! The proxied wire's tenant: named per call, from the call's namespace.
//!
//! A tenant-multiplexing cortexdb-saas proxy serves many tenants behind one
//! service credential, and the caller names the tenant of each request in
//! `X-Memory-Tenant`. One engine serves every tenant, so the tenant cannot
//! be fixed when the engine is built: it is read from each call. A host
//! binds every agent to a namespace rooted at `user:<id>`, and `<id>` is the
//! tenant. So:
//!
//! - every namespace a call names must start with a `user:` segment, and
//!   all of them must name the same id;
//! - a call that names no such namespace (no reach, the root, `team:…`, or a
//!   forget by bare ids, which carry no namespace) fails with
//!   [`Error::Config`] before anything is sent. It never falls back to a
//!   shared or default tenant;
//! - the id must be 1 to 96 characters of `[A-Za-z0-9_-]`.
//!
//! The call then runs on a clone of the engine whose transport names that
//! tenant. Clones share the engine's caches, so this costs one clone of the
//! HTTP handle per call.

use std::borrow::Cow;

use tinymemory_api::{Namespace, SegmentKind};

use super::CortexEngine;
use crate::cortex::descriptor::CortexWire;
use crate::cortex::error::{Error, Result};

/// The tenant the proxied wire's health probe names: an id this crate never
/// writes under, so the probe reaches no one's memory. The proxy may
/// register it on first sight; it holds nothing.
pub(super) const HEALTH_PROBE_TENANT: &str = "tinymemory-health";

/// The id of `namespace`'s leading `user:` segment, if it has one.
pub(super) fn tenant_of(namespace: &Namespace) -> Option<&str> {
    namespace
        .segments()
        .first()
        .filter(|segment| segment.kind() == SegmentKind::User)
        .map(tinymemory_api::Segment::id)
}

impl CortexEngine {
    /// This engine for a call naming `namespaces`: itself off the proxied
    /// wire, and on it a clone naming their one tenant (see the module
    /// docs).
    ///
    /// # Errors
    ///
    /// [`Error::Config`] on the proxied wire for no namespace, one without a
    /// leading `user:` segment, two different tenants, or an invalid id.
    pub(super) fn for_call<'a>(
        &self,
        namespaces: impl IntoIterator<Item = &'a Namespace>,
    ) -> Result<Cow<'_, Self>> {
        if self.wire() != CortexWire::Proxied {
            return Ok(Cow::Borrowed(self));
        }
        let mut tenant: Option<&str> = None;
        for namespace in namespaces {
            let id = tenant_of(namespace).ok_or_else(|| {
                Error::Config(format!(
                    "the proxied memory wire names its tenant from a namespace starting \
                     `user:<id>`, and `{namespace}` does not"
                ))
            })?;
            match tenant {
                Some(known) if known != id => {
                    return Err(Error::Config(format!(
                        "one call names two memory tenants, `{known}` and `{id}`"
                    )));
                }
                _ => tenant = Some(id),
            }
        }
        let id = tenant.ok_or_else(|| {
            Error::Config(
                "the proxied memory wire names its tenant from the call's namespace, and this \
                 call names none (give it a reach at or below `user:<id>`)"
                    .to_string(),
            )
        })?;
        self.as_tenant(id).map(Cow::Owned)
    }

    /// A clone naming `id` as its tenant.
    pub(super) fn as_tenant(&self, id: &str) -> Result<Self> {
        let mut engine = self.clone();
        engine.log.client.set_tenant(id)?;
        Ok(engine)
    }
}

#[cfg(test)]
#[path = "tenant_tests.rs"]
mod tests;
