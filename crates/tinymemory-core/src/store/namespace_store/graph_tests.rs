//! Tests for the surrounding module.

use super::*;
use std::sync::Arc;
use tempfile::TempDir;
use tinymemory_api::host::NoopEmbedding;

#[test]
fn merge_graph_attrs_accumulates_evidence_and_dedupes_ids() {
    let existing = json!({
        "evidence_count": 2,
        "document_ids": ["doc-1"],
        "chunk_ids": ["doc-1:chunk-1"],
        "order_index": 7,
        "created_at": 1.0
    });
    let incoming = json!({
        "evidence_count": 3,
        "document_ids": ["doc-1", "doc-2"],
        "chunk_ids": ["doc-2:chunk-9"],
        "order_index": 3,
        "attrs_only": true
    });

    let merged = UnifiedMemory::merge_graph_attrs(Some(&existing.to_string()), &incoming, 9.0);
    assert_eq!(merged["evidence_count"], json!(5));
    assert_eq!(merged["document_ids"], json!(["doc-1", "doc-2"]));
    assert_eq!(
        merged["chunk_ids"],
        json!(["doc-1:chunk-1", "doc-2:chunk-9"])
    );
    assert_eq!(merged["order_index"], json!(3));
    assert_eq!(merged["created_at"], json!(1.0));
    assert_eq!(merged["updated_at"], json!(9.0));
    assert_eq!(merged["attrs_only"], json!(true));
}

#[test]
fn graph_relation_from_parts_extracts_counts_and_ids() {
    let record = UnifiedMemory::graph_relation_from_parts(
        Some("global".into()),
        "Alice".into(),
        "OWNS".into(),
        "OpenHuman".into(),
        r#"{"evidence_count":2,"order_index":4,"document_ids":["doc-1"],"chunk_ids":["doc-1:chunk-1"]}"#,
        5.0,
    );
    assert_eq!(record.namespace.as_deref(), Some("global"));
    assert_eq!(record.evidence_count, 2);
    assert_eq!(record.order_index, Some(4));
    assert_eq!(record.document_ids, vec!["doc-1".to_string()]);
    assert_eq!(record.chunk_ids, vec!["doc-1:chunk-1".to_string()]);
}

#[test]
fn merge_graph_attrs_recovers_from_invalid_existing_json_and_negative_evidence() {
    let incoming = json!({
        "evidence_count": -4,
        "document_id": "doc-2",
        "chunk_id": "doc-2:chunk-9",
        "order_index": 8
    });

    let merged = UnifiedMemory::merge_graph_attrs(Some("not-json"), &incoming, 11.0);
    assert_eq!(
        merged["evidence_count"],
        json!(1),
        "negative evidence should clamp to the minimum count"
    );
    assert_eq!(merged["document_ids"], json!(["doc-2"]));
    assert_eq!(merged["chunk_ids"], json!(["doc-2:chunk-9"]));
    assert_eq!(merged["order_index"], json!(8));
    assert_eq!(merged["created_at"], json!(11.0));
    assert_eq!(merged["updated_at"], json!(11.0));
}

#[test]
fn graph_relation_from_parts_defaults_invalid_attrs_payload() {
    let record = UnifiedMemory::graph_relation_from_parts(
        None,
        "Alice".into(),
        "OWNS".into(),
        "Phoenix".into(),
        "not-json",
        7.5,
    );
    assert_eq!(record.evidence_count, 1);
    assert_eq!(record.order_index, None);
    assert!(record.document_ids.is_empty());
    assert!(record.chunk_ids.is_empty());
    assert_eq!(record.attrs, json!({}));
}

#[test]
fn graph_relation_to_json_uses_expected_public_keys() {
    let value = UnifiedMemory::graph_relation_to_json(GraphRelationRecord {
        namespace: None,
        subject: "Alice".into(),
        predicate: "OWNS".into(),
        object: "OpenHuman".into(),
        attrs: json!({"extra": true}),
        updated_at: 1.5,
        evidence_count: 1,
        order_index: Some(2),
        document_ids: vec!["doc-1".into()],
        chunk_ids: vec!["doc-1:chunk-1".into()],
    });
    assert_eq!(value["subject"], "Alice");
    assert_eq!(value["predicate"], "OWNS");
    assert_eq!(value["evidenceCount"], 1);
    assert_eq!(value["orderIndex"], 2);
    assert_eq!(value["documentIds"], json!(["doc-1"]));
    assert_eq!(value["chunkIds"], json!(["doc-1:chunk-1"]));
}

fn test_memory() -> (TempDir, UnifiedMemory) {
    let tmp = TempDir::new().unwrap();
    let memory = UnifiedMemory::new(tmp.path(), Arc::new(NoopEmbedding), None).unwrap();
    (tmp, memory)
}

#[tokio::test]
async fn graph_upsert_namespace_merges_attrs_and_query_returns_json() {
    let (_tmp, memory) = test_memory();
    memory
        .graph_upsert_namespace(
            "team alpha/#1",
            "Alice",
            "OWNS",
            "Phoenix",
            &json!({
                "document_id": "doc-1",
                "chunk_id": "doc-1:chunk-1",
                "evidence_count": 1
            }),
        )
        .await
        .unwrap();
    memory
        .graph_upsert_namespace(
            "team alpha/#1",
            "Alice",
            "OWNS",
            "Phoenix",
            &json!({
                "document_ids": ["doc-2"],
                "chunk_ids": ["doc-2:chunk-9"],
                "order_index": 2
            }),
        )
        .await
        .unwrap();

    let rows = memory
        .graph_query_namespace("team alpha/#1", Some("Alice"), Some("OWNS"))
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["subject"], "ALICE");
    assert_eq!(rows[0]["predicate"], "OWNS");
    assert_eq!(rows[0]["object"], "PHOENIX");
    assert_eq!(rows[0]["evidenceCount"], 2);
    assert_eq!(rows[0]["orderIndex"], 2);
    assert_eq!(rows[0]["documentIds"], json!(["doc-1", "doc-2"]));
    assert_eq!(
        rows[0]["chunkIds"],
        json!(["doc-1:chunk-1", "doc-2:chunk-9"])
    );

    let scoped = memory
        .graph_relations_for_scope("team alpha/#1")
        .await
        .unwrap();
    assert_eq!(scoped.len(), 1);
    assert_eq!(scoped[0].namespace.as_deref(), Some("team_alpha/_1"));
}

#[tokio::test]
async fn graph_global_and_all_queries_include_expected_rows() {
    let (_tmp, memory) = test_memory();
    memory
        .graph_upsert_global(
            "Bob",
            "MENTIONED",
            "Launch",
            &json!({"document_id": "doc-global"}),
        )
        .await
        .unwrap();
    memory
        .graph_upsert_namespace(
            "project",
            "Alice",
            "OWNS",
            "Phoenix",
            &json!({"document_id": "doc-local"}),
        )
        .await
        .unwrap();

    let global = memory
        .graph_query_global(Some("Bob"), Some("MENTIONED"))
        .await
        .unwrap();
    assert_eq!(global.len(), 1);
    assert_eq!(global[0]["namespace"], Value::Null);
    assert_eq!(global[0]["subject"], "BOB");

    let all = memory.graph_query_all(None, None).await.unwrap();
    assert_eq!(all.len(), 2);
    assert!(all.iter().any(|row| row["subject"] == "ALICE"));
    assert!(all.iter().any(|row| row["subject"] == "BOB"));
}

#[tokio::test]
async fn graph_relations_for_scope_includes_global_rows_and_sorts_newest_first() {
    let (_tmp, memory) = test_memory();
    memory
        .graph_upsert_namespace(
            "scope-a",
            "Alice",
            "OWNS",
            "Phoenix",
            &json!({"document_id": "doc-local"}),
        )
        .await
        .unwrap();
    memory
        .graph_upsert_global(
            "Bob",
            "MENTIONED",
            "Launch",
            &json!({"document_id": "doc-global"}),
        )
        .await
        .unwrap();

    let scoped = memory.graph_relations_for_scope("scope-a").await.unwrap();
    assert_eq!(scoped.len(), 2);
    assert!(scoped
        .iter()
        .any(|row| row.namespace.as_deref() == Some("scope-a")));
    assert!(scoped.iter().any(|row| row.namespace.is_none()));
    assert!(
        scoped[0].updated_at >= scoped[1].updated_at,
        "scope queries should stay sorted newest-first across namespace+global rows"
    );
}

#[tokio::test]
async fn graph_remove_document_namespace_prunes_or_deletes_relations() {
    let (_tmp, memory) = test_memory();
    memory
        .graph_upsert_namespace(
            "cleanup",
            "Alice",
            "OWNS",
            "Phoenix",
            &json!({
                "document_ids": ["doc-1", "doc-2"],
                "chunk_ids": ["doc-1:chunk-1", "doc-2:chunk-2"]
            }),
        )
        .await
        .unwrap();
    memory
        .graph_upsert_namespace(
            "cleanup",
            "Alice",
            "BLOCKED",
            "Atlas",
            &json!({
                "document_id": "doc-1",
                "chunk_id": "doc-1:chunk-9"
            }),
        )
        .await
        .unwrap();

    memory
        .graph_remove_document_namespace("cleanup", "doc-1")
        .await
        .unwrap();

    let rows = memory
        .graph_query_namespace("cleanup", None, None)
        .await
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "single-doc relation should be deleted entirely"
    );
    assert_eq!(rows[0]["predicate"], "OWNS");
    assert_eq!(rows[0]["documentIds"], json!(["doc-2"]));
    assert_eq!(rows[0]["chunkIds"], json!(["doc-2:chunk-2"]));
}

#[tokio::test]
async fn graph_remove_document_namespace_is_noop_for_unrelated_document() {
    let (_tmp, memory) = test_memory();
    memory
        .graph_upsert_namespace(
            "cleanup",
            "Alice",
            "OWNS",
            "Phoenix",
            &json!({
                "document_ids": ["doc-2"],
                "chunk_ids": ["doc-2:chunk-2"]
            }),
        )
        .await
        .unwrap();

    memory
        .graph_remove_document_namespace("cleanup", "doc-missing")
        .await
        .unwrap();

    let rows = memory
        .graph_query_namespace("cleanup", None, None)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["documentIds"], json!(["doc-2"]));
    assert_eq!(rows[0]["chunkIds"], json!(["doc-2:chunk-2"]));
}
