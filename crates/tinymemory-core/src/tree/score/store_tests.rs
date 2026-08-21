//! Round-trip tests for the product score and entity-index adapters.

use super::*;
use crate::engine::backend::score::extract::EntityKind;
use crate::engine::backend::score::resolver::CanonicalEntity;
use crate::engine::backend::score::signals::ScoreSignals;

fn test_config() -> (
    tempfile::TempDir,
    tinymemory_api::host::test_support::TestHostConfig,
) {
    crate::test_seams::init();
    let directory = tempfile::tempdir().expect("temporary workspace");
    let mut config = tinymemory_api::host::test_support::TestHostConfig::default();
    config.workspace_dir = directory.path().to_path_buf();
    (directory, config)
}

fn score(chunk_id: &str, total: f32) -> ScoreRow {
    ScoreRow {
        chunk_id: chunk_id.into(),
        total,
        signals: ScoreSignals {
            token_count: 0.2,
            unique_words: 0.3,
            metadata_weight: 0.4,
            source_weight: 0.5,
            interaction: 0.6,
            entity_density: 0.7,
            llm_importance: 0.8,
        },
        dropped: total < 0.5,
        reason: Some("test rationale".into()),
        computed_at_ms: 1_700_000_000_000,
        llm_importance_reason: Some("not persisted".into()),
    }
}

#[test]
fn score_adapters_round_trip_upsert_batch_and_count() {
    let (_directory, config) = test_config();
    assert_eq!(count_scores(&config).expect("initial count"), 0);
    assert!(get_score(&config, "missing")
        .expect("missing score")
        .is_none());

    upsert_score(&config, &score("chunk-a", 0.25)).expect("insert first score");
    upsert_score(&config, &score("chunk-b", 0.75)).expect("insert second score");
    upsert_score(&config, &score("chunk-a", 0.9)).expect("replace first score");

    assert_eq!(count_scores(&config).expect("score count"), 2);
    let stored = get_score(&config, "chunk-a")
        .expect("read score")
        .expect("stored score");
    assert_eq!(stored.total, 0.9);
    assert!(!stored.dropped);
    assert_eq!(stored.reason.as_deref(), Some("test rationale"));
    assert_eq!(stored.computed_at_ms, 1_700_000_000_000);
    assert_eq!(stored.signals.token_count, 0.2);
    assert_eq!(stored.signals.unique_words, 0.3);
    assert_eq!(stored.signals.metadata_weight, 0.4);
    assert_eq!(stored.signals.source_weight, 0.5);
    assert_eq!(stored.signals.interaction, 0.6);
    assert_eq!(stored.signals.entity_density, 0.7);
    assert_eq!(stored.signals.llm_importance, 0.0);
    assert_eq!(stored.llm_importance_reason, None);
    let batch = get_scores_batch(
        &config,
        &["chunk-b".into(), "missing".into(), "chunk-a".into()],
    )
    .expect("batch scores");
    assert_eq!(batch.len(), 2);
    assert_eq!(batch["chunk-a"], 0.9);
    assert_eq!(batch["chunk-b"], 0.75);
}

#[test]
fn entity_adapters_preserve_kind_span_scope_and_lifecycle() {
    let (_directory, config) = test_config();
    let alice = CanonicalEntity {
        canonical_id: "person:alice".into(),
        kind: EntityKind::Person,
        surface: "Alice".into(),
        span_start: 4,
        span_end: 9,
        score: 0.95,
    };
    let rust = CanonicalEntity {
        canonical_id: "technology:rust".into(),
        kind: EntityKind::Technology,
        surface: "Rust".into(),
        span_start: 14,
        span_end: 18,
        score: 0.9,
    };
    let converted = to_store_entity(&alice).expect("convert resolver entity");
    assert_eq!(converted.canonical_id, "person:alice");
    assert_eq!(converted.kind.as_str(), "person");
    assert_eq!(converted.surface, "Alice");
    assert_eq!(converted.span_start, 4);
    assert_eq!(converted.span_end, 9);
    assert_eq!(converted.score, 0.95);

    index_entity(&config, &alice, "node-a", "chunk", 100, Some("tree-a"))
        .expect("index one entity");
    assert_eq!(
        index_entities(
            &config,
            &[alice.clone(), rust],
            "node-b",
            "summary",
            200,
            Some("tree-a"),
        )
        .expect("index entity batch"),
        2
    );
    assert_eq!(count_entity_index(&config).expect("entity count"), 3);
    assert_eq!(
        list_entity_ids_for_node(&config, "node-b").expect("node entities"),
        vec!["person:alice", "technology:rust"]
    );
    let hits = lookup_entity(&config, "person:alice", None).expect("entity hits");
    assert_eq!(hits.len(), 2);
    let chunk_hit = hits
        .iter()
        .find(|hit| hit.node_id == "node-a")
        .expect("chunk occurrence");
    assert_eq!(chunk_hit.node_kind, "chunk");
    assert_eq!(chunk_hit.entity_kind.as_str(), "person");
    assert_eq!(chunk_hit.surface, "Alice");
    assert_eq!(chunk_hit.score, 0.95);
    assert_eq!(chunk_hit.timestamp_ms, 100);
    assert_eq!(chunk_hit.tree_id.as_deref(), Some("tree-a"));
    let summary_hit = hits
        .iter()
        .find(|hit| hit.node_id == "node-b")
        .expect("summary occurrence");
    assert_eq!(summary_hit.node_kind, "summary");
    assert_eq!(summary_hit.surface, "Alice");
    assert_eq!(summary_hit.score, 0.95);
    assert_eq!(summary_hit.timestamp_ms, 200);
    assert_eq!(summary_hit.tree_id.as_deref(), Some("tree-a"));
    let rust_hits = lookup_entity(&config, "technology:rust", None).expect("technology hits");
    assert_eq!(rust_hits.len(), 1);
    assert_eq!(rust_hits[0].surface, "Rust");
    assert_eq!(rust_hits[0].score, 0.9);

    let other_scope = CanonicalEntity {
        canonical_id: "person:mallory".into(),
        kind: EntityKind::Person,
        surface: "Mallory".into(),
        span_start: 2,
        span_end: 9,
        score: 0.8,
    };
    index_entity(
        &config,
        &other_scope,
        "node-c",
        "chunk",
        300,
        Some("tree-b"),
    )
    .expect("index other scope");
    let tree_a = crate::store::entities::namespace_entities(&config, "tree-a", None, 10)
        .expect("tree-a entities");
    assert!(tree_a.iter().all(|entity| entity.id != "person:mallory"));
    assert_eq!(
        crate::store::entities::namespace_entities(&config, "tree-b", None, 10)
            .expect("tree-b entities")[0]
            .id,
        "person:mallory"
    );
    assert_eq!(
        clear_entity_index_for_node(&config, "node-b").expect("clear node entities"),
        2
    );
    assert!(list_entity_ids_for_node(&config, "node-b")
        .expect("cleared node")
        .is_empty());
    assert_eq!(
        list_entity_ids_for_node(&config, "node-a").expect("preserved node"),
        vec!["person:alice"]
    );
    assert_eq!(
        lookup_entity(&config, "person:alice", None)
            .expect("remaining hit")
            .len(),
        1
    );
    assert_eq!(count_entity_index(&config).expect("remaining entities"), 2);
}
