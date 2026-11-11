//! Item scrubbing reaches every text field and leaves identifiers alone.

use tinymemory_api::{LearningKind, MemoryMeta, Role, Turn};

use super::*;

const SECRET: &str = "sk-proj-abcdefghijklmnopqrstuvwxyz0123456789ABCD";

fn has_secret(text: &str) -> bool {
    text.contains(SECRET)
}

#[test]
fn a_document_title_and_body_are_scrubbed() {
    let item = StoreItem::Document {
        title: Some(format!("key {SECRET}")),
        body: DocumentBody::Text(format!("the key is {SECRET}")),
        mime: None,
        meta: MemoryMeta::default(),
    };
    let scrubbed = scrub_item(item);
    assert!(scrubbed.report.changed());
    let StoreItem::Document { title, body, .. } = scrubbed.value else {
        panic!("kind changed");
    };
    assert!(!has_secret(title.as_deref().unwrap_or_default()));
    assert!(matches!(body, DocumentBody::Text(text) if !has_secret(&text)));
}

#[test]
fn every_turn_is_scrubbed() {
    let item = StoreItem::Conversation {
        turns: vec![
            Turn::new(Role::User, format!("use {SECRET}")),
            Turn::new(Role::Assistant, "ok"),
        ],
        meta: MemoryMeta::default(),
    };
    let scrubbed = scrub_item(item);
    let StoreItem::Conversation { turns, .. } = scrubbed.value else {
        panic!("kind changed");
    };
    assert!(!has_secret(&turns[0].text));
    assert_eq!(turns[1].text, "ok");
}

#[test]
fn learning_text_evidence_and_meta_url_are_scrubbed_but_ids_are_not() {
    let meta = MemoryMeta {
        url: Some(format!("https://x.test/?token={SECRET}")),
        file_path: Some("/repo/src/main.rs".into()),
        thread_id: Some("thread-1".into()),
        ..MemoryMeta::default()
    };
    let mut item = StoreItem::learning(format!("key {SECRET}"), LearningKind::Fact, 0.5, meta);
    if let StoreItem::Learning { evidence, .. } = &mut item {
        *evidence = Some(format!("seen {SECRET}"));
    }
    let scrubbed = scrub_item_with(item, Policy::corroborated());
    let value = scrubbed.value;
    assert!(!has_secret(&value.render_text()));
    let StoreItem::Learning { evidence, meta, .. } = value else {
        panic!("kind changed");
    };
    assert!(!has_secret(evidence.as_deref().unwrap_or_default()));
    assert!(!has_secret(meta.url.as_deref().unwrap_or_default()));
    assert_eq!(meta.file_path.as_deref(), Some("/repo/src/main.rs"));
    assert_eq!(meta.thread_id.as_deref(), Some("thread-1"));
}

#[test]
fn clean_items_are_unchanged() {
    let item = StoreItem::document("nothing sensitive here", MemoryMeta::default());
    let scrubbed = scrub_item(item.clone());
    assert!(!scrubbed.report.changed());
    assert_eq!(scrubbed.value, item);
}
