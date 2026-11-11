//! Cleanup: removing the legacy copies of what was moved.
//!
//! Nothing is removed on the strength of the state file: the legacy tree is
//! exported again, each item placed again, and its twin in the per-user tree
//! read back by id. Only items whose twin is there are removed.
//!
//! - **A legacy scope whose every item has its twin**, with no partial item
//!   anywhere, is erased whole (`MemoryEngine::erase`), which releases the
//!   write keys.
//! - **Otherwise**, on an engine that cannot erase (the hosted one), and on
//!   a legacy tree other accounts share (whose writes can land between the
//!   survey and an erase), the items with a twin are forgotten by id; the
//!   rest stay where they are.
//!
//! The pass ends by exporting the legacy tree once more: [`Phase::Cleaned`]
//! is written only if nothing left there has a twin.

use std::collections::BTreeMap;

use tinymemory_api::{
    EraseRequest, ForgetTarget, GetRequest, ItemId, ItemKind, ListRequest, MAX_GET_IDS, MetaFilter,
    Namespace, Reach,
};

use super::copy::Engines;
use super::error::Result;
use super::map::Placement;
use super::state::{MigrationState, MigrationStateStore, Phase};

/// The legacy items of one kind at one node: their ids, and whether every
/// one of them has its twin.
#[derive(Default)]
struct Group {
    moved: Vec<ItemId>,
    complete: bool,
}

/// What a pass over the legacy tree found.
#[derive(Default)]
struct Survey {
    groups: BTreeMap<(String, ItemKind), (Namespace, Group)>,
    partial: bool,
}

/// Exports the whole legacy tree and finds, for every item, whether its twin
/// is in the per-user tree.
async fn survey(engines: &Engines, placement: &Placement) -> Result<Survey> {
    let mut found = Survey::default();
    let mut cursor = None;
    loop {
        let mut request = ListRequest::new(MetaFilter::default(), MAX_GET_IDS);
        request.cursor = cursor;
        let page = engines.legacy.export(request).await?;
        found.partial |= !page.incomplete.is_empty();
        let mut twins = Vec::with_capacity(page.items.len());
        for exported in &page.items {
            let twin = placement
                .place(exported.item.clone())
                .map(|item| ItemId(item.fingerprint()))
                .ok();
            twins.push(twin);
        }
        let ids: Vec<ItemId> = twins.iter().flatten().cloned().collect();
        let present = if ids.is_empty() {
            Vec::new()
        } else {
            engines
                .tree
                .get(GetRequest { ids, reach: None })
                .await?
                .into_iter()
                .map(|hit| hit.id)
                .collect()
        };
        for (exported, twin) in page.items.into_iter().zip(twins) {
            let namespace = exported.item.meta().namespace.clone();
            let key = (namespace.to_string(), exported.item.kind());
            let (_, group) = found.groups.entry(key).or_insert_with(|| {
                (
                    namespace,
                    Group {
                        complete: true,
                        ..Group::default()
                    },
                )
            });
            if twin.is_some_and(|twin| present.contains(&twin)) {
                group.moved.push(exported.id);
            } else {
                group.complete = false;
            }
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return Ok(found),
        }
    }
}

/// Removes the legacy copies of everything moved, then checks nothing moved
/// is left. Pauses (resumably) when `paused` says so or the account cannot
/// go on.
///
/// # Errors
///
/// When the state cannot be saved, or the legacy tree cannot be read or
/// changed for a reason that is not account-wide.
pub async fn cleanup<P>(
    store: &dyn MigrationStateStore,
    engines: &Engines,
    placement: &Placement,
    shared: bool,
    unconfirmed: bool,
    state: &mut MigrationState,
    paused: impl Fn() -> P,
) -> Result<()>
where
    P: std::future::Future<Output = bool>,
{
    state.phase = Phase::Cleaning;
    state.cleaning = true;
    state.error = None;
    store.save(state)?;
    // An import that finished without its last batch confirmed listed may
    // hold items the survey cannot see yet: never erase a scope whole then,
    // forget by verified id as for a shared tree.
    if unconfirmed {
        log::info!("[memory:layout_migration] import not confirmed listed; forgetting by id");
    }
    match remove_moved(engines, placement, shared || unconfirmed, &paused).await {
        Ok(true) => {}
        Ok(false) => return pause(store, state, "background work is paused".into()),
        Err(error) if error.is_account_wide() => return pause(store, state, error.to_string()),
        Err(error) => return Err(error),
    }
    let left = survey(engines, placement).await?;
    let still_moved: usize = left.groups.values().map(|(_, g)| g.moved.len()).sum();
    if still_moved > 0 {
        return pause(
            store,
            state,
            format!("{still_moved} moved items are still in the legacy tree"),
        );
    }
    state.phase = Phase::Cleaned;
    store.save(state)?;
    log::info!(
        "[memory:layout_migration] legacy tree cleaned left={}",
        left.groups.len()
    );
    Ok(())
}

/// One removal pass; `false` when it stopped for a pause.
async fn remove_moved<P>(
    engines: &Engines,
    placement: &Placement,
    shared: bool,
    paused: &impl Fn() -> P,
) -> Result<bool>
where
    P: std::future::Future<Output = bool>,
{
    let found = survey(engines, placement).await?;
    let mut can_erase = !found.partial && !shared;
    for ((_, kind), (namespace, group)) in found.groups {
        if paused().await {
            return Ok(false);
        }
        if group.moved.is_empty() {
            continue;
        }
        if can_erase && group.complete {
            let mut erase = EraseRequest::new(Reach::exact(namespace.clone()));
            erase.kinds = vec![kind];
            match engines.legacy.erase(erase).await {
                Ok(_) => continue,
                Err(tinymemory_api::Error::Unsupported(_)) => can_erase = false,
                Err(error) => return Err(error.into()),
            }
        }
        for batch in group.moved.chunks(100) {
            engines
                .legacy
                .forget(ForgetTarget::Ids(batch.to_vec()))
                .await?;
        }
    }
    Ok(true)
}

fn pause(store: &dyn MigrationStateStore, state: &mut MigrationState, why: String) -> Result<()> {
    log::info!("[memory:layout_migration] cleanup paused why={why}");
    state.phase = Phase::Paused;
    state.error = Some(why);
    store.save(state)
}

#[cfg(test)]
#[path = "cleanup_tests.rs"]
mod tests;
