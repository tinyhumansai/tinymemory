//! Behavioral tests for engine binding, isolation, roots, and host installs.

use super::*;

use crate::config::EngineSettings;
use crate::cortex::{BearerSource, CORTEXDB_ENGINE_ID, StaticBearer, TINYHUMANS_ENGINE_ID};
use crate::registry::EngineCredential;
use crate::safety::Policy;
use std::sync::{Arc, Mutex};
use tinymemory_api::conformance::ReferenceEngine;
use tinymemory_api::{ListRequest, MemoryEngine, MemoryMeta, MetaFilter, StoreItem};

fn fake_secret() -> String {
    let mut secret = String::from("sk-ant-");
    secret.push_str("api03-");
    secret.push_str(&"A".repeat(48));
    secret
}

fn configured_engine(
    binder: &EngineBinder,
    endpoint: Option<&str>,
    fingerprint: &str,
    key: &str,
    policy: Policy,
) -> BoundEngine {
    let binding = binder.bind(
        CORTEXDB_ENGINE_ID,
        EngineSettings {
            endpoint: endpoint.map(str::to_string),
            ..EngineSettings::default()
        },
        fingerprint,
        EngineCredential::Static(key.to_string()),
        policy,
    );
    match binding {
        Binding::On(engine) => engine,
        Binding::Off { reason, .. } => panic!("expected engine to bind: {reason}"),
    }
}

#[test]
fn matching_settings_reuse_a_strongly_cached_engine() {
    let binder = EngineBinder::new(|id| {
        (id == CORTEXDB_ENGINE_ID).then(|| "https://cortex.example.test".to_string())
    });

    let first = configured_engine(
        &binder,
        None,
        "credential-source-1",
        "key-one",
        Policy::default(),
    );
    let weak = Arc::downgrade(&first.engine);
    drop(first);
    let second = configured_engine(
        &binder,
        None,
        "credential-source-1",
        "key-one",
        Policy::default(),
    );

    assert!(Arc::ptr_eq(
        &weak.upgrade().expect("cache retains a strong engine"),
        &second.engine
    ));
    assert_eq!(second.endpoint, "https://cortex.example.test");
}

#[test]
fn engine_cache_is_isolated_per_binder_instance() {
    let first_binder = EngineBinder::default();
    let second_binder = EngineBinder::default();
    let first = configured_engine(
        &first_binder,
        Some("https://cortex.example.test"),
        "same-source",
        "same-key",
        Policy::default(),
    );
    let second = configured_engine(
        &second_binder,
        Some("https://cortex.example.test"),
        "same-source",
        "same-key",
        Policy::default(),
    );

    assert!(!Arc::ptr_eq(&first.engine, &second.engine));
}

#[test]
fn dynamic_bearer_source_identity_participates_in_the_cache_key() {
    let binder = EngineBinder::default();
    let source: Arc<dyn BearerSource> = Arc::new(StaticBearer::new("rotating-source"));
    let bind = |identity| {
        binder.bind_configured(
            CORTEXDB_ENGINE_ID,
            EngineSettings {
                endpoint: Some("https://cortex.example.test".to_string()),
                ..EngineSettings::default()
            },
            identity,
            EngineCredential::Dynamic(Arc::clone(&source)),
            Policy::default(),
        )
    };

    let Binding::On(first) = bind("keyring-slot-one") else {
        panic!("dynamic bearer binds");
    };
    let Binding::On(same_source) = bind("keyring-slot-one") else {
        panic!("same source binds");
    };
    let Binding::On(other_source) = bind("keyring-slot-two") else {
        panic!("other source binds");
    };

    assert!(Arc::ptr_eq(&first.engine, &same_source.engine));
    assert!(!Arc::ptr_eq(&first.engine, &other_source.engine));
}

#[test]
fn static_credentials_change_the_digest_without_appearing_in_its_bytes() {
    let settings = EngineSettings::default();
    let first = cache_key(
        CORTEXDB_ENGINE_ID,
        &settings,
        "keyring-slot",
        &EngineCredential::Static("private-static-token".to_string()),
        Policy::default(),
    );
    let second = cache_key(
        CORTEXDB_ENGINE_ID,
        &settings,
        "keyring-slot",
        &EngineCredential::Static("different-static-token".to_string()),
        Policy::default(),
    );

    assert_ne!(first, second);
    assert!(!format!("{first:?}").contains("private-static-token"));
}

#[test]
fn endpoint_credential_policy_and_root_changes_do_not_hit_an_incompatible_entry() {
    let binder = EngineBinder::new(|_| None);
    let initial = configured_engine(
        &binder,
        Some("https://one.example.test"),
        "same-source",
        "same-key",
        Policy::default(),
    );

    let changed_endpoint = configured_engine(
        &binder,
        Some("https://two.example.test"),
        "same-source",
        "same-key",
        Policy::default(),
    );
    let changed_credential = configured_engine(
        &binder,
        Some("https://one.example.test"),
        "same-source",
        "different-key",
        Policy::default(),
    );
    let changed_policy = configured_engine(
        &binder,
        Some("https://one.example.test"),
        "same-source",
        "same-key",
        Policy::corroborated(),
    );
    let Binding::On(changed_observed_actor) = binder.bind_configured(
        CORTEXDB_ENGINE_ID,
        EngineSettings {
            endpoint: Some("https://one.example.test".to_string()),
            observed_actor: true,
            ..EngineSettings::default()
        },
        "same-source",
        EngineCredential::Static("same-key".to_string()),
        Policy::default(),
    ) else {
        panic!("observed_actor engine should bind");
    };
    let changed_root = EngineBinder::rooted(
        EngineSettings {
            endpoint: Some("https://one.example.test".to_string()),
            ..EngineSettings::default()
        },
        Some("org:42"),
        false,
        false,
    );
    let (rooted, root_key) = changed_root;
    let with_root = binder.bind(
        CORTEXDB_ENGINE_ID,
        rooted,
        &format!("same-source|{root_key}"),
        EngineCredential::Static("same-key".to_string()),
        Policy::default(),
    );

    let Binding::On(with_root) = with_root else {
        panic!("rooted engine should bind");
    };
    for engine in [
        changed_endpoint,
        changed_credential,
        changed_policy,
        changed_observed_actor,
        with_root,
    ] {
        assert!(!Arc::ptr_eq(&initial.engine, &engine.engine));
    }
}

#[test]
fn invalidate_rebuilds_a_matching_engine() {
    let binder = EngineBinder::new(|_| None);
    let first = configured_engine(
        &binder,
        Some("https://cortex.example.test"),
        "same-source",
        "same-key",
        Policy::default(),
    );

    binder.invalidate();

    let second = configured_engine(
        &binder,
        Some("https://cortex.example.test"),
        "same-source",
        "same-key",
        Policy::default(),
    );
    assert!(!Arc::ptr_eq(&first.engine, &second.engine));
}

#[test]
fn rooted_settings_keep_the_tenant_and_legacy_layouts_distinct() {
    let settings = EngineSettings {
        endpoint: Some("https://memory.example.test".to_string()),
        ..EngineSettings::default()
    };

    let (direct, direct_key) = EngineBinder::rooted(settings.clone(), Some("org:42"), false, true);
    let (tenant, tenant_key) = EngineBinder::rooted(settings, Some("org:42"), true, true);

    assert_eq!(direct.scope_root.as_deref(), Some("org:42"));
    assert_eq!(direct.scope_owner.as_deref(), Some("user:42"));
    assert_eq!(direct.retired_scope_root.as_deref(), Some("user:42"));
    assert!(!direct.tenant_root);
    assert!(tenant.scope_root.is_none());
    assert!(tenant.scope_owner.is_none());
    assert!(tenant.tenant_root);
    assert_eq!(tenant.retired_scope_root.as_deref(), Some("user:42"));
    assert_ne!(direct_key, tenant_key);

    let (legacy, legacy_key) = EngineBinder::rooted(EngineSettings::default(), None, false, false);
    assert_eq!(legacy, EngineSettings::default());
    assert_ne!(legacy_key, direct_key);
}

#[tokio::test]
async fn host_engine_install_is_scrubbed_and_replacement_is_immediate() {
    let binder = EngineBinder::default();
    let first = Arc::new(ReferenceEngine::new());
    binder.install_host_engine(first.clone(), Policy::corroborated());
    let installed = binder.host_engine().expect("host engine installed");
    binder.invalidate();
    assert!(Arc::ptr_eq(
        &installed.engine,
        &binder
            .host_engine()
            .expect("cache invalidation keeps host slot")
            .engine
    ));
    let first_bound = binder.bind(
        "unknown-configured-engine",
        EngineSettings::default(),
        "irrelevant",
        EngineCredential::None,
        Policy::default(),
    );
    let Binding::On(first_bound) = first_bound else {
        panic!("the installed host engine should win");
    };
    assert_eq!(first_bound.id, first.descriptor().id);
    assert_eq!(first_bound.endpoint, HOST_ENGINE_ENDPOINT);

    let secret = fake_secret();
    first_bound
        .engine
        .store(StoreItem::document(
            format!("private note key={secret}"),
            MemoryMeta::default(),
        ))
        .await
        .expect("host engine writes are scrubbed");
    first_bound
        .engine
        .store(StoreItem::document(
            "checksum-only identifier 79927398713",
            MemoryMeta::default(),
        ))
        .await
        .expect("host binding keeps its install-time policy");
    let stored = first
        .list(ListRequest::new(MetaFilter::default(), 5))
        .await
        .expect("reference list")
        .items;
    assert!(!stored[0].text.contains(&secret));
    assert!(
        stored
            .iter()
            .any(|hit| hit.text == "checksum-only identifier 79927398713")
    );

    let second = Arc::new(ReferenceEngine::new());
    binder.install_host_engine(second.clone(), Policy::default());
    let Binding::On(second_bound) = binder.bind(
        "still-ignored",
        EngineSettings::default(),
        "same-fingerprint",
        EngineCredential::None,
        Policy::default(),
    ) else {
        panic!("the replacement host engine should bind");
    };
    assert!(Arc::ptr_eq(
        &second_bound.engine,
        &binder.host_engine().unwrap().engine
    ));
    assert!(!Arc::ptr_eq(&first_bound.engine, &second_bound.engine));
    assert!(binder.clear_host_engine());
    assert!(!binder.clear_host_engine());
}

#[test]
fn an_unknown_configured_engine_returns_a_safe_off_binding() {
    let binder = EngineBinder::default();

    let binding = binder.bind(
        "not-registered",
        EngineSettings::default(),
        "source",
        EngineCredential::None,
        Policy::default(),
    );

    assert!(matches!(
        binding,
        Binding::Off {
            engine: None,
            reason,
            ..
        } if reason.contains("unknown memory engine")
    ));
}

#[test]
fn an_unavailable_known_engine_keeps_its_endpoint_and_safe_reason() {
    let binder = EngineBinder::default();
    let binding = binder.bind_configured(
        CORTEXDB_ENGINE_ID,
        EngineSettings {
            endpoint: Some("https://cortex.example.test".to_string()),
            ..EngineSettings::default()
        },
        "key-source",
        EngineCredential::None,
        Policy::default(),
    );
    let error = binding.clone().engine().expect_err("known engine is off");
    assert_eq!(error.code(), "MEMORY_OFF");

    assert!(matches!(
        binding,
        Binding::Off {
            engine: Some(id),
            endpoint: Some(endpoint),
            reason,
        } if id == CORTEXDB_ENGINE_ID
            && endpoint == "https://cortex.example.test"
            && reason.contains("needs a credential")
    ));
}

#[test]
fn endpoint_source_only_fills_an_unset_endpoint() {
    let binder = EngineBinder::new(|id| {
        (id == CORTEXDB_ENGINE_ID).then(|| "https://default.example.test".to_string())
    });
    let configured = configured_engine(
        &binder,
        Some("https://configured.example.test"),
        "source",
        "key",
        Policy::default(),
    );
    let fallback = configured_engine(&binder, None, "source", "key", Policy::default());

    assert_eq!(configured.endpoint, "https://configured.example.test");
    assert_eq!(fallback.endpoint, "https://default.example.test");
    assert_ne!(configured.endpoint, fallback.endpoint);
    assert_eq!(TINYHUMANS_ENGINE_ID, "tinyhumans");
}

#[test]
fn a_changed_endpoint_source_result_cannot_reuse_the_old_engine() {
    let endpoint = Arc::new(Mutex::new("https://first.example.test".to_string()));
    let source_endpoint = Arc::clone(&endpoint);
    let binder = EngineBinder::new(move |_| Some(source_endpoint.lock().unwrap().clone()));

    let first = configured_engine(&binder, None, "same-source", "same-key", Policy::default());
    *endpoint.lock().unwrap() = "https://second.example.test".to_string();
    let second = configured_engine(&binder, None, "same-source", "same-key", Policy::default());

    assert_eq!(first.endpoint, "https://first.example.test");
    assert_eq!(second.endpoint, "https://second.example.test");
    assert!(!Arc::ptr_eq(&first.engine, &second.engine));
}

#[test]
fn configured_binding_can_bypass_a_process_engine_without_clearing_it() {
    let binder = EngineBinder::new(|_| Some("https://configured.example.test".to_string()));
    let host = Arc::new(ReferenceEngine::new());
    binder.install_host_engine(host, Policy::default());

    let binding = binder.bind_configured(
        CORTEXDB_ENGINE_ID,
        EngineSettings::default(),
        "key-source",
        EngineCredential::Static("key".to_string()),
        Policy::default(),
    );

    let Binding::On(configured) = binding else {
        panic!("configured engine should bind");
    };
    assert_eq!(configured.id, CORTEXDB_ENGINE_ID);
    assert_eq!(configured.endpoint, "https://configured.example.test");
    assert!(binder.host_engine().is_some());
    assert!(matches!(
        binder.bind(
            "unknown",
            EngineSettings::default(),
            "unused",
            EngineCredential::None,
            Policy::corroborated(),
        ),
        Binding::On(_)
    ));
}

#[tokio::test]
async fn transient_external_engine_is_guarded_without_becoming_process_global() {
    let binder = EngineBinder::default();
    let raw = Arc::new(ReferenceEngine::new());
    let secret = fake_secret();
    let bound = binder.bind_external(raw.clone(), "profile://memory", Policy::default());

    bound
        .engine
        .store(StoreItem::document(
            format!("profile note key={secret}"),
            MemoryMeta::default(),
        ))
        .await
        .expect("external engine uses the safety decorators");

    let stored = raw
        .list(ListRequest::new(MetaFilter::default(), 5))
        .await
        .unwrap();
    assert!(!stored.items[0].text.contains(&secret));
    assert!(binder.host_engine().is_none());
}
