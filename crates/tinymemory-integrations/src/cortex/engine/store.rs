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
//! - some turns of a conversation present — a previous store failed part
//!   way, and only the missing turns are written, in order;
//! - nothing present — every event is written.
//!
//! Writes use fresh idempotency keys (see `transport::fresh_idempotency_key`)
//! rather than ones derived from content: CortexDB never releases a key on
//! forget, so a content key would make re-storing a forgotten item a silent
//! no-op.

use std::collections::{BTreeMap, HashMap, HashSet};

use tinymemory_api::{ItemId, StoreItem, StoreReceipt, WaitFor, validate_many};

use super::CortexEngine;
use super::scopes::KindScope;
use crate::cortex::envelope::Envelope;
use crate::cortex::error::Result;
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
        let ids: Vec<String> = items.iter().map(StoreItem::fingerprint).collect();
        let mut held: HashMap<String, HashSet<Option<u32>>> = HashMap::new();
        let mut by_scope: BTreeMap<KindScope, Vec<String>> = BTreeMap::new();
        for (item, id) in items.iter().zip(&ids) {
            by_scope
                .entry(KindScope::new(
                    &self.root,
                    item.meta().namespace.clone(),
                    item.kind(),
                ))
                .or_default()
                .push(id.clone());
        }
        for (scope, of_scope) in &by_scope {
            for (id, events) in self.item_events(scope, of_scope).await? {
                held.entry(id).or_default().extend(
                    events
                        .iter()
                        .map(|decoded| decoded.envelope.turn.as_ref().map(|turn| turn.index)),
                );
            }
        }
        let mut receipts = Vec::with_capacity(items.len());
        let mut written_here: HashSet<String> = HashSet::new();
        let mut last_per_scope: Vec<Written> = Vec::new();
        for (item, id) in items.iter().zip(ids) {
            let present = held.get(&id);
            let mut requests = Vec::new();
            if !written_here.contains(&id) {
                for envelope in Envelope::for_item(item, &id)? {
                    let turn = envelope.turn.as_ref().map(|turn| turn.index);
                    if present.is_some_and(|present| present.contains(&turn)) {
                        continue;
                    }
                    requests.push(envelope.request(&self.root, &envelope.encode()?));
                }
            }
            let replayed = requests.is_empty();
            if let Some(written) = self.log.write(&requests, wait).await? {
                last_per_scope.retain(|w| w.scope != written.scope);
                last_per_scope.push(written);
            }
            written_here.insert(id.clone());
            receipts.push(StoreReceipt {
                id: ItemId::new(id),
                replayed,
            });
        }
        if wait == WaitFor::Accepted {
            return Ok(receipts);
        }
        let final_index = last_per_scope.len().saturating_sub(1);
        for (index, written) in last_per_scope.iter().enumerate() {
            self.log
                .await_written(written, index == final_index)
                .await?;
        }
        Ok(receipts)
    }
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
