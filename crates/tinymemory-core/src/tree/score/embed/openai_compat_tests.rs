//! Tests for the surrounding module.

use super::*;
use tempfile::TempDir;

fn cfg_with_provider(p: &str) -> (TempDir, TestHostConfig) {
    crate::test_seams::init();
    let tmp = TempDir::new().unwrap();
    let mut cfg = TestHostConfig::default();
    cfg.workspace_dir = tmp.path().to_path_buf();
    cfg.config_path = tmp.path().join("config.toml");
    cfg.memory.embedding_provider = p.to_string();
    cfg.memory.embedding_model = "text-embedding-3-large".to_string();
    (tmp, cfg)
}

#[test]
fn none_for_non_openai_providers() {
    // managed / voyage / ollama / none must fall through (Ok(None)).
    for p in ["managed", "cloud", "voyage", "ollama:bge-m3", "none"] {
        let (_tmp, cfg) = cfg_with_provider(p);
        let got = OpenAiCompatEmbedder::try_from_config(&cfg).expect("no error");
        assert!(got.is_none(), "{p} should fall through, got Some");
    }
}

#[test]
fn some_for_openai() {
    let (_tmp, cfg) = cfg_with_provider("openai");
    let got = OpenAiCompatEmbedder::try_from_config(&cfg).expect("no error");
    let e = got.expect("openai should build an adapter");
    assert_eq!(e.name(), "openai");
}

#[test]
fn some_for_custom() {
    let (_tmp, cfg) = cfg_with_provider("custom:https://embed.example/v1");
    let got = OpenAiCompatEmbedder::try_from_config(&cfg).expect("no error");
    let e = got.expect("custom should build an adapter");
    assert_eq!(e.name(), "custom");
}

/// When `embedding_model` is unset, a `custom:<url>` provider must NOT treat
/// the endpoint URL suffix as an inline model name (CodeRabbit #3781). The
/// adapter still builds; the model is simply left empty.
#[test]
fn some_for_custom_endpoint_does_not_use_url_as_model() {
    let (_tmp, mut cfg) = cfg_with_provider("custom:https://embed.example/v1");
    cfg.memory.embedding_model = String::new(); // force the inline fallback path
    let got = OpenAiCompatEmbedder::try_from_config(&cfg).expect("no error");
    let e = got.expect("custom endpoint with no model should still build");
    assert_eq!(e.name(), "custom");
}

/// Build a `cloud_providers` entry the way AI Settings persists a local
/// OpenAI-compatible server.
fn lmstudio_entry(endpoint: &str) -> tinymemory_api::host::cloud_providers::CloudProviderCreds {
    tinymemory_api::host::cloud_providers::CloudProviderCreds {
        id: "p_lmstudio_test".to_string(),
        slug: "lmstudio".to_string(),
        endpoint: endpoint.to_string(),
        ..Default::default()
    }
}

/// #3781: a configured `lmstudio` slug (OpenAI-compatible, like LM Studio at
/// localhost:1234) must resolve to its `cloud_providers` endpoint and route
/// as a `custom` OpenAI-compatible embedder — NOT fall through to managed
/// cloud. This is the headline bug: sealing ignored the local backend.
#[test]
fn some_for_configured_lmstudio_slug() {
    let (_tmp, mut cfg) = cfg_with_provider("lmstudio");
    cfg.memory.embedding_model = "bge-m3".to_string();
    cfg.cloud_providers = vec![lmstudio_entry("http://localhost:1234/v1")];

    let got = OpenAiCompatEmbedder::try_from_config(&cfg).expect("no error");
    let e = got.expect("configured lmstudio slug must build an adapter, not fall through");
    assert_eq!(e.name(), "custom");
}

/// The `slug:model` form (mirroring the top-level
/// `embeddings_provider = "lmstudio:bge-m3"` shape) also resolves, taking the
/// model from the inline suffix when `embedding_model` is unset.
#[test]
fn some_for_lmstudio_slug_with_inline_model() {
    let (_tmp, mut cfg) = cfg_with_provider("lmstudio:bge-m3");
    cfg.memory.embedding_model = String::new(); // force inline-suffix fallback
    cfg.cloud_providers = vec![lmstudio_entry("http://localhost:1234/v1")];

    let got = OpenAiCompatEmbedder::try_from_config(&cfg).expect("no error");
    let e = got.expect("lmstudio:model slug must resolve");
    assert_eq!(e.name(), "custom");
}

/// A custom slug with no matching `cloud_providers` entry must fall through
/// (Ok(None)) so the caller's ladder continues to the managed default —
/// rather than erroring or hijacking the resolution.
#[test]
fn none_for_unconfigured_custom_slug() {
    let (_tmp, cfg) = cfg_with_provider("lmstudio"); // no cloud_providers entry
    let got = OpenAiCompatEmbedder::try_from_config(&cfg).expect("no error");
    assert!(
        got.is_none(),
        "unconfigured slug should fall through, not build an adapter"
    );
}

/// An entry that exists but has a blank endpoint is unusable → fall through.
#[test]
fn none_for_configured_slug_with_blank_endpoint() {
    let (_tmp, mut cfg) = cfg_with_provider("lmstudio");
    cfg.cloud_providers = vec![lmstudio_entry("   ")];
    let got = OpenAiCompatEmbedder::try_from_config(&cfg).expect("no error");
    assert!(got.is_none(), "blank endpoint should fall through");
}

/// Reserved/managed/native slugs must keep falling through even if a stray
/// `cloud_providers` entry exists for them — they are owned by other ladder
/// branches, not the OpenAI-compatible adapter.
#[test]
fn reserved_slugs_still_fall_through() {
    use tinymemory_api::host::cloud_providers::CloudProviderCreds;
    for p in ["managed", "cloud", "voyage", "cohere", "ollama", "none"] {
        let (_tmp, mut cfg) = cfg_with_provider(p);
        cfg.cloud_providers = vec![CloudProviderCreds {
            id: format!("p_{p}"),
            slug: p.to_string(),
            endpoint: "http://localhost:1234/v1".to_string(),
            ..Default::default()
        }];
        let got = OpenAiCompatEmbedder::try_from_config(&cfg).expect("no error");
        assert!(got.is_none(), "{p} must fall through, got Some");
    }
}

/// Codex review on #4056: a custom config whose stored dimension isn't the
/// tree's fixed [`EMBEDDING_DIM`] (and whose model can't reduce to it via the
/// OpenAI `dimensions` param) must be refused at construction with a clear,
/// actionable error — not built and then failed at the first embed with a raw
/// "expected 1024, got N". This is what keeps an auto-detected non-1024 custom
/// endpoint (which the embeddings RPC still accepts) out of the 1024-only tree.
#[test]
fn err_for_non_reducible_model_with_incompatible_dimension() {
    let (_tmp, mut cfg) = cfg_with_provider("custom:https://embed.example/v1");
    cfg.memory.embedding_model = "nomic-embed-text".to_string(); // not text-embedding-3-*
    cfg.memory.embedding_dimensions = 768; // != EMBEDDING_DIM (1024)
                                           // `expect_err` would require the Ok type (the embedder) to impl Debug,
                                           // which it can't (boxed trait object) — match instead.
    let err = match OpenAiCompatEmbedder::try_from_config(&cfg) {
        Err(e) => e,
        Ok(_) => panic!("768 != tree dim must error, got Ok"),
    };
    let msg = format!("{err:#}");
    assert!(
        msg.contains("768") && msg.contains(&EMBEDDING_DIM.to_string()),
        "error must name both the model's dim and the required dim: {msg}"
    );
}

/// A non-reducible model that natively matches [`EMBEDDING_DIM`] still builds —
/// only an incompatible dimension is refused.
#[test]
fn some_for_non_reducible_model_at_tree_dimension() {
    let (_tmp, mut cfg) = cfg_with_provider("custom:https://embed.example/v1");
    cfg.memory.embedding_model = "mxbai-embed-large".to_string();
    cfg.memory.embedding_dimensions = EMBEDDING_DIM; // 1024
    let got = OpenAiCompatEmbedder::try_from_config(&cfg).expect("no error");
    assert!(
        got.is_some(),
        "a 1024-native custom model must build the tree adapter"
    );
}

/// `text-embedding-3-*` is exempt from the dimension guard: the adapter
/// requests `EMBEDDING_DIM` and the server reduces to it, so even a config
/// stored at a different dimension still builds.
#[test]
fn some_for_reducible_model_regardless_of_stored_dimension() {
    let (_tmp, mut cfg) = cfg_with_provider("openai");
    cfg.memory.embedding_model = "text-embedding-3-large".to_string();
    cfg.memory.embedding_dimensions = 256; // reducible — tree still requests 1024
    let got = OpenAiCompatEmbedder::try_from_config(&cfg).expect("no error");
    assert!(
        got.is_some(),
        "reducible model must build regardless of stored dim"
    );
}
