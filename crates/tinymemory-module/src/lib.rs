//! Loadable `TinyBus` module adapter for `TinyMemory`.
//!
//! This private workspace crate keeps the vendored `TinyBus` dependency out of
//! the published `tinymemory` crates. Its `cdylib` output is the
//! target-specific binary distributed in GitHub releases.
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
//! `libsqlite3-sys` in particular has several other parents — `tinyagents`'
//! session store among them — so the native `SQLite` build does not leave. That
//! was measured on `OpenHuman`, on both its kernel and its shipping feature
//! profiles: four crate names leave, and all four are ours.
//!
//! What it does buy is **compile time on the critical path**, and that was
//! measured too. `tinycortex` and `tinymemory-core` compile strictly serially
//! ahead of the host crate — `tinyagents` → `tinycortex` → `tinymemory-core` →
//! host, each starting as the previous one ends — putting 14.7s directly in
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
pub mod embedding;
mod host;
mod provider;
mod service;

pub use chat::{CHAT_HOST_BUS_NAME, CHAT_HOST_INTERFACE, CHAT_HOST_OBJECT_PATH};
pub use config::ModuleConfig;
pub use embedding::{
    BusEmbeddingHost, BusEmbeddingProvider, EMBEDDING_HOST_BUS_NAME, EMBEDDING_HOST_INTERFACE,
    EMBEDDING_HOST_OBJECT_PATH,
};
pub use host::{RUNTIME_HOST_BUS_NAME, RUNTIME_HOST_INTERFACE, RUNTIME_HOST_OBJECT_PATH};
pub use service::{BUS_NAME, OBJECT_PATH};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tinybus::{Connection, Error as BusError, Result as BusResult};

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
    host::install(connection.clone());

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

    let provider = provider::ModuleMemoryProvider::new(&config, Arc::new(client));
    service::serve(&connection, Arc::new(provider)).await
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

// Isolate the generated public C symbols so the lint exception cannot hide
// undocumented Rust API. Their contract is TinyBus ABI v1, and none is a
// Rust-callable export from this crate.
#[allow(
    missing_docs,
    unreachable_pub,
    reason = "generated C ABI symbols are documented by the TinyBus module SDK"
)]
mod exports {
    tinybus_module::module_export! {
        setup = super::setup,
        config = super::ModuleConfig,
        // Two, not one: a recall that triggers an embed makes an outbound call
        // while still inside its own inbound call, so a single worker would
        // deadlock on the first semantic query.
        worker_threads = 2,
        provides = ["ai.tinyhumans.tinymemory.Memory"],
        methods = [
            "DriverId",
            "Capabilities",
            "Health",
            "Shutdown",
            "Store",
            "Get",
            "Forget",
            "List",
            "Namespaces",
            "Recall",
            "ExportPage",
            "ImportRecords",
            "IngestDocument",
            "IngestChat",
            "PutDocument",
            "GetDocument",
            "ListDocuments",
            "ListNamespaces",
            "DeleteDocument",
            "ClearNamespace",
            "QueryDocuments",
            "Append",
            "QuerySource",
            "DrillDown",
            "Seal",
            "Cascade",
            "Entities",
            "EntityEdges",
            "TouchEntities",
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
            "Reembed",
            "Compact",
            "Consolidate",
            "Doctor",
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
