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
//! text, assembled from all its turns.
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

use std::collections::HashSet;

use futures::{StreamExt, TryStreamExt, stream};
use serde_json::{Value, json};
use tinymemory_api::{FetchPage, FetchRequest, Hit, ItemKind, MetaFilter};

use super::CortexEngine;
use super::beliefs::{beliefs_in, merge};
use super::cursor::{self, FetchCursor};
use super::items::{hit, keeps};
use crate::cortex::envelope::{Envelope, decode_event, labels, rebuild};
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

/// A recall body for `query` over `scope`, narrowed by `filter`'s label.
pub(super) fn recall_body(scope: &str, query: &str, events: usize, filter: &MetaFilter) -> Value {
    let mut body = json!({
        "scope": scope,
        "query": query,
        "budgets": { "per_layer_limits": { "events": events } },
    });
    if let Some(labels) = labels::narrowing(filter) {
        body["filters"] = json!({ "metadata": { "labels": labels } });
    }
    body
}

/// The distinct items of `kind` a pack's events decode to, best rank first,
/// keeping only what `filter` matches.
pub(super) fn ranked(pack: &Value, kind: Option<ItemKind>, filter: &MetaFilter) -> Vec<Envelope> {
    let mut seen = HashSet::new();
    pack.pointer("/layers/events")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(decode_event)
        .map(|decoded| decoded.envelope)
        .filter(|envelope| keeps(filter, kind.unwrap_or(envelope.kind), envelope))
        .filter(|envelope| seen.insert(envelope.id.clone()))
        .collect()
}

impl CortexEngine {
    /// See the module docs.
    pub(super) async fn fetch_page(&self, req: FetchRequest) -> Result<FetchPage> {
        self.descriptor.ensure_mode(req.mode)?;
        req.validate()?;
        let offset = match &req.cursor {
            Some(raw) => cursor::decode::<FetchCursor>(TAG, raw)?.offset,
            None => 0,
        };
        let end = offset.saturating_add(req.limit);
        let events = end
            .saturating_add(1)
            .saturating_mul(EVENTS_PER_HIT)
            .min(MAX_PACK_EVENTS);
        let scopes = self.scopes_for(&req.filter).await?;
        let wanted_beliefs = if offset == 0 { req.beliefs } else { 0 };
        let req = &req;
        let packs: Vec<(Vec<Envelope>, Vec<Hit>)> = stream::iter(scopes)
            .map(|scope| async move {
                let mut body = recall_body(&scope.path, &req.query, events, &req.filter);
                if wanted_beliefs > 0 {
                    body["budgets"]["per_layer_limits"]["beliefs"] = json!(wanted_beliefs);
                }
                let pack = self.log.recall(&body).await?;
                let beliefs = if wanted_beliefs > 0 {
                    beliefs_in(&self.root, &pack, "/layers/beliefs")
                } else {
                    Vec::new()
                };
                Ok::<_, Error>((ranked(&pack, Some(scope.kind), &req.filter), beliefs))
            })
            .buffered(PACKS_AT_ONCE)
            .try_collect()
            .await?;
        let (per_scope, beliefs): (Vec<Vec<Envelope>>, Vec<Vec<Hit>>) = packs.into_iter().unzip();
        let beliefs: Vec<Hit> = merge(beliefs, wanted_beliefs)
            .into_iter()
            .filter(|belief| {
                req.filter
                    .reach
                    .as_ref()
                    .is_none_or(|reach| reach.admits(&belief.meta.namespace))
            })
            .collect();
        let merged = interleave(per_scope);
        let more = merged.len() > end;
        let page: Vec<(usize, Envelope)> = merged
            .into_iter()
            .enumerate()
            .skip(offset)
            .take(req.limit)
            .collect();
        let conversations = self
            .conversations(
                &page
                    .iter()
                    .filter(|(_, e)| e.kind == ItemKind::Conversation)
                    .map(|(_, e)| (e.id.clone(), e.meta.namespace.clone()))
                    .collect::<Vec<_>>(),
            )
            .await?;
        let hits: Vec<Hit> = page
            .into_iter()
            .filter_map(|(rank, envelope)| {
                let score = 1.0 / (1.0 + rank as f32);
                let item = match envelope.kind {
                    ItemKind::Conversation => conversations.get(&envelope.id)?.clone(),
                    _ => rebuild(std::slice::from_ref(&envelope))?,
                };
                Some(hit(&envelope.id, &item, score))
            })
            .collect();
        let next_cursor = if more {
            Some(cursor::encode(TAG, &FetchCursor { offset: end })?)
        } else {
            None
        };
        Ok(FetchPage {
            hits,
            next_cursor,
            beliefs,
        })
    }
}

/// Merges per-scope rankings rank by rank: every scope's best, then every
/// scope's second, and so on.
fn interleave(mut lists: Vec<Vec<Envelope>>) -> Vec<Envelope> {
    let longest = lists.iter().map(Vec::len).max().unwrap_or(0);
    let mut iters: Vec<_> = lists.iter_mut().map(|list| list.drain(..)).collect();
    let mut out = Vec::new();
    for _ in 0..longest {
        for iter in &mut iters {
            if let Some(envelope) = iter.next() {
                out.push(envelope);
            }
        }
    }
    out
}

#[cfg(test)]
#[path = "fetch_tests.rs"]
mod tests;
