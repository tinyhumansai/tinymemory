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
    advertised_capabilities, audit_entry, diagnosis_failure, ensure_syncable_toolkit,
    facet_type_to_engine, handle_to_contract, handle_to_engine, parse_person_id, scope_to_engine,
    validate_ingest_item, EngineRuntimeConfig,
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
        author: None,
        channel_label: None,
        platform: None,
        to: Vec::new(),
        cc: Vec::new(),
        subject: None,
        list_unsubscribe: None,
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
        memory_sync_interval_secs: Some(14_400),
        composio_mode: tinymemory_api::host::COMPOSIO_MODE_DIRECT.to_string(),
        composio_entity_id: "entity-1".to_string(),
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
    // Not `Ok(None)`: see the accessor's own doc. `Ok(None)` reads as "signed
    // out" and sends a reader after a sign-in that cannot help.
    let session = config
        .session_token()
        .expect_err("a module-side config must refuse rather than report signed-out");
    assert!(session.contains("no backend session token"), "{session}");
    assert_eq!(config.memory_sync_interval_secs(), Some(14_400));
    assert!(config.onboarding_completed());
    assert!(!config.secrets_encrypt());
    assert!(config.composio().is_direct());
    assert_eq!(config.composio().entity_id, "entity-1");
    // The key never rides in the config — `composio_config` resolves it through
    // the `ComposioHost` seam, per call.
    assert!(config.composio().api_key.is_none());
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

/// The cadence is answered from the field, including the two values that mean
/// something other than a number of seconds.
///
/// This is the blocker the periodic loops could not see past: the accessor used
/// to answer the constant `Some(0)`, which
/// `sync::composio::periodic::effective_interval_secs` maps to `None` — the
/// contract's manual-only — so every source was skipped on every tick with
/// nothing logged. A cadence that reads as a *setting* has to come from the
/// host, and the only wrong answer that is silent is this one.
#[test]
fn the_sync_cadence_is_answered_from_the_host_and_not_from_a_constant() {
    let mut config = runtime_config();

    // No explicit user choice: callers fall back to the 24h default.
    config.memory_sync_interval_secs = None;
    assert_eq!(config.memory_sync_interval_secs(), None);

    // "Manual only", which the host can now actually express.
    config.memory_sync_interval_secs = Some(0);
    assert_eq!(config.memory_sync_interval_secs(), Some(0));

    config.memory_sync_interval_secs = Some(3_600);
    assert_eq!(config.memory_sync_interval_secs(), Some(3_600));
}

/// A host that states no Composio mode reads as "not direct", which is what an
/// unconfigured integration should look like — and is exactly what the accessor
/// answered before the field existed, so an older host's behaviour is unchanged.
#[test]
fn an_unstated_composio_mode_is_not_direct() {
    let config = EngineRuntimeConfig {
        composio_mode: String::new(),
        composio_entity_id: String::new(),
        ..runtime_config()
    };

    assert!(!config.composio().is_direct());
    assert!(config.composio().entity_id.is_empty());
}

/// Backend mode fails with the structural cause, not with a sign-in prompt.
///
/// `composio_config` reaches `session_token` only on its backend branch, so this
/// is the message a backend-mode Composio sync inside a module actually
/// produces. It has to say *why* — no sign-in fixes a config that has no field
/// for a bearer.
#[test]
fn the_backend_branch_names_why_a_module_cannot_serve_it() {
    let config = EngineRuntimeConfig {
        composio_mode: tinymemory_api::host::COMPOSIO_MODE_BACKEND.to_string(),
        ..runtime_config()
    };

    let error = config
        .session_token()
        .expect_err("backend mode must fail, and say why");

    assert!(error.contains("no backend session token"), "{error}");
    assert!(
        error.contains("ComposioHost::execute"),
        "the message must name what would close the gap: {error}"
    );
    // The old wording sent readers to a sign-in that cannot help.
    assert!(!error.contains("not configured"), "{error}");
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

#[test]
fn an_unsyncable_toolkit_is_refused_before_anything_is_dispatched() {
    // The pipeline builder refuses these too, but as a message inside a
    // `PipelineFailure` — by which point this adapter can no longer tell
    // "there is no such provider" from "the provider failed". On a call that
    // spends money, the caller acts differently on each.
    let error = ensure_syncable_toolkit("definitely-not-a-provider")
        .expect_err("a toolkit with no pipeline must be refused");
    assert!(
        matches!(error, MemoryError::Invalid(_)),
        "expected Invalid, got {error:?}"
    );

    // Every toolkit the engine-free pipelines actually build, and the
    // case/whitespace forms a caller may send: the gate normalises, so a
    // padded slug must not be refused here and then accepted downstream.
    for toolkit in ["gmail", "Slack", " github ", "notion", "linear", "clickup"] {
        assert!(
            ensure_syncable_toolkit(toolkit).is_ok(),
            "`{toolkit}` has a native pipeline and must not be refused"
        );
    }
}

#[test]
fn a_pipeline_failure_crosses_with_the_engines_own_wire_strings() {
    // The frontend resolves `remediation_key` to localised text and compares
    // `code` for equality, so a re-spelling on this side stops matching keys
    // that already exist. Pinned against the engine's own `as_str`.
    use tinymemory_core::tree::health::{FailureCode, PipelineFailure};

    let failure = PipelineFailure::new(FailureCode::EmbeddingsUnconfigured);
    let crossed = diagnosis_failure(&failure);
    assert_eq!(crossed.code, FailureCode::EmbeddingsUnconfigured.as_str());
    assert_eq!(
        crossed.class.as_deref(),
        Some(FailureCode::EmbeddingsUnconfigured.class().as_str())
    );
    assert_eq!(
        crossed.remediation_key,
        FailureCode::EmbeddingsUnconfigured.remediation_key()
    );
    assert_eq!(crossed.detail, None);
}

#[test]
fn an_audit_row_crosses_field_for_field_and_keeps_its_price() {
    // The row was priced when it was written. Carrying `estimated_cost_usd`
    // verbatim — rather than re-deriving it from the token counts on this side
    // — is what keeps a historical total summed at the rate it was recorded at.
    let entry = tinymemory_core::sync::audit::SyncAuditEntry {
        timestamp: chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_000_000, 0)
            .expect("valid timestamp"),
        source_id: "composio:gmail:conn-1".to_string(),
        source_kind: "composio".to_string(),
        scope: "gmail:conn-1".to_string(),
        items_fetched: 12,
        batches: 2,
        input_tokens: 1_000,
        output_tokens: 100,
        estimated_cost_usd: 0.42,
        composio_actions_called: 4,
        composio_cost_usd: 0.02,
        actual_charged_usd: None,
        duration_ms: 4_200,
        success: true,
        error: None,
    };
    let crossed = audit_entry(entry);
    assert_eq!(crossed.source_id, "composio:gmail:conn-1");
    assert_eq!(crossed.items_fetched, 12);
    assert!((crossed.estimated_cost_usd - 0.42).abs() < 1e-9);
    // The contract's own arithmetic, over the same fields the engine's copy
    // uses: estimate when nothing was charged, plus Composio's action cost.
    assert!((crossed.effective_cost_usd() - 0.44).abs() < 1e-9);
    assert!(crossed.success);
    assert_eq!(crossed.error, None);
}

#[test]
fn the_two_new_families_are_advertised_by_the_full_engine() {
    // The families exist because the driver serves them; advertising is what
    // makes a host register their RPC surface, and `audit_provider` fails the
    // bind if the accessor and the advertisement disagree.
    let caps = advertised_capabilities();
    assert!(caps.contains(Capability::SourceSync));
    assert!(caps.contains(Capability::CodingSessions));
}
