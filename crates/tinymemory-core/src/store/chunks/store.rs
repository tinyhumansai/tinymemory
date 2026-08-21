//! `Config` and transaction adapters for tinycortex chunk persistence.

use std::collections::HashMap;

use anyhow::Result;
use rusqlite::Transaction;

use crate::engine::engine_config;
use crate::store::chunks::types::{Chunk, SourceKind};
use crate::store::content::StagedChunk;
use crate::Config;

pub use crate::engine::backend::chunks::{
    ListChunksQuery, RawRef, CHUNK_STATUS_ADMITTED, CHUNK_STATUS_BUFFERED, CHUNK_STATUS_DROPPED,
    CHUNK_STATUS_PENDING_EXTRACTION, CHUNK_STATUS_SEALED, RAW_FILE_GATE_KIND,
};

pub fn upsert_chunks(config: &Config, chunks: &[Chunk]) -> Result<usize> {
    crate::engine::backend::chunks::upsert_chunks(&engine_config(config), chunks)
}

pub fn upsert_chunks_tx(tx: &Transaction<'_>, chunks: &[Chunk]) -> Result<usize> {
    crate::engine::backend::chunks::upsert_chunks_tx(tx, chunks)
}

pub fn upsert_staged_chunks_tx(tx: &Transaction<'_>, chunks: &[StagedChunk]) -> Result<usize> {
    crate::engine::backend::chunks::upsert_staged_chunks_tx(tx, chunks)
}

pub fn update_chunk_content_sha256(config: &Config, id: &str, sha256: &str) -> Result<()> {
    crate::engine::backend::chunks::update_chunk_content_sha256(&engine_config(config), id, sha256)
}

pub fn update_summary_content_sha256(config: &Config, id: &str, sha256: &str) -> Result<()> {
    crate::engine::backend::chunks::update_summary_content_sha256(
        &engine_config(config),
        id,
        sha256,
    )
}

pub fn list_source_ids_with_prefix(
    config: &Config,
    kind: SourceKind,
    prefix: &str,
) -> Result<Vec<String>> {
    crate::engine::backend::chunks::list_source_ids_with_prefix(
        &engine_config(config),
        kind,
        prefix,
    )
}

pub fn get_chunk(config: &Config, id: &str) -> Result<Option<Chunk>> {
    crate::engine::backend::chunks::get_chunk(&engine_config(config), id)
}

pub fn get_chunks_batch(config: &Config, ids: &[String]) -> Result<HashMap<String, Chunk>> {
    crate::engine::backend::chunks::get_chunks_batch(&engine_config(config), ids)
}

pub fn list_chunks(config: &Config, query: &ListChunksQuery) -> Result<Vec<Chunk>> {
    crate::engine::backend::chunks::list_chunks(&engine_config(config), query)
}

pub fn count_chunks(config: &Config) -> Result<u64> {
    crate::engine::backend::chunks::count_chunks(&engine_config(config))
}

pub fn extraction_coverage(config: &Config) -> Result<f32> {
    crate::engine::backend::chunks::extraction_coverage(&engine_config(config))
}

pub fn set_chunk_lifecycle_status(config: &Config, id: &str, status: &str) -> Result<()> {
    crate::engine::backend::chunks::set_chunk_lifecycle_status(&engine_config(config), id, status)
}

pub(crate) fn set_chunk_lifecycle_status_tx(
    tx: &Transaction<'_>,
    id: &str,
    status: &str,
) -> Result<()> {
    crate::engine::backend::chunks::set_chunk_lifecycle_status_tx(tx, id, status)
}

pub fn get_chunk_lifecycle_status(config: &Config, id: &str) -> Result<Option<String>> {
    crate::engine::backend::chunks::get_chunk_lifecycle_status(&engine_config(config), id)
}

pub fn get_chunk_lifecycle_status_tx(tx: &Transaction<'_>, id: &str) -> Result<Option<String>> {
    crate::engine::backend::chunks::get_chunk_lifecycle_status_tx(tx, id)
}

pub fn count_chunks_by_lifecycle_status(config: &Config, status: &str) -> Result<u64> {
    crate::engine::backend::chunks::count_chunks_by_lifecycle_status(&engine_config(config), status)
}

pub fn is_source_ingested(config: &Config, kind: SourceKind, id: &str) -> Result<bool> {
    crate::engine::backend::chunks::is_source_ingested(&engine_config(config), kind, id)
}

pub fn claim_source_ingest_tx(
    tx: &Transaction<'_>,
    kind: SourceKind,
    id: &str,
    now_ms: i64,
) -> Result<bool> {
    crate::engine::backend::chunks::claim_source_ingest_tx(tx, kind, id, now_ms)
}

pub fn mark_raw_paths_ingested(config: &Config, paths: &[String]) -> Result<u64> {
    crate::engine::backend::chunks::mark_raw_paths_ingested(&engine_config(config), paths)
}

pub fn filter_raw_paths_not_ingested(config: &Config, paths: &[String]) -> Result<Vec<String>> {
    crate::engine::backend::chunks::filter_raw_paths_not_ingested(&engine_config(config), paths)
}

pub fn count_raw_paths_ingested_with_prefix(config: &Config, prefix: &str) -> Result<u64> {
    crate::engine::backend::chunks::count_raw_paths_ingested_with_prefix(
        &engine_config(config),
        prefix,
    )
}

pub fn delete_chunks_by_source(config: &Config, kind: SourceKind, id: &str) -> Result<usize> {
    crate::engine::backend::chunks::delete_chunks_by_source(&engine_config(config), kind, id)
}

pub fn delete_chunks_by_source_prefix(
    config: &Config,
    kind: SourceKind,
    prefix: &str,
) -> Result<usize> {
    crate::engine::backend::chunks::delete_chunks_by_source_prefix(
        &engine_config(config),
        kind,
        prefix,
    )
}

pub fn delete_chunks_by_owner(config: &Config, kind: SourceKind, owner: &str) -> Result<usize> {
    crate::engine::backend::chunks::delete_chunks_by_owner(&engine_config(config), kind, owner)
}

pub fn delete_orphaned_source_tree(config: &Config, kind: SourceKind, id: &str) -> Result<bool> {
    crate::engine::backend::chunks::delete_orphaned_source_tree(&engine_config(config), kind, id)
}

#[path = "connection.rs"]
mod connection;
pub(crate) use connection::recover_corrupt_db;
pub use connection::with_connection;

#[path = "raw_refs.rs"]
mod raw_refs;
pub use raw_refs::{
    get_chunk_content_path, get_chunk_content_pointers, get_chunk_raw_refs,
    get_summary_content_pointers, list_chunk_raw_ref_paths_with_prefix,
    list_summaries_with_content_path, set_chunk_raw_refs, set_chunk_raw_refs_tx,
};

#[path = "embeddings.rs"]
mod embeddings;
pub use embeddings::{
    clear_chunk_reembed_skipped, clear_reembed_skipped_for_signature, get_chunk_embedding,
    get_chunk_embedding_for_signature, get_chunk_embeddings_batch,
    get_chunk_embeddings_for_signature_batch, mark_chunk_reembed_skipped, set_chunk_embedding,
    set_chunk_embedding_for_signature,
};
pub(crate) use embeddings::{
    has_uncovered_reembed_work, set_chunk_embedding_for_signature_tx, tree_active_signature,
};

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
