//! Behavioral tests for the host graph persistence and traversal adapters.

use super::*;
use crate::store::chunks::store::with_connection;
use crate::tree::graph::store::count_edges;
use tinymemory_api::host::test_support::TestHostConfig;

fn fixture() -> (tempfile::TempDir, TestHostConfig) {
    let workspace = tempfile::tempdir().expect("workspace");
    let mut config = TestHostConfig::default();
    config.workspace_dir = workspace.path().join("memory");
    (workspace, config)
}

#[test]
fn graph_store_round_trips_neighbors_distances_and_transactional_clear() {
    let (_workspace, config) = fixture();
    let entities = vec!["alice".to_string(), "bob".to_string(), "carol".to_string()];
    let pairs = pairs_from_entities(&entities);
    assert_eq!(pairs.len(), 3);

    assert_eq!(upsert_edges(&config, &pairs, 100).expect("insert graph"), 3);
    assert_eq!(count_edges(&config).expect("count graph"), 3);
    assert_eq!(
        upsert_edges(&config, &pairs, 200).expect("increment graph"),
        3
    );

    let alice = neighbors(&config, "alice").expect("alice neighbors");
    assert_eq!(alice.len(), 2);
    assert!(alice.iter().all(|(_, weight)| *weight == 2));
    assert!(neighbors(&config, "missing")
        .expect("missing neighbors")
        .is_empty());

    let distances = pair_distances(&config, &entities, 1).expect("bounded distances");
    assert_eq!(distances.len(), 3);
    assert!(distances.iter().all(|distance| distance.dist == 1));

    with_connection(&config, |connection| {
        let transaction = connection.unchecked_transaction()?;
        assert_eq!(
            clear_edges_for_entities_tx(&transaction, &["alice".to_string()])?,
            2
        );
        assert_eq!(
            upsert_edges_tx(
                &transaction,
                &[("dave".to_string(), "erin".to_string())],
                300,
            )?,
            1
        );
        transaction.commit()?;
        Ok(())
    })
    .expect("transactional graph update");

    assert_eq!(count_edges(&config).expect("count after transaction"), 2);
    assert_eq!(
        neighbors(&config, "dave").expect("dave neighbors"),
        vec![("erin".to_string(), 1)]
    );
}

#[test]
fn empty_and_duplicate_entity_inputs_are_safe() {
    let (_workspace, config) = fixture();
    assert!(pairs_from_entities(&[]).is_empty());
    assert_eq!(upsert_edges(&config, &[], 0).expect("empty upsert"), 0);
    assert!(pair_distances(&config, &[], 3)
        .expect("empty traversal")
        .is_empty());
}
