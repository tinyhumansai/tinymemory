//! Tests for the surrounding module.

use super::*;

fn scoped_config() -> (
    tempfile::TempDir,
    tinymemory_api::host::test_support::TestHostConfig,
) {
    crate::test_seams::init();
    let temp = tempfile::tempdir().expect("tempdir");
    let mut config = tinymemory_api::host::test_support::TestHostConfig::default();
    config.workspace_dir = temp.path().to_path_buf();
    (temp, config)
}

fn insert_entity(config: &Config, tree: &str, entity: &str, node: &str, surface: &str) {
    let memory = memory_config_from(config, config.workspace_dir().clone());
    let connection = crate::engine::backend::chunks::shared_connection(&memory).expect("db");
    connection
        .lock()
        .execute(
            "INSERT INTO mem_tree_entity_index
             (entity_id,node_id,node_kind,entity_kind,surface,score,timestamp_ms,tree_id)
             VALUES (?1,?2,'chunk','person',?3,1.0,1,?4)",
            rusqlite::params![entity, node, surface, tree],
        )
        .expect("insert");
}

#[test]
fn crate_entity_hit_is_the_host_facade_type() {
    let hit = EntityHit {
        entity_id: "person:alice".into(),
        node_id: "chunk-1".into(),
        node_kind: "leaf".into(),
        entity_kind: EntityKind::Person,
        surface: "Alice".into(),
        score: 1.0,
        timestamp_ms: 123,
        tree_id: Some("tree-1".into()),
        is_user: false,
    };
    assert_eq!(hit.entity_id, "person:alice");
}

#[test]
fn namespace_entity_reads_do_not_cross_tree_ids() {
    let (_temp, config) = scoped_config();
    insert_entity(&config, "team-a", "person:alice", "a1", "Alice");
    insert_entity(&config, "team-b", "person:bob", "b1", "Bob");

    let rows = namespace_entities(&config, "team-a", None, 10).expect("entities");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, "person:alice");
    assert!(namespace_entities(&config, "team-a", Some("bob"), 10)
        .expect("search")
        .is_empty());
}

#[test]
fn namespace_edges_join_only_rows_in_the_same_tree() {
    let (_temp, config) = scoped_config();
    insert_entity(&config, "team-a", "person:alice", "shared", "Alice");
    insert_entity(&config, "team-a", "person:bob", "shared", "Bob");
    insert_entity(&config, "team-b", "person:mallory", "shared", "Mallory");

    let rows = namespace_entity_edges(&config, "team-a", "person:alice", 10).expect("edges");
    assert_eq!(rows, vec![("person:bob".to_string(), 1)]);
}
