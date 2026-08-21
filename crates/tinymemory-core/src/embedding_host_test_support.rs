//! Test-only embedding host with deterministic no-op providers.

use super::*;

#[derive(Debug, Clone, Copy)]
pub(crate) struct TestEmbeddingHost;

impl TestEmbeddingHost {
    pub(crate) const CLOUD_MODEL: &'static str = "test-cloud-embed";
    pub(crate) const CLOUD_DIMENSIONS: usize = 1024;
    pub(crate) fn install() {
        set_embedding_host(Arc::new(Self));
    }
}

impl EmbeddingHost for TestEmbeddingHost {
    fn resolve_api_key(&self, _provider: &str) -> Option<String> {
        None
    }
    fn ollama_base_url(&self) -> String {
        std::env::var("OPENHUMAN_OLLAMA_BASE_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:11434".to_string())
    }
    fn default_embedding_provider(&self) -> Arc<dyn tinymemory_api::host::EmbeddingProvider> {
        Arc::new(tinymemory_api::host::NoopEmbedding)
    }
    fn create_embedding_provider_with_credentials(
        &self,
        _provider: &str,
        _model: &str,
        _dims: usize,
        _api_key: &str,
        _custom_endpoint: Option<&str>,
    ) -> Result<Box<dyn tinymemory_api::host::EmbeddingProvider>, String> {
        Ok(Box::new(tinymemory_api::host::NoopEmbedding))
    }
    fn model_supports_dimensions(&self, model: &str) -> bool {
        model.starts_with("text-embedding-3-")
    }
    fn cloud_embedding_provider(
        &self,
        _model: &str,
        _dims: usize,
    ) -> Result<Box<dyn tinymemory_api::host::EmbeddingProvider>, String> {
        Ok(Box::new(tinymemory_api::host::NoopEmbedding))
    }
    fn default_cloud_embedding_model(&self) -> &str {
        Self::CLOUD_MODEL
    }
    fn default_cloud_embedding_dimensions(&self) -> usize {
        Self::CLOUD_DIMENSIONS
    }
    fn ollama_embedding_provider(
        &self,
        _base_url: &str,
        _model: &str,
        _dims: usize,
    ) -> Result<Box<dyn tinymemory_api::host::EmbeddingProvider>, String> {
        Ok(Box::new(tinymemory_api::host::NoopEmbedding))
    }
}
