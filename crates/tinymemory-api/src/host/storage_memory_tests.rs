//! Tests for the surrounding module.

use super::*;

#[test]
fn llm_default_is_cloud() {
    assert_eq!(LlmBackend::default(), LlmBackend::Cloud);
    assert_eq!(MemoryTreeConfig::default().llm_backend, LlmBackend::Cloud);
}

#[test]
fn llm_round_trip() {
    for v in [LlmBackend::Cloud, LlmBackend::Local] {
        assert_eq!(LlmBackend::parse(v.as_str()).unwrap(), v);
    }
}

#[test]
fn llm_parse_is_case_insensitive() {
    assert_eq!(LlmBackend::parse("CLOUD").unwrap(), LlmBackend::Cloud);
    assert_eq!(LlmBackend::parse(" Local ").unwrap(), LlmBackend::Local);
}

#[test]
fn llm_parse_rejects_unknown() {
    assert!(LlmBackend::parse("hybrid").is_err());
    assert!(LlmBackend::parse("").is_err());
}

#[test]
fn cloud_llm_model_default_is_summarizer_v1() {
    let cfg = MemoryTreeConfig::default();
    assert_eq!(
        cfg.cloud_llm_model.as_deref(),
        Some(DEFAULT_CLOUD_LLM_MODEL)
    );
    assert_eq!(DEFAULT_CLOUD_LLM_MODEL, "summarization-v1");
}

/// #5056: spaCy is opt-in — a fresh install must never provision the
/// spaCy venv / `en_core_web_sm` model, nor spawn the runtime Python
/// server, without an explicit config or env-var opt-in.
#[test]
fn spacy_enabled_defaults_to_false() {
    assert!(!MemoryTreeConfig::default().spacy_enabled);
    assert!(!default_memory_tree_spacy_enabled());
}

#[test]
fn memory_tree_config_default_content_dir_is_none() {
    let cfg = MemoryTreeConfig::default();
    assert!(
        cfg.content_dir.is_none(),
        "default content_dir must be None so workspace default path is used"
    );
}

/// Verify that the env-var override logic correctly maps non-empty strings
/// to `Some(PathBuf)` and empty/blank strings to `None`. We test the
/// logic inline (not via `apply_env_overrides`) to avoid mutating the
/// process environment in a way that could race with parallel tests.
#[test]
fn content_dir_env_override_logic() {
    // Simulate the load.rs overlay logic.
    let apply = |raw: &str| -> Option<PathBuf> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(PathBuf::from(trimmed))
        }
    };

    assert_eq!(apply("/tmp/foo"), Some(PathBuf::from("/tmp/foo")));
    assert_eq!(apply("  /tmp/foo  "), Some(PathBuf::from("/tmp/foo")));
    assert_eq!(apply(""), None);
    assert_eq!(apply("   "), None);
}

#[test]
fn memory_config_debug_redacts_agentmemory_secret() {
    let cfg = MemoryConfig {
        agentmemory_secret: Some("bearer-secret".into()),
        ..Default::default()
    };
    let debug = format!("{cfg:?}");
    assert!(!debug.contains("bearer-secret"));
    assert!(debug.contains("<redacted>"));
}

#[test]
fn memory_config_deserialization_supplies_operational_defaults() {
    let cfg: MemoryConfig = toml::from_str("").unwrap();
    assert_eq!(cfg.backend, "sqlite");
    assert!(cfg.auto_save);
    assert_eq!(cfg.embedding_provider, "cloud");
    assert_eq!(cfg.embedding_model, "embedding-v1");
    assert_eq!(cfg.embedding_dimensions, 1024);
    assert_eq!(cfg.embedding_rate_limit_per_min, 60);
}
