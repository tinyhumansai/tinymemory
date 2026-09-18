//! [`TinyInferenceEmbeddingProvider`] — the adapter from TinyInference's embedding
//! model trait onto the seam's [`EmbeddingProvider`].
//!
//! It lives in this crate rather than in `tinymemory-api` because the contract
//! crate must stay dependency-light and cannot name `tinyinference-llm`; and rather
//! than in the host because the tree's embedder factory — which is core code —
//! builds Ollama models directly and needs to wrap them. The host re-exports it
//! from `inference::embeddings`, so every existing path there keeps resolving
//! and keeps naming this one type.

use async_trait::async_trait;
use tinyinference_embeddings::EmbeddingModel;

pub use tinymemory_api::host::{format_embedding_signature, EmbeddingProvider};

/// Compatibility adapter from the canonical TinyInference embedding model.
pub struct TinyInferenceEmbeddingProvider {
    model: Box<dyn EmbeddingModel>,
}

impl TinyInferenceEmbeddingProvider {
    pub fn new(model: impl EmbeddingModel + 'static) -> Self {
        Self {
            model: Box::new(model),
        }
    }

    pub fn boxed(model: impl EmbeddingModel + 'static) -> Box<dyn EmbeddingProvider> {
        Box::new(Self::new(model))
    }
}

#[async_trait]
impl EmbeddingProvider for TinyInferenceEmbeddingProvider {
    fn name(&self) -> &str {
        self.model.name()
    }

    fn model_id(&self) -> &str {
        self.model.model_id()
    }

    fn dimensions(&self) -> usize {
        self.model.dimensions()
    }

    fn signature(&self) -> String {
        self.model.signature()
    }

    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        let owned = texts
            .iter()
            .map(|text| (*text).to_owned())
            .collect::<Vec<_>>();
        self.model
            .embed(&owned)
            .await
            .map_err(|error| anyhow::anyhow!(error))
    }
}
