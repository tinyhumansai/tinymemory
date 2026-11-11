//! The copy pass: the legacy tree into the per-user tree, page by page.
//!
//! Each page is exported whole from the legacy engine
//! ([`tinymemory_api::MemoryEngine::export`]), placed ([`Placement`]),
//! stored in the per-user engine, and read back by id before the state
//! moves past it. A page is never marked copied before its items are
//! readable where they now live, and a crash or a pause repeats at most one
//! page: storing an item the tree already holds is a replay.

use std::sync::Arc;

use tinymemory_api::{
    Error as EngineError, GetRequest, ItemId, ListRequest, MemoryEngine, MetaFilter, StoreItem,
};

use super::error::{Error, Result};
use super::map::Placement;
use super::state::{Failure, MigrationState, MigrationStateStore, Phase};

impl std::fmt::Debug for Engines {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engines").finish_non_exhaustive()
    }
}

/// Whether the engine refused this item on its own (the item is skipped and
/// recorded) rather than the whole account (the copy pauses). Skipping an
/// account-wide refusal would advance the cursor past items never stored.
fn skips_item(error: &EngineError) -> bool {
    matches!(
        error,
        EngineError::InvalidRequest(_)
            | EngineError::Unsupported(_)
            | EngineError::NotFound(_)
            | EngineError::Conflict(_)
    )
}

/// Items one page exports and stores.
pub const PAGE: usize = 50;

/// The two engines a migration works between, on one endpoint and
/// credential: the legacy tree it reads, and the per-user tree it writes.
#[derive(Clone)]
pub struct Engines {
    /// The legacy layout (`app:tinymemory/…`).
    pub legacy: Arc<dyn MemoryEngine>,
    /// The per-user layout.
    pub tree: Arc<dyn MemoryEngine>,
}

/// Whether the legacy tree holds anything to move: one export of one item.
///
/// # Errors
///
/// The legacy engine's failures, including a tree too large to list whole.
pub async fn legacy_present(legacy: &dyn MemoryEngine) -> Result<bool> {
    let page = legacy
        .export(ListRequest::new(MetaFilter::default(), 1))
        .await?;
    Ok(!page.items.is_empty() || !page.incomplete.is_empty())
}

/// Copies from `state.cursor` to the end of the legacy tree, saving the
/// state after every page. Returns with [`Phase::Copied`] at the end, or
/// [`Phase::Paused`] when `paused` says so or the account cannot go on
/// (credits, credential, engine unreachable).
///
/// # Errors
///
/// When the state cannot be saved, or the legacy tree cannot be read for a
/// reason that is not account-wide.
pub async fn copy<P>(
    store: &dyn MigrationStateStore,
    engines: &Engines,
    placement: &Placement,
    state: &mut MigrationState,
    paused: impl Fn() -> P,
) -> Result<()>
where
    P: std::future::Future<Output = bool>,
{
    state.phase = Phase::Copying;
    state.error = None;
    if state.cursor.is_none() {
        // A pass from the top (the first, or the catch-up after the switch)
        // meets every item again: count it afresh.
        state.copied = 0;
        state.replayed = 0;
        state.failures.clear();
        state.incomplete.clear();
    }
    store.save(state)?;
    loop {
        if paused().await {
            return pause(store, state, "background work is paused".to_string());
        }
        let mut request = ListRequest::new(MetaFilter::default(), PAGE);
        request.cursor = state.cursor.clone();
        let page = match engines.legacy.export(request).await {
            Ok(page) => page,
            Err(error) => {
                let error = Error::from(error);
                // A failed legacy read must pause this cursor for later
                // retry. Unavailable is transient, not account-wide, but
                // continuing without the unread page would skip data.
                if error.is_account_wide()
                    || matches!(&error, Error::Engine(tinymemory_api::Error::Unavailable(_)))
                {
                    return pause(store, state, error.to_string());
                }
                return Err(error);
            }
        };
        for id in page.incomplete {
            if !state.incomplete.contains(&id.0) {
                state.incomplete.push(id.0);
            }
        }
        let mut placed = Vec::with_capacity(page.items.len());
        for exported in page.items {
            match placement.place(exported.item) {
                Ok(item) => placed.push((exported.id.0, item)),
                Err(error) => note_failure(state, exported.id.0, error.code()),
            }
        }
        if let Err(error) = store_and_check(engines.tree.as_ref(), placed, state).await {
            return pause(store, state, error.to_string());
        }
        state.cursor = page.next_cursor;
        if state.cursor.is_none() {
            state.phase = Phase::Copied;
            store.save(state)?;
            log::info!(
                "[memory:layout_migration] legacy tree copied copied={} failures={} incomplete={}",
                state.copied,
                state.failures.len(),
                state.incomplete.len()
            );
            return Ok(());
        }
        store.save(state)?;
    }
}

/// Records `id` as not copied, once: a page sent again after a pause meets
/// the same items.
fn note_failure(state: &mut MigrationState, id: String, reason: &str) {
    if !state.failures.iter().any(|failure| failure.id == id) {
        state.failures.push(Failure {
            id,
            reason: reason.to_string(),
        });
    }
}

fn pause(store: &dyn MigrationStateStore, state: &mut MigrationState, why: String) -> Result<()> {
    log::info!(
        "[memory:layout_migration] paused copied={} why={why}",
        state.copied
    );
    state.phase = Phase::Paused;
    state.error = Some(why);
    store.save(state)
}

/// Stores `placed` in the per-user tree and reads every item back by id.
/// An item the engine refuses on its own is recorded as a failure; an
/// account-wide failure stops the page (the caller pauses, and the page is
/// sent again on resume).
async fn store_and_check(
    tree: &dyn MemoryEngine,
    placed: Vec<(String, StoreItem)>,
    state: &mut MigrationState,
) -> std::result::Result<(), Error> {
    if placed.is_empty() {
        return Ok(());
    }
    let items: Vec<StoreItem> = placed.iter().map(|(_, item)| item.clone()).collect();
    let mut stored: Vec<(String, ItemId, bool)> = Vec::new();
    match tree.store_many(items).await {
        Ok(receipts) => stored.extend(
            placed
                .iter()
                .zip(receipts)
                .map(|((legacy, _), receipt)| (legacy.clone(), receipt.id, receipt.replayed)),
        ),
        Err(error) if !skips_item(&error) => return Err(error.into()),
        Err(_) => {
            for (legacy, item) in placed {
                match tree.store(item).await {
                    Ok(receipt) => stored.push((legacy, receipt.id, receipt.replayed)),
                    Err(error) if !skips_item(&error) => return Err(error.into()),
                    Err(error) => note_failure(state, legacy, Error::from(error).code()),
                }
            }
        }
    }
    let ids: Vec<ItemId> = stored.iter().map(|(_, id, _)| id.clone()).collect();
    let found = tree
        .get(GetRequest { ids, reach: None })
        .await
        .map_err(Error::from)?;
    for (legacy, id, replayed) in stored {
        if found.iter().any(|hit| hit.id == id) {
            state.copied += 1;
            state.replayed += u64::from(replayed);
        } else {
            note_failure(state, legacy, "not_readable_after_store");
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "copy_tests.rs"]
mod tests;
