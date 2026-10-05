//! Tests for the v2 envelope: layout, round trip, and foreign events.

use super::*;
use crate::cortex::tenancy::CortexTenancy;
use tinymemory_api::{SourceKind, Turn};

fn single_user() -> ScopeRoot {
    ScopeRoot::direct(&CortexTenancy::SingleUser)
}

fn meta() -> MemoryMeta {
    let mut meta = MemoryMeta::from_source(SourceKind::Folder, Some("notes".into()));
    meta.file_path = Some("/notes/a.md".into());
    meta
}

fn conversation() -> StoreItem {
    StoreItem::Conversation {
        turns: vec![
            Turn::new(Role::User, "hello"),
            Turn {
                role: Role::Assistant,
                text: "hi there".into(),
                at: None,
                tool_calls: vec![ToolCallRef {
                    name: "search".into(),
                    id: Some("c1".into()),
                }],
            },
        ],
        meta: meta(),
    }
}

#[test]
fn every_kind_round_trips_through_its_events() {
    let items = [
        StoreItem::Document {
            title: Some("Title".into()),
            body: DocumentBody::Text("body".into()),
            mime: Some("text/markdown".into()),
            meta: meta(),
        },
        StoreItem::Learning {
            text: "prefers tabs".into(),
            kind: LearningKind::Preference,
            confidence: 0.7,
            evidence: Some("said so".into()),
            meta: meta(),
        },
        conversation(),
    ];
    for item in items {
        let id = item.fingerprint();
        let envelopes = Envelope::for_item(&item, &id).unwrap();
        let decoded: Vec<Envelope> = envelopes
            .iter()
            .map(|e| Envelope::decode(&e.encode().unwrap()).unwrap())
            .collect();
        let rebuilt = rebuild(&decoded).unwrap();
        assert_eq!(rebuilt, item);
        assert_eq!(
            rebuilt.fingerprint(),
            id,
            "identity survives the round trip"
        );
    }
}

#[test]
fn a_conversation_is_one_event_per_turn_in_order() {
    let item = conversation();
    let envelopes = Envelope::for_item(&item, "id").unwrap();
    assert_eq!(envelopes.len(), 2);
    let turns: Vec<_> = envelopes
        .iter()
        .map(|e| e.turn.as_ref().map(|t| (t.index, t.count)))
        .collect();
    assert_eq!(turns, vec![Some((0, 2)), Some((1, 2))]);
    let request = envelopes[1].request(&single_user(), "x");
    assert_eq!(request["content"]["role"], "assistant");
    assert_eq!(request["scope"], "app:tinymemory/app:conversations");
    assert_eq!(request["modality"], "conversation");
}

#[test]
fn a_recall_rendering_is_read_as_well_as_the_stored_text() {
    let envelope = &Envelope::for_item(&conversation(), "id").unwrap()[0];
    let stored = envelope.encode().unwrap();
    assert_eq!(
        Envelope::decode(&format!("[user] {stored}")).as_ref(),
        Some(envelope)
    );
    assert_eq!(Envelope::decode(&stored).as_ref(), Some(envelope));
}

#[test]
fn events_this_crate_did_not_write_are_ignored() {
    assert!(Envelope::decode("just a sentence").is_none());
    assert!(Envelope::decode(r#"{"k":"v1-key","c":"v1 content"}"#).is_none());
    let mut old = Envelope::for_item(&conversation(), "id").unwrap().remove(0);
    old.v = 1;
    assert!(Envelope::decode(&old.encode().unwrap()).is_none());
    assert!(decode_event(&json!({ "id": "e", "content": { "text": "plain" } })).is_none());
}

#[test]
fn rebuilding_drops_repeated_turns_and_orders_by_index() {
    let envelopes = Envelope::for_item(&conversation(), "id").unwrap();
    let shuffled = vec![
        envelopes[1].clone(),
        envelopes[0].clone(),
        envelopes[1].clone(),
    ];
    assert_eq!(rebuild(&shuffled), Some(conversation()));
}

#[test]
fn observed_at_and_labels_reach_the_event_context() {
    let mut meta = meta();
    meta.observed_at = Some("2026-01-02T03:04:05Z".parse().unwrap());
    let item = StoreItem::document("text", meta);
    let envelope = &Envelope::for_item(&item, "id").unwrap()[0];
    let request = envelope.request(&single_user(), "payload");
    assert_eq!(
        request["context"]["observed_at"],
        "2026-01-02T03:04:05+00:00"
    );
    assert_eq!(request["context"]["labels"][0], labels::item("id"));
    assert_eq!(request["scope"], "app:tinymemory/app:documents");
    assert_ne!(
        request["idempotency_key"],
        envelope.request(&single_user(), "payload")["idempotency_key"],
        "every write mints a fresh key"
    );
}

#[test]
fn scopes_nest_kinds_under_their_namespace_node() {
    let root = ScopeRoot::direct(&CortexTenancy::SingleUser);
    let writer: Namespace = "team:acme/agent:writer".parse().unwrap();
    assert_eq!(
        scope_path(&root, &Namespace::ROOT, ItemKind::Learning),
        "app:tinymemory/app:learnings",
        "the root keeps the original layout"
    );
    let path = scope_path(&root, &writer, ItemKind::Conversation);
    assert_eq!(
        path,
        "app:tinymemory/team:acme/agent:writer/app:conversations"
    );
    assert_eq!(
        parse_scope(&root, &path),
        Some((writer.clone(), ItemKind::Conversation))
    );
    assert_eq!(
        parse_scope(&root, &format!("org:t1/{path}")),
        None,
        "a direct engine reads only scopes under its own root"
    );
    assert_eq!(
        parse_scope(&ScopeRoot::hosted(), &format!("org:t1/{path}")),
        Some((writer, ItemKind::Conversation)),
        "the hosted backend's tenant prefix is skipped"
    );
    assert_eq!(
        parse_scope(&root, "app:tinymemory/app:documents"),
        Some((Namespace::ROOT, ItemKind::Document))
    );
    for other in [
        "app:other/app:documents",
        "app:tinymemory",
        "app:tinymemory/agent:x",
        "app:tinymemory/robot:x/app:documents",
    ] {
        assert_eq!(parse_scope(&root, other), None, "{other}");
    }
}

#[test]
fn a_pinned_engine_writes_and_reads_only_under_its_pin() {
    let alice = ScopeRoot::direct(&CortexTenancy::pinned("org:acme/user:alice").unwrap());
    let bob = ScopeRoot::direct(&CortexTenancy::pinned("org:acme/user:bob").unwrap());
    let writer: Namespace = "agent:writer".parse().unwrap();
    let path = scope_path(&alice, &writer, ItemKind::Learning);
    assert_eq!(
        path,
        "org:acme/user:alice/app:tinymemory/agent:writer/app:learnings"
    );
    assert_eq!(
        parse_scope(&alice, &path),
        Some((writer, ItemKind::Learning))
    );
    assert_eq!(parse_scope(&bob, &path), None, "bob never reads alice");
}

#[test]
fn a_namespace_naming_another_tenant_lands_inside_the_callers_own() {
    let alice = ScopeRoot::direct(&CortexTenancy::pinned("org:acme/user:alice").unwrap());
    // A namespace that names another user is only a node in alice's tree.
    let bob: Namespace = "user:bob".parse().unwrap();
    assert_eq!(
        scope_path(&alice, &bob, ItemKind::Document),
        "org:acme/user:alice/app:tinymemory/user:bob/app:documents"
    );
    // Nothing else that could climb out is a namespace at all.
    for escape in [
        "..",
        "user:..",
        "user:../user:bob",
        "/org:acme/user:bob",
        "org:acme",
        "org:acme/user:bob",
        "oc:u-bob",
        "oc:u-bob/user:bob",
        "app:tinymemory",
        "user:bob/../..",
        "user:bob%2F..",
    ] {
        assert!(escape.parse::<Namespace>().is_err(), "{escape}");
    }
}

#[test]
fn a_brain_source_is_a_cortex_source_scope() {
    let root = ScopeRoot::direct(&CortexTenancy::SingleUser);
    let pdf: Namespace = "team:acme/source:pdf".parse().unwrap();
    let path = scope_path(&root, &pdf, ItemKind::Document);
    assert_eq!(path, "app:tinymemory/team:acme/source:pdf/app:documents");
    assert_eq!(parse_scope(&root, &path), Some((pdf, ItemKind::Document)));
}

#[test]
fn an_item_is_written_to_its_namespace_scope() {
    let meta = MemoryMeta {
        namespace: Namespace::agent("researcher"),
        ..MemoryMeta::default()
    };
    let item = StoreItem::document("notes", meta);
    let id = item.fingerprint();
    let envelope = Envelope::for_item(&item, &id).unwrap().remove(0);
    let request = envelope.request(&single_user(), &envelope.encode().unwrap());
    assert_eq!(
        request["scope"],
        "app:tinymemory/agent:researcher/app:documents"
    );
}
