//! Capability honesty for the full engine provider.
//!
//! The point of lifting the optional families here (issue #18 §C3) is that a
//! host filtering its surface from a negotiated capability set gets the whole
//! engine rather than the mandatory third of it. That is only safe if the set
//! is true.
//!
//! These assert the rule directly rather than through a constructed provider.
//! Construction needs a `MemoryClient`, which needs the host's process-global
//! seams (`set_embedding_host` and friends) installed — and a test that installs
//! a process global is order-dependent, which `AGENTS.md` rules out. The
//! provider-level check that `capabilities()` equals the reachable accessors is
//! `audit_provider`, and it runs against a real engine in the conformance suite
//! once a host has wired those seams.

#![allow(clippy::expect_used, clippy::panic)]

use tinymemory_api::capabilities::{Capabilities, Capability};
use tinymemory_api::chunks::DataSource;
use tinymemory_api::error::MemoryError;
use tinymemory_api::host::MemoryHostConfig;
use tinymemory_api::provider::types::IngestItem;
use tinymemory_api::types::MemoryTaint;

use super::{
    advertised_capabilities, facet_type_to_engine, handle_to_contract, handle_to_engine,
    parse_person_id, scope_to_engine, validate_ingest_item, EngineRuntimeConfig,
};

fn ingest_item(content: &str, mime: Option<&str>, taint: MemoryTaint) -> IngestItem {
    IngestItem {
        namespace: None,
        source: DataSource::Upload,
        source_id: "doc-1".to_string(),
        owner: "owner".to_string(),
        source_ref: None,
        content: content.to_string(),
        mime: mime.map(str::to_string),
        timestamp: None,
        tags: Vec::new(),
        taint,
        path_scope: None,
    }
}

fn runtime_config() -> EngineRuntimeConfig {
    EngineRuntimeConfig {
        workspace_dir: "/workspace".into(),
        config_path: "/workspace/config.toml".into(),
        memory: Default::default(),
        memory_tree: Default::default(),
        scheduler_gate: Default::default(),
        local_ai: Default::default(),
        embeddings_provider: Some("ollama: nomic-embed-text".to_string()),
        memory_provider: Some("ollama: qwen3".to_string()),
        default_model: Some("host/model".to_string()),
        default_temperature: 0.3,
        output_language: Some("en".to_string()),
        memory_sources: serde_json::json!([{"id": "source-1"}]),
    }
}

#[test]
fn the_mandatory_families_are_always_advertised() {
    let caps = advertised_capabilities();
    for mandatory in Capability::MANDATORY {
        assert!(
            caps.contains(mandatory),
            "`{}` must be advertised in every build",
            mandatory.as_str()
        );
    }
}

#[cfg(feature = "memory-git")]
#[test]
fn the_full_engine_advertises_every_family_with_memory_git() {
    // The lift's headline: this adapter used to advertise three families.
    assert_eq!(advertised_capabilities(), Capabilities::all());
    assert!(advertised_capabilities().contains(Capability::Diff));
}

#[cfg(not(feature = "memory-git"))]
#[test]
fn diff_is_withheld_when_the_snapshot_store_is_compiled_out() {
    // The gate has to reach the advertisement, not just the accessor. A build
    // that advertised `Diff` here would fail `audit_provider` — which is how
    // that audit earns its place.
    let caps = advertised_capabilities();
    assert!(!caps.contains(Capability::Diff));
    // Everything else the engine serves is still advertised: withholding one
    // family must not quietly withhold the rest.
    assert_eq!(caps, Capabilities::all().without(Capability::Diff));
    assert_eq!(caps.len(), Capabilities::all().len() - 1);
}

#[test]
fn ingest_accepts_decoded_text_mime_families() {
    for mime in [
        None,
        Some("text/plain"),
        Some("text/markdown; charset=utf-8"),
        Some("application/json"),
        Some("application/activity+json"),
        Some("application/xml"),
        Some("application/atom+xml"),
        Some("application/x-ndjson"),
    ] {
        validate_ingest_item(&ingest_item("decoded text", mime, MemoryTaint::Internal))
            .unwrap_or_else(|error| panic!("{mime:?} should be accepted: {error}"));
    }
}

#[test]
fn ingest_rejects_empty_binary_and_non_default_taint() {
    let cases = [
        ingest_item("   ", Some("text/plain"), MemoryTaint::Internal),
        ingest_item(
            "binary already decoded badly",
            Some("application/pdf"),
            MemoryTaint::Internal,
        ),
        ingest_item(
            "external content",
            Some("text/plain"),
            MemoryTaint::ExternalSync,
        ),
    ];
    for item in cases {
        assert!(
            matches!(validate_ingest_item(&item), Err(MemoryError::Invalid(_))),
            "invalid ingest item was accepted: {item:?}"
        );
    }
}

#[tokio::test]
async fn runtime_config_routes_models_and_round_trips_source_configuration() {
    let mut config = runtime_config();
    assert_eq!(
        config.workspace_dir(),
        &std::path::PathBuf::from("/workspace")
    );
    assert_eq!(
        config.config_path(),
        &std::path::PathBuf::from("/workspace/config.toml")
    );
    assert_eq!(
        config.memory_tree_content_root(),
        std::path::PathBuf::from("/workspace/memory_tree/content")
    );
    let _ = config.memory();
    let _ = config.memory_tree();
    let _ = config.scheduler_gate();
    let _ = config.local_ai();
    assert!(config.cloud_providers().is_empty());
    assert_eq!(
        config.embeddings_provider(),
        Some("ollama: nomic-embed-text")
    );
    assert_eq!(config.memory_provider(), Some("ollama: qwen3"));
    assert_eq!(
        config.workload_local_model("embeddings").as_deref(),
        Some("nomic-embed-text")
    );
    assert_eq!(
        config.workload_local_model("memory").as_deref(),
        Some("qwen3")
    );
    assert_eq!(config.workload_local_model("chat"), None);
    assert_eq!(config.default_model(), Some("host/model"));
    assert_eq!(config.default_temperature(), 0.3);
    assert_eq!(config.output_language(), Some("en"));
    assert!(config.as_any().is::<EngineRuntimeConfig>());
    assert!(config.to_arc().as_any().is::<EngineRuntimeConfig>());
    assert_eq!(config.api_url(), None);
    assert!(config.effective_backend_api_url().is_empty());
    assert_eq!(config.session_token().expect("session token"), None);
    assert_eq!(config.memory_sync_interval_secs(), Some(0));
    assert!(config.onboarding_completed());
    assert!(!config.secrets_encrypt());
    assert!(!config.composio().is_direct());
    assert_eq!(config.composio_source_caps_migration_version(), 0);
    config.set_composio_source_caps_migration_version(2);
    config.apply_env_overrides();
    assert_eq!(
        config.memory_sources_json().expect("source JSON"),
        serde_json::json!([{"id": "source-1"}])
    );

    config
        .set_memory_sources_json(serde_json::json!([{"id": "source-2"}]))
        .expect("replace source JSON");
    assert_eq!(
        config.memory_sources_json().expect("source JSON"),
        serde_json::json!([{"id": "source-2"}])
    );
    config
        .save()
        .await
        .expect("the in-memory adapter save is a no-op");
}

#[test]
fn people_profile_and_scope_boundary_conversions_are_total_and_fail_closed() {
    use tinymemory_api::provider::types::SourceScope;
    use tinymemory_api::provider::{FacetType, PersonHandle};
    use tinymemory_core::store::namespace_store::profile::FacetType as EngineFacet;

    for handle in [
        PersonHandle::IMessage("+15551234567".into()),
        PersonHandle::Email("person@example.com".into()),
        PersonHandle::DisplayName("Ada Lovelace".into()),
    ] {
        assert_eq!(handle_to_contract(handle_to_engine(&handle)), handle);
    }

    let id = uuid::Uuid::nil().to_string();
    assert_eq!(parse_person_id(&id).expect("valid id").0.to_string(), id);
    assert!(matches!(
        parse_person_id("not-a-person-id"),
        Err(MemoryError::Invalid(_))
    ));

    assert_eq!(
        [
            FacetType::Preference,
            FacetType::Workflow,
            FacetType::Role,
            FacetType::Personality,
            FacetType::Context,
        ]
        .map(facet_type_to_engine),
        [
            EngineFacet::Preference,
            EngineFacet::Workflow,
            EngineFacet::Role,
            EngineFacet::Personality,
            EngineFacet::Context,
        ]
    );

    assert!(scope_to_engine(None).is_none());
    assert_eq!(
        scope_to_engine(Some(&SourceScope::new(["source-a", "source-b"])))
            .expect("scoped set")
            .len(),
        2
    );
}
