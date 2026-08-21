//! Phase 4 embedding layer (#710).
//!
//! Produces a fixed-dimension vector per chunk / summary so retrieval can
//! rerank candidates by semantic similarity. Phase 4's default backend is a
//! local [Ollama](https://ollama.com) endpoint running `bge-m3`;
//! tests use the deterministic [`InertEmbedder`] so no network is required.
//!
//! Dimension is hard-coded at [`EMBEDDING_DIM`] (1024) — matches the
//! bge-m3 output and keeps the blob layout on `mem_tree_chunks` /
//! `mem_tree_summaries` consistent across providers. Mixing dimensions
//! mid-run would corrupt cosine comparisons; we catch that at the trait
//! level rather than deferring to retrieval-time diagnostics.
//!
//! NOTE: bge-m3 replaces the prior `nomic-embed-text` (768-dim, 2048
//! token context). Migration was driven by nomic's hard 2048-token
//! context cap causing long-chunk embed failures (chunker estimates
//! undercount BERT-WordPiece tokens by ~1.5-2× for HTML-derived
//! markdown, so 1500 chunker-tokens routinely exceed nomic's cap).
//! bge-m3 has a native 8192-token context. Existing `embedding` blobs
//! from the 768-dim era are invalid against the new dimension and
//! must be wiped or re-embedded.
//!
//! Write-time semantics: ingest + seal call [`Embedder::embed`] **before**
//! persisting the new row, so a provider error cascades into "don't write
//! this row". Legacy rows from Phases 1-3 predate embeddings and read back
//! with `Option::None`; retrieval tolerates that by dropping legacy rows
//! to the bottom of a semantic rerank.

use anyhow::{Context, Result};
use async_trait::async_trait;

pub mod factory;
pub mod inert;
pub mod openai_compat;

pub use factory::{build_embedder_from_config, build_write_embedder, effective_embedder_slug};
pub use inert::InertEmbedder;
pub use openai_compat::OpenAiCompatEmbedder;

/// Embedding dimensionality used across the memory tree.
///
/// Hard-coded to match `bge-m3`; swapping providers requires a matching
/// dimension or the trait's post-call validation will bail. Any change
/// to this constant breaks on-disk compatibility with existing
/// `mem_tree_chunks.embedding` / `mem_tree_summaries.embedding` blobs.
pub const EMBEDDING_DIM: usize = 1024;

/// Trait backing all Phase 4 embedders. Implementations MUST produce
/// exactly [`EMBEDDING_DIM`] floats per call — callers that persist the
/// result rely on the fixed layout.
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Stable short name, used in debug logs and provider diagnostics.
    fn name(&self) -> &'static str;

    /// Embed one text. Must return a `Vec<f32>` of length
    /// [`EMBEDDING_DIM`]. Hard failure — ingest / seal treat `Err` as
    /// "don't persist the row" so retries stay idempotent on `chunk_id`.
    async fn embed(&self, text: &str) -> Result<Vec<f32>>;

    /// Embed many texts, returning **one [`Result`] per input position**
    /// aligned by index. A single failing text does not strand the rest of
    /// the batch — its slot carries the `Err` while the others succeed —
    /// which lets bulk callers (e.g. the re-embed backfill) attribute and
    /// skip individual rows exactly as a per-text loop would.
    ///
    /// The default implementation issues one sequential [`Embedder::embed`]
    /// call per text: correct for any provider, but with no batching win.
    /// Providers whose backend accepts many texts in a single request
    /// (cloud / OpenAI-compatible) override this to collapse N network
    /// round-trips into one — see `embed_batch_via_provider`.
    ///
    /// The returned vector always has `texts.len()` elements.
    async fn embed_batch(&self, texts: &[&str]) -> Vec<Result<Vec<f32>>> {
        log::debug!(
            "[memory_tree::embed::{}] embed_batch:enter sequential texts={}",
            self.name(),
            texts.len()
        );
        let mut out = Vec::with_capacity(texts.len());
        for text in texts {
            out.push(self.embed(text).await);
        }
        out
    }
}

/// Adapts the canonical host embedding-provider contract to the legacy
/// memory-tree embedder shape. Concrete network implementations live in
/// `tinyagents::harness::embeddings`; this bridge owns only dimension checks
/// and the memory tree's per-position batch fallback contract.
pub struct ProviderEmbedder {
    inner: Box<dyn tinymemory_api::host::EmbeddingProvider>,
    label: &'static str,
}

impl ProviderEmbedder {
    pub fn new(
        inner: Box<dyn tinymemory_api::host::EmbeddingProvider>,
        label: &'static str,
    ) -> Self {
        Self { inner, label }
    }
}

#[async_trait]
impl Embedder for ProviderEmbedder {
    fn name(&self) -> &'static str {
        self.label
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        self.inner
            .embed_one(text)
            .await
            .with_context(|| format!("{} embeddings failed", self.label))
            .and_then(|vector| check_embed_dim(vector, self.label))
    }

    async fn embed_batch(&self, texts: &[&str]) -> Vec<Result<Vec<f32>>> {
        embed_batch_via_provider(self.inner.as_ref(), self.label, texts).await
    }
}

/// Validate that a freshly-produced embedding has exactly [`EMBEDDING_DIM`]
/// floats, returning a labelled error otherwise. Shared by the per-text and
/// batched provider adapters so the "wrong dims" diagnostic is identical
/// regardless of path.
pub(crate) fn check_embed_dim(v: Vec<f32>, label: &str) -> Result<Vec<f32>> {
    if v.len() != EMBEDDING_DIM {
        anyhow::bail!(
            "{label} embedder returned {} dims, expected {}",
            v.len(),
            EMBEDDING_DIM
        );
    }
    Ok(v)
}

/// Voyage batch API limits (conservative estimates).
const MAX_BATCH_ITEMS: usize = 1000;
const MAX_BATCH_TOKENS: usize = 1_000_000;
const CHARS_PER_TOKEN_ESTIMATE: usize = 4;

fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(CHARS_PER_TOKEN_ESTIMATE)
}

/// Split `texts` into sub-batches that respect the batch API limits:
/// at most `MAX_BATCH_ITEMS` items per batch and at most
/// `MAX_BATCH_TOKENS` estimated tokens per batch.
fn split_into_sub_batches<'a>(texts: &[&'a str]) -> Vec<Vec<&'a str>> {
    let mut batches: Vec<Vec<&'a str>> = Vec::new();
    let mut current: Vec<&'a str> = Vec::new();
    let mut current_tokens: usize = 0;

    for &text in texts {
        let tokens = estimate_tokens(text);
        if !current.is_empty()
            && (current.len() >= MAX_BATCH_ITEMS || current_tokens + tokens > MAX_BATCH_TOKENS)
        {
            batches.push(std::mem::take(&mut current));
            current_tokens = 0;
        }
        current.push(text);
        current_tokens += tokens;
    }
    if !current.is_empty() {
        batches.push(current);
    }
    batches
}

/// Batch-embed `texts` through a unified [`EmbeddingProvider`], splitting
/// into sub-batches that respect the batch API limits (1000 items, ~1M
/// tokens per request).
///
/// Each sub-batch is sent as a single provider `embed()` call. On a
/// wholesale batch failure **or** a length-contract violation, the failing
/// sub-batch falls back to per-text [`EmbeddingProvider::embed_one`] so a
/// single transient blip cannot fail — and, in the backfill, *tombstone* —
/// every row in the batch.
pub(crate) async fn embed_batch_via_provider(
    inner: &dyn tinymemory_api::host::EmbeddingProvider,
    label: &str,
    texts: &[&str],
) -> Vec<Result<Vec<f32>>> {
    if texts.is_empty() {
        return Vec::new();
    }

    let sub_batches = split_into_sub_batches(texts);
    log::debug!(
        "[memory_tree::embed::{label}] embed_batch:enter texts={} sub_batches={}",
        texts.len(),
        sub_batches.len()
    );

    let mut all_results: Vec<Result<Vec<f32>>> = Vec::with_capacity(texts.len());

    for (batch_idx, batch) in sub_batches.iter().enumerate() {
        let batch_results = embed_one_sub_batch(inner, label, batch, batch_idx).await;
        all_results.extend(batch_results);
    }

    all_results
}

/// Embed a single sub-batch via the provider, with per-text fallback on
/// batch failure.
async fn embed_one_sub_batch(
    inner: &dyn tinymemory_api::host::EmbeddingProvider,
    label: &str,
    texts: &[&str],
    batch_idx: usize,
) -> Vec<Result<Vec<f32>>> {
    match inner.embed(texts).await {
        Ok(vectors) if vectors.len() == texts.len() => {
            log::debug!(
                "[memory_tree::embed::{label}] embed_batch:success sub_batch={batch_idx} \
                 collapsed {} texts into one provider call",
                texts.len()
            );
            vectors
                .into_iter()
                .map(|v| check_embed_dim(v, label))
                .collect()
        }
        Ok(vectors) => {
            log::warn!(
                "[memory_tree::embed::{label}] embed_batch:fallback sub_batch={batch_idx} \
                 returned {} vectors for {} texts; falling back to per-text embedding",
                vectors.len(),
                texts.len()
            );
            embed_each_via_provider(inner, label, texts).await
        }
        Err(e) => {
            log::warn!(
                "[memory_tree::embed::{label}] embed_batch:fallback sub_batch={batch_idx} \
                 batch embed failed ({e:#}); falling back to per-text embedding"
            );
            embed_each_via_provider(inner, label, texts).await
        }
    }
}

/// Sequential per-text fallback used when a provider's native batch call is
/// unavailable or fails wholesale. Each slot is dimension-checked so the
/// result is interchangeable with the happy-path mapping in
/// [`embed_batch_via_provider`].
async fn embed_each_via_provider(
    inner: &dyn tinymemory_api::host::EmbeddingProvider,
    label: &str,
    texts: &[&str],
) -> Vec<Result<Vec<f32>>> {
    let mut out = Vec::with_capacity(texts.len());
    for text in texts {
        let result = inner
            .embed_one(text)
            .await
            .with_context(|| format!("{label} embeddings failed"))
            .and_then(|v| check_embed_dim(v, label));
        out.push(result);
    }
    out
}

/// Cosine similarity between two equal-length vectors.
///
/// Returns `0.0` when either vector has zero magnitude (including empty
/// vectors) to keep the rerank sort stable instead of surfacing `NaN`.
/// Length mismatch also returns `0.0` — callers upstream of the
/// comparison should normalise to [`EMBEDDING_DIM`] before calling.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0_f32;
    let mut na = 0.0_f32;
    let mut nb = 0.0_f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// Pack a `Vec<f32>` into little-endian bytes for SQLite BLOB storage.
///
/// Output length is `v.len() * 4`. The inverse is [`unpack_embedding`].
pub fn pack_embedding(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for f in v {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

/// Unpack little-endian bytes into a `Vec<f32>`.
///
/// Errors when the byte length isn't a multiple of 4 or doesn't match
/// [`EMBEDDING_DIM`] (after decoding). The latter guards against rows
/// written with a mismatched-provider blob silently passing as valid.
pub fn unpack_embedding(b: &[u8]) -> Result<Vec<f32>> {
    if !b.len().is_multiple_of(4) {
        anyhow::bail!(
            "embedding blob length {} not a multiple of 4 — corrupt row",
            b.len()
        );
    }
    let (chunks, _remainder) = b.as_chunks::<4>();
    let floats: Vec<f32> = chunks.iter().map(|c| f32::from_le_bytes(*c)).collect();
    if floats.len() != EMBEDDING_DIM {
        anyhow::bail!(
            "embedding blob length {} floats, expected {}",
            floats.len(),
            EMBEDDING_DIM
        );
    }
    Ok(floats)
}

/// Pack helper that also validates the input dimension before storing.
/// Used by write-time call sites where we want a loud error if a provider
/// misbehaves rather than writing a differently-shaped blob.
pub fn pack_checked(v: &[f32]) -> Result<Vec<u8>> {
    if v.len() != EMBEDDING_DIM {
        anyhow::bail!(
            "embedding vector has {} dims, expected {}",
            v.len(),
            EMBEDDING_DIM
        );
    }
    Ok(pack_embedding(v))
}

/// Decode a possibly-NULL embedding blob straight from a query row.
/// Returns `Ok(None)` for NULL (legacy rows predating Phase 4) and
/// surfaces decoding errors with context so the caller sees which row
/// was malformed.
pub fn decode_optional_blob(
    blob: Option<Vec<u8>>,
    context_label: &str,
) -> Result<Option<Vec<f32>>> {
    match blob {
        None => Ok(None),
        Some(bytes) => {
            let v = unpack_embedding(&bytes)
                .with_context(|| format!("decode embedding for {context_label}"))?;
            Ok(Some(v))
        }
    }
}

#[cfg(test)]
#[path = "embed_tests.rs"]
mod tests;
