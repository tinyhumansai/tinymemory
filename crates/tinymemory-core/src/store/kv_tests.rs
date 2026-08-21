//! Tests for the surrounding module.

use serde_json::json;
use tempfile::TempDir;

use super::*;
use tinymemory_api::host::NoopEmbedding;

fn test_memory() -> (TempDir, UnifiedMemory) {
    let tmp = TempDir::new().unwrap();
    let memory = UnifiedMemory::new(tmp.path(), std::sync::Arc::new(NoopEmbedding), None).unwrap();
    (tmp, memory)
}

#[tokio::test]
async fn global_kv_roundtrips_and_deletes_through_tinycortex() {
    let (_tmp, memory) = test_memory();
    memory.kv_set_global("theme", &json!("dark")).await.unwrap();
    assert_eq!(
        memory.kv_get_global("theme").await.unwrap(),
        Some(json!("dark"))
    );
    assert!(memory.kv_delete_global("theme").await.unwrap());
}

#[tokio::test]
async fn namespace_records_share_the_unified_connection() {
    let (_tmp, memory) = test_memory();
    memory
        .kv_set_namespace("team alpha/#1", "state", &json!({"open": true}))
        .await
        .unwrap();
    let records = memory.kv_records_namespace("team alpha/#1").await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].namespace.as_deref(), Some("team_alpha/_1"));
}
