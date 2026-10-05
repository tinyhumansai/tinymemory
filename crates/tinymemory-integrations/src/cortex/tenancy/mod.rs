//! Tenancy: whose memory one engine serves, and the scope root that follows.
//!
//! CortexDB does not keep one caller out of another's scopes by itself: on
//! a `cloud_shared_saas` deployment, `v1/events?scope=` reads any scope, a
//! write lands in any scope, and `view: "descend"` at a shared ancestor reads
//! every scope beneath it. Isolation is the application's job, and on the
//! direct wire the application is this engine.
//!
//! So a direct engine must say whose memory it holds before it is built
//! ([`CortexTenancy`]); one without a declaration is refused with
//! [`Error::Config`] rather than writing every caller into one shared tree:
//!
//! - [`CortexTenancy::SingleUser`]: the credential belongs to one person (a
//!   desktop install with the user's own key, a self-hosted server). Scopes
//!   sit under the TinyMemory root as they always have.
//! - [`CortexTenancy::Pinned`]: every scope sits under a [`TenantScope`],
//!   a CortexDB scope path such as `org:acme/user:alice`. Nothing an engine
//!   reads, writes, discovers or forgets is outside it: every scope this
//!   crate names is built under the pin, every scope the server reports is
//!   accepted only if it starts with the pin, and the one server-side
//!   traversal (`view: "descend"`) starts at the pin's own root.
//!
//! A pin isolates the hosts that go through this engine. It is not a
//! credential: anyone holding the deployment's key can still name any scope
//! on `/v1/*`. A deployment whose users must be kept apart from each other's
//! *clients* needs a key per user, or the hosted wire.
//!
//! The hosted (TinyHumans) wire takes no declaration: the backend derives the
//! tenant from the caller's credential and re-roots every scope under it.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::cortex::error::{Error, Result};

/// How [`CortexTenancy::SingleUser`] is written in configuration.
pub const SINGLE_USER: &str = "single_user";

/// Most segments a pin may have. CortexDB recommends at most eight per path,
/// and a TinyMemory scope adds its root and kind below the pin.
const MAX_PIN_SEGMENTS: usize = 4;

/// Longest a pin segment's id may be (CortexDB's recommended segment length).
const MAX_PIN_ID: usize = 64;

/// The scope type a pin may not use: the TinyMemory root and kind segments
/// are `app:`, so a pin naming one could be read as a TinyMemory scope.
const RESERVED_TYPE: &str = "app";

/// Whose memory a direct engine holds. See the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CortexTenancy {
    /// One person's credential: scopes sit under the TinyMemory root.
    SingleUser,
    /// Every scope sits under this tenant's scope.
    Pinned(TenantScope),
}

impl CortexTenancy {
    /// Every scope pinned under `scope` (for example `org:acme/user:alice`).
    ///
    /// # Errors
    ///
    /// As [`TenantScope::new`].
    pub fn pinned(scope: &str) -> Result<Self> {
        TenantScope::new(scope).map(Self::Pinned)
    }
}

impl fmt::Display for CortexTenancy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SingleUser => f.write_str(SINGLE_USER),
            Self::Pinned(scope) => scope.fmt(f),
        }
    }
}

impl FromStr for CortexTenancy {
    type Err = Error;

    /// `single_user`, or a pin such as `org:acme/user:alice`.
    fn from_str(value: &str) -> Result<Self> {
        match value.trim() {
            SINGLE_USER => Ok(Self::SingleUser),
            pin => Self::pinned(pin),
        }
    }
}

impl Serialize for CortexTenancy {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for CortexTenancy {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// A CortexDB scope path every scope of one tenant sits under, such as
/// `org:acme/user:alice` or `user:u-42`.
///
/// Checked on construction, so the only pins that exist are ones CortexDB
/// reads as exactly the path written: 1 to 4 `type:id` segments joined by
/// `/`, each type a lowercase identifier other than `app`, each id 1 to 64
/// characters of `[A-Za-z0-9_.-]` other than `.` and `..`. The type must also
/// be in the deployment's `allowed_scope_types` (every shipped preset allows
/// `org, dept, team, user, agent, service, ws, project, global, system,
/// source`), which only the server can check.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TenantScope(String);

impl TenantScope {
    /// A pin, checked as described on [`TenantScope`].
    ///
    /// # Errors
    ///
    /// [`Error::Config`] for an empty path, too many segments, a bare word, a
    /// reserved or malformed type, or an id outside the charset or length.
    pub fn new(path: &str) -> Result<Self> {
        let path = path.trim();
        let refuse = |why: &str| Error::Config(format!("tenant scope `{path}` {why}"));
        if path.is_empty() {
            return Err(refuse("is empty"));
        }
        let segments: Vec<&str> = path.split('/').collect();
        if segments.len() > MAX_PIN_SEGMENTS {
            return Err(refuse(&format!(
                "has more than {MAX_PIN_SEGMENTS} segments"
            )));
        }
        for segment in segments {
            let (kind, id) = segment
                .split_once(':')
                .ok_or_else(|| refuse(&format!("has segment `{segment}`, not type:id")))?;
            if !valid_type(kind) {
                return Err(refuse(&format!(
                    "has type `{kind}`; a type is a lowercase identifier"
                )));
            }
            if kind == RESERVED_TYPE {
                return Err(refuse(
                    "uses the `app` type, which TinyMemory's own scopes use",
                ));
            }
            if !valid_id(id) {
                return Err(refuse(&format!(
                    "has id `{id}`; an id is 1 to {MAX_PIN_ID} characters of A-Z, a-z, 0-9, `_`, `.` or `-`, and not `.` or `..`"
                )));
            }
        }
        Ok(Self(path.to_string()))
    }

    /// The path, as written.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TenantScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for TenantScope {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        Self::new(value)
    }
}

fn valid_type(kind: &str) -> bool {
    let mut chars = kind.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_PIN_ID
        && id != "."
        && id != ".."
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// The scope every TinyMemory scope of one engine sits under, and how
/// strictly a scope the server reports must match it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScopeRoot {
    /// `app:tinymemory`, or `<pin>/app:tinymemory`.
    path: String,
    /// Whether a reported scope must start with [`Self::path`]. The direct
    /// wire sees scopes as stored, so it must. The hosted backend may report
    /// them under the caller's tenant, which this crate cannot name, so a
    /// hosted root is found wherever it sits.
    anchored: bool,
}

impl ScopeRoot {
    /// The TinyMemory root segment.
    pub(crate) const BASE: &'static str = "app:tinymemory";

    /// The direct wire's root for `tenancy`.
    pub(crate) fn direct(tenancy: &CortexTenancy) -> Self {
        let path = match tenancy {
            CortexTenancy::SingleUser => Self::BASE.to_string(),
            CortexTenancy::Pinned(pin) => format!("{pin}/{}", Self::BASE),
        };
        Self {
            path,
            anchored: true,
        }
    }

    /// The hosted wire's root: the backend pins the tenant.
    pub(crate) fn hosted() -> Self {
        Self {
            path: Self::BASE.to_string(),
            anchored: false,
        }
    }

    /// The root's path.
    pub(crate) fn path(&self) -> &str {
        &self.path
    }

    /// What follows the root in `scope`, or `None` when `scope` is not under
    /// it.
    pub(crate) fn strip<'a>(&self, scope: &'a str) -> Option<&'a str> {
        if self.anchored {
            return scope.strip_prefix(self.path.as_str())?.strip_prefix('/');
        }
        let needle = format!("{}/", self.path);
        let mut from = 0;
        while let Some(at) = scope[from..].find(&needle) {
            let start = from + at;
            if start == 0 || scope.as_bytes()[start - 1] == b'/' {
                return Some(&scope[start + needle.len()..]);
            }
            from = start + 1;
        }
        None
    }
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
