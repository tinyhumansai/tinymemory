//! The isolation check: two users on one engine never reach each other.
//!
//! [`run_isolation`] takes two engines that a host built for two different
//! users over the **same** backing store (one CortexDB server, one key, two
//! tenant pins; or one backend, two users' credentials). User A stores one
//! item of each kind; then every read and write user B can make is pointed at
//! A's items, through the same workspace filter, A's ids and A's text:
//!
//! 1. `list`, unscoped and over the whole subtree, lists none of A's items;
//! 2. `get` of A's ids, without and with a reach, returns nothing;
//! 3. `fetch` for A's text, in every declared mode, finds none of A's items;
//! 4. `recall` for A's text cites none of A's items (an unscoped recall is
//!    the one read an engine may serve by traversing down from its root);
//! 5. the namespace facet counts nothing of A's;
//! 6. `forget` of A's ids, and by the shared workspace filter, removes
//!    nothing of A's;
//! 7. B storing A's exact item is a new item for B, not a replay of A's, and
//!    leaves A's copy alone;
//! 8. B storing under a namespace that names A (`user:<A>`) is visible to B
//!    and not to A.
//!
//! Each user also reads its own items back first, so an engine that returns
//! nothing to anyone cannot pass. Both users' items are forgotten afterwards.

use std::collections::BTreeSet;

use crate::{
    ExploreRequest, Facet, FetchRequest, ForgetTarget, GetRequest, ItemId, MemoryEngine, Namespace,
    Reach, RecallRequest, StoreItem,
};

use super::fixtures::Run;
use super::{Ctx, checks, ensure};
use crate::conformance::error::{Error, Result};

const CHECK: &str = "isolation";

/// Runs the isolation check with `a` and `b` as two users of one engine.
///
/// Passing the same engine (or two handles on one user's memory) as both
/// must fail: that is what the check detects.
///
/// # Errors
///
/// The first leak as [`Error::Check`], or [`Error::Engine`] when either
/// engine failed a call it must serve.
pub async fn run_isolation(a: &dyn MemoryEngine, b: &dyn MemoryEngine) -> Result<()> {
    let run = Run::fresh();
    let a = Ctx {
        engine: a,
        run: run.clone(),
    };
    let b = Ctx { engine: b, run };
    let outcome = isolation(&a, &b).await;
    let cleanup_b = checks::cleanup(&b).await;
    let cleanup_a = checks::cleanup(&a).await;
    outcome.and(cleanup_b).and(cleanup_a)
}

async fn isolation(a: &Ctx<'_>, b: &Ctx<'_>) -> Result<()> {
    let items = a.run.round_trip_items();
    let receipts = a.call(CHECK, a.engine.store_many(items.clone())).await?;
    let mine: BTreeSet<ItemId> = receipts.iter().map(|r| r.id.clone()).collect();
    let listed = ids(a, None).await?;
    ensure(CHECK, listed == mine, || {
        format!("user A stored {mine:?} but lists {listed:?}")
    })?;

    for reach in [None, Some(Reach::subtree(Namespace::ROOT))] {
        let seen = ids(b, reach.clone()).await?;
        leak(&seen, &mine, || format!("user B's list over {reach:?}"))?;
    }

    let ids_of_a: Vec<ItemId> = mine.iter().cloned().collect();
    for reach in [None, Some(Reach::subtree(Namespace::ROOT))] {
        let hits = b
            .call(
                CHECK,
                b.engine.get(GetRequest {
                    ids: ids_of_a.clone(),
                    reach: reach.clone(),
                }),
            )
            .await?;
        let seen: BTreeSet<ItemId> = hits.into_iter().map(|hit| hit.id).collect();
        leak(&seen, &mine, || {
            format!("user B's get of A's ids over {reach:?}")
        })?;
    }

    for mode in b.engine.descriptor().fetch_modes.clone() {
        let mut req = FetchRequest::new(a.run.marker.clone(), mode, 20);
        req.filter = b.run.filter();
        let page = b.call(CHECK, b.engine.fetch(req)).await?;
        let seen: BTreeSet<ItemId> = page.hits.into_iter().map(|hit| hit.id).collect();
        leak(&seen, &mine, || {
            format!("user B's {mode:?} fetch of A's text")
        })?;
    }

    let mut req = RecallRequest::new(a.run.marker.clone(), 5);
    req.filter = b.run.filter();
    let answer = b.call(CHECK, b.engine.recall(req)).await?;
    let seen: BTreeSet<ItemId> = answer.citations.into_iter().map(|c| c.id).collect();
    leak(&seen, &mine, || "user B's recall of A's text".to_string())?;

    let mut req = ExploreRequest::new(Facet::Namespace, 10);
    req.filter = b.run.filter();
    let page = b.call(CHECK, b.engine.explore(req)).await?;
    let counted: u64 = page.buckets.iter().map(|bucket| bucket.count).sum();
    ensure(CHECK, counted == 0, || {
        format!("user B's namespace facet counted {counted} of A's items")
    })?;

    for target in [
        ForgetTarget::Ids(ids_of_a.clone()),
        ForgetTarget::Filter(b.run.filter()),
    ] {
        let described = format!("{target:?}");
        let report = b.call(CHECK, b.engine.forget(target)).await?;
        ensure(CHECK, report.forgotten == 0, || {
            format!(
                "user B's forget {described} removed {} items",
                report.forgotten
            )
        })?;
        let left = ids(a, None).await?;
        ensure(CHECK, left == mine, || {
            format!("after user B's forget {described}, user A lists {left:?}")
        })?;
    }

    let copied = items[0].clone();
    let receipt = b.call(CHECK, b.engine.store(copied)).await?;
    ensure(CHECK, !receipt.replayed, || {
        "user B storing A's exact item was a replay of A's".to_string()
    })?;
    let ours = ids(b, None).await?;
    ensure(CHECK, ours == BTreeSet::from([receipt.id.clone()]), || {
        format!("user B stored one item but lists {ours:?}")
    })?;
    let left = ids(a, None).await?;
    ensure(CHECK, left == mine, || {
        format!("after user B stored A's item, user A lists {left:?}")
    })?;

    let named = format!("user:{}-a", a.run.marker)
        .parse::<Namespace>()
        .map_err(|source| Error::Engine {
            check: CHECK,
            source,
        })?;
    let mut meta = b.run.meta();
    meta.namespace = named.clone();
    let impostor = b
        .call(
            CHECK,
            b.engine.store(StoreItem::document(
                format!("{} written by B at A's name", b.run.marker),
                meta,
            )),
        )
        .await?;
    let theirs = ids(b, Some(Reach::exact(named.clone()))).await?;
    ensure(CHECK, theirs.contains(&impostor.id), || {
        format!("user B cannot read back what it stored at {named}")
    })?;
    for reach in [
        None,
        Some(Reach::subtree(Namespace::ROOT)),
        Some(Reach::exact(named)),
    ] {
        let seen = ids(a, reach.clone()).await?;
        ensure(CHECK, !seen.contains(&impostor.id), || {
            format!("user A's list over {reach:?} shows what user B wrote at A's name")
        })?;
    }
    Ok(())
}

/// Fails unless `seen` holds none of `theirs`.
fn leak(
    seen: &BTreeSet<ItemId>,
    theirs: &BTreeSet<ItemId>,
    what: impl FnOnce() -> String,
) -> Result<()> {
    let leaked: Vec<&ItemId> = seen.intersection(theirs).collect();
    ensure(CHECK, leaked.is_empty(), || {
        format!("{} reached the other user's items {leaked:?}", what())
    })
}

/// The ids `ctx`'s user lists in the run's workspace, over `reach`.
async fn ids(ctx: &Ctx<'_>, reach: Option<Reach>) -> Result<BTreeSet<ItemId>> {
    let mut filter = ctx.run.filter();
    filter.reach = reach;
    Ok(ctx
        .list_all(CHECK, &filter)
        .await?
        .into_iter()
        .map(|hit| hit.id)
        .collect())
}
