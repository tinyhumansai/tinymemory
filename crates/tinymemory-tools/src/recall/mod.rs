//! Holistic recall: one read across several scopes, rendered as one
//! budgeted markdown block — the [`ContextPack`] a host injects into a
//! prompt.
//!
//! A [`HolisticRecall`] lists [`ScopeSection`]s, highest priority first. Each
//! names a scope (a [`tinymemory_api::MetaFilter`]: its reach and kinds) and
//! how to fill it ([`SectionQuery`]):
//!
//! - **Fetch** — ranked hits for the pack's query; no model runs, so this is
//!   what a live turn uses.
//! - **Answer** — a synthesised answer to a fixed question; an engine may run
//!   a model, so this is for session start and compaction.
//! - **Latest** — the newest, most confident items, with no query.
//!
//! The sections are read concurrently. An item is listed once, in the first
//! section that lists it, so overlapping scopes (one agent's history inside
//! the team's) never repeat a bullet; an answer citing an item does not
//! hide it. Then they are rendered under one `#` title,
//! one `##` heading per section that found something. The block fits
//! `budget_tokens` (four characters per token): bullets are trimmed from the
//! last section first, then answers shorten (see `render`).
//!
//! Nothing an engine does fails a pack. A section whose read fails, or finds
//! nothing, is left out and reported in [`ContextPack::skipped`]. The only
//! error is an invalid request.
//!
//! Every lifecycle read is a preset of this: `context.md`
//! ([`crate::context`]) is answered briefs plus the latest learnings with
//! frontmatter, and [`crate::AgentMemory`]'s session start, pre-turn and
//! compaction packs are the layout's scopes filled for their moment.
//!
//! # Example
//!
//! ```
//! use tinymemory_api::conformance::ReferenceEngine;
//! use tinymemory_api::{ItemKind, LearningKind, MemoryEngine, MemoryMeta, MetaFilter, StoreItem};
//! use tinymemory_tools::recall::{HolisticRecall, ScopeSection, holistic_recall};
//!
//! # let runtime = tokio::runtime::Builder::new_current_thread().build()?;
//! # runtime.block_on(async {
//! let engine = ReferenceEngine::new();
//! engine
//!     .store(StoreItem::document("Refunds take five business days.", MemoryMeta::default()))
//!     .await?;
//! engine
//!     .store(StoreItem::learning("Customers prefer email", LearningKind::Fact, 0.8, MemoryMeta::default()))
//!     .await?;
//!
//! let request = HolisticRecall::new(
//!     Some("how long do refunds take".into()),
//!     vec![
//!         ScopeSection::fetch("Documents", MetaFilter::kinds([ItemKind::Document]), 5),
//!         ScopeSection::latest("Learnings", MetaFilter::kinds([ItemKind::Learning]), 5),
//!     ],
//! );
//! let pack = holistic_recall(&engine, &request).await?;
//! assert!(pack.markdown.starts_with("# Memory\n"));
//! assert!(pack.markdown.contains("## Documents\n\n- Refunds take five business days."));
//! assert!(pack.markdown.contains("## Learnings\n\n- Customers prefer email"));
//! # Ok::<(), tinymemory_api::Error>(())
//! # })?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod gather;
pub(crate) mod render;
mod types;

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;

use futures::future::{join, join_all};
use tinymemory_api::{MemoryEngine, Result, TimeHint};

use gather::Settled;
pub(crate) use render::Frontmatter;
pub use render::estimate_tokens;
pub use types::{
    ContextPack, DEFAULT_PACK_BUDGET_TOKENS, DEFAULT_PACK_TITLE, HolisticRecall, ScopeSection,
    SectionHits, SectionQuery, SkipReason, SkippedSection, ThreadWindow,
};

/// Reads every section of `request` from `engine`, concurrently, and renders
/// the pack.
///
/// # Errors
///
/// [`tinymemory_api::Error::InvalidRequest`] for an invalid request
/// ([`HolisticRecall::validate`]). Engine failures are not errors: see the
/// module docs.
pub async fn holistic_recall(
    engine: &dyn MemoryEngine,
    request: &HolisticRecall,
) -> Result<ContextPack> {
    run(engine, request, None, None, None).await
}

/// [`holistic_recall`] for a question whose date is still being worked out.
///
/// The sections are read at once, without waiting for `hint`; fetch sections
/// read deeper than they show. When the reads are done and `hint` has
/// resolved to a [`TimeHint`], each fetch section's hits from those days move
/// ahead of the rest ([`TimeHint::rank`]) before the sections are cut to
/// their limits and rendered. The pack waits for whichever of the two
/// finishes last, so `hint` must carry its own deadline: resolve to `None`
/// when the date is not known in time, and the pack is the undated one.
///
/// # Errors
///
/// As [`holistic_recall`].
pub async fn holistic_recall_dated(
    engine: &dyn MemoryEngine,
    request: &HolisticRecall,
    hint: impl Future<Output = Option<TimeHint>> + Send,
) -> Result<ContextPack> {
    run(engine, request, None, Some(Box::pin(hint)), None).await
}

/// A date that may arrive while the sections are read.
pub(crate) type LateHint<'a> = Pin<Box<dyn Future<Output = Option<TimeHint>> + Send + 'a>>;

/// [`holistic_recall`], optionally with `context.md` frontmatter, a late
/// date, and one lifecycle team section that excludes its current agent.
pub(crate) async fn run(
    engine: &dyn MemoryEngine,
    request: &HolisticRecall,
    frontmatter: Option<Frontmatter<'_>>,
    late: Option<LateHint<'_>>,
    excluded_team: Option<(usize, &str)>,
) -> Result<ContextPack> {
    request.validate()?;
    let beliefs = gather::belief_budget(request);
    let dated = late.is_some() || request.refers_to.is_some();
    let reads = join_all(request.sections.iter().enumerate().map(|(index, section)| {
        let excluded_agent = excluded_team
            .filter(|(team, _)| *team == index)
            .map(|(_, agent)| agent);
        gather::section(engine, request, section, beliefs, dated, excluded_agent)
    }));
    let (mut gathered, late) = match late {
        Some(late) => join(reads, late).await,
        None => (reads.await, None),
    };
    let hint = late
        .filter(|hint| hint.validate().is_ok())
        .or_else(|| request.refers_to.clone());
    if let Some(hint) = &hint {
        for (section, outcome) in request.sections.iter().zip(&mut gathered) {
            // Sections that ranked by a query (fetch, or an answer that fell
            // back to fetch); a section that read the newest items, latest
            // or a fetch with no query at all, keeps its newest-first order.
            let queried = match &section.query {
                SectionQuery::Fetch { query } => query
                    .as_deref()
                    .or(request.query.as_deref())
                    .is_some_and(|query| !query.trim().is_empty()),
                SectionQuery::Answer { .. } => true,
                SectionQuery::Latest => false,
            };
            if let (true, gather::Gathered::Hits { hits, .. }) = (queried, outcome) {
                hint.rank(hits);
            }
        }
    }
    gather::fold_beliefs(request, &mut gathered);
    let mut sections = Vec::new();
    let mut rendered_from = Vec::new();
    let mut skipped = Vec::new();
    let mut shown = HashSet::new();
    for (index, (section, outcome)) in request.sections.iter().zip(gathered).enumerate() {
        let excluded_agent = excluded_team
            .filter(|(team, _)| *team == index)
            .map(|(_, agent)| agent);
        match gather::settle(request, section, outcome, excluded_agent, &mut shown) {
            Settled::Filled(rendered, hits) => {
                rendered_from.push(rendered);
                sections.push(hits);
            }
            Settled::Skipped(reason) => skipped.push(reason),
        }
    }
    let rendered = render::render(
        rendered_from,
        request.budget_tokens,
        &request.title,
        frontmatter,
    );
    let engine_id = engine.descriptor().id;
    log::debug!(
        "[recall] pack engine={engine_id} sections={} skipped={} tokens={} dated={}",
        sections.len(),
        skipped.len(),
        rendered.tokens,
        hint.is_some()
    );
    Ok(ContextPack {
        markdown: rendered.markdown,
        tokens: rendered.tokens,
        refs: rendered.refs,
        sections,
        skipped,
        engine: engine_id.to_string(),
    })
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "mod_dated_tests.rs"]
mod dated_tests;
