//! [`CortexEngine`]: TinyMemory's recall, fetch, store, list and forget over
//! CortexDB's event log.
//!
//! Each operation lives in its own module:
//!
//! - `store` — one path for `store` and `store_many`: replay detection by
//!   item label, then each item's experience (or ordered batch of turns),
//!   then the readability waits;
//! - `list` — a cursor over the kind scopes' listings, each item once;
//! - `fetch` — hybrid retrieval through recall packs, ranked by the engine;
//! - `recall` — one pack, one answer, citations from the pack;
//! - `forget` — look the items' events up, remove them by `memory_ids`;
//! - `consolidate` — one `v1/beliefs/build` per held scope in reach;
//! - `tenant` — on the proxied wire, the tenant each call names.

mod attribution;
mod beliefs;
mod consolidate;
mod cursor;
mod erase;
mod explore;
mod fetch;
mod forget;
mod items;
mod list;
mod recall;
mod recency;
mod refers;
mod scopes;
mod store;
mod tenant;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use tinymemory_api::{
    BeliefsRequest, ConsolidateReceipt, ConsolidateRequest, Consolidation, EngineDescriptor,
    EngineHealth, EraseReport, EraseRequest, ExplorePage, ExploreRequest, ExportPage, FetchPage,
    FetchRequest, ForgetReport, ForgetTarget, GetRequest, Hit, ItemId, ListPage, ListRequest,
    MemoryEngine, Reach, RecallAnswer, RecallRequest, StoreItem, StoreReceipt, WaitFor,
    WriteOptions,
};

use crate::cortex::credential::{BearerSource, CortexCredential};
use crate::cortex::descriptor::{CortexWire, Route, direct_consolidation};
use crate::cortex::envelope::ScopeLayout;
use crate::cortex::error::{Error, Result};
use crate::cortex::log::Log;
use crate::cortex::transport::{HttpClient, health_reason, urlencode};

/// The scope prefix the hosted health probe lists under. The memory API
/// refuses a prefix that is not `type:id` segments (a bare word is a 400, which
/// would report a healthy service as broken); `tmh` is a type this crate never
/// writes, so the listing is empty and cheap.
const HEALTH_PROBE_SCOPE: &str = "tmh:probe";

/// The CortexDB memory engine, on any of its wires.
///
/// Build it with [`CortexEngine::direct`] for CortexDB's own API,
/// [`CortexEngine::tinyhumans`] for CortexDB behind the TinyHumans backend,
/// or [`CortexEngine::proxied`] for CortexDB behind a tenant-multiplexing
/// cortexdb-saas proxy.
/// `Debug` shows the wire and endpoint origin, never the credential.
#[derive(Clone)]
pub struct CortexEngine {
    descriptor: EngineDescriptor,
    log: Log,
    layout: ScopeLayout,
    /// The actor a v3 root is registered as owned by.
    owner: Option<String>,
    /// Whether the v3 root is registered (shared by clones).
    registered: Arc<AtomicBool>,
    refers: Arc<refers::RefersSupport>,
    /// Whether writes name their observed actor (shared by clones, so a
    /// refusal turns it off for all of them).
    attribution: Arc<attribution::Attribution>,
    /// When each scope was last written (shared by clones), for a capped
    /// fetch's choice of scopes.
    recency: Arc<recency::Recency>,
}

impl std::fmt::Debug for CortexEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CortexEngine")
            .field("id", &self.descriptor.id)
            .field("endpoint", &self.log.client.origin())
            .finish_non_exhaustive()
    }
}

impl CortexEngine {
    /// An engine on `wire` at `endpoint`, authenticating with `credential`.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] for an invalid or non-HTTP(S) endpoint, a cleartext
    /// endpoint that is not loopback (the credential would cross the network
    /// in the clear), or a blank static credential. A cleartext endpoint on a
    /// cluster-service host (`*.svc`, `*.svc.cluster.local`) builds, but
    /// every call fails with [`Error::Config`] until
    /// [`CortexEngine::with_private_network_cleartext`] opts in.
    ///
    /// A direct engine consolidates as its endpoint does by default:
    /// [`Consolidation::Automatic`] on CortexDB's managed API,
    /// [`Consolidation::OnDemand`] on any other (see
    /// [`CortexEngine::with_consolidation`]). A proxied engine starts in the
    /// tenant layout ([`CortexEngine::with_tenant_root`]).
    pub fn new(wire: CortexWire, endpoint: &str, credential: CortexCredential) -> Result<Self> {
        let client = HttpClient::new(wire, endpoint, credential)?;
        let mut descriptor = wire.descriptor();
        if wire == CortexWire::Direct {
            descriptor.consolidation = direct_consolidation(&client.origin());
        }
        Ok(Self {
            descriptor,
            log: Log::new(client),
            layout: if wire == CortexWire::Proxied {
                ScopeLayout::tenant()
            } else {
                ScopeLayout::Legacy
            },
            owner: None,
            registered: Arc::new(AtomicBool::new(false)),
            refers: Arc::default(),
            attribution: Arc::default(),
            recency: Arc::default(),
        })
    }

    /// The same engine, laying its scopes out below `root` (layout v3)
    /// instead of the legacy `app:tinymemory` tree: one person's memory as
    /// one subtree (`org:<id>`), each kind under a leaf of its own (see the
    /// `envelope` module docs). With an `owner` actor (`user:<id>`), a
    /// direct engine registers the root as owned by it before its first
    /// write, so the person owns their root rather than whichever key wrote
    /// first; the hosted backend keeps its own tenancy and is never asked.
    ///
    /// Switching layout moves nothing: what was written under the other
    /// layout stays there and is no longer read.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] for a root that is not `type:id` segments of
    /// CortexDB's hosted scope types, or a blank owner, and on the proxied
    /// wire, where the proxy pins each tenant's root itself.
    pub fn with_scope_root(mut self, root: &str, owner: Option<&str>) -> Result<Self> {
        if self.wire() == CortexWire::Proxied {
            return Err(Error::Config(
                "the proxied wire's root is the tenant's own; use the tenant root".to_string(),
            ));
        }
        let owner = owner.map(str::trim);
        if owner.is_some_and(str::is_empty) {
            return Err(Error::Config("a scope root's owner is blank".to_string()));
        }
        self.layout = ScopeLayout::v3(root, self.wire() == CortexWire::TinyHumans)?;
        self.owner = owner.map(str::to_string);
        self.registered = Arc::new(AtomicBool::new(false));
        Ok(self)
    }

    /// The same engine in layout v3 relative to the hosted tenant's root:
    /// no root segment is sent, and the TinyHumans backend (or, on the
    /// proxied wire, the proxy) pins its own (`org:<id>`), so a person's
    /// chats are stored at `org:<id>/ws:main/app:conversations`. See the
    /// `envelope` module docs. A proxied engine is already in this layout.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] on the direct wire, where nothing pins a root and
    /// every install would share one tree.
    pub fn with_tenant_root(mut self) -> Result<Self> {
        if self.wire() == CortexWire::Direct {
            return Err(Error::Config(
                "only the TinyHumans and proxied wires have a tenant root; name a scope root"
                    .to_string(),
            ));
        }
        self.layout = ScopeLayout::tenant();
        self.owner = None;
        self.registered = Arc::new(AtomicBool::new(false));
        Ok(self)
    }

    /// The same v3 engine, also reading and forgetting below `retired`
    /// (`user:<id>`, where an earlier layout wrote), while writing only
    /// below its root: the transition until that memory has moved. Every
    /// read merges both by item id; a forget or erasure removes from both.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] without a scope root (call
    /// [`CortexEngine::with_scope_root`] or [`CortexEngine::with_tenant_root`]
    /// first), or for a retired root that is not a valid v3 root or is the
    /// root itself.
    pub fn with_retired_root(mut self, retired: &str) -> Result<Self> {
        self.layout = self.layout.with_retired_root(retired)?;
        Ok(self)
    }

    /// Registers the v3 root as owned by its owner, once, before a write.
    /// Already registered (`409`), the root's record is read and the owner
    /// added to its members when it is not an owner yet, so a `409` never
    /// passes for ownership. Any failure does not fail the write, which
    /// CortexDB then admits as usual, and the next write tries again. That
    /// cannot hand the root to another owner:
    /// CortexDB auto-registers only the scope a write lands in, and v3
    /// writes only to leaves below the root, never to the root itself, so
    /// the root stays unregistered until this succeeds.
    async fn register_root(&self) {
        let (ScopeLayout::V3 { root, .. }, Some(owner)) = (&self.layout, &self.owner) else {
            return;
        };
        if root.is_empty() {
            return;
        }
        if self.wire() != CortexWire::Direct || self.registered.load(Ordering::Acquire) {
            return;
        }
        let body = serde_json::json!({
            "path": root,
            "members": [{ "actor": owner, "role": "owner" }],
        });
        let path = self.wire().path(Route::RegisterScope);
        match self
            .log
            .client
            .json(
                reqwest::Method::POST,
                path,
                Some(&body),
                crate::cortex::transport::Attempts::Once,
            )
            .await
        {
            Ok(_) => self.registered.store(true, Ordering::Release),
            Err(Error::Conflict(_)) => match self.claim_root(root, owner).await {
                Ok(()) => self.registered.store(true, Ordering::Release),
                Err(error) => {
                    log::warn!("[cortex] making `{owner}` an owner of `{root}` failed: {error}");
                }
            },
            Err(error) => {
                log::warn!("[cortex] registering the scope root `{root}` failed: {error}");
            }
        }
    }

    /// Makes `owner` an owner of the already registered `root`: reads its
    /// record, and when `owner` is not among its owners, writes the members
    /// back with it added (CortexDB's member edit takes the whole list).
    async fn claim_root(&self, root: &str, owner: &str) -> Result<()> {
        let path = format!(
            "{}?path={}",
            self.wire().path(Route::RegisterScope),
            urlencode(root)
        );
        let record = self
            .log
            .client
            .json(
                reqwest::Method::GET,
                &path,
                None,
                crate::cortex::transport::Attempts::RetryTransient,
            )
            .await?;
        let mut members: Vec<serde_json::Value> = record
            .get("members")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        let owns = members
            .iter()
            .any(|member| member["actor"] == owner && member["role"] == "owner");
        if owns {
            return Ok(());
        }
        members.push(serde_json::json!({ "actor": owner, "role": "owner" }));
        let path = format!(
            "{}?path={}",
            self.wire().path(Route::ScopeMembers),
            urlencode(root)
        );
        self.log
            .client
            .json(
                reqwest::Method::PUT,
                &path,
                Some(&serde_json::json!({ "members": members })),
                crate::cortex::transport::Attempts::Once,
            )
            .await
            .map(|_| ())
    }

    /// The same engine, attributing each write to who said or did it when
    /// `on` (CortexDB's `observed_actor`, with the owner as `subject`): an
    /// assistant turn to its agent, a user turn or an item naming an
    /// observed actor to that person. Only the direct wire attributes; off, or on the TinyHumans
    /// backend, nothing on the wire changes. See the `attribution` module.
    #[must_use]
    pub fn with_observed_actor(mut self, on: bool) -> Self {
        self.attribution = Arc::new(attribution::Attribution::new(on));
        self
    }

    /// The same engine, declaring `consolidation` instead of the endpoint's
    /// default: for a self-hosted CortexDB whose operator runs the layer
    /// scheduler ([`Consolidation::Automatic`]), or a managed one a host
    /// wants to build by hand ([`Consolidation::OnDemand`]). Either way an
    /// explicit [`MemoryEngine::consolidate`] still builds.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] for a mode the wire cannot serve: a direct engine
    /// is [`Consolidation::OnDemand`] or [`Consolidation::Automatic`], and
    /// the TinyHumans backend only [`Consolidation::Scheduled`]; a proxied
    /// one is either of the direct engine's.
    pub fn with_consolidation(mut self, consolidation: Consolidation) -> Result<Self> {
        let served = match self.wire() {
            CortexWire::Direct | CortexWire::Proxied => matches!(
                consolidation,
                Consolidation::OnDemand | Consolidation::Automatic
            ),
            CortexWire::TinyHumans => consolidation == Consolidation::Scheduled,
        };
        if !served {
            return Err(Error::Config(format!(
                "the `{}` engine cannot consolidate as {consolidation:?}",
                self.descriptor.id
            )));
        }
        self.descriptor.consolidation = consolidation;
        Ok(self)
    }

    /// CortexDB's own `/v1/*` API at `endpoint` (for example
    /// [`crate::cortex::CORTEX_API_ENDPOINT`]), registered as `cortexdb`.
    ///
    /// # Errors
    ///
    /// As [`CortexEngine::new`].
    pub fn direct(endpoint: &str, credential: CortexCredential) -> Result<Self> {
        Self::new(CortexWire::Direct, endpoint, credential)
    }

    /// CortexDB behind the TinyHumans backend at `base_url` (for example
    /// [`crate::cortex::TINYHUMANS_API_ENDPOINT`]), registered as `tinyhumans`.
    /// `bearer` supplies the session JWT or `tiny_live_` API key and is
    /// consulted on every request, so a refreshed session is used at once.
    ///
    /// # Errors
    ///
    /// As [`CortexEngine::new`].
    pub fn tinyhumans(base_url: &str, bearer: Arc<dyn BearerSource>) -> Result<Self> {
        Self::new(
            CortexWire::TinyHumans,
            base_url,
            CortexCredential::Dynamic(bearer),
        )
    }

    /// CortexDB behind a tenant-multiplexing cortexdb-saas proxy (memory-api)
    /// at `endpoint`, registered as `cortexdb-proxied`, with the proxy's
    /// service `credential`.
    ///
    /// The proxy serves many tenants behind one credential and pins each
    /// one's scopes under its own root, so this engine:
    ///
    /// - names the tenant of every call in `X-Memory-Tenant`: the id of the
    ///   leading `user:` segment of the call's namespace (a host binds each
    ///   agent to a namespace rooted at `user:<id>`). A call naming no such
    ///   namespace, or two different ones, fails with [`Error::Config`]
    ///   before anything is sent; there is no shared or default tenant.
    ///   Ids are 1 to 96 characters of `[A-Za-z0-9_-]`;
    /// - lays its scopes out relative to the tenant's root, sending no root
    ///   and registering none ([`CortexEngine::with_tenant_root`] is
    ///   already in effect; [`CortexEngine::with_scope_root`] is refused);
    /// - speaks only the `/v1/*` routes the proxy allows: a scope is erased
    ///   by forgetting its events (`v1/forget`), and an erasure of a
    ///   tenant's whole `user:<id>` subtree, every kind, purges the tenant
    ///   (`POST v1/tenant/purge`, which also takes anything else stored
    ///   under that tenant's root).
    ///
    /// An in-cluster proxy reached over plain HTTP needs
    /// [`CortexEngine::with_private_network_cleartext`].
    ///
    /// # Errors
    ///
    /// As [`CortexEngine::new`].
    pub fn proxied(endpoint: &str, credential: CortexCredential) -> Result<Self> {
        Self::new(CortexWire::Proxied, endpoint, credential)
    }

    /// The same engine, allowed to send its credential over cleartext HTTP
    /// to a Kubernetes cluster-service host (`<service>.<namespace>.svc`,
    /// optionally `.cluster.local`): for an in-cluster service whose traffic
    /// NetworkPolicy keeps inside the cluster. Any other cleartext endpoint
    /// is still refused when the engine is built, and HTTPS or loopback
    /// endpoints are unchanged. Without this, an engine on a cleartext
    /// cluster-service endpoint fails every call with [`Error::Config`].
    #[must_use]
    pub fn with_private_network_cleartext(mut self) -> Self {
        self.log.client.allow_private_network_cleartext();
        self
    }

    /// The same engine, sending `headers` on every request: fixed,
    /// non-credential headers such as a host's product attribution
    /// (`x-sdk-name`). Later calls replace earlier ones.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] for an invalid header name or value, or a header the
    /// transport sets itself (`Authorization`, `Idempotency-Key`, the actor
    /// header, the proxied wire's tenant header, `Host`, `Cookie`, …).
    pub fn with_default_headers<K, V>(
        mut self,
        headers: impl IntoIterator<Item = (K, V)>,
    ) -> Result<Self>
    where
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let map = crate::cortex::transport::default_headers(headers)?;
        self.log.client.set_default_headers(map);
        Ok(self)
    }

    /// Which HTTP surface this engine talks to.
    #[must_use]
    pub fn wire(&self) -> CortexWire {
        self.log.client.wire()
    }
}

#[async_trait]
impl MemoryEngine for CortexEngine {
    fn descriptor(&self) -> &EngineDescriptor {
        &self.descriptor
    }

    /// Direct probes `v1/admin/health`; hosted lists one scope under a
    /// prefix this crate never writes (the backend has no health route, and
    /// this proves reachability and the credential in one round trip);
    /// proxied probes `v1/admin/health` as a tenant this crate never writes
    /// under (the proxy needs a tenant on every request). An
    /// [`Error::Unavailable`] failure is `Degraded`, any other `Down`; the
    /// reason never carries the backend's own text.
    async fn health(&self) -> EngineHealth {
        let wire = self.wire();
        if wire == CortexWire::Proxied {
            let probe = match self.as_tenant(tenant::HEALTH_PROBE_TENANT) {
                Ok(probe) => probe,
                Err(error) => return EngineHealth::Down(health_reason(&error)),
            };
            return match probe.log.client.probe(wire.path(Route::Health)).await {
                Ok(()) => EngineHealth::Ok,
                Err(error @ Error::Unavailable(_)) => EngineHealth::Degraded(health_reason(&error)),
                Err(error) => EngineHealth::Down(health_reason(&error)),
            };
        }
        let path = match wire {
            CortexWire::Direct | CortexWire::Proxied => wire.path(Route::Health).to_string(),
            CortexWire::TinyHumans => format!(
                "{}?prefix={}&limit=1",
                wire.path(Route::Health),
                urlencode(HEALTH_PROBE_SCOPE)
            ),
        };
        match self.log.client.probe(&path).await {
            Ok(()) => EngineHealth::Ok,
            Err(error @ Error::Unavailable(_)) => EngineHealth::Degraded(health_reason(&error)),
            Err(error) => EngineHealth::Down(health_reason(&error)),
        }
    }

    async fn recall(&self, req: RecallRequest) -> Result<RecallAnswer> {
        self.for_call(reach_at(req.filter.reach.as_ref()))?
            .recall_answer(req)
            .await
    }

    async fn fetch(&self, req: FetchRequest) -> Result<FetchPage> {
        self.for_call(reach_at(req.filter.reach.as_ref()))?
            .fetch_page(req)
            .await
    }

    /// A batch of one (see `store`): listed and ranked on return.
    async fn store(&self, item: StoreItem) -> Result<StoreReceipt> {
        self.store_with(item, WriteOptions::visible()).await
    }

    /// [`WaitFor::Accepted`] returns once CortexDB captured the events,
    /// without `?wait=indexed` and without the visibility waits.
    async fn store_with(&self, item: StoreItem, options: WriteOptions) -> Result<StoreReceipt> {
        self.for_call([&item.meta().namespace])?
            .store_items(vec![item], options.wait)
            .await?
            .pop()
            .ok_or_else(|| Error::Engine("a store of one item returned no receipt".to_string()))
    }

    /// Ranked recall is awaited for the last item only (see `store`).
    async fn store_many(&self, items: Vec<StoreItem>) -> Result<Vec<StoreReceipt>> {
        if items.is_empty() {
            return self.store_items(items, WaitFor::Visible).await;
        }
        self.for_call(items.iter().map(|item| &item.meta().namespace))?
            .store_items(items, WaitFor::Visible)
            .await
    }

    /// [`WaitFor::Accepted`] returns once CortexDB captured every event,
    /// without `?wait=indexed` and without the visibility waits.
    async fn store_many_with(
        &self,
        items: Vec<StoreItem>,
        options: WriteOptions,
    ) -> Result<Vec<StoreReceipt>> {
        if items.is_empty() {
            return self.store_items(items, options.wait).await;
        }
        self.for_call(items.iter().map(|item| &item.meta().namespace))?
            .store_items(items, options.wait)
            .await
    }

    /// On the proxied wire, a forget by bare ids names no tenant and is
    /// refused: forget within a reach instead.
    async fn forget(&self, target: ForgetTarget) -> Result<ForgetReport> {
        let reach = match &target {
            ForgetTarget::Filter(filter) => filter.reach.as_ref(),
            ForgetTarget::Ids(_) => None,
        };
        self.for_call(reach_at(reach))?.forget_items(target).await
    }

    /// By the ids' labels in the scopes the reach admits only, never a
    /// listing of the whole tree.
    async fn forget_within(&self, ids: Vec<ItemId>, reach: Reach) -> Result<ForgetReport> {
        self.for_call([&reach.at])?
            .forget_items_within(ids, reach)
            .await
    }

    async fn list(&self, req: ListRequest) -> Result<ListPage> {
        self.for_call(reach_at(req.filter.reach.as_ref()))?
            .list_page(req)
            .await
    }

    /// A conversation or chunked document from the event that starts it,
    /// without assembling it (see `list`).
    async fn list_preview(&self, req: ListRequest) -> Result<ListPage> {
        self.for_call(reach_at(req.filter.reach.as_ref()))?
            .list_preview_page(req)
            .await
    }

    async fn export(&self, req: ListRequest) -> Result<ExportPage> {
        self.for_call(reach_at(req.filter.reach.as_ref()))?
            .export_page(req)
            .await
    }

    async fn erase(&self, req: EraseRequest) -> Result<EraseReport> {
        req.validate()?;
        self.for_call([&req.reach.at])?.erase_scopes(req).await
    }

    /// From each item's first event, without assembling conversations or
    /// chunked documents (see `explore`).
    async fn explore(&self, req: ExploreRequest) -> Result<ExplorePage> {
        self.for_call(reach_at(req.filter.reach.as_ref()))?
            .explore_items(req)
            .await
    }

    /// By the items' id labels, one lookup per kind, rather than a scan.
    async fn get(&self, req: GetRequest) -> Result<Vec<Hit>> {
        self.for_call(reach_at(req.reach.as_ref()))?
            .get_items(req)
            .await
    }

    /// Direct: one `v1/beliefs/build` per held scope in reach, whether the
    /// engine declares [`Consolidation::OnDemand`] or
    /// [`Consolidation::Automatic`]. Hosted: acknowledged as scheduled, with
    /// no request.
    async fn consolidate(&self, req: ConsolidateRequest) -> Result<ConsolidateReceipt> {
        self.for_call([&req.reach.at])?.build_beliefs(req).await
    }

    async fn beliefs(&self, req: BeliefsRequest) -> Result<Vec<Hit>> {
        self.for_call([&req.reach.at])?.read_beliefs(req).await
    }
}

/// The node a call reads from, as the namespaces naming its tenant: none
/// without a reach.
fn reach_at(reach: Option<&Reach>) -> Option<&tinymemory_api::Namespace> {
    reach.map(|reach| &reach.at)
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "mod_list_tests.rs"]
mod list_tests;

#[cfg(test)]
#[path = "mod_list_preview_tests.rs"]
mod list_preview_tests;

#[cfg(test)]
#[path = "mod_chunk_tests.rs"]
mod chunk_tests;

#[cfg(test)]
#[path = "mod_export_tests.rs"]
mod export_tests;

#[cfg(test)]
#[path = "mod_erase_tests.rs"]
mod erase_tests;

#[cfg(test)]
#[path = "mod_direct_tests.rs"]
mod direct_tests;

#[cfg(test)]
#[path = "mod_hosted_tests.rs"]
mod hosted_tests;

#[cfg(test)]
#[path = "mod_layout_tests.rs"]
mod layout_tests;

#[cfg(test)]
#[path = "engine_test_support.rs"]
mod test_support;

#[cfg(test)]
#[path = "mod_retired_root_tests.rs"]
mod retired_root_tests;

#[cfg(test)]
#[path = "mod_forget_within_tests.rs"]
mod forget_within_tests;

#[cfg(test)]
#[path = "mod_subtree_listing_tests.rs"]
mod subtree_listing_tests;

#[cfg(test)]
#[path = "mod_proxied_tests.rs"]
mod proxied_tests;
