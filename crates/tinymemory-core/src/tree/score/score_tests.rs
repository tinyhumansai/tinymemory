//! Tests for product scoring-policy construction.

use super::*;
use crate::tree::score::extract::EntityExtractor;

#[tokio::test]
async fn missing_chat_runtime_builds_the_safe_regex_only_policy() {
    let config = tinymemory_api::host::test_support::TestHostConfig::default();
    let scoring = scoring_config_from(&config);
    assert!(scoring.llm_extractor.is_none());
    assert_eq!(scoring.drop_threshold, DEFAULT_DROP_THRESHOLD);
    assert_eq!(scoring.definite_keep_threshold, DEFAULT_DEFINITE_KEEP);
    assert_eq!(scoring.definite_drop_threshold, DEFAULT_DEFINITE_DROP);
    assert_eq!(scoring.extractor.name(), "composite");
    let extracted = EntityExtractor::extract(&*scoring.extractor, "alice@example.com")
        .await
        .expect("regex-only scoring extractor");
    assert!(!extracted.entities.is_empty());
}
