//! The behavioural suite: [`run`].
//!
//! Every check writes under a workspace unique to the run and filters by it,
//! so the suite can run against an engine that already holds data. The checks,
//! in order:
//!
//! 1. `health` — the engine reports itself serving.
//! 2. `round_trip` — one item of each kind stores and lists back with the same
//!    kind, metadata and rendered text, and paging terminates.
//! 3. `replay` — storing an identical item again is a replay with the same id.
//! 4. `explore` — per-kind and per-workspace counts agree with `list`, buckets
//!    are largest first, and each bucket narrows to exactly its count.
//! 5. `get` — the run's items read back by id in the order asked, equal to
//!    their listing, with an unknown id left out.
//! 6. `store_many` — a batch stores in order, every item is listed on
//!    return, a repeat is all replays, and an empty batch is refused.
//! 7. `store_with` — a store waiting for visibility is listed on return and
//!    replays on a repeat; one only waiting for acceptance answers with the
//!    item's own id.
//! 8. `fetch_filters` — for every declared fetch mode, a filter on each
//!    metadata field selects exactly the item carrying it (and `list` agrees).
//! 9. `unsupported_modes` — every undeclared fetch mode fails `Unsupported`.
//! 10. `namespaces` — items at the root, two sibling agents and a sub-agent:
//!     each reach (own and inherited, exact, subtree) lists exactly its nodes,
//!     never a sibling's; `get` and `fetch` honour the reach; the same text in
//!     two namespaces is two items; the namespace facet counts each node; and
//!     a forget scoped to one node removes only it.
//! 11. `empty_forget` — a forget with no ids or an empty filter is refused and
//!     removes nothing.
//! 12. `forget_by_id` and `forget_by_filter` — forgotten items stop listing and
//!     are counted; others stay.
//! 13. `recall` — an answer cites items that resolve through `list`.
//! 14. `consolidate` — a malformed request is refused, and a valid one is
//!     answered as the descriptor's `consolidation` promises.
//!
//! Finally the run's items are forgotten by filter and must be gone.
//!
//! [`run_isolation`] is a separate check over two engines: two users of one
//! backing store, neither of which may reach the other (see `isolation`).

mod bulk;
mod checks;
mod explore;
mod fixtures;
mod isolation;
mod lifecycle;
mod namespaces;

use std::collections::HashSet;
use std::future::Future;

use crate::{Hit, ListRequest, MemoryEngine, MetaFilter};

use crate::conformance::error::{Error, Result};
use fixtures::Run;
pub use isolation::run_isolation;

/// Most pages one listing may take before the suite calls the cursor endless.
const MAX_PAGES: usize = 10_000;

/// Runs the whole suite against `engine`.
///
/// # Errors
///
/// The first failed check: [`Error::Check`] when the engine answered wrongly,
/// [`Error::Engine`] when it failed a call it must serve.
pub async fn run(engine: &dyn MemoryEngine) -> Result<()> {
    let ctx = Ctx {
        engine,
        run: Run::fresh(),
    };
    let outcome = checks::all(&ctx).await;
    let cleanup = checks::cleanup(&ctx).await;
    outcome.and(cleanup)
}

/// What every check needs: the engine and the run's identity.
pub(crate) struct Ctx<'a> {
    pub(crate) engine: &'a dyn MemoryEngine,
    pub(crate) run: Run,
}

impl Ctx<'_> {
    /// Awaits an engine call, naming `check` if it fails.
    pub(crate) async fn call<T>(
        &self,
        check: &'static str,
        call: impl Future<Output = crate::Result<T>>,
    ) -> Result<T> {
        call.await.map_err(|source| Error::Engine { check, source })
    }

    /// Every item matching `filter`, following cursors two at a time so paging
    /// is exercised on every listing.
    pub(crate) async fn list_all(
        &self,
        check: &'static str,
        filter: &MetaFilter,
    ) -> Result<Vec<Hit>> {
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen_cursors = HashSet::new();
        for _ in 0..MAX_PAGES {
            let mut request = ListRequest::new(filter.clone(), 2);
            request.cursor = cursor.clone();
            let page = self.call(check, self.engine.list(request)).await?;
            ensure(check, page.items.iter().all(|hit| hit.score == 0.0), || {
                "a listing returned a hit with a non-zero score".to_string()
            })?;
            all.extend(page.items);
            match page.next_cursor {
                Some(next) => {
                    ensure(check, seen_cursors.insert(next.clone()), || {
                        format!("the listing cursor `{next}` repeated; paging never ends")
                    })?;
                    cursor = Some(next);
                }
                None => return Ok(all),
            }
        }
        Err(Error::Check {
            check,
            detail: format!("listing did not end within {MAX_PAGES} pages"),
        })
    }
}

/// Fails `check` with `detail` unless `holds`.
pub(crate) fn ensure(
    check: &'static str,
    holds: bool,
    detail: impl FnOnce() -> String,
) -> Result<()> {
    if holds {
        Ok(())
    } else {
        Err(Error::Check {
            check,
            detail: detail(),
        })
    }
}
