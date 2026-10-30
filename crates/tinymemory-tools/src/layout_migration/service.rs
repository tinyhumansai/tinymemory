//! The migration as a service: what a host's RPCs and background job call.
//!
//! - [`scan`]: whether there is anything to move (a UI shows nothing when
//!   there is not).
//! - [`status`]: its progress, with a run the app quit in the middle of
//!   reported as interrupted.
//! - [`retry`]: puts the items that could not be moved back in line.
//! - [`is_finished`]: whether a background tick has nothing left to start.
//! - [`record_failure`]: saves why a run stopped on an error.
//!
//! Running in the background (one run per workspace, a spawned task) uses a
//! host-provided [`MigrationSpawner`]; this crate links no executor.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use serde::Serialize;

use super::copy::legacy_present;
use super::error::{Error, Result};
use super::job::LayoutHost;
use super::state::{MigrationState, Phase};
use crate::run_slots::{RunGuard, RunSlots, StartGate};

/// A migration task handed to the host's executor.
pub type MigrationTask = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// The host's executor boundary. An error means the task was not accepted;
/// implementations must drop it before returning so its run guard releases.
pub trait MigrationSpawner: Send + Sync {
    /// Starts `task`, returning an error if it could not be scheduled.
    ///
    /// # Errors
    ///
    /// Returns a description if the executor rejects the task. The task must
    /// be dropped before this method returns an error.
    fn spawn(&self, task: MigrationTask) -> std::result::Result<(), String>;
}

/// Run-slot registry and executor adapter for layout migration.
///
/// Clone and inject one [`RunSlots`] instance into every service that may
/// start a migration for the same workspace set.
pub struct MigrationService<G, S> {
    slots: RunSlots<PathBuf>,
    gate: G,
    spawner: S,
}

impl<G, S> std::fmt::Debug for MigrationService<G, S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MigrationService").finish_non_exhaustive()
    }
}

impl<G, S> MigrationService<G, S>
where
    G: StartGate,
    S: MigrationSpawner,
{
    /// A service sharing `slots`, using `gate` for the host's import and
    /// migration start interlock, and `spawner` for accepted background work.
    #[must_use]
    pub fn new(slots: RunSlots<PathBuf>, gate: G, spawner: S) -> Self {
        Self {
            slots,
            gate,
            spawner,
        }
    }

    /// Whether `key` currently has a migration task holding a run slot.
    #[must_use]
    pub fn is_running(&self, key: &Path) -> bool {
        self.slots.is_running(&key.to_path_buf())
    }

    /// Claims the workspace and schedules a migration. `peer_running` is
    /// checked under the common start gate, atomically with this service's
    /// run-slot reservation.
    pub fn start<Peer, Paused>(
        &self,
        key: PathBuf,
        host: Arc<dyn LayoutHost>,
        peer_running: Peer,
        trigger: super::job::Trigger,
        paused: Paused,
    ) -> bool
    where
        Peer: FnOnce() -> bool,
        Paused: Fn() -> bool + Send + Sync + 'static,
    {
        let mut peer_running = Some(peer_running);
        let mut run_guard: Option<RunGuard<PathBuf>> = None;
        let reserved = self.gate.with_gate(&mut || {
            if run_guard.is_some() {
                return true;
            }
            let Some(peer_running) = peer_running.take() else {
                return false;
            };
            if peer_running() {
                return false;
            }
            run_guard = self.slots.try_acquire(key.clone());
            run_guard.is_some()
        });
        if !reserved {
            return false;
        }
        let Some(run_guard) = run_guard else {
            return false;
        };

        let task_host = Arc::clone(&host);
        let task = Box::pin(async move {
            let _run_guard = run_guard;
            match super::job::run(task_host.as_ref(), trigger, || std::future::ready(paused()))
                .await
            {
                Ok(_) => {}
                Err(error) => record_failure(task_host.as_ref(), &error),
            }
        });
        match self.spawner.spawn(task) {
            Ok(()) => true,
            Err(error) => {
                record_failure(
                    host.as_ref(),
                    &Error::State(format!("migration task spawn failed: {error}")),
                );
                false
            }
        }
    }

    /// The background tick policy: finished, paused, already running, then
    /// start. A run or spawn failure is recorded once by [`Self::start`].
    pub fn tick<Peer, Paused>(
        &self,
        key: PathBuf,
        host: Arc<dyn LayoutHost>,
        peer_running: Peer,
        paused: Paused,
    ) -> bool
    where
        Peer: FnOnce() -> bool,
        Paused: Fn() -> bool + Send + Sync + 'static,
    {
        match is_finished(host.as_ref()) {
            Ok(true) => return false,
            Ok(false) => {}
            Err(error) => {
                log::warn!(
                    "[memory:layout_migration] state unreadable code={}",
                    error.code()
                );
                return false;
            }
        }
        if paused() || self.is_running(&key) {
            return false;
        }
        self.start(key, host, peer_running, super::job::Trigger::Auto, paused)
    }
}

/// What [`scan`] found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScanView {
    /// There is legacy memory still to move.
    pub needed: bool,
    /// The legacy tree may be shared with other accounts, so moving it needs
    /// the user's consent.
    pub shared: bool,
}

/// [`status`]'s answer.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MigrationStatus {
    /// The saved progress.
    pub state: MigrationState,
    /// A run is going now.
    pub running: bool,
    /// The app stopped in the middle of a run; it resumes on its own, or the
    /// user starts it again.
    pub interrupted: bool,
}

/// Whether the host's account has legacy memory to move.
///
/// # Errors
///
/// When the state cannot be read, the engines cannot be bound, or the legacy
/// tree cannot be read.
pub async fn scan(host: &dyn LayoutHost) -> Result<ScanView> {
    let state = host.store().load()?;
    let claim = match host.legacy_claim().await {
        // Memory off or signed out: nothing to show.
        Err(Error::Off(_)) => None,
        claim => claim?,
    };
    let shared = claim.is_some();
    let needed = match state.phase {
        Phase::Cleaned => false,
        // Another account on this machine took the shared tree.
        Phase::Idle
            if claim
                .as_ref()
                .map(|claim| claim.held_by_other())
                .transpose()?
                == Some(true) =>
        {
            false
        }
        Phase::Idle if !state.switched => match host.engines().await {
            // Memory off or signed out: nothing to show.
            Err(Error::Off(_)) => false,
            Err(error) => return Err(error),
            Ok(engines) => legacy_present(&*engines.legacy).await?,
        },
        _ => true,
    };
    Ok(ScanView { needed, shared })
}

/// The migration's progress; `running` is whether the host has a run going.
///
/// # Errors
///
/// When the state cannot be read.
pub fn status(host: &dyn LayoutHost, running: bool) -> Result<MigrationStatus> {
    let state = host.store().load()?;
    let interrupted = !running && matches!(state.phase, Phase::Copying | Phase::Cleaning);
    Ok(MigrationStatus {
        state,
        running,
        interrupted,
    })
}

/// Puts the items that could not be moved back in line: the next run copies
/// again from the start (what was copied replays) and cleans up again.
///
/// # Errors
///
/// When the state cannot be read or saved, or a run is going now.
pub fn retry(host: &dyn LayoutHost, running: bool) -> Result<MigrationState> {
    if running {
        return Err(Error::invalid("the migration is running"));
    }
    let store = host.store();
    let mut state = store.load()?;
    state.failures.clear();
    state.incomplete.clear();
    state.cursor = None;
    state.error = None;
    if state.switched {
        state.caught_up = false;
        state.cleaning = false;
    }
    if state.phase != Phase::Idle {
        state.phase = Phase::Paused;
    }
    store.save(&state)?;
    Ok(state)
}

/// Whether the migration is done for good: cleaned, with no import left to
/// re-check. A background tick starts nothing then.
///
/// # Errors
///
/// When the state cannot be read.
pub fn is_finished(host: &dyn LayoutHost) -> Result<bool> {
    let state = host.store().load()?;
    Ok(state.phase == Phase::Cleaned && (state.rechecked || !host.listed_unconfirmed()))
}

/// Saves why a run stopped on `error`, so the status shows it.
pub fn record_failure(host: &dyn LayoutHost, error: &Error) {
    let store = host.store();
    if let Ok(mut state) = store.load() {
        state.phase = Phase::Paused;
        state.error = Some(error.to_string());
        if let Err(save) = store.save(&state) {
            log::warn!(
                "[memory:layout_migration] state not saved code={}",
                save.code()
            );
        }
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
