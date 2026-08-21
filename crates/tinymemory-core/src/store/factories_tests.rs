//! Tests for the surrounding module.

use super::*;

use axum::{routing::get, Json, Router};
use std::ffi::OsString;
use std::net::SocketAddr;

fn reset_health_gate_for_test() {
    OLLAMA_HEALTH_REPORTED.store(false, Ordering::Release);
}

/// RAII helper that swaps `OPENHUMAN_OLLAMA_BASE_URL` to `value` for the
/// duration of the scope while holding the local-AI domain test mutex.
/// The previous value (if any) is restored on drop.
struct EnvGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    prev: Option<OsString>,
}

impl EnvGuard {
    fn set(value: &str) -> Self {
        let lock = crate::embedding_host::embedding_test_guard();
        let prev = std::env::var_os("OPENHUMAN_OLLAMA_BASE_URL");
        // SAFETY: env mutation is wrapped because Rust 2024 marks it
        // unsafe; the call is gated by the local-AI domain mutex so no
        // other local-AI test is observing the env concurrently.
        unsafe {
            std::env::set_var("OPENHUMAN_OLLAMA_BASE_URL", value);
        }
        Self { _lock: lock, prev }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: same justification as `set` — still under the same lock.
        unsafe {
            match self.prev.take() {
                Some(v) => std::env::set_var("OPENHUMAN_OLLAMA_BASE_URL", v),
                None => std::env::remove_var("OPENHUMAN_OLLAMA_BASE_URL"),
            }
        }
    }
}

// ── effective_embedding_settings (unprobed selection priority) ────────

#[test]
fn embedding_settings_defaults_to_cloud_when_no_local_ai() {
    let mem = MemoryConfig::default();
    let (provider, model, dims) = effective_embedding_settings(&mem, None);
    assert_eq!(
        provider, "cloud",
        "no local-AI config must default to cloud"
    );
    assert!(!model.is_empty(), "cloud model must be non-empty");
    assert!(dims > 0, "cloud dimensions must be positive");
}

#[test]
fn embedding_settings_uses_memory_config_when_local_disabled() {
    let mem = MemoryConfig {
        embedding_provider: "openai".to_string(),
        embedding_model: "text-embedding-3-small".to_string(),
        embedding_dimensions: 1536,
        ..Default::default()
    };

    // Local embedding model = None means workload routes to cloud.
    let (provider, model, dims) = effective_embedding_settings(&mem, None);
    assert_eq!(
        provider, "openai",
        "when local embeddings disabled, memory config must be used"
    );
    assert_eq!(model, "text-embedding-3-small");
    assert_eq!(dims, 1536);
}

#[test]
fn embedding_settings_local_overrides_memory_config() {
    // memory.embedding_provider says "cloud" — but a Some(local_model)
    // is the stronger signal and must override it.
    let mem = MemoryConfig::default(); // cloud by default
    let (provider, model, dims) =
        effective_embedding_settings(&mem, Some("nomic-embed-text:latest"));
    assert_eq!(
        provider, "ollama",
        "Some(local_model) must override memory.embedding_provider"
    );
    assert_eq!(model, "nomic-embed-text:latest");
    assert_eq!(
        dims,
        tinyagents::harness::embeddings::DEFAULT_OLLAMA_DIMENSIONS,
        "dimensions must default to Ollama default"
    );
}

#[test]
fn embedding_settings_local_with_empty_model_uses_default() {
    // When the user has opted in but the model field is empty/whitespace,
    // the default Ollama model must be used rather than passing "" to Ollama.
    let mem = MemoryConfig::default();
    let (provider, model, dims) = effective_embedding_settings(&mem, Some("   "));
    assert_eq!(provider, "ollama");
    assert_eq!(
        model,
        tinyagents::harness::embeddings::DEFAULT_OLLAMA_MODEL,
        "empty model ID must fall back to default Ollama model"
    );
    assert_eq!(
        dims,
        tinyagents::harness::embeddings::DEFAULT_OLLAMA_DIMENSIONS
    );
}

#[test]
fn active_signature_ignores_probe_fallback() {
    // active_embedding_signature keys off the *intended* selection
    // (effective_embedding_settings), NOT the health-checked variant — so
    // a transient Ollama-down fallback can't flip it to cloud. The dim is
    // base/config-dependent (not what this test pins); the provider+model
    // staying the intended ollama/bge-m3 is the probe-stability property.
    let mem = MemoryConfig::default();
    let sig = active_embedding_signature(&mem, Some("bge-m3"));
    assert!(
        sig.starts_with("provider=ollama;model=bge-m3;dims="),
        "intended local selection must survive (no cloud fallback); got {sig}"
    );
    // And it must equal the non-probed settings, formatted identically.
    let (p, m, d) = effective_embedding_settings(&mem, Some("bge-m3"));
    assert_eq!(sig, format_embedding_signature(&p, &m, d));
}

#[test]
fn effective_memory_backend_name_always_returns_namespace() {
    assert_eq!(effective_memory_backend_name("sqlite", None), "namespace");
    assert_eq!(effective_memory_backend_name("anything", None), "namespace");
    assert_eq!(effective_memory_backend_name("", None), "namespace");
}

#[test]
fn create_memory_for_migration_returns_writable_memory_on_unified_core() {
    // Regression for #1440: prior to that PR this factory unconditionally
    // bailed with "memory migration is disabled for the unified namespace
    // memory core", which broke the OpenClaw importer's Apply path even
    // though the dry-run / preview path worked. Now it delegates to
    // `create_memory` so the migration importer gets a real workspace-
    // scoped memory handle. Box<dyn Memory> doesn't impl Debug, so we
    // match instead of unwrap.
    let tmp = tempfile::tempdir().unwrap();
    let cfg = MemoryConfig::default();
    match create_memory_for_migration(&cfg, tmp.path()) {
        Ok(_) => {}
        Err(e) => panic!("expected Ok for unified namespace core, got: {e}"),
    }
}

#[tokio::test]
async fn factory_entry_points_build_stores_and_provider_contracts() {
    crate::embedding_host::TestEmbeddingHost::install();
    let tmp = tempfile::tempdir().unwrap();
    let config = MemoryConfig::default();

    let memory = create_memory(&config, tmp.path()).unwrap();
    let provider = bind_as_provider(memory);
    assert_eq!(
        provider.driver_id(),
        tinymemory_api::drivers::NAMESPACE_DRIVER_ID
    );
    assert_eq!(
        provider.capabilities(),
        tinymemory_api::capabilities::Capabilities::mandatory()
    );
    assert!(create_memory_provider(&config, tmp.path()).is_ok());
    assert!(create_memory_with_local_ai(&config, None, "", &[], None, tmp.path()).is_ok());
    assert!(create_memory_client_with_local_ai(&config, None, "", &[], None, tmp.path()).is_ok());
}

#[tokio::test]
async fn session_and_explicit_subdir_factories_keep_storage_isolated() {
    crate::embedding_host::TestEmbeddingHost::install();
    let tmp = tempfile::tempdir().unwrap();
    let config = MemoryConfig::default();
    let session = create_session_memory_with_local_ai(
        &config,
        None,
        "",
        &[],
        None,
        tmp.path(),
        "memory-profile",
    )
    .unwrap();
    assert!(session.sqlite_connection.lock().is_autocommit());
    drop(session.memory);

    assert!(
        create_memory_client_in_subdir(&config, None, "", &[], None, tmp.path(), "memory-a",)
            .is_ok()
    );
    assert!(
        create_memory_client_in_subdir(&config, None, "", &[], None, tmp.path(), "memory-b",)
            .is_ok()
    );
    assert!(tmp.path().join("memory-a").exists());
    assert!(tmp.path().join("memory-b").exists());
}

/// Spin up a mock Ollama-shaped server that responds 200 OK on `/api/tags`.
async fn start_mock_ollama() -> String {
    let app = Router::new().route(
        "/api/tags",
        get(|| async { Json(serde_json::json!({ "models": [] })) }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://127.0.0.1:{}", addr.port())
}

/// The parsed local-embedding model string that
/// `Config::workload_local_model("embeddings")` would have produced when
/// the legacy `local_ai.usage.embeddings = true` flag was set. Used so
/// the existing test scenarios continue to drive the local code path.
fn local_embedding_for_test() -> &'static str {
    tinyagents::harness::embeddings::DEFAULT_OLLAMA_MODEL
}

#[tokio::test]
async fn probe_returns_true_when_ollama_responds_200() {
    let url = start_mock_ollama().await;
    assert!(probe_ollama_reachable(&url).await);
}

#[tokio::test]
async fn probe_returns_false_for_unreachable_host() {
    // Port 1 on loopback is reliably refused.
    assert!(!probe_ollama_reachable("http://127.0.0.1:1").await);
}

#[tokio::test]
async fn probe_returns_false_on_non_2xx() {
    // Mock that responds 500.
    let app = Router::new().route(
        "/api/tags",
        get(|| async { (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "boom") }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let url = format!("http://127.0.0.1:{}", addr.port());
    assert!(!probe_ollama_reachable(&url).await);
}

#[tokio::test]
async fn probed_settings_keep_cloud_when_provider_is_cloud() {
    // No local-AI opt-in → intended provider is cloud, probe is skipped.
    let mem = MemoryConfig::default();
    let (provider, _, _) = effective_embedding_settings_probed(&mem, None).await;
    assert_eq!(provider, "cloud");
}

/// Sets `OPENHUMAN_OLLAMA_BASE_URL` to a deliberately unreachable address
/// under the local-AI domain mutex, then verifies that the probed settings
/// fall back to cloud when the user has opted into local embeddings.
#[tokio::test]
async fn probed_settings_fall_back_to_cloud_when_ollama_unreachable() {
    let _env = EnvGuard::set("http://127.0.0.1:1");
    // Independent of suite ordering: an earlier fallback test must not
    // leave the latch tripped and silently turn this assertion green.
    reset_health_gate_for_test();

    // The cloud defaults are the host's to state, so the fallback tuple is
    // only meaningful with an embedding host installed.
    crate::embedding_host::TestEmbeddingHost::install();
    let mem = MemoryConfig::default();

    let (provider, model, dims) =
        effective_embedding_settings_probed(&mem, Some(local_embedding_for_test())).await;

    assert_eq!(
        provider, "cloud",
        "opted-in but unreachable Ollama must fall back to cloud"
    );
    assert_eq!(model, crate::embedding_host::TestEmbeddingHost::CLOUD_MODEL);
    assert_eq!(
        dims,
        crate::embedding_host::TestEmbeddingHost::CLOUD_DIMENSIONS
    );
}

#[tokio::test]
async fn probed_settings_keep_ollama_when_daemon_responds() {
    let url = start_mock_ollama().await;
    let _env = EnvGuard::set(&url);

    let mem = MemoryConfig::default();

    let (provider, _model, dims) =
        effective_embedding_settings_probed(&mem, Some(local_embedding_for_test())).await;

    assert_eq!(provider, "ollama", "healthy Ollama must be honoured");
    assert_eq!(dims, DEFAULT_OLLAMA_DIMENSIONS);
}

#[test]
fn redact_ollama_host_strips_scheme_userinfo_path_and_query() {
    // Strips scheme.
    assert_eq!(
        redact_ollama_host("http://localhost:11434"),
        "localhost:11434"
    );
    // Strips userinfo (would be the credential leak vector).
    assert_eq!(
        redact_ollama_host("http://user:secret@10.0.0.1:11434"),
        "10.0.0.1:11434"
    );
    // Strips path / query / fragment.
    assert_eq!(
        redact_ollama_host("https://host:11434/api/tags?key=v#frag"),
        "host:11434"
    );
    // Scheme-less inputs survive (matches `local_ai::ollama_base_url`'s
    // contract: it may or may not prepend `http://`).
    assert_eq!(redact_ollama_host("host:1234"), "host:1234");
    // Empty / malformed inputs fall back to a safe constant.
    assert_eq!(redact_ollama_host(""), "unknown");
}

/// #5354 — the client broadcast must NOT ride the once-per-process Sentry
/// latch.
///
/// `publish_web_channel_event` is a `broadcast::send` with no buffering: if
/// no socket client is attached the event is dropped outright. Memory is
/// built early (once per agent), so the first failed probe typically fires
/// before the renderer connects. Latched, that single dropped send would be
/// the only attempt ever made and the UserErrorCenter would stay empty for
/// the entire outage. Subscribing here proves a second gate call still
/// broadcasts even though its Sentry half is suppressed.
#[test]
fn user_error_broadcast_is_not_suppressed_by_the_sentry_latch() {
    let _lock = crate::embedding_host::embedding_test_guard();
    reset_health_gate_for_test();

    let sink = crate::events::RecordingSink::install();

    assert!(
        report_ollama_health_gate_once("http://127.0.0.1:1", "bge-m3"),
        "first call must fire the Sentry report"
    );
    assert!(
        !report_ollama_health_gate_once("http://127.0.0.1:1", "bge-m3"),
        "second call must suppress the Sentry report"
    );

    // Both calls must still have been announced — the Sentry latch
    // suppresses only the *report*, never the user-facing event.
    // Count only the user-facing announcement. The health-gate also emits
    // `EmbeddingModelUnhealthy` on the first call; that is a different
    // event with its own latch and is not what this test pins.
    let announcements = sink
        .drain()
        .into_iter()
        .filter(|event| {
            matches!(
                event,
                crate::events::MemoryEvent::LocalModelUnavailable { .. }
            )
        })
        .count();
    assert_eq!(
        announcements, 2,
        "the Sentry latch must suppress the report, never the announcement"
    );
}

/// First call to `report_ollama_health_gate_once` fires the report;
/// subsequent calls in the same process must be suppressed. We can't
/// observe the Sentry side effect directly here, but the boolean return
/// value is the gate's contract — covers the once-per-process guarantee.
/// Event publication is fire-and-forget via the global event bus and is
/// verified manually/log-side rather than by this unit test.
///
/// Acquires the local-AI domain mutex to serialize with `probed_settings_*`
/// tests that also touch the latch; without that, parallel test execution
/// can reset the flag between this test's two
/// `report_ollama_health_gate_once` calls and turn the second one into a
/// fresh "first", flaking the suppression assertion.
#[test]
fn ollama_health_gate_reports_at_most_once_per_process() {
    let _lock = crate::embedding_host::embedding_test_guard();
    reset_health_gate_for_test();

    assert!(
        report_ollama_health_gate_once("http://127.0.0.1:1", "bge-m3"),
        "first call must fire the report"
    );
    assert!(
        !report_ollama_health_gate_once("http://127.0.0.1:1", "bge-m3"),
        "second call must be suppressed"
    );
    assert!(
        !report_ollama_health_gate_once("http://example.invalid:11434", "nomic-embed-text"),
        "different URL also suppressed — gate is process-scoped, not per-URL"
    );
}
