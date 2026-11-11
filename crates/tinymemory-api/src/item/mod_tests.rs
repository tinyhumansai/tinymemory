//! Item validation, rendering, fingerprints and serde shape.

use super::*;
use crate::meta::{SourceKind, ToolCallRef};

fn conversation(texts: &[&str]) -> StoreItem {
    StoreItem::Conversation {
        turns: texts
            .iter()
            .enumerate()
            .map(|(i, t)| {
                Turn::new(
                    if i % 2 == 0 {
                        Role::User
                    } else {
                        Role::Assistant
                    },
                    *t,
                )
            })
            .collect(),
        meta: MemoryMeta::default(),
    }
}

#[test]
fn kinds_follow_the_variant() {
    assert_eq!(
        StoreItem::document("x", MemoryMeta::default()).kind(),
        ItemKind::Document
    );
    assert_eq!(conversation(&["hi"]).kind(), ItemKind::Conversation);
    assert_eq!(
        StoreItem::learning("x", LearningKind::Fact, 0.5, MemoryMeta::default()).kind(),
        ItemKind::Learning
    );
}

#[test]
fn invalid_items_are_refused() {
    let refused = [
        StoreItem::document("  ", MemoryMeta::default()),
        StoreItem::Document {
            title: None,
            body: DocumentBody::Uri("file:///x".into()),
            mime: None,
            meta: MemoryMeta::default(),
        },
        conversation(&[]),
        conversation(&["hi", " "]),
        StoreItem::learning("", LearningKind::Fact, 0.5, MemoryMeta::default()),
        StoreItem::learning("x", LearningKind::Fact, 1.5, MemoryMeta::default()),
        StoreItem::learning("x", LearningKind::Fact, f32::NAN, MemoryMeta::default()),
    ];
    for item in refused {
        assert!(
            matches!(item.validate(), Err(Error::InvalidRequest(_))),
            "{item:?}"
        );
    }
    assert!(conversation(&["hi", "hello"]).validate().is_ok());
}

#[test]
fn rendering_reads_like_the_item() {
    let doc = StoreItem::Document {
        title: Some("Notes".into()),
        body: DocumentBody::Text("body".into()),
        mime: None,
        meta: MemoryMeta::default(),
    };
    assert_eq!(doc.render_text(), "# Notes\n\nbody");
    assert_eq!(
        conversation(&["hi", "yo"]).render_text(),
        "user: hi\nassistant: yo"
    );
}

#[test]
fn fingerprints_cover_metadata_and_are_stable() {
    let a = StoreItem::document("same", MemoryMeta::default());
    let b = StoreItem::document("same", MemoryMeta::default());
    let c = StoreItem::document(
        "same",
        MemoryMeta::from_source(SourceKind::Folder, Some("s".into())),
    );
    assert_eq!(a.fingerprint(), b.fingerprint());
    assert_ne!(a.fingerprint(), c.fingerprint());
    assert_eq!(a.fingerprint().len(), 40);
}

#[test]
fn fingerprints_ignore_when_an_item_was_observed() {
    let mut first =
        StoreItem::learning("tea", LearningKind::Preference, 0.9, MemoryMeta::default());
    let mut retry = first.clone();
    first.meta_mut().observed_at = Some(chrono::DateTime::UNIX_EPOCH);
    retry.meta_mut().observed_at =
        Some(chrono::DateTime::UNIX_EPOCH + chrono::Duration::seconds(1));
    assert_eq!(first.fingerprint(), retry.fingerprint());
    retry.meta_mut().thread_id = Some("t".into());
    assert_ne!(
        first.fingerprint(),
        retry.fingerprint(),
        "other meta still counts"
    );
}

#[test]
fn fingerprints_ignore_who_an_item_is_attributed_to() {
    let plain = StoreItem::document("hello", MemoryMeta::default());
    let mut attributed = plain.clone();
    attributed.meta_mut().observed_actor = Some(crate::ObservedActor {
        id: "user:priya@acme.com".into(),
        name: Some("Priya".into()),
    });
    assert_eq!(plain.fingerprint(), attributed.fingerprint());
    let json = serde_json::to_value(plain.meta()).unwrap();
    assert!(
        json.get("observed_actor").is_none(),
        "unset, not serialized"
    );
}

#[test]
fn conversation_channel_is_metadata_not_turn_identity() {
    let untagged = conversation(&["hello"]);
    let mut tagged = untagged.clone();
    tagged.meta_mut().channel = Some("telegram".into());
    assert_eq!(untagged.fingerprint(), tagged.fingerprint());

    let untagged_document = StoreItem::document("hello", MemoryMeta::default());
    let mut tagged_document = untagged_document.clone();
    tagged_document.meta_mut().channel = Some("telegram".into());
    assert_ne!(
        untagged_document.fingerprint(),
        tagged_document.fingerprint()
    );
}

#[test]
fn legacy_conversation_fingerprint_keeps_the_old_channel_identity() {
    let mut conversation = conversation(&["hello"]);
    conversation.meta_mut().channel = Some("telegram".into());
    let legacy = conversation
        .legacy_conversation_fingerprint()
        .expect("channel-bearing conversations have a legacy fingerprint");
    assert_ne!(legacy, conversation.fingerprint());

    conversation.meta_mut().channel = None;
    assert!(conversation.legacy_conversation_fingerprint().is_none());
    assert!(
        StoreItem::document("hello", MemoryMeta::default())
            .legacy_conversation_fingerprint()
            .is_none()
    );
}

#[test]
fn retried_learning_ignores_the_provider_tool_call_id() {
    let mut first = StoreItem::learning(
        "Prefers tea",
        LearningKind::Preference,
        0.8,
        MemoryMeta::default(),
    );
    first.meta_mut().namespace = "user:alice".parse().unwrap();
    first.meta_mut().tool_call = Some(ToolCallRef {
        name: "memory".into(),
        id: Some("call-first".into()),
    });
    let mut retry = first.clone();
    retry.meta_mut().tool_call.as_mut().unwrap().id = Some("call-retry".into());
    assert_eq!(first.fingerprint(), retry.fingerprint());
    assert_eq!(
        first.meta().tool_call.as_ref().unwrap().id.as_deref(),
        Some("call-first")
    );
    retry.meta_mut().namespace = "user:bob".parse().unwrap();
    assert_ne!(first.fingerprint(), retry.fingerprint());
    retry.meta_mut().namespace = first.meta().namespace.clone();
    retry.meta_mut().tool_call.as_mut().unwrap().name = "another-tool".into();
    assert_ne!(first.fingerprint(), retry.fingerprint());
}

#[test]
fn items_serialise_with_a_type_tag_and_round_trip() {
    let item = StoreItem::learning("tea", LearningKind::Preference, 0.9, MemoryMeta::default());
    let json = serde_json::to_value(&item).expect("serialise");
    assert_eq!(json["type"], "learning");
    assert_eq!(json["type"], ItemKind::Learning.as_str());
    let back: StoreItem = serde_json::from_value(json).expect("deserialise");
    assert_eq!(back, item);
}

#[test]
fn meta_mut_edits_in_place() {
    let mut item = conversation(&["hi"]);
    item.meta_mut().thread_id = Some("t".into());
    assert_eq!(item.meta().thread_id.as_deref(), Some("t"));
}

#[test]
fn ids_display_and_convert() {
    let id = ItemId::from("abc");
    assert_eq!(id.to_string(), "abc");
    assert_eq!(id.as_str(), "abc");
    assert_eq!(ItemId::new(String::from("abc")), id);
    assert_eq!(
        serde_json::to_value(&id).expect("json"),
        serde_json::json!("abc")
    );
}

#[test]
fn wire_strings_match_serde() {
    for kind in ItemKind::ALL {
        assert_eq!(serde_json::to_value(kind).expect("json"), kind.as_str());
    }
    for role in [Role::User, Role::Assistant, Role::System, Role::Tool] {
        assert_eq!(serde_json::to_value(role).expect("json"), role.as_str());
    }
    for kind in [
        LearningKind::Preference,
        LearningKind::Fact,
        LearningKind::Procedure,
        LearningKind::Correction,
        LearningKind::Other,
    ] {
        assert_eq!(serde_json::to_value(kind).expect("json"), kind.as_str());
    }
}

#[test]
fn a_turn_renders_its_tool_calls() {
    let mut turn = Turn::new(Role::Assistant, "done");
    assert_eq!(turn.render(), "assistant: done");
    turn.tool_calls = vec![
        ToolCallRef {
            name: "shell".into(),
            id: Some("call-1".into()),
        },
        ToolCallRef {
            name: "grep".into(),
            id: None,
        },
    ];
    assert_eq!(
        turn.render(),
        "assistant: done [tools: shell (call-1), grep]"
    );
}

#[test]
fn an_unset_derive_flag_leaves_the_fingerprint_as_it_was() {
    let item = StoreItem::document("Refunds take five days.", MemoryMeta::default());
    let json = serde_json::to_string(item.meta()).unwrap();
    assert!(!json.contains("derive"), "{json}");
    let mut opted_out = item.clone();
    opted_out.meta_mut().derive = Some(false);
    assert_ne!(opted_out.fingerprint(), item.fingerprint());
    let back: MemoryMeta =
        serde_json::from_str(&serde_json::to_string(opted_out.meta()).unwrap()).unwrap();
    assert_eq!(back.derive, Some(false));
}
