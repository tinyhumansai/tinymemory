//! Host adapters for tinycortex's entity occurrence index.

use std::sync::Arc;

use crate::engine::backend::store::entity_index::{
    CanonicalEntity, EntityIndex, EntityKind, SelfIdentity,
};
use anyhow::Result;

use crate::engine::memory_config_from;
use crate::sync::composio::providers::profile::{is_self_identity_any_toolkit, IdentityKind};
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
    let connection = crate::engine::backend::chunks::shared_connection(&memory)?;
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
    let connection = crate::engine::backend::chunks::shared_connection(&memory)?;
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

pub use crate::engine::backend::store::entity_index::EntityHit;

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
    let connection = crate::engine::backend::chunks::shared_connection(&memory)?;
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
    let connection = crate::engine::backend::chunks::shared_connection(&memory)?;
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
#[path = "entities_tests.rs"]
mod tests;
