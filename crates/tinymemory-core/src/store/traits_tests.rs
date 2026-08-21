//! Tests for the surrounding module.

use super::*;
use crate::store::chunks::types::{Metadata, SourceKind};
use chrono::Utc;

fn sample_chunk() -> Chunk {
    let ts = Utc::now();
    Chunk {
        id: "chunk-1".into(),
        content: "hello world".into(),
        metadata: Metadata {
            source_kind: SourceKind::Chat,
            source_id: "slack:#eng".into(),
            timestamp: ts,
            time_range: (ts, ts),
            owner: "alice".into(),
            source_ref: None,
            tags: vec!["person:alice".into()],
            path_scope: None,
        },
        seq_in_source: 7,
        token_count: 2,
        created_at: ts,
        partial_message: false,
    }
}

#[test]
fn chunk_traits_render_expected_kind_and_obsidian_path() {
    let chunk = sample_chunk();
    assert_eq!(chunk.memory_kind(), MemoryKind::Chunk);
    assert_eq!(chunk.embeddable_text(), "hello world");

    let obsidian = chunk.to_obsidian();
    assert_eq!(obsidian.relative_path, PathBuf::from("chunks/chunk-1.md"));
    assert!(obsidian.markdown.contains("source_kind: chat"));
    assert!(obsidian.markdown.contains("source_id: slack:#eng"));
    assert!(obsidian.markdown.contains("hello world"));
}

#[test]
fn summary_node_traits_render_expected_kind_and_path() {
    let node = SummaryNode {
        id: "summary-1".into(),
        tree_id: "tree-1".into(),
        tree_kind: crate::store::trees::TreeKind::Source,
        level: 1,
        parent_id: None,
        child_ids: vec!["chunk-1".into()],
        content: "summary body".into(),
        token_count: 3,
        entities: vec![],
        topics: vec![],
        time_range_start: Utc::now(),
        time_range_end: Utc::now(),
        score: 0.5,
        sealed_at: Utc::now(),
        deleted: false,
        embedding: None,
        doc_id: None,
        version_ms: None,
    };
    assert_eq!(node.memory_kind(), MemoryKind::Tree);
    assert_eq!(node.embeddable_text(), "summary body");
    let obsidian = node.to_obsidian();
    assert_eq!(
        obsidian.relative_path,
        PathBuf::from("summaries/summary-1.md")
    );
    assert!(obsidian.markdown.contains("tree_id: tree-1"));
    assert!(obsidian.markdown.contains("summary body"));
}

#[test]
fn tree_traits_render_obsidian_metadata() {
    let tree = Tree {
        id: "tree-1".into(),
        kind: crate::store::trees::TreeKind::Topic,
        scope: "topic:phoenix".into(),
        ask: None,
        root_id: Some("summary-root".into()),
        max_level: 2,
        status: crate::store::trees::TreeStatus::Active,
        created_at: Utc::now(),
        last_sealed_at: None,
    };
    let obsidian = tree.to_obsidian();
    assert_eq!(obsidian.relative_path, PathBuf::from("trees/tree-1.md"));
    assert!(obsidian.markdown.contains("id: tree-1"));
    assert!(obsidian.markdown.contains("Tree tree-1"));
    assert!(obsidian.markdown.contains("Topic"));
}

#[test]
fn person_traits_render_name_and_email_when_present() {
    let now = Utc::now();
    let person = Person {
        id: crate::people::types::PersonId::new(),
        display_name: Some("Alice Example".into()),
        primary_email: Some("alice@example.com".into()),
        primary_phone: Some("+1 555 0100".into()),
        handles: vec![
            crate::people::types::Handle::DisplayName("Alice Example".into()),
            crate::people::types::Handle::Email("alice@example.com".into()),
        ],
        created_at: now,
        updated_at: now,
    };
    assert_eq!(person.memory_kind(), MemoryKind::Contact);
    assert_eq!(person.embeddable_text(), "Alice Example\nalice@example.com");
    let obsidian = person.to_obsidian();
    assert_eq!(
        obsidian.relative_path,
        PathBuf::from("contacts").join(format!("{}.md", person.id))
    );
    assert!(obsidian.markdown.contains("# Alice Example"));
    assert!(obsidian.markdown.contains("Email: alice@example.com"));
}

#[test]
fn person_traits_fall_back_when_fields_are_missing() {
    let now = Utc::now();
    let person = Person {
        id: crate::people::types::PersonId::new(),
        display_name: None,
        primary_email: None,
        primary_phone: None,
        handles: vec![],
        created_at: now,
        updated_at: now,
    };
    assert_eq!(person.embeddable_text(), "");
    let obsidian = person.to_obsidian();
    assert!(obsidian.markdown.contains("# Unknown"));
    assert!(obsidian.markdown.contains("Email: "));
}
