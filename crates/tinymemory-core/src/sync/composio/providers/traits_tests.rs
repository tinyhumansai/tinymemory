//! Tests for the surrounding module.

use super::*;

#[test]
fn sync_interval_env_var_uppercases_slug() {
    assert_eq!(
        sync_interval_env_var("slack"),
        "OPENHUMAN_COMPOSIO_SLACK_SYNC_INTERVAL_SECS"
    );
    assert_eq!(
        sync_interval_env_var("GitHub"),
        "OPENHUMAN_COMPOSIO_GITHUB_SYNC_INTERVAL_SECS"
    );
}

/// RAII guard for env var save/restore so the test does not leak
/// state to siblings within the same process.
struct EnvGuard {
    key: String,
    previous: Option<String>,
}

impl EnvGuard {
    fn set(key: &str, value: &str) -> Self {
        let previous = std::env::var(key).ok();
        std::env::set_var(key, value);
        Self {
            key: key.to_string(),
            previous,
        }
    }
    fn unset(key: &str) -> Self {
        let previous = std::env::var(key).ok();
        std::env::remove_var(key);
        Self {
            key: key.to_string(),
            previous,
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(v) => std::env::set_var(&self.key, v),
            None => std::env::remove_var(&self.key),
        }
    }
}

// Bundled into a single `#[test]` so cargo's per-test parallelism
// does not race on the shared env var. Each scenario explicitly
// drops its guard before the next so the env is in a known state.
#[test]
fn resolve_sync_interval_honors_per_toolkit_env() {
    let _lock = crate::test_env_lock::TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let key = sync_interval_env_var("slack");
    let default = 15 * 60;

    // Unset → default.
    let _g = EnvGuard::unset(&key);
    assert_eq!(resolve_sync_interval_secs("slack", default), default);
    drop(_g);

    // Valid override slows the cadence.
    let _g = EnvGuard::set(&key, "3600");
    assert_eq!(resolve_sync_interval_secs("slack", default), 3600);
    drop(_g);

    // Whitespace tolerated.
    let _g = EnvGuard::set(&key, "  1800  ");
    assert_eq!(resolve_sync_interval_secs("slack", default), 1800);
    drop(_g);

    // Zero rejected (would spin the scheduler).
    let _g = EnvGuard::set(&key, "0");
    assert_eq!(resolve_sync_interval_secs("slack", default), default);
    drop(_g);

    // Garbage rejected.
    let _g = EnvGuard::set(&key, "soon");
    assert_eq!(resolve_sync_interval_secs("slack", default), default);
    drop(_g);

    // Per-toolkit scoping: a different toolkit's var does not bleed
    // into slack's lookup.
    let gmail_key = sync_interval_env_var("gmail");
    let _slack_unset = EnvGuard::unset(&key);
    let _gmail_set = EnvGuard::set(&gmail_key, "120");
    assert_eq!(resolve_sync_interval_secs("slack", default), default);
    assert_eq!(resolve_sync_interval_secs("gmail", default), 120);
}
