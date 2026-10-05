//! Beliefs: decoding CortexDB's belief layer, and reading it from a scope's
//! recall pack or listing.

use super::*;
use crate::cortex::testing::{both, direct_double, direct_engine};
use tinymemory_api::{MemoryEngine, Namespace, Reach};

fn belief(scope: &str, subject: &str, predicate: &str, object: &str, stance: &str) -> Value {
    json!({
        "id": format!("belief_{object}"),
        "scope": scope,
        "claim": {
            "subject": { "type": "entity", "id": "ent_x", "name": subject },
            "predicate": predicate,
            "object": { "type": "literal", "datatype": "string", "value": object },
        },
        "stance": stance,
        "confidence": 0.93,
        "valid_from": "2026-09-01T09:00:00Z",
    })
}

const SCOPE: &str = "app:tinymemory/agent:coder-42/app:conversations";

#[test]
fn a_belief_reads_as_a_tagged_learning_sentence_at_its_node() {
    let hit = belief_hit(
        &belief(SCOPE, "user", "prefers_using", "pnpm over npm", "supported"),
        0,
    )
    .unwrap();
    assert_eq!(hit.kind, ItemKind::Learning);
    assert_eq!(hit.text, "user prefers using pnpm over npm");
    assert_eq!(hit.meta.namespace, Namespace::agent("coder-42"));
    assert_eq!(hit.meta.tags, [BELIEF_TAG]);
    assert_eq!(hit.confidence, Some(0.93));
    assert!(hit.meta.observed_at.is_some());
    assert_eq!(hit.id.as_str(), "belief_pnpm over npm");
}

#[test]
fn a_contested_belief_says_so_and_a_retired_one_is_left_out() {
    let contested = belief_hit(&belief(SCOPE, "Acme", "has_plan", "Team", "contested"), 0);
    assert_eq!(contested.unwrap().text, "Acme has plan Team (contested)");
    assert!(
        belief_hit(
            &belief(SCOPE, "backup", "succeeded", "true", "deprecated"),
            0
        )
        .is_none()
    );
    assert!(belief_hit(&belief("org:elsewhere/x", "a", "b", "c", "supported"), 0).is_none());
    assert!(belief_hit(&json!({ "id": "b", "scope": SCOPE }), 0).is_none());
}

#[test]
fn merging_keeps_each_sentence_once_rank_by_rank() {
    let a = belief_hit(&belief(SCOPE, "user", "likes", "tea", "supported"), 0).unwrap();
    let b = belief_hit(&belief(SCOPE, "user", "likes", "coffee", "supported"), 1).unwrap();
    let merged = merge(vec![vec![a.clone(), b.clone()], vec![a.clone()]], 10);
    assert_eq!(merged, [a.clone(), b]);
    assert_eq!(merge(vec![vec![a.clone()]], 0), Vec::<Hit>::new());
}

#[tokio::test]
async fn a_built_scope_s_beliefs_are_read_with_and_without_a_query() {
    let (endpoint, _state) = direct_double().await;
    let engine = direct_engine(&endpoint);
    let node = Namespace::agent("coder-42");
    engine
        .store(StoreItem::document(
            "In this repo always use pnpm.",
            MemoryMeta {
                namespace: node.clone(),
                ..MemoryMeta::default()
            },
        ))
        .await
        .unwrap();
    let none = engine
        .beliefs(BeliefsRequest::new(Reach::subtree(Namespace::ROOT), 5))
        .await
        .unwrap();
    assert!(none.is_empty(), "nothing is built yet: {none:?}");

    engine
        .consolidate(tinymemory_api::ConsolidateRequest::new(Reach::exact(
            node.clone(),
        )))
        .await
        .unwrap();
    let ranked = engine
        .beliefs(BeliefsRequest::new(Reach::subtree(Namespace::ROOT), 5).query("pnpm"))
        .await
        .unwrap();
    assert_eq!(ranked.len(), 1, "{ranked:?}");
    assert_eq!(ranked[0].text, "user said In this repo always use pnpm.");
    assert_eq!(ranked[0].meta.namespace, node);
    let listed = engine
        .beliefs(BeliefsRequest::new(Reach::exact(node), 5))
        .await
        .unwrap();
    let texts = |hits: &[Hit]| hits.iter().map(|h| h.text.clone()).collect::<Vec<_>>();
    assert_eq!(
        texts(&listed),
        texts(&ranked),
        "the listing holds the same belief"
    );
    let elsewhere = engine
        .beliefs(BeliefsRequest::new(Reach::exact(Namespace::agent("other")), 5).query("pnpm"))
        .await
        .unwrap();
    assert!(
        elsewhere.is_empty(),
        "a sibling's beliefs stay out of reach"
    );
}

#[tokio::test]
async fn the_hosted_wire_reads_beliefs_only_for_a_query() {
    for (engine, state) in both().await {
        let before = state.requests().len();
        let listed = engine
            .beliefs(BeliefsRequest::new(Reach::subtree(Namespace::ROOT), 5))
            .await
            .unwrap();
        assert!(listed.is_empty());
        if engine.wire() == CortexWire::TinyHumans {
            assert_eq!(state.requests().len(), before, "no listing is sent");
        }
    }
}

#[tokio::test]
async fn a_malformed_request_is_refused() {
    let (endpoint, state) = direct_double().await;
    let error = direct_engine(&endpoint)
        .beliefs(BeliefsRequest::new(Reach::default(), 0))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");
    assert!(state.requests().is_empty());
}

#[tokio::test]
async fn a_fetch_reads_beliefs_from_its_own_recall_packs() {
    let (endpoint, state) = direct_double().await;
    let engine = direct_engine(&endpoint);
    let node = Namespace::agent("coder-42");
    engine
        .store(StoreItem::document(
            "In this repo always use pnpm.",
            MemoryMeta {
                namespace: node.clone(),
                ..MemoryMeta::default()
            },
        ))
        .await
        .unwrap();
    engine
        .consolidate(tinymemory_api::ConsolidateRequest::new(Reach::exact(
            node.clone(),
        )))
        .await
        .unwrap();
    let before = state.seen.lock().unwrap().recalls.len();
    let mut request =
        tinymemory_api::FetchRequest::new("pnpm", tinymemory_api::FetchMode::Hybrid, 5);
    request.filter.reach = Some(Reach::exact(node.clone()));
    request.filter.kinds = vec![ItemKind::Document];
    request.beliefs = 3;
    let page = engine.fetch(request).await.unwrap();
    assert_eq!(page.hits.len(), 1, "the document itself");
    assert_eq!(page.beliefs.len(), 1, "{:?}", page.beliefs);
    assert_eq!(
        page.beliefs[0].text,
        "user said In this repo always use pnpm."
    );
    let recalls = state.seen.lock().unwrap().recalls[before..].to_vec();
    assert_eq!(
        recalls.len(),
        1,
        "one pack for the one scope, beliefs included"
    );
    assert_eq!(recalls[0]["budgets"]["per_layer_limits"]["beliefs"], 3);

    let mut plain = tinymemory_api::FetchRequest::new("pnpm", tinymemory_api::FetchMode::Hybrid, 5);
    plain.filter.reach = Some(Reach::exact(node));
    let page = engine.fetch(plain).await.unwrap();
    assert!(page.beliefs.is_empty(), "no beliefs unless asked");
    let last = state.seen.lock().unwrap().recalls.last().cloned().unwrap();
    assert!(last["budgets"]["per_layer_limits"].get("beliefs").is_none());
}
