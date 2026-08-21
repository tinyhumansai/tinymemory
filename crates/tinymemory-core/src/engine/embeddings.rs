//! Embedding seam — bridge OpenHuman's [`EmbeddingProvider`] onto the crate's
//! two embedding traits (W1).
//!
//! OpenHuman owns the concrete providers (voyage / openai / cohere / ollama /
//! cloud / noop) and the `embeddings/factory.rs` construction policy (rate-limit
//! + retry). TinyCortex "never makes a network call" — it takes compute through
//!
//! [`EmbeddingBackend`] (the vector store) and [`Embedder`] (retrieval / seal
//! scoring). This adapter wraps one `Arc<dyn EmbeddingProvider>` and re-exposes
//! it as both, so the engine drives OpenHuman embeddings without cloning any
//! provider logic.
//!
//! The two host and crate contracts are shape-identical (`name` / `model_id` /
//! `dimensions` / `signature` / async `embed`), and both use `anyhow::Result`,
//! so this is a near-pure pass-through. Critically, `signature()` delegates to
//! the provider so the persisted embedding-space signature
//! (`provider=…;model=…;dims=…`) stays byte-identical whether the store keys off
//! the crate backend or the raw provider (#1574 fidelity).

use std::sync::Arc;

use async_trait::async_trait;
use tinycortex::memory::score::embed::Embedder;
use tinycortex::memory::store::vectors::EmbeddingBackend;

use tinymemory_api::host::EmbeddingProvider;

/// Wraps an OpenHuman [`EmbeddingProvider`] as the crate's [`EmbeddingBackend`]
/// (vector store) and [`Embedder`] (retrieval / seal scoring).
pub struct SeamEmbedder {
    provider: Arc<dyn EmbeddingProvider>,
}

impl SeamEmbedder {
    /// Build the adapter over an OpenHuman embedding provider, preserving its
    /// factory-configured rate-limit + retry policy.
    pub fn new(provider: Arc<dyn EmbeddingProvider>) -> Self {
        tracing::debug!(
            provider = provider.name(),
            model_id = provider.model_id(),
            dimensions = provider.dimensions(),
            signature = %provider.signature(),
            "[memory] constructing tinycortex embedding seam over EmbeddingProvider"
        );
        Self { provider }
    }
}

#[async_trait]
impl EmbeddingBackend for SeamEmbedder {
    fn name(&self) -> &str {
        self.provider.name()
    }

    fn model_id(&self) -> &str {
        self.provider.model_id()
    }

    fn dimensions(&self) -> usize {
        self.provider.dimensions()
    }

    /// Delegate to the provider so the persisted signature is byte-identical to
    /// the config-derived `active_embedding_signature` — a mismatch would split
    /// one embedding space into two (#1574).
    fn signature(&self) -> String {
        self.provider.signature()
    }

    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        self.provider.embed(texts).await
    }
}

#[async_trait]
impl Embedder for SeamEmbedder {
    fn name(&self) -> &'static str {
        // The crate's `Embedder` requires a `'static` name (debug/diagnostics
        // only); the provider's own `name()` is borrowed, so report a stable
        // seam label rather than leaking a lifetime.
        "openhuman-seam"
    }

    async fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        self.provider.embed_one(text).await
    }
}

#[cfg(test)]
#[path = "embeddings_tests.rs"]
mod tests;
