//! Store: replay detection, then the items' missing events, then the wait.
//!
//! `store` is `store_many` of one item: there is one path, so a single store
//! gets exactly the batch's guarantees (listed on return, and ranked recall
//! awaited for its final event).
//!
//! An item id is the item's fingerprint, so the engine first looks up the
//! events already carrying that id's label in the item's scope (its kind at
//! its namespace node):
//!
//! - every event present (the whole document, learning, or every turn) —
//!   a replay: nothing is written and the receipt says so;
//! - some turns of a conversation, or some pieces of a chunked document,
//!   present — a previous store failed part way, and only the missing ones
//!   are written, in order;
//! - nothing present — every event is written.
//!
//! Each event is keyed by its own body (`transport::body_idempotency_key`),
//! so an identical retry is a replay CortexDB answers without writing
//! (`replayed_from_idempotency`), and forgetting an item releases its keys.
//! A key lasts 24 hours and changes with any byte of the body, so it does
//! not replace the lookup: an unchanged file synced again a day later, or
//! with a new `observed_at`, would be written again without it. The lookup
//! is skipped only on the Direct, accepted-only hot path for a single-turn
//! conversation with neither a channel nor a thread id. Channel- or
//! thread-bearing conversations retain lookup so a pre-channel-neutral item
//! id can be recovered. If the exact old id is unknown because the channel
//! changed, recovery walks that hashed thread label in the same namespace/
//! kind scope; its cost follows the events for that thread and is bounded by
//! the ordinary listing page cap. A changed-channel miss is not cached across
//! calls: the Events listing has no revision watermark to detect legacy
//! records written by another client (or removed by forget), so a later retry
//! must search again. A walk past the page cap fails instead of returning an
//! incomplete candidate set.
//!
//! With attribution on, an event observed from someone other than the owner
//! names them, and a write refused for it is sent once more without
//! (`attribution` module).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Instant;

use tinymemory_api::{ItemId, StoreItem, StoreReceipt, WaitFor, validate_many};

use super::CortexEngine;
use super::attribution;
use super::scopes::KindScope;
use crate::cortex::descriptor::CortexWire;
use crate::cortex::envelope::{Encoded, Envelope, decode_event, labels};
use crate::cortex::error::{Error, Result};
use crate::cortex::log::Written;

impl CortexEngine {
    /// `store_many`, paying per batch rather than per item:
    ///
    /// - one id lookup per scope (kind and namespace) finds what the batch
    ///   already holds;
    /// - every missing event is written, in item order, without waiting;
    /// - then one listing wait per scope, for the last event written there
    ///   (the scope's log is ordered, so it being listed implies the earlier
    ///   ones are), and ranked recall for the batch's final event only.
    ///
    /// An item repeated inside the batch is a replay of its first copy.
    ///
    /// With [`WaitFor::Accepted`] the writes ask for no indexing and the
    /// waits are skipped: the call returns once CortexDB captured every event.
    pub(super) async fn store_items(
        &self,
        items: Vec<StoreItem>,
        wait: WaitFor,
    ) -> Result<Vec<StoreReceipt>> {
        validate_many(&items)?;
        if let Some(item) = items
            .iter()
            .find(|item| self.layout.repeats_root(&item.meta().namespace))
        {
            return Err(Error::InvalidRequest(format!(
                "namespace `{}` repeats the scope root `{}`; nodes go below the root",
                item.meta().namespace,
                self.layout.root()
            )));
        }
        self.register_root().await;
        let subject = self.attribution_subject().await;
        let mut items = items;
        for item in &mut items {
            attribution::screen(item.meta_mut(), subject.is_some());
        }
        let ids: Vec<String> = items.iter().map(StoreItem::fingerprint).collect();
        // Every event of the batch is laid out and size-checked before any is
        // sent, so an item CortexDB would refuse leaves nothing half-written.
        let mut planned: Vec<Vec<(Option<u32>, Envelope, Encoded)>> =
            Vec::with_capacity(items.len());
        for (item, id) in items.iter().zip(&ids) {
            let mut events = Vec::new();
            for envelope in Envelope::for_item(item, id)? {
                let encoded = envelope.encode_checked()?;
                events.push((envelope.part(), envelope, encoded));
            }
            planned.push(events);
        }
        let mut held: HashMap<String, HashSet<Option<u32>>> = HashMap::new();
        let mut by_scope: BTreeMap<KindScope, Vec<String>> = BTreeMap::new();
        let mut legacy_ids_by_scope: BTreeMap<KindScope, Vec<(String, String)>> = BTreeMap::new();
        let mut conversation_scopes = Vec::with_capacity(items.len());
        for (item, id) in items.iter().zip(&ids) {
            let scope = KindScope::new(&self.layout, item.meta().namespace.clone(), item.kind());
            by_scope.entry(scope.clone()).or_default().push(id.clone());
            if let Some(legacy_id) = item.legacy_conversation_fingerprint() {
                legacy_ids_by_scope
                    .entry(scope.clone())
                    .or_default()
                    .push((id.clone(), legacy_id));
            }
            conversation_scopes.push(scope);
        }
        let lookup = looks_up(self.wire(), &items, wait);
        let mut aliases = HashMap::<String, String>::new();
        let looking = Instant::now();
        for (scope, of_scope) in by_scope.iter().filter(|_| lookup) {
            let mut lookup_ids = of_scope.clone();
            let mut seen_ids: HashSet<String> = lookup_ids.iter().cloned().collect();
            if let Some(legacy_ids) = legacy_ids_by_scope.get(scope) {
                for (_, legacy_id) in legacy_ids {
                    if seen_ids.insert(legacy_id.clone()) {
                        lookup_ids.push(legacy_id.clone());
                    }
                }
            }
            for (id, events) in self.item_events(scope, &lookup_ids).await? {
                held.entry(id)
                    .or_default()
                    .extend(events.iter().map(|decoded| decoded.envelope.part()));
            }
            if let Some(legacy_ids) = legacy_ids_by_scope.get(scope) {
                for (current_id, legacy_id) in legacy_ids {
                    if !held.contains_key(current_id) && held.contains_key(legacy_id) {
                        aliases
                            .entry(current_id.clone())
                            .or_insert_with(|| legacy_id.clone());
                    }
                }
            }
        }

        // A stored conversation may have a different old channel, so its old
        // fingerprint is unknowable from the new item alone. On an ID miss,
        // scan only the exact namespace/kind scope for the hashed thread id,
        // then compare turn content and the envelope's path-safe metadata.
        let mut by_thread: BTreeMap<KindScope, BTreeMap<String, BTreeMap<String, &StoreItem>>> =
            BTreeMap::new();
        for (index, (item, id)) in items.iter().zip(&ids).enumerate() {
            if held.contains_key(id) || aliases.contains_key(id) {
                continue;
            }
            let StoreItem::Conversation { meta, .. } = item else {
                continue;
            };
            let Some(thread_id) = meta.thread_id.as_deref().filter(|id| !id.trim().is_empty())
            else {
                continue;
            };
            by_thread
                .entry(conversation_scopes[index].clone())
                .or_default()
                .entry(thread_id.to_string())
                .or_default()
                .entry(id.clone())
                .or_insert(item);
        }
        for (scope, threads) in by_thread {
            let thread_labels: Vec<String> = threads
                .keys()
                .map(|thread| labels::thread(thread))
                .collect();
            let mut candidates: BTreeMap<String, Vec<Envelope>> = BTreeMap::new();
            for event in self.log.walk_labels(&scope.path, &thread_labels).await? {
                let Some(decoded) = decode_event(&event) else {
                    continue;
                };
                let Some(thread_id) = decoded.envelope.meta.thread_id.as_deref() else {
                    continue;
                };
                if threads.contains_key(thread_id) {
                    candidates
                        .entry(decoded.envelope.id.clone())
                        .or_default()
                        .push(decoded.envelope);
                }
            }
            for (legacy_id, events) in candidates {
                let mut matches = Vec::new();
                for items_for_thread in threads.values() {
                    for (current_id, item) in items_for_thread {
                        if let Some(parts) = legacy_conversation_parts(item, &events) {
                            matches.push((current_id.clone(), parts));
                        }
                    }
                    if matches.len() > 1 {
                        break;
                    }
                }
                if let [(current_id, parts)] = matches.as_slice()
                    && !held.contains_key(current_id)
                    && !aliases.contains_key(current_id)
                {
                    aliases.insert(current_id.clone(), legacy_id.clone());
                    held.insert(legacy_id, parts.clone());
                }
            }
        }

        let effective_ids: Vec<String> = ids
            .iter()
            .map(|id| aliases.get(id).unwrap_or(id).clone())
            .collect();
        for ((events, current_id), effective_id) in planned.iter_mut().zip(&ids).zip(&effective_ids)
        {
            if current_id == effective_id {
                continue;
            }
            for (_, envelope, encoded) in events {
                envelope.id.clone_from(effective_id);
                *encoded = envelope.encode_checked()?;
            }
        }
        let lookup_ms = looking.elapsed().as_millis();
        let looked_up = if lookup { by_scope.len() } else { 0 };
        let writing = Instant::now();
        let mut sent = 0;
        let mut receipts = Vec::with_capacity(items.len());
        let mut written_here: HashSet<String> = HashSet::new();
        let mut last_per_scope: Vec<Written> = Vec::new();
        for (events, id) in planned.into_iter().zip(effective_ids) {
            let present = held.get(&id);
            let mut requests = Vec::new();
            let mut attributed = false;
            if !written_here.contains(&id) {
                for (part, envelope, encoded) in events {
                    if present.is_some_and(|present| present.contains(&part)) {
                        continue;
                    }
                    let mut request = envelope.request(&encoded, &self.layout);
                    if let Some(subject) = &subject
                        && let Some(actor) = attribution::actor_of(&envelope)
                        && actor != *subject
                    {
                        attribution::attribute(&mut request, &actor, subject);
                        attributed = true;
                    }
                    requests.push(request);
                }
            }
            let mut replayed = requests.is_empty();
            sent += requests.len();
            if let Some(written) = self
                .write_attributed(&mut requests, attributed, wait)
                .await?
            {
                replayed = written.replayed;
                last_per_scope.retain(|w| w.scope != written.scope);
                if !written.replayed {
                    self.recency.touch(&written.scope);
                }
                last_per_scope.push(written);
            }
            written_here.insert(id.clone());
            receipts.push(StoreReceipt {
                id: ItemId::new(id),
                replayed,
            });
        }
        let writes_ms = writing.elapsed().as_millis();
        let stored = receipts.len();
        log::debug!(
            "[cortex] store of {stored} items: lookup {lookup_ms} ms over {looked_up} scopes, \
             {sent} events written in {writes_ms} ms ({wait:?})"
        );
        if wait == WaitFor::Accepted {
            return Ok(receipts);
        }
        let waiting = Instant::now();
        let final_index = last_per_scope.len().saturating_sub(1);
        for (index, written) in last_per_scope.iter().enumerate() {
            self.log
                .await_written(written, index == final_index)
                .await?;
        }
        let await_ms = waiting.elapsed().as_millis();
        let awaited = last_per_scope.len();
        log::debug!("[cortex] store of {stored} items: waited {await_ms} ms for {awaited} scopes");
        Ok(receipts)
    }
}

impl CortexEngine {
    /// The subject attributed events are about, when attributing: the v3
    /// root's owner, else the caller `whoami` reports. `None` (nothing is
    /// attributed) when attribution is off or refused, on the TinyHumans
    /// backend, or with no well-formed (`type:id`) owner known.
    async fn attribution_subject(&self) -> Option<String> {
        if !self.attribution.active() || self.wire() != CortexWire::Direct {
            return None;
        }
        let subject = match &self.owner {
            Some(owner) => owner.clone(),
            None => self.log.client.caller().await?,
        };
        attribution::actor_id(&subject)
    }

    /// Writes one item's events. An `attributed` write CortexDB refuses for
    /// its credential or its body is written once more without attribution;
    /// a credential refusal the plain write gets past turns attribution off.
    async fn write_attributed(
        &self,
        requests: &mut [serde_json::Value],
        attributed: bool,
        wait: WaitFor,
    ) -> Result<Option<Written>> {
        let first = self.log.write(requests, wait).await;
        match first {
            Err(error @ (Error::Unauthorized(_) | Error::InvalidRequest(_))) if attributed => {
                log::debug!("[cortex] attributed write refused, writing it plainly: {error}");
                requests.iter_mut().for_each(attribution::strip);
                let written = self.log.write(requests, wait).await?;
                if let Error::Unauthorized(reason) = &error {
                    self.attribution.refuse(reason);
                }
                Ok(written)
            }
            other => other,
        }
    }
}

/// Whether a store looks its items up before writing. The Direct accepted-only
/// hot path skips that lookup only for a single-turn conversation without a
/// channel or thread id; those fields are needed to recover a pre-change id.
/// The hosted wire always looks up because its answer need not say it replayed.
fn looks_up(wire: CortexWire, items: &[StoreItem], wait: WaitFor) -> bool {
    let hot = matches!(
        items,
        [StoreItem::Conversation { turns, .. }] if turns.len() == 1
    );
    let can_have_legacy_id = items.iter().any(|item| {
        matches!(
            item,
            StoreItem::Conversation { meta, .. }
                if meta.channel.is_some() || meta.thread_id.is_some()
        )
    });
    !(wire == CortexWire::Direct && wait == WaitFor::Accepted && hot && !can_have_legacy_id)
}

/// The parts of a pre-channel-neutral conversation that are already stored,
/// if every returned event belongs to this item's exact identity. The turn
/// count disambiguates a partial multi-turn item from a shorter conversation
/// sharing its first turn.
fn legacy_conversation_parts(
    item: &StoreItem,
    events: &[Envelope],
) -> Option<HashSet<Option<u32>>> {
    let StoreItem::Conversation { turns, meta } = item else {
        return None;
    };
    if turns.is_empty() || events.is_empty() {
        return None;
    }
    let mut parts = HashSet::new();
    for event in events {
        let info = event.turn.as_ref()?;
        if event.kind != tinymemory_api::ItemKind::Conversation
            || usize::try_from(info.count).ok()? != turns.len()
            || !event.matches_meta_identity(meta)
        {
            return None;
        }
        let index = usize::try_from(info.index).ok()?;
        let expected = turns.get(index)?;
        if event.text != expected.text
            || info.role != expected.role
            || info.at != expected.at
            || info.tool_calls != expected.tool_calls
            || !parts.insert(Some(info.index))
        {
            return None;
        }
    }
    Some(parts)
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "store_attribution_tests.rs"]
mod attribution_tests;
