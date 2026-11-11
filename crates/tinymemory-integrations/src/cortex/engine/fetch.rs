//! Fetch: hybrid retrieval through CortexDB recall packs.
//!
//! Only [`tinymemory_api::FetchMode::Hybrid`] is served: the recall body has no field that
//! chooses lexical or embedding retrieval (see `descriptor`).
//!
//! For each scope the filter reads (each admitted kind at each namespace
//! node in reach, see `scopes`) the engine asks recall for a pack of events (`budgets.per_layer_limits.events`), narrowed by one label filter
//! when the [`tinymemory_api::MetaFilter`] has a labelled field. The events
//! are decoded back to items, the full filter is applied, repeats of an item
//! are dropped keeping its best rank, and the scopes are interleaved rank by
//! rank. The scopes are read a few at a time, in order. CortexDB reports no per-hit score, so the score is the rank's,
//! `1 / (1 + rank)`. A conversation hit carries the whole conversation's
//! text: a one-turn conversation's comes from its pack event, and a longer
//! one's is assembled from all its turns (`items::assembled`, a few
//! namespaces at a time).
//!
//! **Beliefs.** When the request asks for beliefs
//! ([`FetchRequest::beliefs`]), each scope's pack also budgets the `beliefs`
//! layer, so the same pack (one query embedding per scope) ranks both. The
//! beliefs are decoded as in `beliefs`, kept within the filter's reach,
//! merged rank by rank across scopes, each sentence once, and returned on
//! the first page only.
//!
//! **Cursor.** Recall is a ranking, not a log, so it has no cursor of its
//! own. The fetch cursor is an offset into the merged ranking; the next page
//! asks again with a budget large enough to reach past it. A page ends the
//! ranking (`next_cursor: None`) when no hit beyond it was found.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use futures::{StreamExt, TryStreamExt, stream};
use serde_json::{Value, json};
use tinymemory_api::{
    FetchPage, FetchRequest, Hit, ItemKind, MetaFilter, Namespace, Reach, StoreItem, ThreadWindow,
};

use super::CortexEngine;
use super::beliefs::{beliefs_in, merge};
use super::cursor::{self, FetchCursor};
use super::items::{event_hit, hit, keeps, one_turn_conversation};
use super::refers;
use crate::cortex::envelope::chunks::MAX_EVENT_TEXT_BYTES;
use crate::cortex::envelope::{Envelope, decode_event, labels};
use crate::cortex::error::{Error, Result};

/// The cursor tag of a fetch.
const TAG: char = 'f';

/// Events one recall pack may hold. Bounds how deep fetch pages can go.
const MAX_PACK_EVENTS: usize = 1000;

/// Recall packs read at once when a filter spans several scopes. Each is one
/// query embedding and one ranking on the server; reading them one after
/// the other made a turn's latency grow with the number of scopes.
const PACKS_AT_ONCE: usize = 4;

/// Raw events asked for per wanted hit: a conversation contributes several
/// turns, and the client-side filter drops some.
const EVENTS_PER_HIT: usize = 3;

/// A recall body for `query` over exactly `scope`, narrowed by `filter`'s
/// label.
///
/// - `view: "granular"` reads the scope alone. CortexDB's public recall
///   defaults to `holistic`, which also reads the scope's ancestors and
///   descendants; that is never what one per-scope pack wants.
/// - `include` lists `events` first, so the cross-layer token budget funds
///   the events this crate reads before any derived layer (by default it
///   evicts events first). A caller that reads more layers names them after.
/// - `budgets.max_tokens` is [`whole_items_budget`]: room for every event
///   asked for to come back whole. Its client-side token budget applies
///   after the read.
pub(super) fn recall_body(scope: &str, query: &str, events: usize, filter: &MetaFilter) -> Value {
    let mut body = json!({
        "scope": scope,
        "query": query,
        "view": "granular",
        "include": ["events"],
        "budgets": {
            "max_tokens": whole_items_budget(events),
            "per_layer_limits": { "events": events },
        },
    });
    if let Some(labels) = labels::narrowing(filter) {
        body["filters"] = json!({ "metadata": { "labels": labels } });
    }
    body
}

/// Keep the broad legacy read and add a channel-specific pack when needed.
/// Existing records have no channel label, so replacing the broad read would
/// make those records disappear from filtered recalls.
pub(super) fn recall_bodies(
    scope: &str,
    query: &str,
    events: usize,
    filter: &MetaFilter,
) -> Vec<Value> {
    let broad = recall_body(scope, query, events, filter);
    let Some(channel_labels) = labels::channel_narrowing(filter) else {
        return vec![broad];
    };
    let mut targeted = broad.clone();
    targeted["filters"] = json!({ "metadata": { "labels": channel_labels } });
    vec![broad, targeted]
}

/// The most [`whole_items_budget`] asks for: 8 Mi tokens. CortexDB 0.10.4
/// counts 3 to 3.5 bytes a token (measured: 700,000 bytes of English text
/// come back whole at 210,000 tokens and are cut at 200,000; 900,000 bytes
/// of CJK text whole at 300,000; 300,000 random bytes whole at 100,000), so a
/// pack then holds at most about 24 to 28 MiB of event text, under the
/// 32 MiB request cap.
pub(super) const MAX_PACK_TOKENS: usize = 8 * 1024 * 1024;

/// A pack's `budgets.max_tokens` for `items` items: a token per byte of the
/// largest event this crate writes, for each, at most [`MAX_PACK_TOKENS`].
/// CortexDB's default, 4000 tokens (about 14 KB), cuts a longer event to a
/// `budget_excerpt` (0.10.4 API §9.5): a slice of the stored envelope that
/// no longer decodes, so the hit is lost.
///
/// A token per byte is at least three times the room an event needs. The
/// budget only stops cutting; `per_layer_limits` still bounds what a pack
/// holds. A pack of `n` events carries at most `n` × 768 KiB of event text,
/// and only past [`MAX_PACK_TOKENS`] (at least 32 events of that size in one
/// pack, or about 100 at the 256 KiB chunk target) does the server excerpt
/// again; the pack notes log any excerpt at warn.
pub(super) fn whole_items_budget(items: usize) -> usize {
    items
        .max(1)
        .saturating_mul(MAX_EVENT_TEXT_BYTES)
        .min(MAX_PACK_TOKENS)
}

/// The distinct items of `kind` a pack's events decode to, best rank first,
/// keeping only what `filter` matches and `exclude` does not cover.
pub(super) fn ranked(
    pack: &Value,
    kind: Option<ItemKind>,
    filter: &MetaFilter,
    exclude: Option<&ThreadWindow>,
) -> Vec<Envelope> {
    let mut seen = HashSet::new();
    pack.pointer("/layers/events")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(decode_event)
        .map(|decoded| decoded.envelope)
        .filter(|envelope| keeps(filter, kind.unwrap_or(envelope.kind), envelope))
        // CortexDB's recall filters can only narrow by label, so an excluded
        // thread is dropped here, from what the pack returned.
        .filter(|envelope| {
            exclude.is_none_or(|window| !window.excludes(envelope.kind, &envelope.meta))
        })
        .filter(|envelope| seen.insert(envelope.id.clone()))
        .collect()
}

impl CortexEngine {
    /// See the module docs.
    pub(super) async fn fetch_page(&self, req: FetchRequest) -> Result<FetchPage> {
        let fetch_started = Instant::now();
        self.descriptor.ensure_mode(req.mode)?;
        req.validate()?;
        let (offset, chosen) = match &req.cursor {
            Some(raw) => {
                let cursor = cursor::decode::<FetchCursor>(TAG, raw)?;
                (cursor.offset, cursor.scopes)
            }
            None => (0, None),
        };
        let end = offset.saturating_add(req.limit);
        let events = end
            .saturating_add(1)
            .saturating_mul(EVENTS_PER_HIT)
            .min(MAX_PACK_EVENTS);
        let discovery_started = Instant::now();
        let scopes = self.scopes_for(&req.filter).await?;
        log::trace!(target: "tinymemory_eval_timing", "cortex_scope_discovery={}", discovery_started.elapsed().as_secs_f64() * 1_000.0);
        let (scopes, chosen) = match (req.max_scopes, chosen) {
            // A later page reads the scopes the first page chose.
            (Some(_), Some(paths)) => {
                let kept = scopes
                    .into_iter()
                    .filter(|scope| paths.contains(&scope.path))
                    .collect();
                (kept, Some(paths))
            }
            (Some(max), None) if scopes.len() > max => {
                let kept = self.pick_scopes(scopes, &req.query, max).await;
                let paths = kept.iter().map(|scope| scope.path.clone()).collect();
                (kept, Some(paths))
            }
            _ => (scopes, None),
        };
        let wanted_beliefs = if offset == 0 { req.beliefs } else { 0 };
        let hint = match &req.refers_to {
            Some(hint) if self.sends_refers().await => Some(refers::temporal(hint)),
            _ => None,
        };
        let req = &req;
        let hint = &hint;
        let packs: Vec<(Vec<Envelope>, Vec<Hit>)> = stream::iter(scopes)
            .map(|scope| async move {
                let mut per_scope = Vec::new();
                let mut beliefs = Vec::new();
                for (index, mut body) in recall_bodies(&scope.path, &req.query, events, &req.filter)
                    .into_iter().enumerate()
                {
                    if wanted_beliefs > 0 && index == 0 {
                        body["include"] = json!(["events", "beliefs"]);
                        body["budgets"]["per_layer_limits"]["beliefs"] = json!(wanted_beliefs);
                    }
                    if let Some(temporal) = hint.as_ref().filter(|_| !self.refers_off()) {
                        body["temporal"] = temporal.clone();
                    }
                    let pack_started = Instant::now();
                    let pack = match self.log.recall(&body).await {
                        Err(error @ Error::InvalidRequest(_)) if hint.is_some() => {
                            body.as_object_mut().map(|body| body.remove("temporal"));
                            let pack = self.log.recall(&body).await?;
                            self.refers_refused(&error);
                            pack
                        }
                        pack => pack?,
                    };
                    log::trace!(target: "tinymemory_eval_timing", "cortex_recall_pack={}", pack_started.elapsed().as_secs_f64() * 1_000.0);
                    if index == 0 && wanted_beliefs > 0 {
                        beliefs = beliefs_in(&self.layout, &pack, "/layers/beliefs");
                    }
                    per_scope.extend(ranked(
                        &pack,
                        Some(scope.kind),
                        &req.filter,
                        req.exclude_thread.as_ref(),
                    ));
                }
                Ok::<_, Error>((per_scope, beliefs))
            })
            .buffered(PACKS_AT_ONCE)
            .try_collect()
            .await?;
        let (per_scope, beliefs): (Vec<Vec<Envelope>>, Vec<Vec<Hit>>) = packs.into_iter().unzip();
        let beliefs: Vec<Hit> = merge(beliefs, wanted_beliefs)
            .into_iter()
            .filter(|belief| Reach::admitted_by(req.filter.reach.as_ref(), &belief.meta.namespace))
            .collect();
        let mut merged = interleave(per_scope);
        if let Some(hint) = &req.refers_to {
            // The server lifted the hinted days inside each scope's pack; the
            // rank-by-rank merge would bury them again under other scopes'
            // undated best, so lift them once more across the merged list.
            let (on, off): (Vec<Envelope>, Vec<Envelope>) = merged
                .into_iter()
                .partition(|e| e.meta.observed_at.is_some_and(|at| hint.covers(at)));
            merged = on.into_iter().chain(off).collect();
        }
        let more = merged.len() > end;
        let page: Vec<(usize, Envelope)> = merged
            .into_iter()
            .enumerate()
            .skip(offset)
            .take(req.limit)
            .collect();
        // A one-turn conversation is whole in its pack event; only longer
        // ones are assembled from their turns.
        let mut conversations: HashMap<String, StoreItem> = page
            .iter()
            .filter_map(|(_, e)| Some((e.id.clone(), one_turn_conversation(e)?)))
            .collect();
        let longer: Vec<(String, Namespace)> = page
            .iter()
            .filter(|(_, e)| e.kind == ItemKind::Conversation && !conversations.contains_key(&e.id))
            .map(|(_, e)| (e.id.clone(), e.meta.namespace.clone()))
            .collect();
        conversations.extend(self.conversations(&longer).await?);
        let hits: Vec<Hit> = page
            .into_iter()
            .filter_map(|(rank, envelope)| {
                let score = 1.0 / (1.0 + rank as f32);
                match envelope.kind {
                    ItemKind::Conversation => {
                        Some(hit(&envelope.id, conversations.get(&envelope.id)?, score))
                    }
                    _ => event_hit(&envelope, score),
                }
            })
            .collect();
        let next_cursor = if more {
            Some(cursor::encode(
                TAG,
                &FetchCursor {
                    offset: end,
                    scopes: chosen,
                },
            )?)
        } else {
            None
        };
        log::trace!(target: "tinymemory_eval_timing", "cortex_fetch_total={}", fetch_started.elapsed().as_secs_f64() * 1_000.0);
        Ok(FetchPage {
            hits,
            next_cursor,
            beliefs,
        })
    }
}

/// Merges per-scope rankings rank by rank: every scope's best, then every
/// scope's second, and so on, each item once.
pub(super) fn interleave(mut lists: Vec<Vec<Envelope>>) -> Vec<Envelope> {
    let longest = lists.iter().map(Vec::len).max().unwrap_or(0);
    let mut iters: Vec<_> = lists.iter_mut().map(|list| list.drain(..)).collect();
    let mut out = Vec::new();
    // An item held below both the root and a retired root is one hit, at its
    // best rank.
    let mut seen = HashSet::new();
    for _ in 0..longest {
        for iter in &mut iters {
            if let Some(envelope) = iter.next()
                && seen.insert(envelope.id.clone())
            {
                out.push(envelope);
            }
        }
    }
    out
}

#[cfg(test)]
#[path = "fetch_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "fetch_assembly_tests.rs"]
mod assembly_tests;
