//! `Config` → [`tinycortex::memory::MemoryConfig`] mapping (W1).
//!
//! The crate's [`MemoryConfig`] is the single input every engine primitive
//! takes (`workspace`, embedding dims/model/strict, tree budgets, retrieval
//! weight profile, sync budget). This adapter derives it from OpenHuman's
//! host [`Config`] plus the resolved memory workspace root, so the rest of the
//! seam constructs engine calls from real product configuration.
//!
//! Field provenance:
//! - `workspace` ← the memory workspace root (same root `MemoryClient` opens).
//! - `embedding.dim` ← `config.memory().embedding_dimensions`.
//! - `embedding.provider` ← [`effective_embedder_slug`], the slug the embedder
//!   ladder actually resolves to — **not** `config.memory().embedding_provider`.
//!   See the note on the mapping below; reading that field here would mis-key
//!   every locally-embedded row.
//! - `embedding.model` ← `config.memory().embedding_model`.
//! - `embedding.strict` ← `config.memory_tree().embedding_strict` (when false the
//!   engine tolerates an inert embedder and falls back to scope+recency rerank).
//! - `tree` / `retrieval` / `sync_budget` ← crate defaults, which already match
//!   the host engine's constants (`INPUT_TOKEN_BUDGET = 50_000`,
//!   `OUTPUT_TOKEN_BUDGET = 5_000`, `SUMMARY_FANOUT = 10`,
//!   `DEFAULT_FLUSH_AGE_SECS = 604_800`). The `tree_policy.rs` flavour overlays
//!   and per-source `WeightProfile` selection are layered on at call sites in
//!   later workstreams; this base mapping is the W1 foundation.

use std::path::PathBuf;

use tinycortex::memory::config::EmbeddingConfig;
use tinycortex::memory::MemoryConfig;

#[cfg(test)]
use tinymemory_api::host::test_support::TestHostConfig;

use crate::tree::score::embed::effective_embedder_slug;
use crate::Config;

/// Build a [`MemoryConfig`] from the host [`Config`] and the resolved memory
/// workspace root.
///
/// `workspace` is the directory under which the engine stores `chunks.db`, the
/// content vault, and the tree DBs — it must be the same root the host
/// `MemoryClient` opens so an existing user workspace is read in place (parity
/// is gated by the W3 golden-workspace harness).
pub fn memory_config_from(config: &Config, workspace: PathBuf) -> MemoryConfig {
    let mut mc = MemoryConfig::new(workspace);
    mc.content_root = Some(config.memory_tree_content_root());
    // `provider` is part of the signature every per-model sidecar row is keyed
    // by, so it has to name the backend that actually produced the vectors —
    // two backends serving one model id must never share a vector space.
    //
    // That is `effective_embedder_slug`, which walks the same resolution ladder
    // the read and write factories walk, and NOT `config.memory()
    // .embedding_provider`: the ladder resolves local Ollama from
    // `memory_tree.embedding_endpoint` or the unified
    // `workload_local_model("embeddings")` setting, and neither path rewrites
    // that field — so a user embedding entirely locally still reads as `"cloud"`
    // there. Keying on it would file local vectors under the cloud provider.
    mc.embedding = EmbeddingConfig {
        dim: config.memory().embedding_dimensions,
        provider: effective_embedder_slug(config).to_string(),
        model: config.memory().embedding_model.clone(),
        strict: config.memory_tree().embedding_strict,
    };
    mc
}

/// Build a [`MemoryConfig`] rooted at the host's own `workspace_dir`.
///
/// This is the shape ~15 `memory/**` adapter modules each used to re-declare as
/// a private `fn engine_config` / `fn memory_config` / `fn config`; they were
/// byte-identical, so they now all call this. Use [`memory_config_from`]
/// directly only when the workspace root is *not* `config.workspace_dir()` (the
/// sync/rebuild paths that address an alternate root).
pub fn engine_config(config: &Config) -> MemoryConfig {
    memory_config_from(config, config.workspace_dir().clone())
}

#[cfg(test)]
mod tests {
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
}
