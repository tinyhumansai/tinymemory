//! Loadable `TinyBus` module adapter for `TinyMemory`.
//!
//! This private workspace crate keeps the vendored `TinyBus` dependency out of
//! the published `tinymemory` crates. Its default `cdylib` output is the
//! target-specific binary distributed in GitHub releases. With `static-link`,
//! a host can instead reference the descriptor, manifest, and initializer by
//! Rust path without colliding with another module's C symbols.
//!
//! # What this module is for, stated honestly
//!
//! It carries the memory **engine** — `tinycortex` and `tinymemory-core` — so a
//! host that loads it compiles neither.
//!
//! It is worth being precise about the benefit, because the obvious guess is
//! wrong. This module sheds **no third-party dependencies** from a host. Every
//! crate the engine uses (`rusqlite`, `reqwest`, `chrono`, `regex`, `uuid`,
//! `walkdir`, `sha2`, `tokio`) is shared with surface a host keeps, and
//! `libsqlite3-sys` in particular has several other parents, so the native
//! `SQLite` build remains in the host dependency graph. This was measured on
//! `OpenHuman`, on both its kernel and its shipping feature
//! profiles: four crate names leave, and all four are ours.
//!
//! What it does buy is **compile time on the critical path**, and that was
//! measured too. `tinycortex` and `tinymemory-core` compile strictly serially
//! ahead of the host crate — `tinycortex` → `tinymemory-core` → host, each
//! starting as the previous one ends — putting 14.7s directly in
//! front of the host's own compilation. Removing them from the host's graph
//! moved a full build from 176s to about 161s.
//!
//! Do not re-justify this module on dependency count. The number is zero and it
//! is written down here so nobody re-derives it optimistically.
//!
//! # It carries no credentials
//!
//! The engine needs embeddings, embeddings need an inference credential, and
//! that credential stays in the host. The module asks the host to embed over the
//! bus instead — see [`embedding`], which is the same split the `tinywallet`
//! module makes with a signing key.
//!
//! [`config::ModuleConfig`]'s own fields cannot hold a key, but that is not
//! sufficient on its own and it is worth saying why: it embeds
//! `tinymemory_api::host::MemoryConfig` **verbatim**, and that struct contains
//! `agentmemory_secret`, a bearer token for a remote memory backend. So the
//! property is *enforced* at setup by
//! [`config::ModuleConfig::strip_host_credentials`], not merely asserted about a
//! field list. "Carried verbatim" carries credentials verbatim too.
//!
//!
//! # Scope: the complete TinyMemory API
//!
//! The module boundary mirrors every capability family in `tinymemory_api`.
//! Host applications keep policy, scheduling, credentials, and bus/event types;
//! memory storage, retrieval, ingestion, trees, graph operations, goals, source
//! persistence, and maintenance execute inside this compiled module.

// Test code may panic; library code may not. The `[lints]` table cannot be
// scoped to non-test builds, so the exemption is expressed here instead.
#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::unwrap_used,
        clippy::panic,
        clippy::cast_precision_loss
    )
)]

pub mod chat;
pub mod config;
pub mod config_loader;
pub mod embedding;
mod host;
mod provider;
#[cfg(test)]
mod seam_lock;
mod service;

pub use chat::{CHAT_HOST_BUS_NAME, CHAT_HOST_INTERFACE, CHAT_HOST_OBJECT_PATH};
pub use config::ModuleConfig;
pub use config_loader::ModuleConfigLoader;
pub use embedding::{
    BusEmbeddingHost, BusEmbeddingProvider, EMBEDDING_HOST_BUS_NAME, EMBEDDING_HOST_INTERFACE,
    EMBEDDING_HOST_OBJECT_PATH,
};
pub use host::{RUNTIME_HOST_BUS_NAME, RUNTIME_HOST_INTERFACE, RUNTIME_HOST_OBJECT_PATH};
pub use service::{BUS_NAME, OBJECT_PATH};

/// Rust-addressable TinyBus ABI entries for an in-process linked host.
#[cfg(feature = "static-link")]
pub use exports::{tinybus_module_init_v1, tinybus_module_manifest_v1, TINYBUS_MODULE_ABI_V1};

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use tinybus::{Connection, Error as BusError, Result as BusResult};
use tinymemory_core::store::MemoryClientRef;

/// The module refused its configuration or could not bring up a store.
const SETUP_FAILED_ERROR: &str = "ai.tinyhumans.tinymemory.Error.SetupFailed";

/// Bring up the engine and serve it.
///
/// # Order matters
///
/// The bus-backed [`BusEmbeddingHost`] is installed **before** the engine is
/// constructed. `tinymemory-core` resolves its embedder through a process-global
/// during construction, and a store built before the host is installed would
/// either fail or — worse — bind the inert zero-dimension provider and write
/// vectors nobody can search. The global is why this is a `set` and not an
/// argument: the construction sites sit deep inside retrieval and sealing call
/// stacks that already thread a config and a store handle.
///
/// # The empty API key is deliberate
///
/// `create_memory_with_local_ai` is handed `""`. Every embed goes over the bus to
/// the host, which holds the real credential, so there is nothing to pass and
/// nothing here that could leak one.
async fn setup(connection: Connection, mut config: ModuleConfig) -> BusResult<()> {
    config.validate().map_err(setup_error)?;
    claim_process_setup()?;

    // `MemoryConfig` travels verbatim, and it contains a bearer token field for a
    // remote memory backend. Carried credentials are exactly what this module
    // refuses to hold, so it goes before anything else touches the config.
    if config.strip_host_credentials() {
        log::warn!(
            "[tinymemory:module] discarded a remote-backend credential from the \
             supplied config; this module serves the local engine only, so bind a \
             remote memory driver directly instead of through it"
        );
    }

    log::debug!(
        "[tinymemory:module] setup driver_id={} routes={} cloud_dims={}",
        config.driver_id,
        config.embedding_routes.len(),
        config.cloud_embedding_dimensions
    );

    // Install the embedder first. See the doc comment.
    tinymemory_core::embedding_host::set_embedding_host(Arc::new(BusEmbeddingHost::new(
        connection.clone(),
        &config,
    )));
    tinymemory_core::chat_host::set_chat_host(Arc::new(chat::BusChatHost::new(
        connection.clone(),
        &config,
    )));
    // The config loader is the opposite call, and deliberately: it is answered
    // from `config` — which is this line's whole argument — rather than asking
    // the host to re-read what it already handed over. It goes *after* the
    // credential strip above, because this is the seam that hands the config
    // back out to the engine repeatedly.
    tinymemory_core::config_loader::set_config_loader(Arc::new(ModuleConfigLoader::new(&config)));
    host::install(connection.clone());
    // The scheduler gate is proxied to the host's SchedulerPolicy member — the
    // host's cron::scheduler_gate policy, polled and cached, so mode=off,
    // signed-out and battery pauses are honoured inside this process too.
    // Shutdown banks the engine's hooks and runs them on this module's own
    // `Shutdown` member, so graceful queue-lock release works whenever the host
    // shuts the driver down before exiting (see `host::install_seams`).
    // Installed with the rest, before the store exists, so nothing can consult
    // a seam this process has not yet decided about — and so a hook registered
    // by the queue pool below always finds the bank already there.
    host::install_seams(Some(connection.clone()));

    let client = tinymemory_core::store::factories::create_memory_client_with_local_ai(
        &config.memory,
        None,
        "",
        &config.embedding_routes,
        config.storage_provider.as_ref(),
        &config.workspace_dir,
    )
    .map_err(|error| {
        // The factory error names the workspace directory it failed under, and
        // a `MethodFailed.message` crosses the bus to a caller that has no
        // business learning this process's filesystem layout. The detail stays
        // in the module's own log; the wire gets the stage only.
        log::error!("[tinymemory:module] create memory store failed: {error}");
        setup_error("create memory store")
    })?;
    let client: MemoryClientRef = Arc::new(client);

    // After the store, never before: `queue::start` recovers stale locks as its
    // first act, which opens the queue database, and the factory above is what
    // creates the workspace it lives in.
    start_queue_pool(&config);

    // Also after the store, and for a second reason on top of that one: what is
    // published is the client just built, and there is nothing to publish until
    // it exists. The sync loops follow the bind rather than the other way round
    // — every runner in `sync::pipelines::host` opens with
    // `global::client_if_ready()`, so a loop started before this would fail
    // every run.
    if bind_memory_client(&config, &client) {
        start_sync_loops(&config);
    }

    let provider = provider::provider(&config, client);
    service::serve(&connection, Arc::new(provider), config).await
}

/// Publish the store this process just built as the client for its workspace.
///
/// # Why this is a `bind` and not `global::init`
///
/// Everything in `tinymemory_core::sync` resolves its store through
/// `global::client_if_ready()`, which is `None` in this process: the module
/// builds its store through `create_memory_client_with_local_ai` — the only
/// entry point that takes this module's embedding routes, storage provider and
/// workspace — and that factory never touches the global slot.
///
/// The obvious repair, `global::init(workspace)`, is the wrong one and quietly
/// so. It constructs a *second* `MemoryClient` over the same SQLite file, with
/// the host's default routes rather than this module's, and each client owns an
/// ingestion worker: duplicate graph extraction and duplicate embedding work
/// against one store, which `global`'s own comments call out as the hazard its
/// per-workspace cache exists to prevent. `global::bind` publishes the client
/// that already exists instead, into both the global slot and the per-workspace
/// cache, so all three resolution paths converge on it.
///
/// # Which slot this writes
///
/// This module's own. The `cdylib` carries its own compiled copy of
/// `tinymemory-core`, so the slot filled here is the static that *this
/// process's module-side* loops read through `client_if_ready`, and not the one
/// a host still booting an in-process engine fills with `global::init`. That is
/// what makes binding safe to do before that host's engine is deleted: this
/// cannot repoint the host's engine at this client, and the host's `init`
/// cannot make this bind refuse.
///
/// The refusal below therefore means one specific thing — a second
/// `MemoryClient` was built for this workspace *inside this module* — which is
/// the hazard the whole function exists to keep from happening quietly.
///
/// # Returns
///
/// Whether the client is bound. A failure is reported and the caller starts no
/// sync loops: with no client resolvable, every run in both loops would fail on
/// its first line with "memory client is not ready" — a named cause, but a loop
/// that can only fail is not worth the ticks or the failed-sync audit rows it
/// would append forever.
fn bind_memory_client(config: &ModuleConfig, client: &MemoryClientRef) -> bool {
    match tinymemory_core::global::bind(config.workspace_dir.clone(), Arc::clone(client)) {
        Ok(_) => true,
        Err(error) => {
            // The path in `error` stays in this module's log, like the factory
            // failure above; nothing here crosses the bus.
            log::error!(
                "[tinymemory:module] could not publish the memory client for this workspace, so \
                 periodic memory sync will not run in this process: {error}"
            );
            false
        }
    }
}

/// Start the engine's workspace periodic sync loop for this process.
///
/// # Why the module has to own these
///
/// The same reason [`start_queue_pool`] does. The workspace loop is engine code
/// and until now the host's in-process engine was the only caller. A host that
/// deletes that engine, which is the entire point of loading this module, would
/// otherwise leave registered repos, folders, RSS feeds, and web pages stale.
///
/// # The host must stop starting them in the same change
///
/// Not "should" — this is the one part the module cannot guard. The `cdylib`
/// carries its own copy of `tinymemory-core`, so the `OnceLock` each loop
/// guards itself with is a *different* static from the host's: a host that
/// still starts this loop while loading this module gets two loops, neither of
/// which can see the other, both walking the same source
/// registry into the same store. [`claim_sync_loops`] catches only the
/// in-process case. So the host's call site goes in the same change that
/// deletes the engine it was calling against.
///
/// # What they do not get in module mode
///
/// Stated rather than hidden, in the same terms [`start_queue_pool`] states its
/// own two:
///
/// - **The loop does not honour scheduler-gate pauses.** It calls
///   `periodic_pause_reason` as step 0 of every tick, precisely so a user who
///   switched Memory Tree off, or who is signed out, gets no background fetch.
///   This module serves no scheduler gate — see the section comment on
///   `host::install_seams` for why it cannot — and the stub in its
///   place always answers `Policy::Normal`, so `periodic_pause_reason` is always
///   `None` and it ticks straight through both pauses. The per-source
///   `enabled` toggle still applies; the two *global* pauses do not.
/// - **Their resume wake never fires.** The stub's `resume_notify` hands back a
///   `Notify` nobody signals, so a user who re-enables sync waits out the
///   remaining 20-minute tick instead of syncing within seconds. That is the
///   benign half of the same gap.
///
fn start_sync_loops(config: &ModuleConfig) {
    match claim_sync_loops(&config.workspace_dir) {
        WorkspaceClaim::Start => {
            // Warn, not debug: it is true on every boot in module mode, and a
            // reader of the log should not have to know which seams are stubbed
            // to find out that the pauses are not in effect.
            log::warn!(
                "[tinymemory:module] starting the periodic memory sync loops in this process. \
                 They do not honour the scheduler gate — it is unserved here, so the \
                 \"Memory Tree off\" and \"signed out\" pauses are ignored and a re-enable is \
                 not woken early — though each source's own enabled toggle still applies"
            );
            // Workspace sources run in every module configuration.
            tinymemory_core::sync::workspace::start_workspace_periodic_sync();
        }
        WorkspaceClaim::AlreadyRunning => {
            log::debug!(
                "[tinymemory:module] the periodic memory sync loops for this workspace are \
                 already running"
            );
        }
        WorkspaceClaim::Foreign => {
            log::error!(
                "[tinymemory:module] the periodic memory sync loops are already running for a \
                 different workspace in this process, and both guard themselves process-wide, \
                 so this store gets no periodic sync: registered sources will not update. \
                 One module process serves one workspace"
            );
        }
    }
}

/// The workspace whose queue this process's worker pool drains.
///
/// The pool is bound to one workspace — every `queue::store` entry point
/// resolves its database through `engine_config`, which roots at
/// `config.workspace_dir()` — while the `Once` inside `queue::start` is
/// process-global. Those two facts together are the trap this cell exists for:
/// a second `start` under a different workspace is not a second pool, it is a
/// silent no-op leaving that store's queue with nothing draining it. Recording
/// which workspace won makes that case loud instead of invisible.
static QUEUE_POOL_WORKSPACE: OnceLock<PathBuf> = OnceLock::new();

/// The workspace whose periodic sync loops this process drives.
///
/// A separate cell from [`QUEUE_POOL_WORKSPACE`] because they are separate
/// services that can each be claimed or not, but the trap is identical and so
/// is the reasoning: `start_periodic_sync` and `start_workspace_periodic_sync`
/// each guard themselves with a process-global `OnceLock<()>`, which makes a
/// second call a no-op that is indistinguishable from a first that worked, while
/// what each loop actually syncs is rooted at whatever workspace the installed
/// `config_loader` answers for.
static SYNC_LOOPS_WORKSPACE: OnceLock<PathBuf> = OnceLock::new();

/// What a claim on one of this process's workspace-bound background services
/// found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspaceClaim {
    /// Nothing had claimed the service; this caller starts it.
    Start,
    /// It is already running for this workspace, so there is nothing to do and
    /// nothing wrong.
    AlreadyRunning,
    /// It is running, but rooted somewhere else. This store cannot be given one
    /// of its own, and goes without.
    Foreign,
}

/// Decide whether this caller is the one that starts `cell`'s service.
///
/// Split out from the two `start_*` functions so each decision can be asserted
/// without spawning real workers and tick loops into a test process, and because
/// the guards inside `tinymemory-core` are not observable from here at all — a
/// second call to any of them is indistinguishable from a first that worked.
fn claim_workspace(cell: &OnceLock<PathBuf>, workspace: &Path) -> WorkspaceClaim {
    match cell.set(workspace.to_path_buf()) {
        Ok(()) => WorkspaceClaim::Start,
        // `set` hands the rejected value back, so the comparison needs no
        // second read and cannot race with a concurrent claim.
        Err(rejected) => {
            if cell.get() == Some(&rejected) {
                WorkspaceClaim::AlreadyRunning
            } else {
                WorkspaceClaim::Foreign
            }
        }
    }
}

/// Claim the queue worker pool for `workspace`. See [`start_queue_pool`].
pub(crate) fn claim_queue_pool(workspace: &Path) -> WorkspaceClaim {
    claim_workspace(&QUEUE_POOL_WORKSPACE, workspace)
}

/// Claim the periodic sync loops for `workspace`. See [`start_sync_loops`].
pub(crate) fn claim_sync_loops(workspace: &Path) -> WorkspaceClaim {
    claim_workspace(&SYNC_LOOPS_WORKSPACE, workspace)
}

/// Start the engine's queue worker pool for this process.
///
/// # Why the module has to own this
///
/// Every enqueue this driver makes is inert without a pool draining it, and the
/// enqueues are not incidental: `FlushPending` and `RetryFailed` schedule work
/// rather than doing it, the re-embed backfill is a queued job, and the ingest
/// path's `extract_chunk` is *how ingested content becomes retrievable at all*.
/// Until now the only `queue::start` call in any tree was the host's, made
/// against the second, in-process engine the host also booted. A host that
/// deletes that engine — which is the entire point of loading this module —
/// turns all four into permanent no-ops with no error anywhere: ingestion still
/// reports success and the content is simply never indexed. So the pool moves
/// in here, alongside the engine that needs it.
///
/// # Two things it does not get in module mode
///
/// Stated rather than hidden, because this is a real product degradation the
/// host does not have today. The pool consults
/// [`tinymemory_core::scheduler_gate`] before every claim and registers a
/// [`tinymemory_core::shutdown`] hook to release in-flight job locks. This
/// module serves neither seam — see the section comment on
/// `host::install_seams` for why neither can be proxied — so both are
/// stubs, and the consequences follow:
///
/// - **It runs unthrottled.** `wait_for_capacity` returns immediately, so
///   background memory work in this process ignores the host's background-AI
///   throttle: the user's toggle, AC power, CPU pressure, signed-out. On a
///   laptop that means the queue drains at full tilt on battery, which the
///   host's in-process engine would not do.
/// - **Its shutdown hook is dropped.** A clean exit therefore leaves `running`
///   rows locked. They are reclaimed by lease expiry at the next start —
///   `recover_stale_locks` is the first thing `queue::start` does, and
///   `queue::worker` documents that as the hard-kill path — so the cost is one
///   lease of latency after a restart, not lost work.
///
/// Closing either properly needs a `SchedulerGate` bus interface this crate
/// owns only one half of, which is separate work. Until then the stubs report
/// once per process the first time the pool consults them.
fn start_queue_pool(config: &ModuleConfig) {
    match claim_queue_pool(&config.workspace_dir) {
        WorkspaceClaim::Start => {
            // Warn, not debug: it is true on every boot in module mode, and a
            // reader of the log should not have to know which seams are stubbed
            // to find out that the throttle is not in effect.
            log::warn!(
                "[tinymemory:module] starting the memory queue worker pool in this process. \
                 It runs unthrottled — the scheduler gate is unserved here, so background \
                 memory work ignores the host's background-AI throttle, AC power and CPU \
                 pressure — and its graceful lock-release hook is dropped, so locks held at \
                 exit are reclaimed by lease expiry on the next start"
            );
            tinymemory_core::queue::start(Arc::new(
                tinymemory_tinycortex::engine::EngineRuntimeConfig::from(config),
            ));
        }
        WorkspaceClaim::AlreadyRunning => {
            log::debug!(
                "[tinymemory:module] the queue worker pool for this workspace is already running"
            );
        }
        WorkspaceClaim::Foreign => {
            log::error!(
                "[tinymemory:module] a queue worker pool is already running for a different \
                 workspace in this process, and `queue::start` is guarded process-wide, so the \
                 store just opened has nothing draining its queue: ingested content will not be \
                 indexed and flushes and retries will not run. One module process serves one \
                 workspace"
            );
        }
    }
}

/// Claim this process's single setup slot.
///
/// `setup` installs **process-global** host callbacks, so it is not
/// re-entrant the way a per-host resource would be. `ModuleHost` rejects a
/// duplicate module name only within one host, and nothing stops a process from
/// building a second host — a test harness is the obvious way it happens. The
/// second `setup` would replace the global embedder while stores built by the
/// first keep the `BusEmbeddingProvider` they captured, so embeds would be split
/// across two connections with no error anywhere.
///
/// Refusing the second setup is the honest outcome: one process serves this
/// module once. tinybus never unloads a library, so there is no release path to
/// pair with this and no state to reset.
///
/// # Errors
///
/// [`SETUP_FAILED_ERROR`], when this process has already run setup.
fn claim_process_setup() -> BusResult<()> {
    static CLAIMED: AtomicBool = AtomicBool::new(false);

    if CLAIMED.swap(true, Ordering::SeqCst) {
        return Err(setup_error(
            "this module is already set up in this process; it installs a \
             process-global host callbacks and cannot be served twice",
        ));
    }
    Ok(())
}

/// A setup failure, carrying no path and no credential.
fn setup_error(message: impl Into<String>) -> BusError {
    BusError::MethodFailed {
        name: SETUP_FAILED_ERROR.to_string(),
        message: message.into(),
    }
}

// Isolate the generated ABI symbols so the lint exception cannot hide
// undocumented Rust API. The static-link feature exports these by Rust path;
// the default gives them the established dynamic C symbol names.
#[allow(
    missing_docs,
    unreachable_pub,
    reason = "generated C ABI symbols are documented by the TinyBus module SDK"
)]
mod exports {
    #[cfg(not(feature = "static-link"))]
    use tinybus_module::module_export as export_module;
    #[cfg(feature = "static-link")]
    use tinybus_module::module_export_static as export_module;

    export_module! {
        setup = super::setup,
        config = super::ModuleConfig,
        // Eight, derived rather than picked. Two are the floor this module has
        // always needed: a recall that triggers an embed makes an outbound call
        // while still inside its own inbound call, so a single worker would
        // deadlock on the first semantic query. `setup` now also starts the
        // engine's queue pool — four job workers plus the daily scheduler — and
        // those five run the engine's SQLite claim and settle synchronously
        // inside their async loops, so a busy one occupies a runtime thread
        // outright instead of yielding it. Two plus five is seven; the eighth
        // is what drives a job's own outbound embed while the rest are busy. At
        // two, a draining queue would starve inbound dispatch and the module
        // would stop answering recalls until the queue emptied.
        //
        // The periodic sync loop `setup` also starts does not move the
        // number. They sleep on a 20-minute `interval` and yield across every
        // fetch, so they hold no worker between ticks; the one moment they do is
        //
        // Nor do the long-running on-demand members. `RunConnectionSync` and
        // `RebuildFromRawArchive` await network and inference, so they yield
        // their worker between every step; every synchronous read behind
        // `SyncAuditLog`, `SyncStatuses` and `RawArchiveCoverage` hops to
        // `spawn_blocking`, which draws on the blocking pool rather than on
        // these eight. `IngestCodingSessions` is the one that occupies a thread
        // outright for its whole run — the persona pipeline is not `Send`, so
        // the driver drives it from a blocking worker — and that is again the
        // blocking pool, not a runtime worker.
        worker_threads = 8,
        provides = ["ai.tinyhumans.tinymemory.Memory"],
        methods = [
            "DriverId",
            "Capabilities",
            "Health",
            "Shutdown",
            "OpenStore",
            "InsertTurn",
            "SessionTurns",
            "OpenSegment",
            "CreateSegment",
            "AppendTurn",
            "CloseSegment",
            "SetSegmentSummary",
            "UpsertSegmentEmbedding",
            "InsertEvent",
            "Store",
            "Get",
            "Forget",
            "List",
            "Namespaces",
            "Recall",
            "ExportPage",
            "ImportRecords",
            // People.
            "ListPeople",
            "GetPerson",
            "ResolveHandle",
            "AddHandleAlias",
            "ScorePerson",
            "RecordInteraction",
            "SeedFromAddressBook",
            // Chunks.
            "ListChunks",
            "GetChunk",
            "ChunkDetail",
            "StorageKinds",
            "ChunkEmbeddings",
            "CountChunks",
            "ListChunkDetails",
            "SourceTotals",
            // Retrieval.
            "FastRetrieve",
            "CoverWindow",
            "RetrieveSource",
            "RetrieveChildren",
            "RetrieveLeaves",
            "RecallNamespaceScored",
            "SearchEntities",
            // Profile.
            "ListActiveFacets",
            "ListAllFacets",
            "GetFacet",
            "FacetsByType",
            "UpsertFacet",
            "UpsertProviderFacet",
            "SetFacetUserState",
            "DeleteFacet",
            "DeleteFacetById",
            "DropFacetsBelow",
            "WorkflowIdentityMatches",
            "IngestDocument",
            "IngestChat",
            "IngestEmail",
            "PutDocument",
            "GetDocument",
            "ListDocuments",
            "ListNamespaces",
            "DeleteDocument",
            "ClearNamespace",
            "QueryDocuments",
            // Predates the five families this port added; it was implemented
            // but never declared, so it was unreachable over the bus too.
            "RecallDocuments",
            "Append",
            "QuerySource",
            "DrillDown",
            "Seal",
            "Cascade",
            "Entities",
            "EntityEdges",
            "TouchEntities",
            "TopEntities",
            "ChunkEntities",
            "EntityChunkIds",
            "KvGet",
            "KvPut",
            "KvDelete",
            "KvList",
            "Relations",
            "PutRelation",
            "CaptureSnapshot",
            "Snapshots",
            "Diff",
            "Goals",
            "SetGoals",
            "ToolRules",
            "PutToolRule",
            "DeleteToolRule",
            "AcceptSourceItems",
            "ForgetSource",
            "ForgetMatching",
            "Reembed",
            "Compact",
            "Consolidate",
            "Doctor",
            "RetryFailed",
            "StoreStats",
            "QueueStats",
            "LatestQueueFailure",
            "BackfillInProgress",
            "FlushPending",
            // Closed-but-unsummarised segments, for the host re-summarisation
            // pass (openhuman#6186). Beside its family: this list is a SET.
            "SegmentsPendingSummary",
            // Re-files connector documents stored before the routing fix
            // (openhuman#6007) into the memory tree. Declared beside its
            // family here because this list is compared as a SET; the
            // wire-order table in `tinymemory_bus::METHODS` is the one
            // that is append-only.
            "BackfillConnectorTrees",
            "ResetDerivedIndex",
            "PurgeAll",
            "RecallNamespaceRecent",
            // Tree, structural: the forest walk and its leaf edge.
            "SummaryForest",
            "RecentLeaves",
            // Tree, by source scope: the flush a user triggers on one source.
            "FlushSourceTree",
            // Maintenance, typed: the diagnosis an operator or an agent reads,
            // beside the uniform report a scheduler reads.
            "Diagnose",
            // Source sync this process runs itself. The periodic loops already
            // live here; these are the on-demand half plus what past runs cost.
            "RunConnectionSync",
            "RunSourceSync",
            "BootstrapConnection",
            "IsToolkitSyncable",
            "SourceSyncState",
            "SyncAuditLog",
            "EstimateSyncCostUsd",
            "SyncStatuses",
            "RawArchiveCoverage",
            "RebuildFromRawArchive",
            // Local coding-agent transcripts.
            "CodingSessionStatus",
            "IngestCodingSessions",
            // Scoring: entity extraction, text embedding, embedder identification.
            "ExtractEntities",
            "EmbedText",
            "EmbedderSlug",
            // The summariser door, and the roots folding leaves behind.
            "Summarise",
            "RootSummaries",
            // The three doors a host opens once it stops linking the engine
            // itself: the cheap degradation poll beside the full diagnosis, the
            // scorer's verdict on one chunk, and per-configured-source ingest
            // progress — none of which any earlier member can answer.
            "DegradedState",
            "ChunkScore",
            "SourceIngestStatus",
            // The final round of the shed: the markdown time tree node by
            // node — the shapes the host's tree-summarizer RPCs report — and
            // the compiled flavoured-root profile read.
            "RuntimeBufferWrite",
            "RuntimeReadNode",
            "RuntimeReadChildren",
            "RuntimeTreeStatus",
            "RuntimeSummarize",
            "RuntimeRebuild",
            "FlavourProfile",
            // Granular ingestion and agentic retrieval, appended so all
            // previously released TinyBus member slots stay stable.
            "IngestLearning",
            "IngestEvent",
            "Answer",
            // Appended at the wire tail (slot 141) to match the bus table's
            // append-only order — member order is wire order.
            "OverrideSchedulerGate",
        ],
        signals = [],
        // The host's embedder is deliberately NOT declared as `requires`. That
        // field is resolved against already-loaded *modules*, and this dependency
        // is served by the host itself, which would leave the module permanently
        // unresolved. It is dialled lazily on the first embed instead, and a host
        // that has not served it gets a named error rather than a module that
        // never starts.
        requires = [],
        optional = [],
        // Eager: bringing up a store opens a database and may run migrations,
        // and charging that to whichever call happens to be first would make an
        // ordinary recall time out on a cold start.
        lazy = false,
    }
}
