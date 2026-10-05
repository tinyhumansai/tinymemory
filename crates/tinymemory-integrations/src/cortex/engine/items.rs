//! Helpers every operation shares: which kinds a filter admits, finding an
//! item's events by its label, and turning a rebuilt item into a `Hit`.

use std::collections::{BTreeMap, HashMap};

use tinymemory_api::explore::in_request_order;
use tinymemory_api::{GetRequest, Hit, ItemId, ItemKind, MetaFilter, Namespace, StoreItem};

use super::CortexEngine;
use super::scopes::KindScope;
use crate::cortex::envelope::{Decoded, Envelope, decode_event, labels, rebuild};
use crate::cortex::error::Result;

/// The kinds `filter` admits, in the fixed order
/// [`ItemKind::ALL`] lists them.
pub(super) fn admitted(filter: &MetaFilter) -> Vec<ItemKind> {
    ItemKind::ALL
        .into_iter()
        .filter(|kind| filter.admits_kind(*kind))
        .collect()
}

/// Whether a decoded event is an item of `kind` that `filter` keeps.
pub(super) fn keeps(filter: &MetaFilter, kind: ItemKind, envelope: &Envelope) -> bool {
    envelope.kind == kind && filter.matches(kind, &envelope.meta)
}

/// A hit for `item`.
pub(super) fn hit(id: &str, item: &StoreItem, score: f32) -> Hit {
    Hit {
        id: ItemId::new(id),
        kind: item.kind(),
        text: item.render_text(),
        meta: item.meta().clone(),
        score,
        confidence: item.confidence(),
    }
}

impl CortexEngine {
    /// Every event of each item in `ids` held in `scope`, grouped by item id.
    /// Found by the items' labels (one listing per batch of ids), then
    /// re-checked against the envelope, because a label is a digest.
    pub(super) async fn item_events(
        &self,
        scope: &KindScope,
        ids: &[String],
    ) -> Result<HashMap<String, Vec<Decoded>>> {
        let kind = scope.kind;
        let mut grouped: HashMap<String, Vec<Decoded>> = HashMap::new();
        if ids.is_empty() {
            return Ok(grouped);
        }
        let wanted: Vec<String> = ids.iter().map(|id| labels::item(id)).collect();
        for event in self.log.walk_labels(&scope.path, &wanted).await? {
            let Some(decoded) = decode_event(&event) else {
                continue;
            };
            if decoded.envelope.kind == kind && ids.contains(&decoded.envelope.id) {
                grouped
                    .entry(decoded.envelope.id.clone())
                    .or_default()
                    .push(decoded);
            }
        }
        Ok(grouped)
    }

    /// `get`: every named item, rebuilt from its events in each scope the
    /// request's reach reads (an id names one item, so one scope holds it).
    pub(super) async fn get_items(&self, req: GetRequest) -> Result<Vec<Hit>> {
        req.validate()?;
        let ids: Vec<String> = req.ids.iter().map(|id| id.as_str().to_string()).collect();
        let filter = MetaFilter {
            reach: req.reach.clone(),
            ..MetaFilter::default()
        };
        let mut found = BTreeMap::new();
        for scope in self.scopes_for(&filter).await? {
            if found.len() == ids.len() {
                break;
            }
            for (id, events) in self.item_events(&scope, &ids).await? {
                let envelopes: Vec<Envelope> = events.into_iter().map(|d| d.envelope).collect();
                if let Some(item) = rebuild(&envelopes) {
                    found.insert(ItemId::new(id.clone()), hit(&id, &item, 0.0));
                }
            }
        }
        Ok(in_request_order(&req.ids, found))
    }

    /// The whole conversations named by `ids`, each at its namespace,
    /// rebuilt from all their turns (one lookup per namespace).
    pub(super) async fn conversations(
        &self,
        ids: &[(String, Namespace)],
    ) -> Result<HashMap<String, StoreItem>> {
        let mut by_node: BTreeMap<&Namespace, Vec<String>> = BTreeMap::new();
        for (id, namespace) in ids {
            by_node.entry(namespace).or_default().push(id.clone());
        }
        let mut out = HashMap::new();
        for (namespace, ids) in by_node {
            let scope = KindScope::new(namespace.clone(), ItemKind::Conversation);
            for (id, events) in self.item_events(&scope, &ids).await? {
                let envelopes: Vec<Envelope> = events.into_iter().map(|d| d.envelope).collect();
                if let Some(item) = rebuild(&envelopes) {
                    out.insert(id, item);
                }
            }
        }
        Ok(out)
    }
}
