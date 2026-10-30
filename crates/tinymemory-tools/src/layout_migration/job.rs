//! A migration end to end: detect, copy, switch, catch up, clean up.
//!
//! - **Detect.** No legacy memory: switch at once, and that is all; there is
//!   no job, no banner and no prompt.
//! - **Gates.** A legacy tree other accounts may share (a self-hosted key)
//!   moves only with the user's consent ([`Trigger::Manual`] with
//!   `takeover`). An automatic run starts, and goes on page by page, only
//!   while moving costs the user nothing; when that ends mid-run it pauses
//!   where it is, and resumes in a later free window or when the user starts
//!   it.
//! - **Copy, then switch.** Reads and writes move to the per-user tree only
//!   after every item that could be copied is verified there, so no turn
//!   ever reads an empty memory.
//! - **Catch up.** Writes to the legacy tree between the copy and the switch
//!   are picked up by copying once more; everything copied before replays.
//! - **Clean up.** Last, because it drops held recall packs.
//!
//! Every step saves its progress, so [`run`] called again (after a pause, a
//! crash or a restart) continues where the last one stopped.

use async_trait::async_trait;

use super::cleanup::cleanup;
use super::copy::{Engines, copy, legacy_present};
use super::error::Result;
use super::map::Placement;
use super::state::{MigrationState, MigrationStateStore, Phase};

/// What started a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// The background job: runs only while moving is free.
    Auto,
    /// The user's "Migrate now"; `takeover` is their consent to take a
    /// legacy tree other accounts may share.
    Manual {
        /// Consent to take a shared legacy tree.
        takeover: bool,
    },
}

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// There was no legacy memory; the per-user tree is in use.
    NothingToMove,
    /// Not started: the legacy tree may be shared, and the user has not
    /// agreed to take it.
    NeedsTakeover,
    /// Not started: another account on this machine took the shared legacy
    /// tree.
    ClaimedElsewhere,
    /// Not started or not continued automatically: moving is not free now.
    NotFree,
    /// Stopped where it was; [`run`] again continues.
    Paused,
    /// Moved and cleaned up.
    Done,
}

/// The claim on a legacy tree other accounts on this machine may share (a
/// self-hosted engine): only one account may take it, and taking it needs
/// the user's consent.
pub trait LegacyClaim: Send + Sync {
    /// Whether another account already took the tree.
    ///
    /// # Errors
    ///
    /// When the claim cannot be read.
    fn held_by_other(&self) -> Result<bool>;

    /// Takes the tree; `false` when another account got there first.
    ///
    /// # Errors
    ///
    /// When the claim cannot be written.
    fn take(&self) -> Result<bool>;
}

/// What the migration needs from the host, behind one trait so the job runs
/// without a TinyHumans session or a CortexDB server. The host binds one
/// account: every method answers for it.
///
/// The real host implements it over its engine binding and layout setting;
/// tests implement it over two in-memory engines.
#[async_trait]
pub trait LayoutHost: Send + Sync {
    /// Where the migration's progress is saved.
    fn store(&self) -> &dyn MigrationStateStore;

    /// The legacy-layout engine and the per-user engine for the account, on
    /// one endpoint and credential.
    ///
    /// # Errors
    ///
    /// When memory is off or the account has no per-user root.
    async fn engines(&self) -> Result<Engines>;

    /// Where items go in the per-user tree.
    ///
    /// # Errors
    ///
    /// When the layout cannot be built.
    fn placement(&self) -> Result<Placement>;

    /// Whether reads and writes already use the per-user tree.
    fn is_switched(&self) -> bool;

    /// Moves reads and writes to the per-user tree: persists the layout
    /// setting and drops the cached engines. The real host loads the migrated
    /// person's own config file fresh, so a setting changed during a long
    /// migration is never reverted and an account switch mid-move never
    /// writes another person's config.
    ///
    /// # Errors
    ///
    /// When the setting cannot be saved.
    async fn switch(&self) -> Result<()>;

    /// Whether moving memory costs the user nothing right now (always, off
    /// the hosted engine).
    async fn free_now(&self) -> bool;

    /// The claim on the legacy tree when other accounts on this machine may
    /// share it, so taking it needs the user's consent and only one account
    /// may; `None` when it is the account's own.
    ///
    /// # Errors
    ///
    /// When memory is off or the account has no per-user root.
    async fn legacy_claim(&self) -> Result<Option<Box<dyn LegacyClaim>>>;

    /// Whether the last import of old local memory ended with its last batch
    /// not confirmed listed. The migration then forgets by id, never erasing
    /// scopes whole, and copies again before it finishes.
    fn listed_unconfirmed(&self) -> bool;

    /// Waits for the engine's listing to catch up with a finished import,
    /// before a re-check copies again. The module runs no executor: the host
    /// sleeps (tests return at once).
    async fn wait_for_listing(&self);
}

/// The most extra catch-up passes after an import that ended with its last
/// batch not confirmed listed.
const MAX_RECHECKS: u32 = 3;

/// Runs (or resumes) the migration of the host's account. `paused` is the
/// scheduler's own pause (background work held), asked before every page.
///
/// # Errors
///
/// When the state cannot be read or saved, the engines cannot be bound, or
/// the legacy tree cannot be read for a reason that is not account-wide.
pub async fn run<P>(
    host: &dyn LayoutHost,
    trigger: Trigger,
    paused: impl Fn() -> P,
) -> Result<Outcome>
where
    P: std::future::Future<Output = bool>,
{
    let store = host.store();
    let mut state = store.load()?;
    // Cleaned with the import still unconfirmed (a stop between cleanup and
    // the re-check below) goes round once more rather than counting as done.
    if state.phase == Phase::Cleaned && (state.rechecked || !host.listed_unconfirmed()) {
        return Ok(Outcome::Done);
    }
    let engines = host.engines().await?;
    if state.phase == Phase::Idle && !state.switched && !legacy_present(&*engines.legacy).await? {
        return nothing_to_move(host, &mut state).await;
    }
    if let Trigger::Manual { takeover: true } = trigger {
        state.takeover = true;
        store.save(&state)?;
    }
    let shared = host.legacy_claim().await?;
    if let Some(claim) = &shared {
        if claim.held_by_other()? {
            return claimed_elsewhere(host, &mut state).await;
        }
        if !state.takeover {
            return Ok(Outcome::NeedsTakeover);
        }
        if !claim.take()? {
            return claimed_elsewhere(host, &mut state).await;
        }
    }
    let auto = trigger == Trigger::Auto;
    if auto && !host.free_now().await {
        return Ok(Outcome::NotFree);
    }
    let paused = &paused;
    let stop = move || async move { paused().await || (auto && !host.free_now().await) };
    let placement = host.placement()?;

    if !state.switched && !state.cleaning {
        copy(store, &engines, &placement, &mut state, stop).await?;
        if state.phase != Phase::Copied {
            return Ok(Outcome::Paused);
        }
        if !host.is_switched() {
            host.switch().await?;
        }
        state.switched = true;
        state.cursor = None;
        store.save(&state)?;
        log::info!("[memory:layout_migration] switched to the per-user tree");
    }
    if !state.caught_up && !state.cleaning {
        copy(store, &engines, &placement, &mut state, stop).await?;
        if state.phase != Phase::Copied {
            return Ok(Outcome::Paused);
        }
        state.caught_up = true;
        store.save(&state)?;
    }
    cleanup(
        store,
        &engines,
        &placement,
        shared.is_some(),
        host.listed_unconfirmed(),
        &mut state,
        stop,
    )
    .await?;
    // An import that ended with its last batch not confirmed listed may hold
    // items no copy saw yet: in this same run, wait for the listing to catch
    // up, then copy, verify and clean up again; at least once, then while a
    // pass still moves something new, at most MAX_RECHECKS times. A late item
    // is never erased meanwhile (`cleanup`). Each recheck is saved before its
    // wait, so a stop or a restart resumes it.
    while state.phase == Phase::Cleaned && !state.rechecked && host.listed_unconfirmed() {
        let new = state.copied > state.replayed;
        if !((state.rechecks == 0 || new) && state.rechecks < MAX_RECHECKS) {
            // Done re-checking. The import's flag stays: nothing here proves
            // its last batch is listed, so cleanup keeps forgetting by id.
            state.rechecked = true;
            store.save(&state)?;
            break;
        }
        state.rechecks += 1;
        state.caught_up = false;
        state.cleaning = false;
        state.cursor = None;
        // Parked as paused, not copied: a scan must not offer a fresh move.
        state.phase = Phase::Paused;
        state.error = Some("copying again for items the import may list late".to_string());
        store.save(&state)?;
        log::info!(
            "[memory:layout_migration] import not confirmed listed; copying again rechecks={}",
            state.rechecks
        );
        host.wait_for_listing().await;
        copy(store, &engines, &placement, &mut state, stop).await?;
        if state.phase != Phase::Copied {
            return Ok(Outcome::Paused);
        }
        state.caught_up = true;
        store.save(&state)?;
        cleanup(
            store,
            &engines,
            &placement,
            shared.is_some(),
            host.listed_unconfirmed(),
            &mut state,
            stop,
        )
        .await?;
    }
    Ok(if state.phase == Phase::Cleaned {
        Outcome::Done
    } else {
        Outcome::Paused
    })
}

/// Another account took the shared legacy tree: this one has nothing to
/// move, and goes on in its own per-user tree like an account that never had
/// legacy memory, rather than in the tree the other account emptied.
async fn claimed_elsewhere(host: &dyn LayoutHost, state: &mut MigrationState) -> Result<Outcome> {
    nothing_to_move(host, state).await?;
    Ok(Outcome::ClaimedElsewhere)
}

async fn nothing_to_move(host: &dyn LayoutHost, state: &mut MigrationState) -> Result<Outcome> {
    if !host.is_switched() {
        host.switch().await?;
    }
    state.switched = true;
    state.caught_up = true;
    state.phase = Phase::Cleaned;
    host.store().save(state)?;
    log::info!("[memory:layout_migration] no legacy memory; per-user tree in use");
    Ok(Outcome::NothingToMove)
}

#[cfg(test)]
#[path = "job_tests.rs"]
mod tests;
