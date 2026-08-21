//! Tests for product construction of entity extractors.

use super::*;
use anyhow::Result;

struct StaticChat;

#[async_trait]
impl crate::chat::ChatProvider for StaticChat {
    fn name(&self) -> &str {
        "static-chat"
    }

    async fn chat_for_json(&self, _prompt: &crate::chat::ChatPrompt) -> Result<String> {
        Ok(r#"{"entities":[{"kind":"person","text":"Alice"}],"topics":["Planning"],"importance":0.8,"importance_reason":"relevant"}"#.into())
    }
}

#[tokio::test]
async fn llm_adapter_preserves_name_and_extracts_through_the_host_chat_seam() {
    let extractor = LlmEntityExtractor::new(
        LlmExtractorConfig {
            emit_topics: true,
            ..Default::default()
        },
        Arc::new(StaticChat),
    );
    assert_eq!(extractor.name(), "llm");
    let extracted = extractor
        .extract("Alice discussed Planning.")
        .await
        .expect("soft-fallible extraction");
    assert_eq!(extracted.entities.len(), 1);
    assert_eq!(extracted.entities[0].text, "Alice");
    assert_eq!(extracted.topics.len(), 1);
}

#[tokio::test]
async fn summary_builder_falls_back_to_a_usable_regex_extractor_without_chat_config() {
    let config = tinymemory_api::host::test_support::TestHostConfig::default();
    let extractor = build_summary_extractor(&config);
    assert_eq!(extractor.name(), "composite");
    let extracted = extractor
        .extract("Email alice@example.com about #Launch")
        .await
        .expect("regex extraction");
    assert!(!extracted.entities.is_empty());
}
