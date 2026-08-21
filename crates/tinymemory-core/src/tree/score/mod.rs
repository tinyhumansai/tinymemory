//! Product adapters over tinycortex scoring and admission.

pub mod embed;
pub mod extract;
pub mod store;

use std::sync::Arc;

pub use crate::engine::backend::score::{
    persist_score_tx, score_chunk, score_chunks, score_chunks_fast, ScoreResult, ScoringConfig,
    DEFAULT_DEFINITE_DROP, DEFAULT_DEFINITE_KEEP, DEFAULT_DROP_THRESHOLD, PRIORITY_BOOST,
    PRIORITY_TAG,
};
pub use crate::engine::backend::score::{resolver, signals};
pub use anyhow::Result;

/// Build crate scoring policy from product inference routing.
pub fn scoring_config_from(config: &crate::Config) -> ScoringConfig {
    let (provider, model) = match crate::chat::build_chat_runtime(config) {
        Ok((provider, model)) => (
            Arc::new(crate::engine::SeamChatProvider::new(provider))
                as Arc<dyn crate::engine::backend::score::extract::ChatProvider>,
            model,
        ),
        Err(error) => {
            log::warn!(
                "[memory::score] chat provider unavailable; using regex-only scoring: {error:#}"
            );
            return ScoringConfig::default_regex_only();
        }
    };
    let extractor = crate::engine::backend::score::extract::LlmEntityExtractor::new(
        crate::engine::backend::score::extract::LlmExtractorConfig {
            model,
            output_language: config.output_language().map(str::to_string),
            ..Default::default()
        },
        provider,
    );
    ScoringConfig::with_llm_extractor(Arc::new(extractor))
}

#[cfg(test)]
#[path = "score_tests.rs"]
mod tests;
