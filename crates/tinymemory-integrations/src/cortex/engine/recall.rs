//! Recall: a broad recall pack per scope, plus a channel-specific supplement
//! when requested, then the answer route once with one real pack id. In
//! [`tinymemory_api::RecallMode::Evidence`] those packs are rendered as
//! evidence instead and the answer route is never asked (see `evidence`).
//!
//! The scopes are the filter's (see `scopes`): the kind scopes of its reach,
//! or, with no reach (an unscoped, administrative read), every kind scope the
//! engine holds (`scopes::held`). Each gets its own pack, read exactly (`view: "granular"`),
//! a few at a time, most specific node first. No pack is ever asked of a
//! parent scope: CortexDB fills such a pack from its children in storage
//! order, not by relevance, so it would answer from an arbitrary sample.
//! With no scope to read, the answer is empty and nothing is sent.
//!
//! The answer route is asked once with `use_pack_id` (again, after every
//! pack is built anew, when CortexDB dropped the packs in between, up to
//! three rounds: it drops every pack on any forget), so it answers from
//! exactly the evidence that pack holds: with several packs, the one holding
//! the most admitted events, the most specific node on a tie. **The answer
//! text is grounded on that one pack, while the citations come from every
//! pack.** CortexDB's `/v1/answer` takes one `use_pack_id`, whose scope must
//! match the request's; it has no multi-pack context. The parent-scope pack
//! this replaced was a storage-order sample of the children, so no coverage
//! was lost. Grounding the answer on every scope waits for CortexDB's ranked
//! `subtree` lane (experimental and off by default in 0.10.4).
//!
//! Citations come from the packs' `layers.events`, decoded back to items,
//! filtered by the full [`tinymemory_api::MetaFilter`] (reach included) and
//! [`tinymemory_api::RecallRequest::exclude_thread`], and
//! merged rank by rank across the packs (each pack's best, the most specific
//! node's first, then each pack's second, …), one per item, at most `limit`.
//! CortexDB scores none of them. A pack with no decodable events still
//! returns the engine's answer, with no citations.
//!
//! **An excluded thread leaves the citations, not the answer text.** CortexDB
//! narrows a recall only by label, so `exclude_thread` is applied to the
//! decoded events; the server writes the answer from the whole pack, which
//! may still hold the excluded turns.

use std::collections::HashSet;

use futures::{StreamExt, TryStreamExt, stream};
use serde_json::{Value, json};
use tinymemory_api::{Citation, ItemId, Namespace, Reach, RecallAnswer, RecallMode, RecallRequest};

use super::CortexEngine;
use super::fetch::{interleave, ranked, recall_bodies, whole_items_budget};
use super::items::{admitted, located_meta};
use super::scopes::KindScope;
use crate::cortex::descriptor::CortexWire;
use crate::cortex::envelope::Envelope;
use crate::cortex::error::{Error, Result};

/// Recall pack requests in flight when a reach spans several scopes.
pub(super) const PACKS_AT_ONCE: usize = 4;

/// Rounds (packs, then one answer) at most per recall: the first, and one
/// after each time CortexDB dropped the packs before the answer used one.
const ANSWER_ATTEMPTS: usize = 3;

/// The derived layers a pack also draws on, besides events.
const DERIVED_LAYERS: [&str; 4] = ["facts", "beliefs", "episodes", "understanding"];

/// The layers an answer pack includes: the events first, so the token
/// budget funds the citations before the derived layers the answer draws on.
pub(super) fn pack_layers() -> Value {
    let mut layers = vec!["events"];
    layers.extend(DERIVED_LAYERS);
    json!(layers)
}

/// Per-layer budgets for a pack answering with at most `limit` citations:
/// twice that many events (a conversation contributes several turns, and the
/// filter drops some), and `limit` shared across the derived layers.
fn pack_budgets(limit: usize) -> Value {
    let mut layers = serde_json::Map::new();
    layers.insert("events".to_string(), json!(limit.saturating_mul(2)));
    let base = limit / DERIVED_LAYERS.len();
    let remainder = limit % DERIVED_LAYERS.len();
    for (index, layer) in DERIVED_LAYERS.into_iter().enumerate() {
        layers.insert(
            layer.to_string(),
            json!(base + usize::from(index < remainder)),
        );
    }
    Value::Object(layers)
}

/// The id a recall pack names.
fn pack_id_of(pack: &Value) -> Result<&str> {
    pack.get("pack_id")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Engine("CortexDB recall omitted pack_id".to_string()))
}

/// The answer request body.
///
/// The hosted route's schema is strict (an unknown key, or a `null`
/// `answer_instructions`, is a 400), so the hosted body omits instructions
/// when there are none. Direct keeps `answer_instructions: null`.
pub(super) fn answer_body(
    wire: CortexWire,
    scope: &str,
    question: &str,
    pack_id: &str,
    instructions: Option<&str>,
) -> Value {
    let mut body = json!({
        "scope": scope,
        "question": question,
        "use_pack_id": pack_id,
        "cite_sources": true,
        "include_context": true,
    });
    match (wire, instructions) {
        (_, Some(text)) => body["answer_instructions"] = json!(text),
        (CortexWire::Direct | CortexWire::Proxied, None) => {
            body["answer_instructions"] = Value::Null;
        }
        (CortexWire::TinyHumans, None) => {}
    }
    body
}

impl CortexEngine {
    /// See the module docs.
    pub(super) async fn recall_answer(&self, req: RecallRequest) -> Result<RecallAnswer> {
        req.validate()?;
        let scopes = match &req.filter.reach {
            Some(_) => self.scopes_for(&req.filter).await?,
            None => {
                self.held(&Reach::subtree(Namespace::ROOT), &admitted(&req.filter))
                    .await?
            }
        };
        if scopes.is_empty() {
            return Ok(RecallAnswer {
                answer: String::new(),
                citations: Vec::new(),
                model: None,
            });
        }
        // Most specific node first, so its citations lead each rank.
        let mut ordered: Vec<&KindScope> = scopes.iter().collect();
        ordered.sort_by_key(|scope| std::cmp::Reverse(scope.namespace.depth()));
        let paths: Vec<String> = ordered.into_iter().map(|s| s.path.clone()).collect();
        if req.mode == RecallMode::Evidence {
            return self.recall_evidence(&req, &paths).await;
        }
        // A pack lives 60 s, and CortexDB drops every pack it holds once
        // anything is forgotten (measured on 0.10.4: a forget in another
        // scope turns the next `use_pack_id` into a 404). On a 404 every
        // pack is built again, so the answer and the citations both come
        // from packs read after that forget, up to [`ANSWER_ATTEMPTS`]
        // rounds in all.
        let mut round = 1;
        let (per_pack, answered) = loop {
            let (per_pack, answered) = self.answer_round(&req, &paths).await?;
            match answered {
                Err(Error::NotFound(_)) if round < ANSWER_ATTEMPTS => round += 1,
                answered => break (per_pack, answered?),
            }
        };
        let answer = answered
            .get("answer")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Engine("CortexDB omitted the answer text".to_string()))?
            .to_string();
        let mut seen = HashSet::new();
        let citations = interleave(per_pack)
            .into_iter()
            .filter(|envelope| seen.insert(envelope.id.clone()))
            .take(req.limit)
            .map(|envelope| Citation {
                meta: located_meta(&envelope),
                id: ItemId::new(envelope.id),
                kind: envelope.kind,
                snippet: envelope.text,
                score: None,
            })
            .collect();
        Ok(RecallAnswer {
            answer,
            citations,
            model: answered
                .pointer("/diagnostics/answer_model")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    }
}

impl CortexEngine {
    /// One round of recall: a broad pack and optional channel supplement per
    /// path (four requests at a time), decoded and filtered, and the answer
    /// route asked once with the real pack holding the
    /// most admitted events, the most specific node on a tie. The answer's
    /// own error is handed back, so the caller can tell a dropped pack.
    async fn answer_round(
        &self,
        req: &RecallRequest,
        paths: &[String],
    ) -> Result<(Vec<Vec<Envelope>>, Result<Value>)> {
        let grouped: Vec<Vec<(String, Value)>> = stream::iter(paths.to_vec())
            .map(|path| async move {
                let packs = self.packs(req, &path).await?;
                Ok::<_, Error>(packs.into_iter().map(|pack| (path.clone(), pack)).collect())
            })
            .buffered(PACKS_AT_ONCE)
            .try_collect()
            .await?;
        let packs: Vec<(String, Value)> = grouped.into_iter().flatten().collect();
        let per_pack: Vec<Vec<Envelope>> = packs
            .iter()
            .map(|(_, pack)| ranked(pack, None, &req.filter, req.exclude_thread.as_ref()))
            .collect();
        let chosen = per_pack
            .iter()
            .enumerate()
            .max_by_key(|(index, events)| (events.len(), std::cmp::Reverse(*index)))
            .map_or(0, |(index, _)| index);
        let (scope, pack) = &packs[chosen];
        let body = answer_body(
            self.wire(),
            scope,
            &req.question,
            pack_id_of(pack)?,
            req.instructions.as_deref(),
        );
        Ok((per_pack, self.log.answer(&body).await))
    }

    /// Broad legacy pack plus a targeted channel pack, each with a real
    /// server-issued id. The broad pack preserves records written before
    /// channel labels existed.
    pub(super) async fn packs(&self, req: &RecallRequest, scope: &str) -> Result<Vec<Value>> {
        let bodies = self.pack_bodies(req, scope);
        let mut packs = Vec::with_capacity(bodies.len());
        for body in bodies {
            packs.push(self.log.recall(&body).await?);
        }
        Ok(packs)
    }

    /// Request bodies for a scope's broad pack and optional channel supplement.
    pub(super) fn pack_bodies(&self, req: &RecallRequest, scope: &str) -> Vec<Value> {
        recall_bodies(scope, &req.question, 0, &req.filter)
            .into_iter()
            .map(|mut body| {
                body["include"] = pack_layers();
                body["budgets"]["per_layer_limits"] = pack_budgets(req.limit);
                body["budgets"]["max_tokens"] =
                    json!(whole_items_budget(req.limit.saturating_mul(3)));
                body
            })
            .collect()
    }
}

#[cfg(test)]
#[path = "recall_tests.rs"]
mod tests;
