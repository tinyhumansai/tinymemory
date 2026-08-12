//! Host adapters for tinycortex's entity occurrence index.

use std::sync::Arc;

use anyhow::Result;
use tinycortex::memory::store::entity_index::{
    CanonicalEntity, EntityIndex, EntityKind, SelfIdentity,
};

use crate::sync::composio::providers::profile::{is_self_identity_any_toolkit, IdentityKind};
use crate::tinycortex::memory_config_from;
use crate::Config;

/// Aggregate entity-index row for capability providers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopEntity {
    /// Canonical entity id.
    pub id: String,
    /// Stable entity kind string.
    pub kind: String,
    /// Representative observed surface form.
    pub name: String,
    /// Number of indexed observations.
    pub mentions: u32,
}

/// Entity row scoped to one memory-tree namespace.
pub fn namespace_entities(
    config: &Config,
    namespace: &str,
    query: Option<&str>,
    limit: usize,
) -> Result<Vec<TopEntity>> {
    let memory = memory_config_from(config, config.workspace_dir().clone());
    let connection = tinycortex::memory::chunks::shared_connection(&memory)?;
    let guard = connection.lock();
    let pattern = query.map(|value| format!("%{}%", value.to_ascii_lowercase()));
    let mut statement = guard.prepare(
        "SELECT entity_id, entity_kind, MAX(surface), COUNT(*)
           FROM mem_tree_entity_index
          WHERE tree_id = ?1
            AND (?2 IS NULL OR LOWER(entity_id) LIKE ?2 OR LOWER(surface) LIKE ?2)
          GROUP BY entity_id, entity_kind
          ORDER BY COUNT(*) DESC, MAX(timestamp_ms) DESC
          LIMIT ?3",
    )?;
    let rows = statement
        .query_map(
            rusqlite::params![namespace, pattern, i64::try_from(limit).unwrap_or(i64::MAX)],
            |row| {
                let mentions: i64 = row.get(3)?;
                Ok(TopEntity {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    name: row.get(2)?,
                    mentions: u32::try_from(mentions.max(0)).unwrap_or(u32::MAX),
                })
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Co-occurrence edges scoped to one memory-tree namespace.
pub fn namespace_entity_edges(
    config: &Config,
    namespace: &str,
    entity_id: &str,
    limit: usize,
) -> Result<Vec<(String, u32)>> {
    let memory = memory_config_from(config, config.workspace_dir().clone());
    let connection = tinycortex::memory::chunks::shared_connection(&memory)?;
    let guard = connection.lock();
    let mut statement = guard.prepare(
        "SELECT b.entity_id, COUNT(*)
           FROM mem_tree_entity_index a
           JOIN mem_tree_entity_index b
             ON a.node_id = b.node_id AND a.tree_id = b.tree_id
          WHERE a.tree_id = ?1 AND a.entity_id = ?2 AND b.entity_id <> a.entity_id
          GROUP BY b.entity_id
          ORDER BY COUNT(*) DESC
          LIMIT ?3",
    )?;
    let rows = statement
        .query_map(
            rusqlite::params![
                namespace,
                entity_id,
                i64::try_from(limit).unwrap_or(i64::MAX)
            ],
            |row| {
                let count: i64 = row.get(1)?;
                Ok((row.get(0)?, u32::try_from(count.max(0)).unwrap_or(u32::MAX)))
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub use tinycortex::memory::store::entity_index::EntityHit;

#[derive(Debug)]
struct HostSelfIdentity;

impl SelfIdentity for HostSelfIdentity {
    fn is_self(&self, kind: EntityKind, surface: &str) -> bool {
        let identity_kind = match kind {
            EntityKind::Email => IdentityKind::Email,
            EntityKind::Handle => IdentityKind::Handle,
            _ => return false,
        };
        is_self_identity_any_toolkit(identity_kind, surface)
    }
}

fn index(config: &Config) -> Result<EntityIndex> {
    let memory = memory_config_from(config, config.workspace_dir().clone());
    let connection = tinycortex::memory::chunks::shared_connection(&memory)?;
    EntityIndex::from_shared_connection(connection, Arc::new(HostSelfIdentity))
}

pub fn index_entity(
    config: &Config,
    entity: &CanonicalEntity,
    node_id: &str,
    node_kind: &str,
    timestamp_ms: i64,
    tree_id: Option<&str>,
) -> Result<()> {
    log::debug!("[memory:entities] index one node_kind={node_kind}");
    index(config)?.index_entity(entity, node_id, node_kind, timestamp_ms, tree_id)
}

pub fn index_entities(
    config: &Config,
    entities: &[CanonicalEntity],
    node_id: &str,
    node_kind: &str,
    timestamp_ms: i64,
    tree_id: Option<&str>,
) -> Result<usize> {
    log::debug!(
        "[memory:entities] index batch count={} node_kind={node_kind}",
        entities.len()
    );
    index(config)?.index_entities(entities, node_id, node_kind, timestamp_ms, tree_id)
}

pub fn clear_entity_index_for_node(config: &Config, node_id: &str) -> Result<usize> {
    index(config)?.clear_entity_index_for_node(node_id)
}

pub fn lookup_entity(
    config: &Config,
    entity_id: &str,
    limit: Option<usize>,
) -> Result<Vec<EntityHit>> {
    index(config)?.lookup_entity(entity_id, limit)
}

pub fn list_entity_ids_for_node(config: &Config, node_id: &str) -> Result<Vec<String>> {
    index(config)?.list_entity_ids_for_node(node_id)
}

pub fn count_entity_index(config: &Config) -> Result<u64> {
    index(config)?.count_entity_index()
}

/// Most frequently observed entities, with recency as the tie-breaker.
pub fn top_entities(config: &Config, limit: usize) -> Result<Vec<TopEntity>> {
    let memory = memory_config_from(config, config.workspace_dir().clone());
    let connection = tinycortex::memory::chunks::shared_connection(&memory)?;
    let guard = connection.lock();
    let mut statement = guard.prepare(
        "SELECT entity_id, entity_kind, MAX(surface), COUNT(*)
           FROM mem_tree_entity_index
          GROUP BY entity_id, entity_kind
          ORDER BY COUNT(*) DESC, MAX(timestamp_ms) DESC
          LIMIT ?1",
    )?;
    let rows = statement
        .query_map([i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
            let mentions: i64 = row.get(3)?;
            Ok(TopEntity {
                id: row.get(0)?,
                kind: row.get(1)?,
                name: row.get(2)?,
                mentions: u32::try_from(mentions.max(0)).unwrap_or(u32::MAX),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
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
        let connection = tinycortex::memory::chunks::shared_connection(&memory).expect("db");
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
}
