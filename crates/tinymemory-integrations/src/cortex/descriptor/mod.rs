//! The registrations of the engine, and the routes each wire speaks.
//!
//! One engine type serves three configuration ids:
//!
//! - [`CORTEXDB_ENGINE_ID`] — a CortexDB server's own `/v1/*` API with an API
//!   key ([`CortexWire::Direct`]);
//! - [`TINYHUMANS_ENGINE_ID`] — CortexDB behind the TinyHumans backend's
//!   `/memory/*` routes with the host's bearer ([`CortexWire::TinyHumans`]);
//! - [`PROXIED_ENGINE_ID`] — CortexDB behind a tenant-multiplexing proxy,
//!   on a subset of CortexDB's own `/v1/*` routes, with the tenant named per
//!   request ([`CortexWire::Proxied`]).
//!
//! # The proxied wire's routes
//!
//! The wire assumes a proxy that forwards CortexDB's own routes, pinning
//! every scope under the calling tenant's root, with one service credential
//! naming the tenant on each request in `X-Memory-Tenant`. It sends only
//! these routes:
//!
//! | Route | Used for |
//! | --- | --- |
//! | `POST v1/experience`, `POST v1/experience/bulk` | writes, as on Direct |
//! | `GET v1/events`, `GET v1/events/{id}` | listings |
//! | `POST v1/recall`, `POST v1/answer` | fetch and recall |
//! | `POST v1/forget` | forget, and erasing a scope event by event |
//! | `GET v1/scopes/list` | the scopes a reach covers |
//! | `GET v1/beliefs`, `POST v1/beliefs/build` | beliefs |
//! | `GET v1/facts`, `v1/understanding`, `v1/derivation/status`, `v1/blobs` | not used by this engine |
//! | `GET v1/admin/health` | health |
//! | `POST v1/tenant/purge` | erasing the tenant's whole memory |
//!
//! Never sent on this wire: `v1/erasures` (a scope is erased by forgetting
//! its events), scope registration (`v1/scopes`, `v1/scopes/members`: the
//! proxy owns the root), `v1/admin/version` and `v1/auth/whoami`.
//!
//! # Why both declare only [`FetchMode::Hybrid`]
//!
//! Fetch maps onto CortexDB's recall route, and its request body accepts only
//! `scope`, `query`, `budgets`, `view`, `include`, `temporal` and `filters`.
//! There is no field that switches between lexical and embedding retrieval:
//! the engine always blends them. Declaring `Keyword` or `Vector` would
//! promise a ranking the wire cannot ask for, so both descriptors list
//! `Hybrid` alone and the other modes fail with
//! [`tinymemory_api::Error::Unsupported`].
//!
//! # Consolidation
//!
//! CortexDB builds a scope's beliefs at `v1/beliefs/build`, one scope per
//! call. Whether a host also has to ask after its writes depends on the
//! deployment, not the wire ([`direct_consolidation`]):
//!
//! - CortexDB's managed API ([`CORTEX_API_ENDPOINT`]) extracts facts and
//!   rebuilds a scope's beliefs on its own shortly after each write, so a
//!   direct engine there declares [`Consolidation::Automatic`]: no build is
//!   queued after a turn or an ingest, and an explicit build is a refresh.
//! - A self-hosted CortexDB builds beliefs in the background only when its
//!   operator turns the layer scheduler on (`CORTEX_V1_LAYERS_AUTO`, off by
//!   default), so a direct engine anywhere else declares
//!   [`Consolidation::OnDemand`].
//!
//! [`cortexdb_descriptor`] describes the engine at its default endpoint, the
//! managed API. A host that knows better overrides the choice with
//! [`crate::cortex::CortexEngine::with_consolidation`].
//!
//! The TinyHumans backend exposes no build route; CortexDB consolidates
//! behind it on its own, so the hosted descriptor declares
//! [`Consolidation::Scheduled`].

use tinymemory_api::{Consolidation, EngineDescriptor, FetchMode};

/// Configuration id of CortexDB reached directly.
pub const CORTEXDB_ENGINE_ID: &str = "cortexdb";

/// Configuration id of CortexDB behind the TinyHumans backend.
pub const TINYHUMANS_ENGINE_ID: &str = "tinyhumans";

/// Configuration id of CortexDB behind a tenant-multiplexing CortexDB
/// proxy ([`CortexWire::Proxied`]).
pub const PROXIED_ENGINE_ID: &str = "cortexdb-proxied";

/// Default base URL of CortexDB's managed API.
pub const CORTEX_API_ENDPOINT: &str = "https://api-v1.cortexdb.ai";

/// Default origin of the TinyHumans backend that hosts CortexDB.
pub const TINYHUMANS_API_ENDPOINT: &str = "https://api.tinyhumans.ai";

/// The fetch modes both wires serve: hybrid only (see the module docs).
const FETCH_MODES: [FetchMode; 1] = [FetchMode::Hybrid];

/// The descriptor of CortexDB reached directly: not hosted by a third party,
/// an endpoint is optional ([`CORTEX_API_ENDPOINT`] by default), an API key
/// is required, fetch is hybrid only, and beliefs build as they do at the
/// default endpoint: on their own ([`Consolidation::Automatic`]).
#[must_use]
pub fn cortexdb_descriptor() -> EngineDescriptor {
    EngineDescriptor {
        id: CORTEXDB_ENGINE_ID,
        label: "CortexDB",
        description: "CortexDB's own API: an append-only memory log with ranked recall and \
                      grounded answers",
        hosted: false,
        needs_endpoint: false,
        needs_key: true,
        default_endpoint: Some(CORTEX_API_ENDPOINT),
        fetch_modes: FETCH_MODES.to_vec(),
        consolidation: direct_consolidation(CORTEX_API_ENDPOINT),
    }
}

/// The descriptor of CortexDB behind the TinyHumans backend: hosted, the
/// endpoint defaults to [`TINYHUMANS_API_ENDPOINT`], a bearer (session JWT or
/// API key) is required, fetch is hybrid only, and beliefs build on the
/// server's schedule.
#[must_use]
pub fn tinyhumans_descriptor() -> EngineDescriptor {
    EngineDescriptor {
        id: TINYHUMANS_ENGINE_ID,
        label: "TinyHumans",
        description: "CortexDB hosted by the TinyHumans backend, billed to the signed-in account",
        hosted: true,
        needs_endpoint: false,
        needs_key: true,
        default_endpoint: Some(TINYHUMANS_API_ENDPOINT),
        fetch_modes: FETCH_MODES.to_vec(),
        consolidation: Consolidation::Scheduled,
    }
}

/// The descriptor of CortexDB behind a tenant-multiplexing CortexDB
/// proxy: run by the operator, not a third party; the endpoint is required
/// (there is no public default), so is the proxy's service credential; fetch
/// is hybrid only, and beliefs build on demand (`v1/beliefs/build` is
/// allowed), as on a self-hosted CortexDB.
#[must_use]
pub fn proxied_descriptor() -> EngineDescriptor {
    EngineDescriptor {
        id: PROXIED_ENGINE_ID,
        label: "CortexDB (proxied)",
        description: "CortexDB behind a tenant-multiplexing CortexDB proxy, one tenant per \
                      user",
        hosted: false,
        needs_endpoint: true,
        needs_key: true,
        default_endpoint: None,
        fetch_modes: FETCH_MODES.to_vec(),
        consolidation: Consolidation::OnDemand,
    }
}

/// How a direct engine whose endpoint has `origin` (scheme, host and port,
/// with no trailing slash) consolidates by
/// default: [`Consolidation::Automatic`] on CortexDB's managed API,
/// [`Consolidation::OnDemand`] anywhere else.
///
/// This is the one place the choice is made. It goes by the endpoint alone
/// because a deployment's layer settings are not readable through the public
/// API today; if CortexDB confirms a readiness signal it can be asked here
/// instead (for example whether `v1/derivation/status` reports a running
/// layer scheduler), and every caller follows.
#[must_use]
pub(crate) fn direct_consolidation(origin: &str) -> Consolidation {
    if origin == CORTEX_API_ENDPOINT {
        Consolidation::Automatic
    } else {
        Consolidation::OnDemand
    }
}

/// Which HTTP surface an engine talks to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CortexWire {
    /// A CortexDB server's own `/v1/*` API: bare JSON bodies, `?wait=indexed`
    /// and a bulk append route.
    Direct,
    /// CortexDB behind the TinyHumans backend's `/memory/*` routes:
    /// `{success,data}` envelopes (except the `/memory/v1/*` passthrough,
    /// which answers in CortexDB's dialect), typed `errorCode` failures, a strict answer
    /// schema, `Idempotency-Key` claims on writes, and no bulk, wait or health
    /// route.
    TinyHumans,
    /// CortexDB behind a tenant-multiplexing CortexDB proxy: the
    /// `/v1/*` routes the proxy allows (see the module docs), in CortexDB's
    /// own dialect, with the tenant named on every request in
    /// `X-Memory-Tenant` from the call's namespace (see
    /// [`crate::cortex::CortexEngine::proxied`]).
    Proxied,
}

impl CortexWire {
    /// The descriptor this wire registers under.
    #[must_use]
    pub fn descriptor(self) -> EngineDescriptor {
        match self {
            Self::Direct => cortexdb_descriptor(),
            Self::TinyHumans => tinyhumans_descriptor(),
            Self::Proxied => proxied_descriptor(),
        }
    }

    /// The request path (no query string) of `route` on this wire.
    pub(crate) fn path(self, route: Route) -> &'static str {
        match (self, route) {
            // `v1/erasures` is not exposed: a scope is erased by forgetting
            // its events (see `log::erase`).
            (Self::Proxied, Route::Erasures) => "v1/forget",
            // Erases everything under the calling tenant's root.
            (Self::Proxied, Route::EraseAll) => "v1/tenant/purge",
            // Never sent: the proxy owns the tenant root and exposes neither
            // scope registration nor the version route.
            (Self::Proxied, Route::RegisterScope) => "v1/scopes",
            (Self::Proxied, Route::ScopeMembers) => "v1/scopes/members",
            (Self::Proxied, Route::Version) => "v1/admin/version",
            (Self::Direct | Self::Proxied, Route::Experience) => "v1/experience",
            (Self::Direct | Self::Proxied, Route::Bulk) => "v1/experience/bulk",
            (Self::Direct | Self::Proxied, Route::Events) => "v1/events",
            (Self::Direct | Self::Proxied, Route::Recall) => "v1/recall",
            (Self::Direct | Self::Proxied, Route::Forget) => "v1/forget",
            (Self::Direct | Self::Proxied, Route::Answer) => "v1/answer",
            (Self::Direct | Self::Proxied, Route::Health) => "v1/admin/health",
            (Self::Direct | Self::Proxied, Route::Scopes) => "v1/scopes/list",
            (Self::Direct, Route::RegisterScope) => "v1/scopes",
            (Self::Direct, Route::ScopeMembers) => "v1/scopes/members",
            (Self::Direct | Self::Proxied, Route::BuildBeliefs) => "v1/beliefs/build",
            (Self::Direct | Self::Proxied, Route::Beliefs) => "v1/beliefs",
            (Self::Direct, Route::Erasures) => "v1/erasures",
            // Never sent: Direct erases the whole tree scope by scope.
            (Self::Direct, Route::EraseAll) => "v1/erasures",
            (Self::Direct, Route::Version) => "v1/admin/version",
            (Self::TinyHumans, Route::Experience | Route::Bulk) => "memory/experience",
            (Self::TinyHumans, Route::Events) => "memory/events",
            (Self::TinyHumans, Route::Recall) => "memory/recall",
            (Self::TinyHumans, Route::Forget) => "memory/forget",
            (Self::TinyHumans, Route::Answer) => "memory/answer",
            (Self::TinyHumans, Route::Health | Route::Scopes) => "memory/scopes",
            // Never sent: the hosted descriptor declares scheduled
            // consolidation, so `consolidate` makes no request there.
            (Self::TinyHumans, Route::BuildBeliefs) => "memory/beliefs/build",
            // Never sent: hosted beliefs are read through recall only.
            (Self::TinyHumans, Route::Beliefs) => "memory/beliefs",
            // The backend's CortexDB-dialect passthrough (no envelope);
            // memory-api pins the scope under the tenant's root. An `erase`
            // narrower than the whole tree goes here, scope by scope.
            (Self::TinyHumans, Route::Erasures) => "memory/v1/erasures",
            // `DELETE memory`: erases the caller's entire hosted memory.
            (Self::TinyHumans, Route::EraseAll) => "memory",
            // Never sent: the hosted backend keeps its own tenancy.
            (Self::TinyHumans, Route::RegisterScope | Route::ScopeMembers) => "memory/scopes",
            // Never sent: the hosted wire has no version route, so a date
            // hint is sent optimistically there (see `engine::refers`).
            (Self::TinyHumans, Route::Version) => "memory/version",
        }
    }
}

/// One logical CortexDB operation, mapped to a path per [`CortexWire`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Route {
    /// Append one event.
    Experience,
    /// Append an ordered batch (Direct only; hosted writes one at a time).
    Bulk,
    /// List a scope's events, newest first.
    Events,
    /// Build a ranked recall pack.
    Recall,
    /// Remove named events.
    Forget,
    /// Answer a question from a recall pack.
    Answer,
    /// The cheapest authenticated probe.
    Health,
    /// List the caller's registered scopes under a prefix.
    Scopes,
    /// Register a scope with its members, or (GET) read its record (Direct
    /// only).
    RegisterScope,
    /// Replace a registered scope's members (Direct only).
    ScopeMembers,
    /// Build one scope's beliefs on demand (Direct only).
    BuildBeliefs,
    /// List one scope's beliefs (Direct only).
    Beliefs,
    /// Erase a whole scope for good (Direct and TinyHumans; the proxied wire
    /// forgets the scope's events instead).
    Erasures,
    /// Erase the caller's entire memory in one request (TinyHumans: `DELETE
    /// memory`; Proxied: `POST v1/tenant/purge`).
    EraseAll,
    /// Build info and the API capabilities the server accepts (Direct only).
    Version,
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
