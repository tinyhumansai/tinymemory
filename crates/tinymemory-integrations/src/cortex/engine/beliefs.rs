//! Beliefs: the claims CortexDB builds (`v1/beliefs/build`), read as
//! learning hits.
//!
//! CortexDB keeps beliefs in a derived layer beside the events this crate
//! writes, so fetch and list (which decode events) never return them. This
//! read serves [`tinymemory_api::MemoryEngine::beliefs`] instead:
//!
//! - **Which scopes.** Every scope CortexDB holds in the request's reach,
//!   whatever its kind (`scopes::held`): a build writes its beliefs into
//!   the scope it read, so a conversation's beliefs sit with the
//!   conversation.
//! - **With a query**, one recall pack per scope asks for the `beliefs`
//!   layer only (no events), a few scopes at once, and the scopes' rankings
//!   are interleaved rank by rank.
//! - **Without one**, the Direct wire lists each scope's beliefs
//!   (`GET v1/beliefs`) and orders them most confident, then newest. The
//!   hosted wire has no listing, so it answers none.
//!
//! Only `supported` and `contested` beliefs are read; a contested one says
//! so in its text. A belief is rendered as a sentence, `subject predicate
//! object` with the predicate's underscores as spaces, and becomes a
//! [`LearningKind::Fact`] at the node of its scope, tagged [`BELIEF_TAG`],
//! with the belief's confidence and `valid_from` as its time. The same
//! sentence from two scopes is kept once.

use std::collections::HashSet;

use futures::{StreamExt, TryStreamExt, stream};
use reqwest::Method;
use serde_json::{Value, json};
use tinymemory_api::chrono::{DateTime, Utc};
use tinymemory_api::{
    BELIEF_TAG, BeliefsRequest, Hit, ItemKind, LearningKind, MemoryMeta, MetaFilter, StoreItem,
};

use super::CortexEngine;
use super::fetch::recall_body;
use super::items::hit;
use crate::cortex::descriptor::{CortexWire, Route};
use crate::cortex::envelope::parse_scope;
use crate::cortex::error::{Error, Result};
use crate::cortex::tenancy::ScopeRoot;
use crate::cortex::transport::{Attempts, urlencode};

/// Scopes read at once.
const SCOPES_AT_ONCE: usize = 4;

/// The stances worth showing a reader.
const SHOWN_STANCES: [&str; 2] = ["supported", "contested"];

/// The text of a claim part: an entity's name, a literal's value, or the
/// part itself when it is a bare string.
fn part(value: Option<&Value>) -> Option<String> {
    let value = value?;
    let text = value
        .get("name")
        .or_else(|| value.get("value"))
        .unwrap_or(value);
    let text = match text {
        Value::String(text) => text.clone(),
        Value::Null | Value::Object(_) | Value::Array(_) => return None,
        other => other.to_string(),
    };
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// One belief as a learning hit at `rank`; `None` for a belief that is not
/// shown (a stance other than [`SHOWN_STANCES`], no claim, or a scope this
/// engine did not write: another tenant's, or none of TinyMemory's).
pub(super) fn belief_hit(root: &ScopeRoot, belief: &Value, rank: usize) -> Option<Hit> {
    let id = belief.get("id").and_then(Value::as_str)?;
    let stance = belief
        .get("stance")
        .and_then(Value::as_str)
        .unwrap_or("supported");
    if !SHOWN_STANCES.contains(&stance) {
        return None;
    }
    let (namespace, _) = parse_scope(root, belief.get("scope").and_then(Value::as_str)?)?;
    let claim = belief.get("claim")?;
    let predicate = part(claim.get("predicate"))?.replace('_', " ");
    let object = part(claim.get("object"))?;
    let mut text = match part(claim.get("subject")) {
        Some(subject) => format!("{subject} {predicate} {object}"),
        None => format!("{predicate} {object}"),
    };
    if stance == "contested" {
        text.push_str(" (contested)");
    }
    let confidence = belief
        .get("confidence")
        .and_then(Value::as_f64)
        .map_or(0.5, |c| c.clamp(0.0, 1.0)) as f32;
    let observed_at = ["valid_from", "updated_at", "created_at"]
        .iter()
        .find_map(|field| belief.get(*field)?.as_str()?.parse::<DateTime<Utc>>().ok());
    let meta = MemoryMeta {
        namespace,
        tags: vec![BELIEF_TAG.to_string()],
        observed_at,
        ..MemoryMeta::default()
    };
    let item = StoreItem::learning(text, LearningKind::Fact, confidence, meta);
    Some(hit(id, &item, 1.0 / (1.0 + rank as f32)))
}

/// The beliefs of one recall pack or listing.
pub(super) fn beliefs_in(root: &ScopeRoot, answer: &Value, pointer: &str) -> Vec<Hit> {
    answer
        .pointer(pointer)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(rank, belief)| belief_hit(root, belief, rank))
        .collect()
}

/// `lists` merged rank by rank, each sentence once, at most `limit`.
pub(super) fn merge(lists: Vec<Vec<Hit>>, limit: usize) -> Vec<Hit> {
    let longest = lists.iter().map(Vec::len).max().unwrap_or(0);
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for rank in 0..longest {
        for list in &lists {
            if let Some(belief) = list.get(rank)
                && seen.insert(belief.text.to_lowercase())
            {
                out.push(belief.clone());
            }
        }
    }
    out.truncate(limit);
    out
}

impl CortexEngine {
    /// See the module docs.
    pub(super) async fn read_beliefs(&self, req: BeliefsRequest) -> Result<Vec<Hit>> {
        req.validate()?;
        let query = req.query.as_deref();
        if query.is_none() && self.wire() == CortexWire::TinyHumans {
            return Ok(Vec::new());
        }
        let scopes = self.held(&req.reach, &ItemKind::ALL).await?;
        let lists: Vec<Vec<Hit>> = stream::iter(scopes)
            .map(|scope| async move {
                match query {
                    Some(query) => self.ranked_beliefs(&scope.path, query, req.limit).await,
                    None => self.listed_beliefs(&scope.path, req.limit).await,
                }
            })
            .buffered(SCOPES_AT_ONCE)
            .try_collect()
            .await?;
        if query.is_some() {
            return Ok(merge(lists, req.limit));
        }
        let mut all = merge(lists, usize::MAX);
        all.sort_by(|a, b| {
            b.confidence
                .unwrap_or_default()
                .total_cmp(&a.confidence.unwrap_or_default())
                .then_with(|| b.meta.observed_at.cmp(&a.meta.observed_at))
        });
        all.truncate(req.limit);
        Ok(all)
    }

    /// The beliefs of `scope` a recall for `query` ranks, at most `limit`.
    async fn ranked_beliefs(&self, scope: &str, query: &str, limit: usize) -> Result<Vec<Hit>> {
        let mut body = recall_body(scope, query, 0, &MetaFilter::default());
        body["budgets"]["per_layer_limits"] = json!({
            "events": 0, "facts": 0, "episodes": 0, "understanding": 0, "beliefs": limit,
        });
        let pack = self.log.recall(&body).await?;
        Ok(beliefs_in(&self.root, &pack, "/layers/beliefs"))
    }

    /// The beliefs CortexDB lists for `scope`, at most `limit`.
    async fn listed_beliefs(&self, scope: &str, limit: usize) -> Result<Vec<Hit>> {
        let path = format!(
            "{base}?scope={scope}&limit={limit}",
            base = self.wire().path(Route::Beliefs),
            scope = urlencode(scope),
        );
        let listed = match self
            .log
            .client
            .json(Method::GET, &path, None, Attempts::RetryTransient)
            .await
        {
            Ok(listed) => listed,
            Err(Error::NotFound(_)) => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        Ok(beliefs_in(&self.root, &listed, "/items"))
    }
}

#[cfg(test)]
#[path = "beliefs_tests.rs"]
mod tests;
