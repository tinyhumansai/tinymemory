//! Tests for the surrounding module.

use super::*;

/// The whole #3111 tally relies on the `usage` handle being *shared*
/// across `ProviderContext` clones: a provider's `sync` runs against a
/// clone (or the same ctx passed by `&`), accumulates via `execute`, and
/// `run_connection_sync` reads the count back from its own handle. Pin
/// that the `Arc<Mutex<_>>` is genuinely shared so a clone's increments
/// are visible from the original — if this regressed to a per-clone
/// counter, the audit cost would silently always read zero.
#[test]
fn usage_handle_is_shared_across_context_clones() {
    let ctx = ProviderContext {
        config: Arc::new(TestHostConfig::default()) as Arc<crate::Config>,
        toolkit: "gmail".to_string(),
        connection_id: None,
        usage: ComposioUsageHandle::default(),
        max_items: None,
        sync_depth_days: None,
    };
    let cloned = ctx.clone();

    // Simulate two `execute` round-trips accumulating on the clone.
    {
        let mut usage = cloned.usage.lock().expect("lock usage");
        usage.actions_called = usage.actions_called.saturating_add(2);
        usage.cost_usd += 0.015;
    }

    // The original handle must observe the clone's tally.
    let observed = ctx.usage.lock().expect("lock usage");
    assert_eq!(observed.actions_called, 2);
    assert!((observed.cost_usd - 0.015).abs() < 1e-9);
}

/// `ComposioUsage` defaults to a zero tally — the value
/// `run_connection_sync` returns for a sync that fired no Composio
/// actions, and what non-sync `ProviderContext` callers carry.
#[test]
fn composio_usage_defaults_to_zero() {
    let usage = ComposioUsage::default();
    assert_eq!(usage.actions_called, 0);
    assert_eq!(usage.cost_usd, 0.0);
}

// `ProviderContext::execute` and `ProviderContext::backend_client` reload
// config from `ctx.config.config_path()` (via `reload_config_snapshot_with_timeout`)
// rather than from the process-global `OPENHUMAN_WORKSPACE`. Tests
// therefore only need to persist the config to `config_path` — no env var
// manipulation required.

#[tokio::test]
async fn provider_context_execute_backend_branch_without_session_errors_cleanly() {
    // Default `Config` (mode = "backend") with no stored session
    // token: the factory should return a backend-session error from
    // `ctx.execute`. Verifies the backend branch is reachable and
    // the error surface is sensible.
    let tmp = tempfile::tempdir().expect("tempdir");

    let mut config = TestHostConfig::default();
    config.config_path = tmp.path().join("config.toml");
    config.workspace_dir = tmp.path().join("workspace");
    config.secrets_encrypt = false;
    config.save().await.expect("save fake config to disk");

    let ctx = ProviderContext {
        config: Arc::new(config) as Arc<crate::Config>,
        toolkit: "gmail".to_string(),
        connection_id: None,
        usage: ComposioUsageHandle::default(),
        max_items: None,
        sync_depth_days: None,
    };
    let res = ctx.execute("GMAIL_FETCH_EMAILS", None).await;
    let err = res.expect_err("no backend session must error");
    let msg = err.to_string();
    assert!(
        msg.contains("backend") || msg.contains("session"),
        "expected backend-session error, got: {msg}"
    );
}
