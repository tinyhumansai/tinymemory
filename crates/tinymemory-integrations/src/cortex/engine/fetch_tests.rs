//! Tests for ranking a recall pack and interleaving kinds.

use super::*;
use tinymemory_api::{MemoryMeta, SourceKind, StoreItem};

fn event(id: &str, item: &StoreItem) -> Value {
    let fingerprint = item.fingerprint();
    let envelope = &Envelope::for_item(item, &fingerprint).unwrap()[0];
    json!({ "id": id, "content": { "role": "user", "text": format!("[user] {}", envelope.encode().unwrap()) } })
}

fn doc(text: &str, repo: Option<&str>) -> StoreItem {
    let mut meta = MemoryMeta::from_source(SourceKind::Github, None);
    meta.repo = repo.map(str::to_owned);
    StoreItem::document(text, meta)
}

#[test]
fn a_pack_ranks_each_item_once_at_its_best_position_and_filters() {
    let a = doc("alpha", Some("o/a"));
    let b = doc("beta", Some("o/b"));
    let pack = json!({ "layers": { "events": [
        event("e1", &a),
        { "id": "e0", "content": { "text": "somebody else's event" } },
        event("e2", &b),
        event("e3", &a),
    ] } });
    let all = ranked(
        &pack,
        Some(ItemKind::Document),
        &MetaFilter::default(),
        None,
    );
    let ids: Vec<_> = all.iter().map(|e| e.id.clone()).collect();
    assert_eq!(ids, vec![a.fingerprint(), b.fingerprint()]);

    let only_b = MetaFilter {
        repo: Some("o/b".into()),
        ..MetaFilter::default()
    };
    let filtered = ranked(&pack, Some(ItemKind::Document), &only_b, None);
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].text, "beta");
    assert!(
        ranked(
            &pack,
            Some(ItemKind::Learning),
            &MetaFilter::default(),
            None
        )
        .is_empty()
    );
}

#[test]
fn kinds_interleave_rank_by_rank() {
    let envelope = |text: &str| {
        Envelope::for_item(&doc(text, None), text)
            .unwrap()
            .remove(0)
    };
    let merged = interleave(vec![
        vec![envelope("d1"), envelope("d2"), envelope("d3")],
        vec![envelope("l1")],
    ]);
    let order: Vec<_> = merged.iter().map(|e| e.text.as_str()).collect();
    assert_eq!(order, vec!["d1", "l1", "d2", "d3"]);
}

#[test]
fn a_labelled_filter_narrows_the_recall_body() {
    let filter = MetaFilter {
        thread_id: Some("t".into()),
        ..MetaFilter::default()
    };
    let body = recall_body("app:tinymemory/app:documents", "q", 9, &filter);
    assert_eq!(body["budgets"]["per_layer_limits"]["events"], 9);
    assert_eq!(
        body["filters"]["metadata"]["labels"][0],
        json!(format!("tm:t:{}", labels::digest("t")))
    );
    assert!(
        recall_body("s", "q", 1, &MetaFilter::default())
            .get("filters")
            .is_none()
    );
}

#[test]
fn a_recall_body_reads_one_scope_exactly_and_funds_events_first() {
    assert_eq!(
        recall_body(
            "app:tinymemory/app:documents",
            "refunds",
            6,
            &MetaFilter::default()
        ),
        json!({
            "scope": "app:tinymemory/app:documents",
            "query": "refunds",
            "view": "granular",
            "include": ["events"],
            "budgets": {
                "max_tokens": 6 * MAX_EVENT_TEXT_BYTES,
                "per_layer_limits": { "events": 6 },
            },
        }),
        "exact scope, events funded first, room for each event whole, no temporal"
    );
}

#[test]
fn a_pack_budget_fits_each_event_whole_up_to_a_ceiling() {
    assert_eq!(whole_items_budget(0), MAX_EVENT_TEXT_BYTES, "never zero");
    assert_eq!(whole_items_budget(6), 6 * MAX_EVENT_TEXT_BYTES);
    assert_eq!(whole_items_budget(MAX_PACK_EVENTS), MAX_PACK_TOKENS);
    assert_eq!(whole_items_budget(usize::MAX), MAX_PACK_TOKENS);
}

#[test]
fn an_item_ranked_in_two_scopes_is_one_hit_at_its_best_rank() {
    // The same item below the root and below a retired root.
    let envelope =
        |text: &str, id: &str| Envelope::for_item(&doc(text, None), id).unwrap().remove(0);
    let merged = interleave(vec![
        vec![envelope("a", "same"), envelope("b", "b")],
        vec![envelope("c", "c"), envelope("a again", "same")],
    ]);
    let ids: Vec<_> = merged.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, vec!["same", "c", "b"]);
}
