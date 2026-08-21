//! Process-global memory client singleton.
//!
//! One `MemoryClient` (and its background ingestion-queue worker) lives for the
//! entire core process. Every subsystem — RPC handlers, node runtime, screen
//! intelligence, CLI — shares this single instance so the worker is never
//! prematurely dropped.
//!
//! # Usage
//!
//! ```ignore
//! // At startup (core server, CLI, etc.)
//! memory::global::init(workspace_dir)?;
//!
//! // Anywhere that needs to write/read memory:
//! let client = memory::global::client()?;
//! client.put_doc(input).await?;
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};

use crate::store::{MemoryClient, MemoryClientRef};

#[derive(Clone)]
struct GlobalMemoryClient {
    workspace_dir: PathBuf,
    client: MemoryClientRef,
}

type GlobalClientSlot = RwLock<Option<GlobalMemoryClient>>;

/// The process-global memory client slot.
static GLOBAL_CLIENT: OnceLock<GlobalClientSlot> = OnceLock::new();

fn global_slot() -> &'static GlobalClientSlot {
    GLOBAL_CLIENT.get_or_init(GlobalClientSlot::default)
}

/// Initialise or re-bind the global memory client from a workspace directory.
///
/// Safe to call multiple times. Calls for the same workspace return the
/// existing client; calls for a different workspace replace the global handle
/// so a post-login active-user switch does not keep writing to the pre-login
/// workspace.
pub fn init(workspace_dir: PathBuf) -> Result<MemoryClientRef, String> {
    init_in_slot(global_slot(), workspace_dir)
}

fn init_in_slot(
    slot: &GlobalClientSlot,
    workspace_dir: PathBuf,
) -> Result<MemoryClientRef, String> {
    if let Some(existing) = slot
        .read()
        .map_err(|e| format!("[memory:global] read lock poisoned: {e}"))?
        .as_ref()
    {
        if existing.workspace_dir == workspace_dir {
            log::debug!("[memory:global] already initialised for current workspace");
            return Ok(Arc::clone(&existing.client));
        }
    }

    // Reuse the per-workspace cache before constructing anything. A desktop
    // active-user switch A -> B -> A lands here with the global slot pointing
    // at B, and building a *second* client for A would put two ingestion
    // workers over A's SQLite file — duplicate graph extraction and duplicate
    // embedding work — while any `MemoryBinding` cached for A still held the
    // first one. `client_for_workspace` writes into the same map, so the two
    // resolution paths converge on one client per workspace.
    if let Some(cached) = cached_client(&workspace_dir)? {
        log::debug!(
            "[memory:global] reusing cached workspace client for {}",
            workspace_dir.display()
        );
        let mut guard = slot
            .write()
            .map_err(|e| format!("[memory:global] write lock poisoned: {e}"))?;
        *guard = Some(GlobalMemoryClient {
            workspace_dir,
            client: Arc::clone(&cached),
        });
        return Ok(cached);
    }

    log::info!(
        "[memory:global] initialising global MemoryClient workspace={}",
        workspace_dir.display()
    );
    let client = match MemoryClient::from_workspace_dir(workspace_dir.clone()) {
        Ok(client) => Arc::new(client),
        Err(error) => {
            let mut guard = slot
                .write()
                .map_err(|e| format!("[memory:global] write lock poisoned: {e}"))?;
            if guard
                .as_ref()
                .is_some_and(|existing| existing.workspace_dir != workspace_dir)
            {
                log::warn!(
                    "[memory:global] clearing stale MemoryClient after failed rebind to {}",
                    workspace_dir.display()
                );
                *guard = None;
            }
            return Err(error);
        }
    };

    let mut guard = slot
        .write()
        .map_err(|e| format!("[memory:global] write lock poisoned: {e}"))?;
    if let Some(existing) = guard.as_ref() {
        if existing.workspace_dir == workspace_dir {
            let client = Arc::clone(&existing.client);
            cache_client(&workspace_dir, &client)?;
            return Ok(client);
        }

        log::info!(
            "[memory:global] rebinding MemoryClient workspace {} -> {}",
            existing.workspace_dir.display(),
            workspace_dir.display()
        );
    }

    // Publish into the shared cache under the same client the global slot is
    // about to hold, so a later `client_for_workspace(workspace)` — or a return
    // to this workspace after a switch — reuses it rather than building a
    // second engine over the same store.
    let client = cache_client(&workspace_dir, &client)?;

    *guard = Some(GlobalMemoryClient {
        workspace_dir,
        client: Arc::clone(&client),
    });
    Ok(client)
}

// The former default-workspace initializer was test-only and unused. It has
// been removed rather than shipped as a hidden production entry point.
//
// Keep its source range non-executable so the global-client functions below
// retain stable coverage coordinates in every independently linked test binary.
// LLVM otherwise reports those identical regions as separate shipped lines.
//
// Production initialization remains explicit through `init(workspace_dir)`.
// Tests that need isolation construct a `MemoryClient` from their own TempDir.
// This avoids pinning process-global state to a developer home directory.
//
// The retained comments are coverage metadata stability, not excluded logic:
// they introduce no branches, statements, functions, or callable surface.
// The CI seam audit also verifies that no cfg-gated executable item returns
// here in a future change.
//
// Keeping the established locations matters because this crate is linked into
// both direct core tests and facade-level integration tests in one coverage run.
//
//
/// Returns the global memory client.
///
/// Returns `Err` if [`init`] has not yet been called. There is **no** lazy
/// fallback: a fallback would pin the global to `~/.openhuman/workspace` on
/// the first stray call (test, early RPC, etc.). The explicit init/rebind path
/// keeps workspace ownership visible at startup and after login.
///
/// Callers that can tolerate "not yet ready" should use
/// [`client_if_ready`] instead.
pub fn client() -> Result<MemoryClientRef, String> {
    client_from(global_slot())
}

/// Implementation backing [`client`] — extracted so unit tests can pass a
/// freshly-constructed local slot and assert the uninitialised-error
/// contract without racing the process-global singleton.
fn client_from(slot: &GlobalClientSlot) -> Result<MemoryClientRef, String> {
    slot.read()
        .map_err(|e| format!("[memory:global] read lock poisoned: {e}"))?
        .as_ref()
        .map(|entry| Arc::clone(&entry.client))
        .ok_or_else(|| {
            "memory global accessed before init — call init(workspace) at startup".to_string()
        })
}

/// The workspace the process-global client is currently bound to, or `None`
/// when [`init`] has not run yet.
///
/// Exists so `memory::ops::guard::active_memory_guard` can resolve *the same*
/// workspace `memory::ops::helpers::active_memory_client` in the host
/// would, in the pre-boot case where there is no ambient `CoreContext` to ask.
/// Reading the workspace rather than the client keeps the two resolutions
/// answering about the same store instead of drifting onto whatever
/// `Config::load_or_init` happens to say.
pub fn active_workspace_dir() -> Option<PathBuf> {
    global_slot()
        .read()
        .ok()?
        .as_ref()
        .map(|entry| entry.workspace_dir.clone())
}

/// Per-workspace client cache used by [`client_for_workspace`].
///
/// A *map*, not a slot, for the same reason
/// the host's `memory::binding` caches bindings in a map: a subsystem
/// driver is resolved per workspace and must never be handed another
/// workspace's handle.
static WORKSPACE_CLIENTS: OnceLock<RwLock<HashMap<PathBuf, MemoryClientRef>>> = OnceLock::new();

/// The cached client for `workspace_dir`, if one has already been built by
/// either resolution path ([`init`] or [`client_for_workspace`]).
fn cached_client(workspace_dir: &Path) -> Result<Option<MemoryClientRef>, String> {
    Ok(WORKSPACE_CLIENTS
        .get_or_init(Default::default)
        .read()
        .map_err(|e| format!("[memory:global] workspace cache read lock poisoned: {e}"))?
        .get(workspace_dir)
        .map(Arc::clone))
}

/// Publish `client` as *the* client for `workspace_dir`, returning whichever
/// client wins.
///
/// A racing caller may have inserted first; theirs wins, so the "one ingestion
/// worker per workspace" property holds even when two paths construct
/// concurrently. Callers must use the returned handle, not the one they passed.
fn cache_client(workspace_dir: &Path, client: &MemoryClientRef) -> Result<MemoryClientRef, String> {
    let mut guard = WORKSPACE_CLIENTS
        .get_or_init(Default::default)
        .write()
        .map_err(|e| format!("[memory:global] workspace cache write lock poisoned: {e}"))?;
    let entry = guard
        .entry(workspace_dir.to_path_buf())
        .or_insert_with(|| Arc::clone(client));
    Ok(Arc::clone(entry))
}

/// The `MemoryClient` for `workspace_dir`, **reusing the process-global client
/// when it already owns that workspace**.
///
/// Exists for the embedded memory driver
/// (the host's `memory::driver::embedded`), which is constructed
/// synchronously at bind time and must resolve its client lazily on the first
/// contract call.
///
/// The reuse check is load-bearing, not an optimisation: [`MemoryClient`] owns
/// a `UnifiedMemory` handle *and* spawns a background ingestion worker, so two
/// clients over one workspace means two workers doing duplicate graph
/// extraction and duplicate embedding work against the same SQLite file.
///
/// # Errors
///
/// Lock poisoning, or any failure constructing a fresh
/// [`MemoryClient::from_workspace_dir`] (directory creation, store open).
pub fn client_for_workspace(workspace_dir: &Path) -> Result<MemoryClientRef, String> {
    if let Some(existing) = global_slot()
        .read()
        .map_err(|e| format!("[memory:global] read lock poisoned: {e}"))?
        .as_ref()
    {
        if existing.workspace_dir == workspace_dir {
            // Record it under the workspace too. Without this the global's
            // client is invisible to the cache, so a switch away and back
            // rebuilds a second client for this workspace while a binding
            // cached here still holds the first.
            return cache_client(workspace_dir, &existing.client);
        }
    }

    if let Some(existing) = cached_client(workspace_dir)? {
        return Ok(existing);
    }

    log::info!(
        "[memory:global] building workspace-scoped MemoryClient workspace={}",
        workspace_dir.display()
    );
    let client: MemoryClientRef = Arc::new(MemoryClient::from_workspace_dir(
        workspace_dir.to_path_buf(),
    )?);

    cache_client(workspace_dir, &client)
}

/// Returns the global client if already initialised, without lazy init.
pub fn client_if_ready() -> Option<MemoryClientRef> {
    global_slot()
        .read()
        .ok()?
        .as_ref()
        .map(|entry| Arc::clone(&entry.client))
}

#[cfg(test)]
#[path = "global_tests.rs"]
mod tests;
