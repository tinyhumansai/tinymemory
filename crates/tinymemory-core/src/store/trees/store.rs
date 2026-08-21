//! `Config` and transaction adapters for tinycortex tree persistence.

use std::collections::HashMap;

use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, Transaction};

use crate::engine::engine_config;
use crate::store::content::StagedSummary;
use crate::store::trees::types::{Buffer, SummaryNode, Tree, TreeKind};
use crate::Config;

pub fn insert_tree(config: &Config, tree: &Tree) -> Result<()> {
    crate::engine::backend::tree::store::insert_tree(&engine_config(config), tree)
}

pub fn get_tree_by_scope(config: &Config, kind: TreeKind, scope: &str) -> Result<Option<Tree>> {
    crate::engine::backend::tree::store::get_tree_by_scope(&engine_config(config), kind, scope)
}

pub fn get_tree(config: &Config, id: &str) -> Result<Option<Tree>> {
    crate::engine::backend::tree::store::get_tree(&engine_config(config), id)
}

pub fn get_trees_batch(config: &Config, ids: &[String]) -> Result<HashMap<String, Tree>> {
    crate::engine::backend::tree::store::get_trees_batch(&engine_config(config), ids)
}

pub fn list_trees_by_kind(config: &Config, kind: TreeKind) -> Result<Vec<Tree>> {
    crate::engine::backend::tree::store::list_trees_by_kind(&engine_config(config), kind)
}

pub fn update_tree_after_seal_tx(
    tx: &Transaction<'_>,
    tree_id: &str,
    root_id: &str,
    max_level: u32,
    sealed_at: DateTime<Utc>,
) -> Result<()> {
    crate::engine::backend::tree::store::update_tree_after_seal_tx(
        tx, tree_id, root_id, max_level, sealed_at,
    )
}

pub fn insert_summary_tx(
    tx: &Transaction<'_>,
    node: &SummaryNode,
    staged: Option<&StagedSummary>,
    model_signature: &str,
) -> Result<()> {
    crate::engine::backend::tree::store::insert_staged_summary_tx(tx, node, staged, model_signature)
}

pub fn set_summary_embedding(
    config: &Config,
    summary_id: &str,
    embedding: &[f32],
) -> Result<usize> {
    crate::engine::backend::tree::store::set_summary_embedding(
        &engine_config(config),
        summary_id,
        embedding,
    )?;
    Ok(1)
}

pub fn get_summary_embedding(config: &Config, summary_id: &str) -> Result<Option<Vec<f32>>> {
    crate::engine::backend::tree::store::get_summary_embedding(&engine_config(config), summary_id)
}

pub fn set_summary_embedding_for_signature(
    config: &Config,
    summary_id: &str,
    signature: &str,
    embedding: &[f32],
) -> Result<()> {
    crate::engine::backend::tree::store::set_summary_embedding_for_signature(
        &engine_config(config),
        summary_id,
        signature,
        embedding,
    )
}

pub fn mark_summary_reembed_skipped(
    config: &Config,
    summary_id: &str,
    signature: &str,
    reason: &str,
) -> Result<()> {
    crate::engine::backend::chunks::mark_summary_reembed_skipped(
        &engine_config(config),
        summary_id,
        signature,
        reason,
    )
}

pub fn clear_summary_reembed_skipped(
    config: &Config,
    summary_id: &str,
    signature: &str,
) -> Result<()> {
    crate::engine::backend::chunks::clear_summary_reembed_skipped(
        &engine_config(config),
        summary_id,
        signature,
    )
}

pub(crate) fn set_summary_embedding_for_signature_tx(
    tx: &Transaction<'_>,
    summary_id: &str,
    signature: &str,
    embedding: &[f32],
) -> Result<()> {
    crate::engine::backend::chunks::set_summary_embedding_for_signature_tx(
        tx, summary_id, signature, embedding,
    )
}

pub fn get_summary_embedding_for_signature(
    config: &Config,
    summary_id: &str,
    signature: &str,
) -> Result<Option<Vec<f32>>> {
    crate::engine::backend::tree::store::get_summary_embedding_for_signature(
        &engine_config(config),
        summary_id,
        signature,
    )
}

pub fn get_summary_embeddings_for_signature_batch(
    config: &Config,
    ids: &[String],
    signature: &str,
) -> Result<HashMap<String, Vec<f32>>> {
    crate::engine::backend::tree::store::get_summary_embeddings_for_signature_batch(
        &engine_config(config),
        ids,
        signature,
    )
}

pub fn get_summary_embeddings_batch(
    config: &Config,
    ids: &[String],
) -> Result<HashMap<String, Vec<f32>>> {
    crate::engine::backend::tree::store::get_summary_embeddings_batch(&engine_config(config), ids)
}

pub fn get_summary(config: &Config, id: &str) -> Result<Option<SummaryNode>> {
    crate::engine::backend::tree::store::get_summary(&engine_config(config), id)
}

pub fn get_summaries_batch(
    config: &Config,
    ids: &[String],
) -> Result<HashMap<String, SummaryNode>> {
    crate::engine::backend::tree::store::get_summaries_batch(&engine_config(config), ids)
}

pub fn list_summaries_at_level(
    config: &Config,
    tree_id: &str,
    level: u32,
) -> Result<Vec<SummaryNode>> {
    crate::engine::backend::tree::store::list_summaries_at_level(
        &engine_config(config),
        tree_id,
        level,
    )
}

pub fn list_summaries_in_window(
    config: &Config,
    tree_id: &str,
    since_ms: i64,
    until_ms: i64,
) -> Result<Vec<SummaryNode>> {
    crate::engine::backend::tree::store::list_summaries_in_window(
        &engine_config(config),
        tree_id,
        since_ms,
        until_ms,
    )
}

pub fn count_summaries(config: &Config, tree_id: &str) -> Result<u64> {
    crate::engine::backend::tree::store::count_summaries(&engine_config(config), tree_id)
}

pub fn get_buffer(config: &Config, tree_id: &str, level: u32) -> Result<Buffer> {
    crate::engine::backend::tree::store::get_buffer(&engine_config(config), tree_id, level)
}

pub(crate) fn get_buffer_conn(conn: &Connection, tree_id: &str, level: u32) -> Result<Buffer> {
    crate::engine::backend::tree::store::get_buffer_conn(conn, tree_id, level)
}

pub fn upsert_buffer_tx(tx: &Transaction<'_>, buffer: &Buffer) -> Result<()> {
    crate::engine::backend::tree::store::upsert_buffer_tx(tx, buffer)
}

pub fn list_stale_buffers(config: &Config, older_than: DateTime<Utc>) -> Result<Vec<Buffer>> {
    crate::engine::backend::tree::store::list_stale_buffers(&engine_config(config), older_than)
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
