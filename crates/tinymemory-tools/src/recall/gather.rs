//! Filling one section from the engine, and turning what it found into a
//! renderable section.
//!
//! Every section runs on its own and none can fail the pack: an engine error
//! or an empty result becomes a [`SkippedSection`], logged and reported.
//! [`section`] reads; [`settle`] then applies the request's exclusions and
//! the items earlier sections already list, in section order, so an item is
//! listed once — in its highest-priority section.
//! In the agent-history section, selected turns from the same thread are
//! shown newest first while the engine's ordering across threads is kept.
//!
//! **Beliefs.** To the reader a belief the engine built is a learning like
//! any other, so a pack with a learnings section (a fetched or latest
//! section that admits learnings) gathers the engine's beliefs into it:
//!
//! - every ranked section's fetch asks for beliefs too
//!   ([`FetchRequest::beliefs`]), so they come from the reads the pack makes
//!   anyway (on CortexDB, the same recall packs) — the brain, history and
//!   team sections between them read every scope a belief can be built in;
//! - a learnings section with no query (a cold session start) lists them
//!   instead ([`MemoryEngine::beliefs`] without a query), alongside its own
//!   read;
//! - once every section is read, [`fold_beliefs`] merges what they returned,
//!   each belief once, and interleaves it with each learnings section's
//!   stored learnings rank by rank, stored learnings first, keeping only the
//!   beliefs the section's filter admits.
//!
//! A belief read that fails leaves the learnings to the stored ones.
//! Answered sections ask for none: the engine's answer draws on its beliefs
//! itself.

use std::collections::HashSet;
use std::time::Instant;

use futures::future::join;
use tinymemory_api::{
    BeliefsRequest, FetchMode, FetchRequest, Hit, ItemId, ItemKind, ListRequest, MemoryEngine,
    MetaFilter, Namespace, Reach, RecallMode, RecallRequest, TimeHint,
};

use super::render::{Body, Line, Section, shorten, single_line};
use super::types::{
    HolisticRecall, ScopeSection, SectionHits, SectionQuery, SkipReason, SkippedSection,
};

/// Longest a single bullet may be, in characters, before it is shortened: one
/// long document must not crowd out a section.
const MAX_LINE_CHARS: usize = 600;

/// How many times deeper a fetch section reads when a date will reorder its
/// hits (see `section`).
pub(super) const DATED_DEPTH: usize = 3;

/// Page size of a [`SectionQuery::Latest`] listing.
const LATEST_PAGE: usize = 100;

/// Most listing pages read before ranking, except for lifecycle team recall.
const LATEST_MAX_PAGES: usize = 50;

/// Most ranked pages a generic fetch section reads to replace excluded hits.
const FETCH_MAX_PAGES: usize = 5;

/// What one section read.
pub(super) enum Gathered {
    /// An answer and its citations.
    Answered(Section, SectionHits),
    /// Ranked or latest hits, before exclusions, and the beliefs the read
    /// returned beside them.
    Hits {
        /// The section's own hits.
        hits: Vec<Hit>,
        /// Beliefs for the pack's learnings, whichever section read them.
        beliefs: Vec<Hit>,
    },
    /// Nothing, and why.
    Skipped(SkippedSection),
}

/// What one section contributes once settled.
pub(super) enum Settled {
    /// Something to render, and what it was drawn from.
    Filled(Section, SectionHits),
    /// Nothing, and why.
    Skipped(SkippedSection),
}

/// Fills `section` for `request`, asking a ranked read for up to
/// `beliefs` beliefs as well.
pub(super) async fn section(
    engine: &dyn MemoryEngine,
    request: &HolisticRecall,
    section: &ScopeSection,
    beliefs: usize,
    dated: bool,
    excluded_agent: Option<&str>,
) -> Gathered {
    let section_started = Instant::now();
    let keep = |hit: &Hit| {
        !request.excludes(hit)
            && excluded_agent.is_none_or(|agent| hit.meta.agent_id.as_deref() != Some(agent))
    };
    let want = wanted(request, section);
    // A date reorders a fetch section's hits after the read, so read deeper
    // than the section shows: a hit from the right day ranked just past the
    // cut must still be there to lift.
    let fetch_want = if dated {
        want.saturating_mul(DATED_DEPTH)
    } else {
        want
    };
    let outcome = match &section.query {
        SectionQuery::Answer {
            question,
            instructions,
            fallback_to_fetch,
        } => match answer(engine, request, section, question, instructions.clone()).await {
            Ok(Some(filled)) => {
                log::trace!(target: "tinymemory_eval_timing", "recall_section={}", section_started.elapsed().as_secs_f64() * 1_000.0);
                return filled;
            }
            Ok(None) => Ok((Vec::new(), Vec::new())),
            Err(error) if *fallback_to_fetch => {
                log::debug!(
                    "[recall] answer failed, fetching instead heading={:?} error={error}",
                    section.heading
                );
                fetch(
                    engine,
                    section,
                    question,
                    fetch_want,
                    &keep,
                    FetchOptions {
                        beliefs: 0,
                        refers_to: request.refers_to.clone(),
                        scan_all: excluded_agent.is_some(),
                    },
                )
                .await
            }
            Err(error) => Err(error),
        },
        SectionQuery::Fetch { query } => {
            match query
                .as_deref()
                .or(request.query.as_deref())
                .filter(|query| !query.trim().is_empty())
            {
                Some(query) => {
                    let hint = request.refers_to.clone();
                    fetch(
                        engine,
                        section,
                        query,
                        fetch_want,
                        &keep,
                        FetchOptions {
                            beliefs,
                            refers_to: hint,
                            scan_all: excluded_agent.is_some(),
                        },
                    )
                    .await
                }
                None => {
                    with_listed_beliefs(engine, section, want, &keep, excluded_agent.is_some())
                        .await
                }
            }
        }
        SectionQuery::Latest => {
            with_listed_beliefs(engine, section, want, &keep, excluded_agent.is_some()).await
        }
    };
    log::trace!(target: "tinymemory_eval_timing", "recall_section={}", section_started.elapsed().as_secs_f64() * 1_000.0);
    match outcome {
        Ok((hits, beliefs)) => Gathered::Hits { hits, beliefs },
        Err(error) => {
            log::warn!(
                "[recall] section skipped heading={:?} error={error}",
                section.heading
            );
            skipped(section, SkipReason::Engine(error))
        }
    }
}

/// How many hits to ask for so that `section.limit` survive the request's
/// exclusions and the items earlier sections already show: one more per
/// excluded id and per item an earlier section may hold, and double when a
/// whole thread window may be left out.
fn wanted(request: &HolisticRecall, section: &ScopeSection) -> usize {
    let window = if request.exclude_thread.is_some() {
        section.limit
    } else {
        0
    };
    let earlier: usize = request
        .sections
        .iter()
        .take_while(|other| !std::ptr::eq(*other, section))
        .map(|other| other.limit)
        .sum();
    section.limit + request.exclude_ids.len() + window + earlier
}

fn skipped(section: &ScopeSection, reason: SkipReason) -> Gathered {
    Gathered::Skipped(skipped_section(section, reason))
}

fn skipped_section(section: &ScopeSection, reason: SkipReason) -> SkippedSection {
    SkippedSection {
        heading: section.heading.clone(),
        reason,
    }
}

/// One recall; `None` when it cited nothing or answered blank.
async fn answer(
    engine: &dyn MemoryEngine,
    request: &HolisticRecall,
    section: &ScopeSection,
    question: &str,
    instructions: Option<String>,
) -> tinymemory_api::Result<Option<Gathered>> {
    let answer = engine
        .recall(RecallRequest {
            question: question.to_string(),
            filter: section.filter.clone(),
            limit: section.limit,
            instructions,
            refers_to: request.refers_to.clone(),
            // The engine leaves the thread out itself, so the window's turns
            // do not use up the section's citations.
            exclude_thread: request.exclude_thread.clone(),
            mode: RecallMode::Answer,
        })
        .await?;
    if answer.citations.is_empty() || answer.answer.trim().is_empty() {
        return Ok(None);
    }
    let text = answer.answer.trim().to_string();
    let hits: Vec<Hit> = answer
        .citations
        .into_iter()
        .map(|citation| Hit {
            id: citation.id,
            kind: citation.kind,
            text: citation.snippet,
            meta: citation.meta,
            score: citation.score.unwrap_or_default(),
            confidence: None,
        })
        .collect();
    let refs = hits.iter().map(|hit| hit.id.clone()).collect();
    Ok(Some(Gathered::Answered(
        Section {
            heading: section.heading.clone(),
            body: Body::Prose {
                text: text.clone(),
                refs,
            },
        },
        SectionHits {
            heading: section.heading.clone(),
            answer: Some(text),
            hits,
        },
    )))
}

/// The fetch mode a section ranks with: hybrid when the engine serves it,
/// else the first it declares.
fn preferred_mode(engine: &dyn MemoryEngine) -> Option<FetchMode> {
    let modes = &engine.descriptor().fetch_modes;
    if modes.contains(&FetchMode::Hybrid) {
        Some(FetchMode::Hybrid)
    } else {
        modes.first().copied()
    }
}

/// Whether `section` is a learnings section: one ranked or latest read
/// that admits learnings, where the pack's beliefs go.
pub(super) fn reads_learnings(section: &ScopeSection) -> bool {
    !matches!(section.query, SectionQuery::Answer { .. })
        && section.filter.admits_kind(ItemKind::Learning)
}

/// How many beliefs each ranked read asks for: what the first learnings
/// section wants, or none when the pack has no learnings section.
pub(super) fn belief_budget(request: &HolisticRecall) -> usize {
    request
        .sections
        .iter()
        .find(|section| reads_learnings(section))
        .map_or(0, |section| wanted(request, section))
}

/// The newest hits of `section`, and, for a learnings section, the beliefs
/// the engine lists in its reach (read concurrently; a failed listing
/// leaves none).
async fn with_listed_beliefs(
    engine: &dyn MemoryEngine,
    section: &ScopeSection,
    want: usize,
    keep: &(dyn Fn(&Hit) -> bool + Sync),
    scan_all: bool,
) -> tinymemory_api::Result<(Vec<Hit>, Vec<Hit>)> {
    if !reads_learnings(section) {
        return Ok((
            latest(engine, &section.filter, want, keep, scan_all).await?,
            Vec::new(),
        ));
    }
    let reach = section
        .filter
        .reach
        .clone()
        .unwrap_or_else(|| Reach::subtree(Namespace::ROOT));
    let (hits, beliefs) = join(
        latest(engine, &section.filter, want, keep, scan_all),
        engine.beliefs(BeliefsRequest::new(reach, want)),
    )
    .await;
    let beliefs = beliefs.unwrap_or_else(|error| {
        log::warn!(
            "[recall] beliefs unavailable heading={:?} error={error}",
            section.heading
        );
        Vec::new()
    });
    Ok((hits?, beliefs))
}

/// Merges the beliefs every section returned (each once, in section order,
/// rank by rank) into each learnings section's hits; see the module docs.
pub(super) fn fold_beliefs(request: &HolisticRecall, gathered: &mut [Gathered]) {
    let lists: Vec<Vec<Hit>> = gathered
        .iter_mut()
        .filter_map(|outcome| match outcome {
            Gathered::Hits { beliefs, .. } => Some(std::mem::take(beliefs)),
            _ => None,
        })
        .collect();
    let longest = lists.iter().map(Vec::len).max().unwrap_or(0);
    let mut seen = HashSet::new();
    let mut beliefs = Vec::new();
    for rank in 0..longest {
        for list in &lists {
            if let Some(belief) = list.get(rank)
                && seen.insert(belief.id.clone())
            {
                beliefs.push(belief.clone());
            }
        }
    }
    if beliefs.is_empty() {
        return;
    }
    for (section, outcome) in request.sections.iter().zip(gathered.iter_mut()) {
        if !reads_learnings(section) {
            continue;
        }
        if let Gathered::Hits { hits, .. } = outcome {
            let admitted = beliefs
                .iter()
                .filter(|belief| section.filter.matches(ItemKind::Learning, &belief.meta))
                .cloned();
            *hits = interleave(std::mem::take(hits), admitted);
        }
    }
}

/// `first` and `second` merged rank by rank, `first` leading at each rank.
fn interleave(first: Vec<Hit>, second: impl IntoIterator<Item = Hit>) -> Vec<Hit> {
    let mut first = first.into_iter();
    let mut second = second.into_iter();
    let mut out = Vec::new();
    loop {
        match (first.next(), second.next()) {
            (None, None) => return out,
            (a, b) => out.extend(a.into_iter().chain(b)),
        }
    }
}

/// Ranked read options that do not identify the section or query.
struct FetchOptions {
    beliefs: usize,
    refers_to: Option<TimeHint>,
    scan_all: bool,
}

/// Ranked pages of hits; an engine that declares no fetch mode is read
/// newest first instead.
async fn fetch(
    engine: &dyn MemoryEngine,
    section: &ScopeSection,
    query: &str,
    limit: usize,
    keep: &(dyn Fn(&Hit) -> bool + Sync),
    options: FetchOptions,
) -> tinymemory_api::Result<(Vec<Hit>, Vec<Hit>)> {
    let Some(mode) = preferred_mode(engine) else {
        return Ok((
            latest(engine, &section.filter, limit, keep, options.scan_all).await?,
            Vec::new(),
        ));
    };
    // Exclusions (the prompt's thread window, shown ids) are dropped page by
    // page. Only when a page lost hits to them and too few remain is the next
    // page read, so the common case is still one request (each page is a
    // round trip on a hosted engine). Generic recall is capped; lifecycle
    // team recall must scan until it finds another agent or reaches the end.
    let mut hits: Vec<Hit> = Vec::new();
    let mut first_beliefs = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen_cursors = HashSet::new();
    let max_pages = if options.scan_all {
        usize::MAX
    } else {
        FETCH_MAX_PAGES
    };
    for page_no in 0..max_pages {
        let mut request = FetchRequest::new(query, mode, limit);
        request.filter = section.filter.clone();
        request.max_scopes = section.max_scopes;
        request.beliefs = if page_no == 0 { options.beliefs } else { 0 };
        request.cursor = cursor.take();
        request.refers_to = options.refers_to.clone();
        let page = engine.fetch(request).await?;
        if page_no == 0 {
            first_beliefs = page.beliefs;
        }
        let fetched = page.hits.len();
        let before = hits.len();
        hits.extend(page.hits.into_iter().filter(|hit| keep(hit)));
        let lost_some = hits.len() - before < fetched;
        match page.next_cursor {
            Some(next) if hits.len() < limit && lost_some && seen_cursors.insert(next.clone()) => {
                cursor = Some(next);
            }
            _ => break,
        }
    }
    Ok((hits, first_beliefs))
}

/// The newest hits, then the most confident, then the latest turn; ties
/// keep the engine's order.
///
/// Hits `keep` refuses (the request's exclusions: the prompt's own thread
/// window, ids already shown) are dropped *before* the cut to `limit`, so a
/// window holding more recent turns than the overfetch allowance cannot
/// crowd every older turn out of the section.
async fn latest(
    engine: &dyn MemoryEngine,
    filter: &MetaFilter,
    limit: usize,
    keep: &(dyn Fn(&Hit) -> bool + Sync),
    scan_all: bool,
) -> tinymemory_api::Result<Vec<Hit>> {
    let mut all: Vec<Hit> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen_cursors = HashSet::new();
    let max_pages = if scan_all {
        usize::MAX
    } else {
        LATEST_MAX_PAGES
    };
    for _ in 0..max_pages {
        let mut request = ListRequest::new(filter.clone(), LATEST_PAGE);
        request.cursor = cursor.take();
        let page = engine.list(request).await?;
        all.extend(page.items.into_iter().filter(|hit| keep(hit)));
        match page.next_cursor {
            Some(next) if seen_cursors.insert(next.clone()) => cursor = Some(next),
            _ => break,
        }
    }
    all.sort_by(|a, b| {
        b.meta
            .observed_at
            .cmp(&a.meta.observed_at)
            .then_with(|| {
                b.confidence
                    .unwrap_or(0.0)
                    .total_cmp(&a.confidence.unwrap_or(0.0))
            })
            .then_with(|| {
                let last = |hit: &Hit| hit.meta.turns.as_ref().map(|turns| turns.last);
                last(b).cmp(&last(a))
            })
    });
    all.truncate(limit);
    Ok(all)
}

/// The text a hit's bullet shows:
///
/// - a titled document as `title: body` rather than its `# title` heading run
///   into the body, and without the body's own copy of that heading when
///   converted markdown repeats it;
/// - a conversation led by when it was said (`[2026-09-15 09:01] user: …`),
///   when the turn carries a time, so a reader can tell a value that was
///   later corrected from the correction.
fn bullet_text(hit: &Hit) -> String {
    if hit.kind == ItemKind::Conversation
        && let Some(at) = hit.meta.observed_at
    {
        return format!("[{}] {}", at.format("%Y-%m-%d %H:%M"), hit.text);
    }
    if hit.kind == ItemKind::Document
        && let Some(rest) = hit.text.strip_prefix("# ")
        && let Some((title, body)) = rest.split_once("\n\n")
        && !title.contains('\n')
    {
        let title = title.trim();
        let body = body.trim_start();
        let body = body
            .strip_prefix("# ")
            .and_then(|heading| heading.strip_prefix(title))
            .filter(|after| after.is_empty() || after.starts_with('\n'))
            .map_or(body, str::trim_start);
        return format!("{title}: {body}");
    }
    hit.text.clone()
}

/// Settles one gathered section: answers pass through untouched (an answer
/// citing an item does not hide its bullet elsewhere); hits lose the
/// request's exclusions and any item an earlier section already lists, are
/// cut to the section's limit, and join `shown`.
pub(super) fn settle(
    request: &HolisticRecall,
    section: &ScopeSection,
    gathered: Gathered,
    excluded_agent: Option<&str>,
    shown: &mut HashSet<ItemId>,
) -> Settled {
    let hits = match gathered {
        Gathered::Skipped(reason) => return Settled::Skipped(reason),
        Gathered::Answered(rendered, hits) => return Settled::Filled(rendered, hits),
        Gathered::Hits { hits, .. } => hits,
    };
    let kinds = &section.filter.kinds;
    let mut hits: Vec<Hit> = hits
        .into_iter()
        .filter(|hit| kinds.is_empty() || kinds.contains(&hit.kind))
        .filter(|hit| !request.excludes(hit) && !shown.contains(&hit.id))
        .filter(|hit| {
            excluded_agent.is_none_or(|agent| hit.meta.agent_id.as_deref() != Some(agent))
        })
        .take(section.limit)
        .collect();
    if section.heading == crate::lifecycle::HISTORY_HEADING
        && section.filter.kinds.as_slice() == [ItemKind::Conversation]
        && matches!(section.query, SectionQuery::Fetch { .. })
    {
        newest_turns_first_within_thread(&mut hits);
    }
    if hits.is_empty() {
        return Settled::Skipped(skipped_section(section, SkipReason::Empty));
    }
    shown.extend(hits.iter().map(|hit| hit.id.clone()));
    let lines = hits
        .iter()
        .map(|hit| {
            let text = single_line(&bullet_text(hit));
            let text = if text.chars().count() > MAX_LINE_CHARS {
                shorten(&text, MAX_LINE_CHARS)
            } else {
                text
            };
            Line {
                id: hit.id.clone(),
                text,
            }
        })
        .collect();
    Settled::Filled(
        Section {
            heading: section.heading.clone(),
            body: Body::Lines(lines),
        },
        SectionHits {
            heading: section.heading.clone(),
            answer: None,
            hits,
        },
    )
}

/// Keep the engine's order across threads, but make each selected thread's
/// updates read newest first. A later tool outcome can supersede an earlier
/// failure even when the earlier turn had a slightly better retrieval rank.
fn newest_turns_first_within_thread(hits: &mut [Hit]) {
    let mut handled = HashSet::new();
    for index in 0..hits.len() {
        let Some(thread) = hits[index].meta.thread_id.clone() else {
            continue;
        };
        if !handled.insert(thread.clone()) {
            continue;
        }
        let positions: Vec<usize> = (index..hits.len())
            .filter(|&at| hits[at].meta.thread_id.as_deref() == Some(thread.as_str()))
            .collect();
        let mut turns: Vec<Hit> = positions.iter().map(|&at| hits[at].clone()).collect();
        turns.sort_by(|left, right| {
            right
                .meta
                .turns
                .as_ref()
                .map(|range| range.last)
                .cmp(&left.meta.turns.as_ref().map(|range| range.last))
                .then_with(|| right.meta.observed_at.cmp(&left.meta.observed_at))
        });
        for (at, hit) in positions.into_iter().zip(turns) {
            hits[at] = hit;
        }
    }
}

#[cfg(test)]
#[path = "gather_tests.rs"]
mod tests;
