//! Tests for the surrounding module.

use std::collections::HashMap;
use std::sync::Mutex;

use super::*;

#[derive(Default)]
struct MemoryStateStore(Mutex<HashMap<String, serde_json::Value>>);

#[async_trait]
impl SyncStateStore for MemoryStateStore {
    async fn get(&self, namespace: &str, key: &str) -> anyhow::Result<Option<serde_json::Value>> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .get(&format!("{namespace}:{key}"))
            .cloned())
    }

    async fn set(
        &self,
        namespace: &str,
        key: &str,
        value: &serde_json::Value,
    ) -> anyhow::Result<()> {
        self.0
            .lock()
            .unwrap()
            .insert(format!("{namespace}:{key}"), value.clone());
        Ok(())
    }
}

#[tokio::test]
async fn state_round_trips_cursor_dedup_and_budget() {
    let store = MemoryStateStore::default();
    let mut state = SyncState::new("gmail", "conn-1");
    state.advance_cursor("cursor-2");
    state.mark_synced("message-1");
    state.record_requests(3);
    state.save(&store).await.unwrap();

    let loaded = SyncState::load(&store, "gmail", "conn-1").await.unwrap();
    assert_eq!(loaded.cursor.as_deref(), Some("cursor-2"));
    assert!(loaded.is_synced("message-1"));
    assert_eq!(loaded.daily_budget.requests_used, 3);
}

/// The namespace is durable: every persisted Composio sync cursor lives
/// under this string, so a change strands all of them. The engine's copy
/// must agree; failing here means a coordinated migration, never a local
/// edit.
#[test]
fn the_state_namespace_is_pinned() {
    assert_eq!(
        KV_NAMESPACE, "composio-sync-state",
        "the Composio sync-state KV namespace changed; every persisted \
         cursor is stored under the old value and needs migrating"
    );
    assert_eq!(STATE_NAMESPACE, KV_NAMESPACE);
}

/// The engine persists the same state with its own copy of this type.
/// Pins the serialised shape so the copies cannot drift silently.
#[test]
fn state_line_format_is_pinned() {
    let mut state = SyncState::new("gmail", "conn-1");
    state.daily_budget.date = "2026-01-02".into();
    state.daily_budget.requests_used = 3;
    state.advance_cursor("c2");
    state.mark_synced("m1");
    state.item_versions.insert("m1".into(), "v1".into());
    state.set_last_seen_id("m1");
    state.set_last_sync_at_ms(1_000);
    let value = serde_json::to_value(&state).unwrap();
    assert_eq!(
        value,
        serde_json::json!({
            "toolkit": "gmail",
            "connection_id": "conn-1",
            "cursor": "c2",
            "synced_ids": ["m1"],
            "item_versions": {"m1": "v1"},
            "daily_budget": {"date": "2026-01-02", "requests_used": 3, "limit": 500},
            "last_seen_id": "m1",
            "last_sync_at_ms": 1000
        })
    );
}

#[test]
fn stale_budget_reports_full_and_resets_on_record() {
    let mut budget = DailyBudget {
        date: "2000-01-01".into(),
        requests_used: 499,
        limit: 500,
    };
    assert_eq!(budget.remaining(), 500);
    budget.record_requests(1);
    assert_eq!(budget.requests_used, 1);
    assert_eq!(budget.remaining(), 499);
}
