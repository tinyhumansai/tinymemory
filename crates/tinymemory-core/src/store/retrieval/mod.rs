//! Unified retrieval facade over the memory_store backends.
//!
//! `memory_store` owns four distinct retrieval modalities, each implemented in
//! a different submodule today:
//!
//! 1. **tree-walk** — BFS over sealed summary nodes (delegates to the
//!    existing drill_down logic in `memory_tree::retrieval::drill_down`).
//! 2. **vector search** — embedding-similarity ranking over namespace docs
//!    (delegates to `UnifiedMemory::query_namespace_hits`).
//! 3. **keyword search** — FTS5/keyword overlap, same hybrid entry point as
//!    vector (the hybrid scorer already blends both signals).
//! 4. **param/tag search** — structured filters over chunk metadata + content
//!    store tags (delegates to `chunks::store::list_chunks` and
//!    `content::tags`).
//!
//! The facade is a thin aggregation layer: it does NOT reimplement any
//! scoring or storage logic. It exists so callers have a single import surface
//! (`memory_store::retrieval::RetrievalFacade`) instead of reaching into four
//! different submodules.
//!
//! Layering note: `tree_walk` calls `memory_tree::retrieval::drill_down`, which is
//! a reverse dependency from `memory_store` up into `memory`. This is
//! intentional and bounded — `drill_down` is "tree walk over stored trees" and
//! conceptually belongs in `memory_store`, but moving it is out of scope for
//! the storage-extraction refactor. Revisit when drill_down's policy bits
//! (entity hits, source-vs-summary precedence) can be cleanly split from the
//! pure tree traversal.

use anyhow::Result;
use std::sync::Arc;

#[cfg(test)]
use tinymemory_api::host::test_support::TestHostConfig;

use crate::store::chunks::store::list_chunks;
use crate::store::chunks::types::{Chunk, SourceKind};
use crate::store::types::NamespaceMemoryHit;
use crate::store::UnifiedMemory;
use crate::tree::retrieval::types::RetrievalHit;
use crate::Config;

/// Optional filter set for `param_tag_search`. All `Some` fields are AND-ed
/// together; `None` fields are unconstrained.
#[derive(Debug, Default, Clone)]
pub struct ParamTagFilters {
    pub source_kind: Option<SourceKind>,
    pub source_id: Option<String>,
    pub owner: Option<String>,
    /// Inclusive lower bound on chunk `timestamp_ms`.
    pub since_ms: Option<i64>,
    /// Inclusive upper bound on chunk `timestamp_ms`.
    pub until_ms: Option<i64>,
    /// If `Some`, post-filter to chunks whose `tags` contains every listed tag.
    pub tags_all_of: Option<Vec<String>>,
    /// Max rows to return (default 100 when `None`).
    pub limit: Option<usize>,
}

/// Unified retrieval entry point. Construct with an `Arc<UnifiedMemory>` for
/// vector/keyword ops; tree-walk and param/tag ops only need `&Config`.
#[derive(Clone)]
pub struct RetrievalFacade {
    unified: Arc<UnifiedMemory>,
}

impl RetrievalFacade {
    pub fn new(unified: Arc<UnifiedMemory>) -> Self {
        Self { unified }
    }

    /// BFS walk from `node_id` down to `max_depth`. When `query` is `Some`,
    /// hits are reranked by cosine similarity to the query embedding.
    ///
    /// See `memory_tree::retrieval::drill_down::drill_down` for the full contract.
    pub async fn tree_walk(
        &self,
        config: &Config,
        node_id: &str,
        max_depth: u32,
        query: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Vec<RetrievalHit>> {
        crate::tree::retrieval::drill_down::drill_down(config, node_id, max_depth, query, limit)
            .await
    }

    /// Hybrid vector + graph + freshness retrieval. Same underlying scorer as
    /// `keyword_search`; the difference is purely semantic intent at the call
    /// site (callers using this entry point are saying "I have an embeddable
    /// query"). Returns the full ranked hit list.
    pub async fn vector_search(
        &self,
        namespace: &str,
        query: &str,
        limit: u32,
    ) -> Result<Vec<NamespaceMemoryHit>, String> {
        self.unified
            .query_namespace_hits(namespace, query, limit)
            .await
    }

    /// Same hybrid scorer as `vector_search` — the underlying retrieval plan
    /// blends keyword overlap and vector similarity in one pass. Exposed as a
    /// separate method so callers that only want lexical matching have an
    /// honest name; the result set is identical for any given query.
    pub async fn keyword_search(
        &self,
        namespace: &str,
        query: &str,
        limit: u32,
    ) -> Result<Vec<NamespaceMemoryHit>, String> {
        self.unified
            .query_namespace_hits(namespace, query, limit)
            .await
    }

    /// Structured chunk search by source/owner/time/tag filters. Bypasses the
    /// ranking pipeline entirely — results are timestamp-DESC ordered. Use
    /// when the caller knows the exact subset of chunks it wants.
    pub fn param_tag_search(
        &self,
        config: &Config,
        filters: &ParamTagFilters,
    ) -> Result<Vec<Chunk>> {
        let query = crate::store::chunks::store::ListChunksQuery {
            source_kind: filters.source_kind,
            source_id: filters.source_id.clone(),
            owner: filters.owner.clone(),
            since_ms: filters.since_ms,
            until_ms: filters.until_ms,
            limit: filters.limit,
            offset: None,
            source_scope: None,
            exclude_dropped: false,
        };
        let rows = list_chunks(config, &query)?;
        let Some(required) = filters.tags_all_of.as_ref() else {
            return Ok(rows);
        };
        if required.is_empty() {
            return Ok(rows);
        }
        Ok(rows
            .into_iter()
            .filter(|c| {
                required
                    .iter()
                    .all(|t| c.metadata.tags.iter().any(|ct| ct == t))
            })
            .collect())
    }
}

#[cfg(test)]
#[path = "retrieval_tests.rs"]
mod tests;
