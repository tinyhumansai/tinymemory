//! Tests for rendering recall packs as evidence.

use super::*;
use crate::cortex::testing::{direct_double, direct_engine, sample_items};
use serde_json::json;
use tinymemory_api::{MemoryEngine, MemoryMeta, Reach, StoreItem, Turn, TurnRange};

fn turn(thread: &str, role: Role, text: &str) -> StoreItem {
    let mut meta = MemoryMeta {
        thread_id: Some(thread.to_string()),
        turns: Some(TurnRange { first: 0, last: 0 }),
        ..MemoryMeta::default()
    };
    meta.observed_at = Some("2023-06-27T10:37:00Z".parse().unwrap());
    let mut said = Turn::new(role, text);
    said.at = meta.observed_at;
    StoreItem::Conversation {
        turns: vec![said],
        meta,
    }
}

fn event(id: &str, item: &StoreItem) -> Value {
    let fingerprint = item.fingerprint();
    let envelope = &Envelope::for_item(item, &fingerprint).unwrap()[0];
    json!({ "id": id, "content": { "role": "user", "text": envelope.encode().unwrap() } })
}

fn fact(id: &str, supports: &[&str], value: &str) -> Value {
    json!({
        "id": id,
        "subject": { "type": "entity", "id": format!("ent_fact_subject_{id}") },
        "predicate": "states",
        "object": { "type": "literal", "datatype": "string", "value": value },
        "supports": supports,
        "valid_from": "2023-06-27T10:37:00Z",
    })
}

fn belief(supports: &[&str], name: &str, predicate: &str, value: &str) -> Value {
    let supports: Vec<Value> = supports
        .iter()
        .map(|id| json!({ "type": "fact", "id": id }))
        .collect();
    json!({
        "claim": {
            "subject": { "type": "entity", "id": format!("ent_{name}"), "name": name },
            "predicate": predicate,
            "object": { "type": "literal", "value": value },
        },
        "supports": supports,
    })
}

fn pack() -> Value {
    json!({ "layers": {
        "events": [
            event("e_live", &turn("live", Role::User, "what did grandma give me")),
            event("e_gift", &turn("past", Role::User, "my grandma gave me a necklace")),
            { "id": "e_other", "content": { "text": "somebody else's event" } },
        ],
        "facts": [
            fact("f_live", &["e_live"], "the user asked about a gift"),
            fact("f_gift", &["e_gift"], "grandma gave the user a necklace"),
        ],
        "beliefs": [
            belief(&["f_live"], "user", "asked_about", "a gift"),
            belief(&["f_gift"], "Caroline", "owns", "a necklace"),
        ],
    } })
}

#[test]
fn an_excluded_turn_and_what_was_derived_from_it_are_left_out() {
    let gathered = gather(
        &pack(),
        &MetaFilter::default(),
        Some(&ThreadWindow::whole("live")),
    );
    let texts: Vec<&str> = gathered.events.iter().map(|e| e.text.as_str()).collect();
    assert_eq!(texts, vec!["my grandma gave me a necklace"]);
    assert_eq!(
        gathered.facts,
        vec!["[2023-06-27] grandma gave the user a necklace"]
    );
    assert_eq!(gathered.beliefs, vec!["Caroline owns a necklace"]);
}

#[test]
fn without_a_window_every_admitted_record_is_kept() {
    let gathered = gather(&pack(), &MetaFilter::default(), None);
    assert_eq!(gathered.events.len(), 2, "the foreign event never decodes");
    assert_eq!(gathered.facts.len(), 2);
    assert_eq!(
        gathered.beliefs,
        vec!["user asked about a gift", "Caroline owns a necklace"]
    );
}

#[test]
fn a_metadata_filter_keeps_derived_records_out() {
    let scoped = MetaFilter {
        reach: Some(Reach::subtree(tinymemory_api::Namespace::ROOT)),
        kinds: vec![ItemKind::Conversation],
        ..MetaFilter::default()
    };
    assert!(
        !gather(&pack(), &scoped, None).facts.is_empty(),
        "reach and kinds alone keep them"
    );
    let narrowed = MetaFilter {
        thread_id: Some("past".into()),
        ..MetaFilter::default()
    };
    let gathered = gather(&pack(), &narrowed, None);
    assert_eq!(gathered.events.len(), 1);
    assert!(gathered.facts.is_empty() && gathered.beliefs.is_empty());
}

#[test]
fn the_evidence_lists_facts_beliefs_then_dated_turns() {
    let gathered = gather(
        &pack(),
        &MetaFilter::default(),
        Some(&ThreadWindow::whole("live")),
    );
    let text = render(&gathered.facts, &gathered.beliefs, &gathered.events);
    assert_eq!(
        text,
        "## Facts\n- [2023-06-27] grandma gave the user a necklace\n\n\
         ## Beliefs\n- Caroline owns a necklace\n\n\
         ## Conversation\n[2023-06-27 10:37 UTC] [user] my grandma gave me a necklace"
    );
    assert!(!text.contains("what did grandma give me"));
}

#[test]
fn repeated_lines_render_once_and_nothing_renders_empty() {
    let facts = vec!["A".to_string(), "a".to_string()];
    assert_eq!(render(&facts, &[], &[]), "## Facts\n- A");
    assert_eq!(render(&[], &[], &[]), "");
}

#[test]
fn turns_are_separated_and_attributed_by_role() {
    let events: Vec<Envelope> = [
        turn("t", Role::User, "hello"),
        turn("t", Role::Assistant, "hi there"),
    ]
    .iter()
    .map(|item| {
        Envelope::for_item(item, &item.fingerprint())
            .unwrap()
            .remove(0)
    })
    .collect();
    assert_eq!(
        render(&[], &[], &events),
        "## Conversation\n[2023-06-27 10:37 UTC] [user] hello\n\n---\n\n\
         [2023-06-27 10:37 UTC] [assistant] hi there"
    );
}

#[test]
fn a_learning_made_in_an_excluded_thread_stays_in_the_evidence() {
    let learned = StoreItem::learning(
        "the user prefers green tea",
        tinymemory_api::LearningKind::Preference,
        0.9,
        MemoryMeta {
            thread_id: Some("live".into()),
            ..MemoryMeta::default()
        },
    );
    let pack = json!({ "layers": { "events": [
        event("e_live", &turn("live", Role::User, "what tea do I like")),
        event("e_learned", &learned),
    ] } });
    let gathered = gather(
        &pack,
        &MetaFilter::default(),
        Some(&ThreadWindow::whole("live")),
    );
    let texts: Vec<&str> = gathered.events.iter().map(|e| e.text.as_str()).collect();
    assert_eq!(texts, vec!["the user prefers green tea"]);
}

/// A Direct engine holding [`sample_items`], with a short evidence
/// deadline, and recalls over `slow` scopes held past it.
async fn engine_with_slow(slow: &'static str) -> CortexEngine {
    let (endpoint, state) = direct_double().await;
    let mut engine = direct_engine(&endpoint);
    engine.log.timing.evidence = std::time::Duration::from_millis(300);
    for item in sample_items() {
        engine.store(item).await.unwrap();
    }
    *state.slow_recall.lock().unwrap() = Some((slow, 3_000));
    engine
}

fn evidence_request() -> RecallRequest {
    let mut req = RecallRequest::new("helix", 5);
    req.mode = tinymemory_api::RecallMode::Evidence;
    req
}

#[tokio::test]
async fn a_scope_still_building_at_the_deadline_is_left_out() {
    let engine = engine_with_slow("app:learnings").await;
    let started = std::time::Instant::now();
    let got = engine.recall(evidence_request()).await.unwrap();
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "the recall waited for the slow pack"
    );
    assert!(got.answer.contains("## Conversation"), "{}", got.answer);
    assert!(!got.citations.is_empty());
    assert!(!got.answer.contains("prefers helix"), "{}", got.answer);
    assert!(
        got.answer.contains("only partly searched: 1 of"),
        "{}",
        got.answer
    );
    assert!(got.citations.iter().all(|c| c.kind != ItemKind::Learning));
}

#[tokio::test]
async fn a_late_scope_beside_empty_ones_is_unavailable_not_nothing_stored() {
    let engine = engine_with_slow("app:conversations").await;
    let mut req = evidence_request();
    req.question = "editor".into();
    let error = engine.recall(req).await.unwrap_err();
    assert!(
        matches!(&error, Error::Unavailable(m) if m.contains("not fully searched")),
        "{error:?}"
    );
}

#[tokio::test]
async fn a_recall_with_every_pack_in_time_carries_no_note() {
    let engine = engine_with_slow("app:no-such-scope").await;
    let got = engine.recall(evidence_request()).await.unwrap();
    assert!(!got.answer.contains("partly searched"), "{}", got.answer);
}

#[tokio::test]
async fn no_pack_by_the_deadline_is_unavailable_not_empty() {
    let engine = engine_with_slow("app:").await;
    let error = engine.recall(evidence_request()).await.unwrap_err();
    assert!(matches!(error, Error::Unavailable(_)), "{error:?}");
}

#[tokio::test]
async fn a_broad_channel_pack_is_kept_when_its_supplement_misses_the_deadline() {
    let (endpoint, state) = direct_double().await;
    let mut engine = direct_engine(&endpoint);
    engine.log.timing.evidence = std::time::Duration::from_millis(300);
    let item = StoreItem::document(
        "helix: the broad pack still has this channel item",
        MemoryMeta {
            channel: Some("support".into()),
            ..MemoryMeta::default()
        },
    );
    let item_id = item.fingerprint();
    engine.store(item).await.unwrap();
    *state.slow_channel_recall.lock().unwrap() = Some(3_000);

    let mut req = evidence_request();
    req.filter = MetaFilter {
        channel: Some("support".into()),
        ..MetaFilter::kinds([ItemKind::Document])
    };
    let got = engine.recall(req).await.unwrap();
    assert_eq!(
        got.citations
            .iter()
            .map(|citation| citation.id.as_str())
            .collect::<Vec<_>>(),
        vec![item_id.as_str()],
    );
    assert!(
        got.answer.contains("only partly searched"),
        "{}",
        got.answer
    );
}
