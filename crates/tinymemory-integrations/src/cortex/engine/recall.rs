//! Recall: recall packs, then the answer route once with one of them.
//!
//! - **One scope** (one kind at one node): one pack over it.
//! - **No reach** (an unscoped, administrative read) over several scopes: one
//!   pack over the TinyMemory root with `view: "descend"`, which recalls the
//!   root and every scope under it.
//! - **A reach** over several scopes (an agent's own node and the nodes it
//!   inherits, each kind apart): one pack per scope, built concurrently and
//!   read exactly, never by server-side traversal, so a sibling agent's scope
//!   is never in the pack.
//!
//! The answer route is asked once with `use_pack_id`, so it answers from
//! exactly the evidence that pack holds: with several packs, the one holding
//! the most admitted events, the most specific node on a tie.
//!
//! Citations come from the packs' `layers.events`, decoded back to items,
//! filtered by the full [`tinymemory_api::MetaFilter`] (reach included), one
//! per item, the most specific node's first, at most `limit`. CortexDB scores
//! none of them. A pack with no decodable events still returns the engine's
//! answer, with no citations.

use std::collections::HashSet;

use futures::{StreamExt, TryStreamExt, stream};
use serde_json::{Value, json};
use tinymemory_api::{Citation, ItemId, RecallAnswer, RecallRequest};

use super::CortexEngine;
use super::fetch::{ranked, recall_body};
use super::scopes::KindScope;
use crate::cortex::descriptor::CortexWire;
use crate::cortex::envelope::Envelope;
use crate::cortex::error::{Error, Result};

/// Recall packs built at once when a reach spans several scopes.
const PACKS_AT_ONCE: usize = 4;

/// The derived layers a pack also draws on, besides events.
const DERIVED_LAYERS: [&str; 4] = ["facts", "beliefs", "episodes", "understanding"];

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
        (CortexWire::Direct, None) => body["answer_instructions"] = Value::Null,
        (CortexWire::TinyHumans, None) => {}
    }
    body
}

impl CortexEngine {
    /// See the module docs.
    pub(super) async fn recall_answer(&self, req: RecallRequest) -> Result<RecallAnswer> {
        req.validate()?;
        let scopes = self.scopes_for(&req.filter).await?;
        let packs: Vec<(String, Value)> = match scopes.as_slice() {
            [single] => vec![(
                single.path.clone(),
                self.pack(&req, &single.path, false).await?,
            )],
            _ if req.filter.reach.is_none() || scopes.is_empty() => vec![(
                self.root.path().to_string(),
                self.pack(&req, self.root.path(), true).await?,
            )],
            _ => {
                // Most specific node first, so its citations lead.
                let mut ordered: Vec<&KindScope> = scopes.iter().collect();
                ordered.sort_by_key(|scope| std::cmp::Reverse(scope.namespace.depth()));
                let paths: Vec<String> = ordered.into_iter().map(|s| s.path.clone()).collect();
                let req = &req;
                stream::iter(paths)
                    .map(|path| async move {
                        let pack = self.pack(req, &path, false).await?;
                        Ok::<_, Error>((path, pack))
                    })
                    .buffered(PACKS_AT_ONCE)
                    .try_collect()
                    .await?
            }
        };
        let per_pack: Vec<Vec<Envelope>> = packs
            .iter()
            .map(|(_, pack)| ranked(pack, None, &req.filter))
            .collect();
        let chosen = per_pack
            .iter()
            .enumerate()
            .max_by_key(|(index, events)| (events.len(), std::cmp::Reverse(*index)))
            .map_or(0, |(index, _)| index);
        let (scope, pack) = &packs[chosen];
        let pack_id = pack
            .get("pack_id")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Engine("CortexDB recall omitted pack_id".to_string()))?;
        let answered = self
            .log
            .answer(&answer_body(
                self.wire(),
                scope,
                &req.question,
                pack_id,
                req.instructions.as_deref(),
            ))
            .await?;
        let answer = answered
            .get("answer")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Engine("CortexDB omitted the answer text".to_string()))?
            .to_string();
        let mut seen = HashSet::new();
        let citations = per_pack
            .into_iter()
            .flatten()
            .filter(|envelope| seen.insert(envelope.id.clone()))
            .take(req.limit)
            .map(|envelope| Citation {
                id: ItemId::new(envelope.id),
                kind: envelope.kind,
                snippet: envelope.text,
                meta: envelope.meta,
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
    /// A recall pack for `req` over `scope`, sized for `req.limit`
    /// citations; `descend` also reads every scope below.
    async fn pack(&self, req: &RecallRequest, scope: &str, descend: bool) -> Result<Value> {
        let mut body = recall_body(scope, &req.question, 0, &req.filter);
        body["budgets"]["per_layer_limits"] = pack_budgets(req.limit);
        if descend {
            body["view"] = json!("descend");
        }
        self.log.recall(&body).await
    }
}

#[cfg(test)]
#[path = "recall_tests.rs"]
mod tests;
