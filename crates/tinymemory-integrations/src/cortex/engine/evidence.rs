//! Recall in [`tinymemory_api::RecallMode::Evidence`]: the evidence an answer
//! would be written from, rendered here, with no answer route.
//!
//! The packs are recall's own (`recall::pack`): one per kind scope the filter
//! reads, exact (`view: "granular"`), events first and then the derived
//! layers, most specific node first. Nothing is sent to `/v1/answer`, so no
//! model runs after retrieval and there is no answer whose grounding differs
//! from its citations. The text is rendered from what the packs decode to:
//!
//! - **Facts:** each pack's `facts` layer, `[valid_from date] statement`.
//! - **Beliefs:** each pack's `beliefs` layer, `subject predicate object`.
//! - **Conversation:** the admitted events, merged rank by rank across the
//!   packs, `[YYYY-MM-DD HH:MM UTC] [role] text`, separated by `---`.
//!
//! **Exclusion.** [`RecallRequest::exclude_thread`] removes the window's turns
//! before rendering, so their text never appears. A fact whose supporting
//! events include an excluded turn the pack returned is left out too, and so
//! is a belief resting on such a fact. A fact derived from an excluded turn
//! the pack did not return cannot be told apart; enrichment usually derives
//! facts after the turn, so a turn logged just before the read has none yet.
//!
//! **Filters.** Derived records carry no item metadata, so facts and beliefs
//! are rendered only for a filter that narrows by reach and kinds alone. Any
//! other field (a workspace, a thread, a tag) leaves them out rather than show
//! a record from an item the filter does not admit. The `episodes` and
//! `understanding` layers (session and activity roll-ups) are not rendered.
//!
//! The citations are the conversation's items, at most `limit`; the evidence
//! may hold more events than that (each pack's events budget is `2 * limit`).
//!
//! **Deadline.** The packs are built at once and waited for at most 12
//! seconds from the start of the recall. A pack still building then is left
//! out (logged as a warning), and the evidence is rendered from the packs
//! that arrived, still most specific first. A host bounds a memory tool call
//! (OpenHuman: 15 seconds), and one scope's pack can take that long on its
//! own, so without the deadline the slowest scope decided whether any
//! evidence arrived. A recall that left a pack out says so: if the packs
//! that arrived hold nothing, it fails as unavailable (so does one where no
//! pack arrived) rather than claim memory holds nothing; otherwise the
//! evidence ends with a note that memory was only partly searched. A pack
//! that fails still fails the recall, as before.

use std::collections::HashSet;

use futures::{StreamExt, stream};
use serde_json::Value;
use tinymemory_api::{
    Citation, ItemId, ItemKind, MetaFilter, RecallAnswer, RecallRequest, Role, ThreadWindow,
};

use super::CortexEngine;
use super::fetch::interleave;
use super::items::{keeps, located_meta};
use super::recall::PACKS_AT_ONCE;
use crate::cortex::envelope::{Envelope, decode_event};
use crate::cortex::error::{Error, Result};

/// What one pack contributes to the evidence.
#[derive(Debug, Default)]
struct Gathered {
    /// Admitted events, best rank first, each item once.
    events: Vec<Envelope>,
    /// Rendered facts.
    facts: Vec<String>,
    /// Rendered beliefs.
    beliefs: Vec<String>,
}

/// The items of `pack`'s `layer`.
fn layer<'a>(pack: &'a Value, name: &str) -> impl Iterator<Item = &'a Value> {
    pack.pointer(&format!("/layers/{name}"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

/// The string ids an array field lists, or each entry's `id` when its
/// entries are objects (a belief's `supports`).
fn ids(record: &Value, field: &str) -> Vec<String> {
    record
        .get(field)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            entry
                .as_str()
                .or_else(|| entry.get("id").and_then(Value::as_str))
                .map(str::to_owned)
        })
        .collect()
}

/// An entity's readable name: its `name`, else its id without CortexDB's
/// `ent_` prefix. `None` for an anonymous fact subject.
fn entity_name(entity: &Value) -> Option<String> {
    if let Some(name) = entity.get("name").and_then(Value::as_str) {
        return Some(name.to_string());
    }
    let id = entity.get("id").and_then(Value::as_str)?;
    if id.starts_with("ent_fact_subject_") {
        return None;
    }
    Some(id.strip_prefix("ent_").unwrap_or(id).to_string())
}

/// A claim's object: a literal's value, else the entity's name.
fn object_text(object: &Value) -> Option<String> {
    object
        .get("value")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| entity_name(object))
}

/// `subject predicate object`; a `states` claim with no named subject is
/// its object alone.
fn claim_text(subject: &Value, predicate: &str, object: &Value) -> Option<String> {
    let object = object_text(object)?;
    let predicate = predicate.replace('_', " ");
    match entity_name(subject) {
        Some(subject) => Some(format!("{subject} {predicate} {object}")),
        None if predicate == "states" => Some(object),
        None => Some(format!("{predicate} {object}")),
    }
}

/// A fact as one line: `[YYYY-MM-DD] statement` when it is dated.
fn fact_line(fact: &Value) -> Option<String> {
    let text = claim_text(
        fact.get("subject")?,
        fact.get("predicate").and_then(Value::as_str).unwrap_or(""),
        fact.get("object")?,
    )?;
    let date = fact
        .get("valid_from")
        .and_then(Value::as_str)
        .and_then(|at| at.get(..10));
    Some(match date {
        Some(date) => format!("[{date}] {text}"),
        None => text,
    })
}

/// A belief's claim as one line.
fn belief_line(belief: &Value) -> Option<String> {
    let claim = belief.get("claim")?;
    claim_text(
        claim.get("subject")?,
        claim.get("predicate").and_then(Value::as_str).unwrap_or(""),
        claim.get("object")?,
    )
}

/// Whether `filter` narrows by reach and kinds alone (see the module docs).
fn scope_only(filter: &MetaFilter) -> bool {
    let mut rest = filter.clone();
    rest.reach = None;
    rest.kinds.clear();
    rest.is_empty()
}

/// What `pack` contributes, with `exclude`'s turns left out.
fn gather(pack: &Value, filter: &MetaFilter, exclude: Option<&ThreadWindow>) -> Gathered {
    let mut gathered = Gathered::default();
    let mut excluded = HashSet::new();
    let mut seen = HashSet::new();
    for decoded in layer(pack, "events").filter_map(decode_event) {
        let envelope = decoded.envelope;
        if !keeps(filter, envelope.kind, &envelope) {
            continue;
        }
        if exclude.is_some_and(|window| window.excludes(envelope.kind, &envelope.meta)) {
            excluded.insert(decoded.event_id);
            continue;
        }
        if seen.insert(envelope.id.clone()) {
            gathered.events.push(envelope);
        }
    }
    if !scope_only(filter) {
        return gathered;
    }
    let mut dropped = HashSet::new();
    for fact in layer(pack, "facts") {
        if ids(fact, "supports").iter().any(|id| excluded.contains(id)) {
            if let Some(id) = fact.get("id").and_then(Value::as_str) {
                dropped.insert(id.to_string());
            }
            continue;
        }
        gathered.facts.extend(fact_line(fact));
    }
    for belief in layer(pack, "beliefs") {
        if ids(belief, "supports")
            .iter()
            .any(|id| dropped.contains(id))
        {
            continue;
        }
        gathered.beliefs.extend(belief_line(belief));
    }
    gathered
}

/// Who an event is from: a turn's role, else its item kind.
fn speaker(envelope: &Envelope) -> String {
    match (&envelope.turn, envelope.kind) {
        (Some(turn), _) => match turn.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::System => "system",
            Role::Tool => "tool",
        }
        .to_string(),
        (None, ItemKind::Document) => match &envelope.title {
            Some(title) => format!("document: {title}"),
            None => "document".to_string(),
        },
        (None, ItemKind::Learning) => "learning".to_string(),
        (None, ItemKind::Conversation) => "conversation".to_string(),
    }
}

/// One event as an evidence entry: `[YYYY-MM-DD HH:MM UTC] [speaker] text`.
fn event_entry(envelope: &Envelope) -> String {
    let at = envelope
        .turn
        .as_ref()
        .and_then(|turn| turn.at)
        .or(envelope.meta.observed_at);
    let speaker = speaker(envelope);
    match at {
        Some(at) => format!(
            "[{}] [{speaker}] {}",
            at.format("%Y-%m-%d %H:%M UTC"),
            envelope.text
        ),
        None => format!("[{speaker}] {}", envelope.text),
    }
}

/// The evidence text: facts, beliefs, then the conversation, each line once.
/// Empty when nothing was gathered.
fn render(facts: &[String], beliefs: &[String], events: &[Envelope]) -> String {
    let unique = |lines: &[String]| -> Vec<String> {
        let mut seen = HashSet::new();
        lines
            .iter()
            .filter(|line| seen.insert(line.to_lowercase()))
            .map(|line| format!("- {line}"))
            .collect()
    };
    let mut sections = Vec::new();
    let facts = unique(facts);
    if !facts.is_empty() {
        sections.push(format!("## Facts\n{}", facts.join("\n")));
    }
    let beliefs = unique(beliefs);
    if !beliefs.is_empty() {
        sections.push(format!("## Beliefs\n{}", beliefs.join("\n")));
    }
    if !events.is_empty() {
        let entries: Vec<String> = events.iter().map(event_entry).collect();
        sections.push(format!("## Conversation\n{}", entries.join("\n\n---\n\n")));
    }
    sections.join("\n\n")
}

impl CortexEngine {
    /// The packs for `paths` that arrive within the evidence deadline (see
    /// the module docs), in `paths` order, and how many were left out.
    async fn packs_by_deadline(
        &self,
        req: &RecallRequest,
        paths: &[String],
    ) -> Result<(Vec<Vec<(usize, Value)>>, usize)> {
        let deadline = tokio::time::Instant::now() + self.log.timing.evidence;
        let requests: Vec<(usize, usize, Value)> = paths
            .iter()
            .enumerate()
            .flat_map(|(scope, path)| {
                self.pack_bodies(req, path)
                    .into_iter()
                    .enumerate()
                    .map(move |(pack, body)| (scope, pack, body))
            })
            .collect();
        let pack_count = requests
            .iter()
            .map(|(_, pack, _)| pack + 1)
            .max()
            .unwrap_or(0);
        let mut building = stream::iter(requests)
            .map(|(scope, pack, body)| async move { (scope, pack, self.log.recall(&body).await) })
            .buffer_unordered(PACKS_AT_ONCE);
        let mut arrived = vec![Vec::new(); paths.len()];
        while let Ok(next) = tokio::time::timeout_at(deadline, building.next()).await {
            let Some((scope, pack, result)) = next else {
                break;
            };
            arrived[scope].push((pack, result?));
        }
        let late: Vec<&str> = paths
            .iter()
            .enumerate()
            .filter(|(index, _)| arrived[*index].len() < pack_count)
            .map(|(_, path)| path.as_str())
            .collect();
        if !paths.is_empty() && arrived.iter().all(Vec::is_empty) {
            return Err(Error::Unavailable(format!(
                "no recall pack arrived within {:?}",
                self.log.timing.evidence
            )));
        }
        if !late.is_empty() {
            log::warn!(
                "[cortex] evidence recall left out {} of {} scopes still building after {:?}: {}",
                late.len(),
                paths.len(),
                self.log.timing.evidence,
                late.join(", ")
            );
        }
        for packs in &mut arrived {
            packs.sort_by_key(|(index, _)| *index);
        }
        Ok((arrived, late.len()))
    }

    /// See the module docs. `paths` are the kind scopes to read, most
    /// specific node first.
    pub(super) async fn recall_evidence(
        &self,
        req: &RecallRequest,
        paths: &[String],
    ) -> Result<RecallAnswer> {
        let (packs, late) = self.packs_by_deadline(req, paths).await?;
        let mut facts = Vec::new();
        let mut beliefs = Vec::new();
        let mut per_pack = Vec::new();
        for scope_packs in &packs {
            for (index, pack) in scope_packs {
                let mut gathered = gather(pack, &req.filter, req.exclude_thread.as_ref());
                // The broad pack is the canonical source for derived layers;
                // the channel pack supplements events only.
                if *index > 0 {
                    gathered.facts.clear();
                    gathered.beliefs.clear();
                }
                facts.extend(gathered.facts);
                beliefs.extend(gathered.beliefs);
                per_pack.push(gathered.events);
            }
        }
        let events = interleave(per_pack);
        let mut answer = render(&facts, &beliefs, &events);
        if late > 0 {
            if answer.is_empty() {
                return Err(Error::Unavailable(format!(
                    "{late} of {} memory areas were still being searched after {:?} and the \
                     rest hold nothing relevant; memory was not fully searched",
                    paths.len(),
                    self.log.timing.evidence
                )));
            }
            answer.push_str(&format!(
                "\n\n(Memory was only partly searched: {late} of {} areas did not answer \
                 within {:?}, so this evidence may be incomplete.)",
                paths.len(),
                self.log.timing.evidence
            ));
        }
        let citations = events
            .into_iter()
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
            model: None,
        })
    }
}

#[cfg(test)]
#[path = "evidence_tests.rs"]
mod tests;
