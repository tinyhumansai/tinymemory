//! Tests for sealing's embedder and observer adapter boundaries.

use super::*;
use crate::store::trees::types::{TreeKind, TreeStatus};
use crate::tree::score::embed::{Embedder, EMBEDDING_DIM};
use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use tempfile::TempDir;
use tinycortex::memory::tree::SealObserver;
use tinymemory_api::host::test_support::TestHostConfig;

struct FixedEmbedder {
    dimensions: usize,
    fails: bool,
}

#[async_trait]
impl Embedder for FixedEmbedder {
    fn name(&self) -> &'static str {
        "fixed"
    }

    async fn embed(&self, _text: &str) -> Result<Vec<f32>> {
        if self.fails {
            anyhow::bail!("provider unavailable")
        }
        Ok(vec![0.25; self.dimensions])
    }
}

fn tree() -> Tree {
    Tree {
        id: "tree-1".into(),
        kind: TreeKind::Source,
        scope: "source-1".into(),
        ask: None,
        root_id: None,
        max_level: 0,
        status: TreeStatus::Active,
        created_at: Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        last_sealed_at: None,
    }
}

#[tokio::test]
async fn embedder_bridge_preserves_success_and_contextualizes_failures() {
    let valid = FixedEmbedder {
        dimensions: EMBEDDING_DIM,
        fails: false,
    };
    let bridge = EmbedderBridge(&valid);
    assert_eq!(
        tinycortex::memory::score::embed::Embedder::name(&bridge),
        "fixed"
    );
    assert_eq!(
        tinycortex::memory::score::embed::Embedder::embed(&bridge, "text")
            .await
            .unwrap()
            .len(),
        EMBEDDING_DIM
    );

    let wrong = FixedEmbedder {
        dimensions: 3,
        fails: false,
    };
    let error = tinycortex::memory::score::embed::Embedder::embed(&EmbedderBridge(&wrong), "text")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("dimension"));

    let failing = FixedEmbedder {
        dimensions: EMBEDDING_DIM,
        fails: true,
    };
    let error =
        tinycortex::memory::score::embed::Embedder::embed(&EmbedderBridge(&failing), "text")
            .await
            .unwrap_err();
    assert!(error.to_string().contains("seal embedding failed"));
}

#[test]
fn observer_progress_publishes_tree_build_event() {
    crate::test_seams::init();
    let tmp = TempDir::new().unwrap();
    let mut config = TestHostConfig::default();
    config.workspace_dir = tmp.path().join("workspace");
    let sink = crate::events::RecordingSink::install();
    Observer { config: &config }.progress(&tree(), "summarise", 2, Some(4));
    assert!(sink.drain().iter().any(|event| matches!(
        event,
        crate::events::MemoryEvent::TreeBuildProgress {
            phase,
            step,
            tree_scope: Some(scope),
            level: Some(2),
            item_count: Some(4),
            ..
        } if phase == "seal" && step == "summarise" && scope == "source-1"
    )));
}
