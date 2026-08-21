//! Product construction over tinycortex entity extraction.

use std::sync::Arc;

use crate::Config;
use async_trait::async_trait;

pub use crate::engine::backend::score::extract::{
    ChatPrompt, ChatProvider, CompositeExtractor, EntityExtractor, EntityKind, ExtractedEntities,
    ExtractedEntity, ExtractedTopic, LlmExtractorConfig, RegexEntityExtractor,
};

pub mod regex {
    pub use crate::engine::backend::score::extract::regex::extract;
}

pub struct LlmEntityExtractor(crate::engine::backend::score::extract::LlmEntityExtractor);

impl LlmEntityExtractor {
    pub fn new(config: LlmExtractorConfig, provider: Arc<dyn crate::chat::ChatProvider>) -> Self {
        let provider = Arc::new(crate::engine::SeamChatProvider::new(provider));
        Self(crate::engine::backend::score::extract::LlmEntityExtractor::new(config, provider))
    }
}

#[async_trait]
impl EntityExtractor for LlmEntityExtractor {
    fn name(&self) -> &'static str {
        self.0.name()
    }

    async fn extract(&self, text: &str) -> anyhow::Result<ExtractedEntities> {
        self.0.extract(text).await
    }
}

pub fn build_summary_extractor(config: &Config) -> Arc<dyn EntityExtractor> {
    let (provider, model) = match crate::chat::build_chat_runtime(config) {
        Ok(runtime) => runtime,
        Err(error) => {
            log::warn!(
                "[memory_tree::extract] chat provider unavailable; using regex-only extraction: {error:#}"
            );
            return Arc::new(CompositeExtractor::regex_only());
        }
    };
    let extractor = LlmEntityExtractor::new(
        LlmExtractorConfig {
            model,
            emit_topics: true,
            output_language: config.output_language().map(str::to_string),
            ..Default::default()
        },
        provider,
    );
    Arc::new(CompositeExtractor::new(vec![
        Box::new(RegexEntityExtractor),
        Box::new(extractor),
    ]))
}

#[cfg(test)]
#[path = "extract_tests.rs"]
mod tests;
