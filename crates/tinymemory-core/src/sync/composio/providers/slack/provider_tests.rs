//! Tests for the surrounding module.

use super::*;
use std::sync::Arc;

fn context(connection_id: Option<&str>) -> ProviderContext {
    ProviderContext {
        config: Arc::new(tinymemory_api::host::test_support::TestHostConfig::default())
            as Arc<crate::Config>,
        toolkit: "slack".into(),
        connection_id: connection_id.map(str::to_string),
        usage: crate::sync::composio::providers::ComposioUsageHandle::default(),
        max_items: None,
        sync_depth_days: None,
    }
}

#[test]
fn toolkit_slug_is_stable() {
    assert_eq!(SlackProvider::new().toolkit_slug(), "slack");
}

#[test]
fn sync_interval_matches_constant() {
    assert_eq!(
        SlackProvider::new().sync_interval_secs(),
        Some(SYNC_INTERVAL_SECS)
    );
}

#[test]
fn curated_tools_returns_slack_catalog() {
    let tools = SlackProvider::new().curated_tools().unwrap();
    assert!(tools
        .iter()
        .any(|t| t.slug == "SLACK_FETCH_CONVERSATION_HISTORY"));
    assert!(tools.iter().any(|t| t.slug == "SLACK_LIST_CONVERSATIONS"));
}

#[test]
fn post_process_action_result_delegates_to_post_process_module() {
    let provider = SlackProvider::new();
    let mut data = serde_json::json!({
        "channels": [{"id": "C1", "name": "eng", "is_private": false}]
    });
    // Calling with an unknown slug should be a no-op.
    provider.post_process_action_result("SLACK_UNKNOWN_ACTION", None, &mut data);
    assert!(
        data.get("channels").is_some(),
        "no-op slug must not mutate data"
    );
}

#[tokio::test]
async fn profile_and_backfill_failures_name_the_missing_boundary() {
    let provider = SlackProvider::new();
    let profile = provider
        .fetch_user_profile(&context(None))
        .await
        .unwrap_err();
    assert!(profile.contains(ACTION_AUTH_TEST));
    let backfill = run_backfill_via_search(&context(None), BACKFILL_DAYS)
        .await
        .unwrap_err();
    assert!(backfill.contains("missing connection_id"));
}

#[tokio::test]
async fn trigger_filters_non_messages_and_requires_connection_for_messages() {
    let provider = SlackProvider::new();
    provider
        .on_trigger(&context(None), "CHANNEL_CREATED", &serde_json::json!({}))
        .await
        .unwrap();
    let error = provider
        .on_trigger(&context(None), "MESSAGE_CREATED", &serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(error.contains("missing connection_id"));
    assert!(now_ms() > 0);
}
