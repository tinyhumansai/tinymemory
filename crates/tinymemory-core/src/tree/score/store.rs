//! Product Config adapters over tinycortex score and entity-index persistence.

use std::collections::HashMap;

use anyhow::Result;

use crate::engine::engine_config;
use crate::Config;

pub use crate::engine::backend::score::store::{EntityHit, ScoreRow};

pub fn upsert_score(config: &Config, row: &ScoreRow) -> Result<()> {
    crate::engine::backend::score::store::upsert_score(&engine_config(config), row)
}

pub fn get_score(config: &Config, chunk_id: &str) -> Result<Option<ScoreRow>> {
    crate::engine::backend::score::store::get_score(&engine_config(config), chunk_id)
}

pub fn get_scores_batch(config: &Config, chunk_ids: &[String]) -> Result<HashMap<String, f32>> {
    crate::engine::backend::score::store::get_scores_batch(&engine_config(config), chunk_ids)
}

pub use crate::store::entities::{
    clear_entity_index_for_node, count_entity_index, list_entity_ids_for_node, lookup_entity,
};

pub fn index_entity(
    config: &Config,
    entity: &crate::engine::backend::score::resolver::CanonicalEntity,
    node_id: &str,
    node_kind: &str,
    timestamp_ms: i64,
    tree_id: Option<&str>,
) -> Result<()> {
    let entity = to_store_entity(entity)?;
    crate::store::entities::index_entity(config, &entity, node_id, node_kind, timestamp_ms, tree_id)
}

pub fn index_entities(
    config: &Config,
    entities: &[crate::engine::backend::score::resolver::CanonicalEntity],
    node_id: &str,
    node_kind: &str,
    timestamp_ms: i64,
    tree_id: Option<&str>,
) -> Result<usize> {
    let entities: Vec<crate::engine::backend::store::CanonicalEntity> = entities
        .iter()
        .map(to_store_entity)
        .collect::<Result<_>>()?;
    crate::store::entities::index_entities(
        config,
        &entities,
        node_id,
        node_kind,
        timestamp_ms,
        tree_id,
    )
}

fn to_store_entity(
    entity: &crate::engine::backend::score::resolver::CanonicalEntity,
) -> Result<crate::engine::backend::store::CanonicalEntity> {
    Ok(crate::engine::backend::store::CanonicalEntity {
        canonical_id: entity.canonical_id.clone(),
        kind: crate::engine::backend::store::EntityKind::parse(entity.kind.as_str())
            .map_err(anyhow::Error::msg)?,
        surface: entity.surface.clone(),
        span_start: entity.span_start,
        span_end: entity.span_end,
        score: entity.score,
    })
}

pub fn count_scores(config: &Config) -> Result<u64> {
    crate::engine::backend::score::store::count_scores(&engine_config(config))
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
