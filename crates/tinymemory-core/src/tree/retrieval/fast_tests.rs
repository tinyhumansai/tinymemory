//! Tests for deterministic fast retrieval and explicit source gating.

use std::collections::HashSet;

use tempfile::TempDir;
use tinymemory_api::host::test_support::TestHostConfig;

use super::{fast_retrieve, fast_retrieve_scoped, FastRetrieveOptions};

fn config() -> (TempDir, TestHostConfig) {
    crate::test_seams::init();
    let tmp = TempDir::new().unwrap();
    let mut config = TestHostConfig::default();
    config.workspace_dir = tmp.path().join("workspace");
    config.embeddings_provider = Some("none".into());
    (tmp, config)
}

#[tokio::test]
async fn empty_store_returns_well_formed_empty_responses_for_every_scope() {
    let (_tmp, config) = config();
    let options = FastRetrieveOptions {
        limit: 5,
        max_hops: 2,
        ..Default::default()
    };
    let unrestricted = fast_retrieve(&config, "missing subject", options.clone())
        .await
        .unwrap();
    assert!(unrestricted.hits.is_empty());
    assert_eq!(unrestricted.total, 0);
    assert!(!unrestricted.truncated);

    let denied = fast_retrieve_scoped(&config, "missing subject", options, Some(HashSet::new()))
        .await
        .unwrap();
    assert!(denied.hits.is_empty());
    assert_eq!(denied.total, 0);
}

#[tokio::test]
async fn ambient_empty_source_scope_remains_fail_closed() {
    let (_tmp, config) = config();
    let response = crate::source_scope::with_source_scope(
        Some(Vec::new()),
        fast_retrieve(&config, "anything", FastRetrieveOptions::default()),
    )
    .await
    .unwrap();
    assert!(response.hits.is_empty());
}
