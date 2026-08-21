//! Tests for the surrounding module.

use super::*;

#[test]
fn maps_workspace_and_embedding_from_host_config() {
    let mut config = TestHostConfig::default();
    config.memory.embedding_dimensions = 1024;
    config.memory.embedding_model = "embedding-v1".to_string();
    config.memory_tree.embedding_strict = true;

    let workspace = PathBuf::from("/tmp/openhuman/ws");
    let mc = memory_config_from(&config, workspace.clone());

    assert_eq!(mc.workspace, workspace);
    assert_eq!(mc.embedding.dim, 1024);
    assert_eq!(mc.embedding.model, "embedding-v1");
    assert!(mc.embedding.strict);
}

#[test]
fn embedding_provider_is_the_resolved_slug_not_the_config_field() {
    // The regression this pins: `memory.embedding_provider` is not
    // authoritative. A user embedding entirely locally still reads as
    // `"cloud"` there, because neither local rung of the ladder rewrites
    // the field. Since `provider` keys the vector space, mapping it
    // straight through would file local vectors under the cloud provider.
    let mut config = TestHostConfig::default();
    config.memory.embedding_provider = "cloud".to_string();
    // Rung 1 of the ladder: an explicit Ollama endpoint + model.
    config.memory_tree.embedding_endpoint = Some("http://127.0.0.1:11434".to_string());
    config.memory_tree.embedding_model = Some("nomic-embed-text".to_string());

    let mc = memory_config_from(&config, PathBuf::from("/tmp/ws"));

    assert_eq!(
        mc.embedding.provider, "ollama",
        "locally-resolved embeddings must be keyed as ollama, not the \
         stale 'cloud' spelling in memory.embedding_provider"
    );
    assert_ne!(mc.embedding.provider, config.memory.embedding_provider);
}

#[test]
fn tree_defaults_match_engine_constants() {
    // The base mapping leaves tree budgets at the crate defaults, which are
    // the host engine's own constants — asserted here so a crate-side change
    // to those defaults surfaces as a failing parity test rather than a
    // silent behaviour drift.
    let mc = memory_config_from(&TestHostConfig::default(), PathBuf::from("/tmp/ws"));
    assert_eq!(mc.tree.input_token_budget, 50_000);
    assert_eq!(mc.tree.output_token_budget, 5_000);
    assert_eq!(mc.tree.summary_fanout, 10);
    assert_eq!(mc.tree.flush_age_secs, 604_800);
}

#[test]
fn engine_config_roots_at_host_workspace_dir() {
    // Pins the wrapper's only behavioural claim: identical to
    // `memory_config_from(config, config.workspace_dir().clone())`.
    let mut config = TestHostConfig::default();
    config.memory.embedding_dimensions = 768;
    config.memory_tree.embedding_strict = true;

    let via_wrapper = engine_config(&config);
    let via_explicit = memory_config_from(&config, config.workspace_dir.clone());

    assert_eq!(via_wrapper.workspace, config.workspace_dir);
    assert_eq!(via_wrapper.workspace, via_explicit.workspace);
    assert_eq!(via_wrapper.content_root, via_explicit.content_root);
    assert_eq!(via_wrapper.embedding.dim, via_explicit.embedding.dim);
    assert_eq!(via_wrapper.embedding.model, via_explicit.embedding.model);
    assert_eq!(via_wrapper.embedding.strict, via_explicit.embedding.strict);
}
