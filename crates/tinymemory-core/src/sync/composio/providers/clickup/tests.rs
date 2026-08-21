//! Unit tests for the ClickUp provider.

use super::normalization::{
    extract_task_name, extract_task_updated, extract_tasks, extract_user_id, extract_workspace_ids,
};
use super::provider::{
    normalize_clickup_task, ACTION_GET_AUTHORIZED_TEAMS_WORKSPACES, ACTION_GET_AUTHORIZED_USER,
    ACTION_GET_FILTERED_TEAM_TASKS,
};
use super::ClickUpProvider;
use crate::sync::composio::providers::{
    ComposioProvider, ComposioUsageHandle, ProviderContext, TaskFetchFilter, TaskKind,
};
use serde_json::json;
use std::sync::Arc;

fn context() -> ProviderContext {
    ProviderContext {
        config: Arc::new(tinymemory_api::host::test_support::TestHostConfig::default())
            as Arc<crate::Config>,
        toolkit: "clickup".into(),
        connection_id: Some("connection-1".into()),
        usage: ComposioUsageHandle::default(),
        max_items: None,
        sync_depth_days: None,
    }
}

#[test]
fn extract_tasks_walks_common_shapes() {
    let v1 = json!({ "data": { "tasks": [{"id": "t1"}] } });
    let v2 = json!({ "tasks": [{"id": "t2"}, {"id": "t3"}] });
    let v3 = json!({ "data": {} });
    assert_eq!(extract_tasks(&v1).len(), 1);
    assert_eq!(extract_tasks(&v2).len(), 2);
    assert_eq!(extract_tasks(&v3).len(), 0);
}

#[test]
fn extract_task_name_finds_name_field() {
    let task = json!({ "id": "abc", "name": "Build feature X" });
    assert_eq!(extract_task_name(&task), Some("Build feature X".into()));
}

#[test]
fn extract_task_name_falls_back_to_wrapped_data() {
    let task = json!({ "data": { "name": "Wrapped" } });
    assert_eq!(extract_task_name(&task), Some("Wrapped".into()));
}

#[test]
fn extract_task_name_returns_none_when_missing() {
    let task = json!({ "id": "abc" });
    assert!(extract_task_name(&task).is_none());
}

#[test]
fn extract_task_updated_handles_string_form() {
    let task = json!({ "date_updated": "1733412345678" });
    assert_eq!(
        extract_task_updated(&task),
        Some("1733412345678".to_string())
    );
}

#[test]
fn extract_task_updated_handles_nested_data() {
    let task = json!({ "data": { "dateUpdated": "1700000000000" } });
    assert_eq!(
        extract_task_updated(&task),
        Some("1700000000000".to_string())
    );
}

#[test]
fn extract_task_updated_returns_none_when_missing() {
    let task = json!({ "id": "abc" });
    assert!(extract_task_updated(&task).is_none());
}

#[test]
fn extract_user_id_handles_numeric_id() {
    let data = json!({ "user": { "id": 12345 } });
    assert_eq!(extract_user_id(&data), Some("12345".to_string()));
}

#[test]
fn extract_user_id_handles_wrapped_payload() {
    let data = json!({ "data": { "user": { "id": "777" } } });
    assert_eq!(extract_user_id(&data), Some("777".to_string()));
}

#[test]
fn extract_user_id_none_when_missing() {
    let data = json!({ "foo": "bar" });
    assert!(extract_user_id(&data).is_none());
}

#[test]
fn extract_workspace_ids_from_teams_array() {
    let data = json!({
        "teams": [
            { "id": "ws1", "name": "Personal" },
            { "id": "ws2", "name": "Acme" },
        ]
    });
    assert_eq!(extract_workspace_ids(&data), vec!["ws1", "ws2"]);
}

#[test]
fn extract_workspace_ids_handles_wrapped_payload() {
    let data = json!({
        "data": {
            "teams": [
                { "id": "ws1" },
                { "id": "ws2" },
                { "id": "ws3" },
            ]
        }
    });
    assert_eq!(extract_workspace_ids(&data), vec!["ws1", "ws2", "ws3"]);
}

#[test]
fn extract_workspace_ids_empty_when_no_teams() {
    let data = json!({ "foo": "bar" });
    assert!(extract_workspace_ids(&data).is_empty());
}

#[test]
fn extract_workspace_ids_skips_entries_without_id() {
    let data = json!({
        "teams": [
            { "name": "Anonymous" },
            { "id": "ws1", "name": "Real" },
        ]
    });
    assert_eq!(extract_workspace_ids(&data), vec!["ws1"]);
}

#[test]
fn provider_metadata_is_stable() {
    let p = ClickUpProvider::new();
    assert_eq!(p.toolkit_slug(), "clickup");
    assert_eq!(p.sync_interval_secs(), Some(30 * 60));
    assert!(p.curated_tools().is_some());
}

#[test]
fn curated_tools_contains_core_read_surface() {
    let p = ClickUpProvider::new();
    let curated = p.curated_tools().expect("CLICKUP_CURATED is registered");
    let slugs: Vec<&str> = curated.iter().map(|t| t.slug).collect();
    // The three actions the sync path depends on must be advertised.
    assert!(slugs.contains(&"CLICKUP_GET_AUTHORIZED_USER"));
    assert!(slugs.contains(&"CLICKUP_GET_AUTHORIZED_TEAMS_WORKSPACES"));
    assert!(slugs.contains(&"CLICKUP_GET_FILTERED_TEAM_TASKS"));
}

#[test]
fn default_impl_matches_new() {
    // `ClickUpProvider` is a unit struct, so we compare observable
    // trait surface instead of deriving `PartialEq`. This catches a
    // future regression where `new()` and `default()` drift apart
    // (e.g. one is given an extra field but the other is forgotten).
    let a = ClickUpProvider::new();
    let b = <ClickUpProvider as Default>::default();
    assert_eq!(a.toolkit_slug(), b.toolkit_slug());
    assert_eq!(a.sync_interval_secs(), b.sync_interval_secs());
    assert_eq!(
        a.curated_tools().map(<[_]>::len),
        b.curated_tools().map(<[_]>::len),
    );
}

#[tokio::test]
async fn provider_calls_fail_with_the_action_that_needs_a_host() {
    let provider = ClickUpProvider::new();
    let ctx = context();
    let profile = provider
        .fetch_user_profile(&ctx)
        .await
        .expect_err("profile requires a configured host");
    assert!(profile.contains(ACTION_GET_AUTHORIZED_USER), "{profile}");

    let workspaces = provider
        .fetch_tasks(&ctx, &TaskFetchFilter::default())
        .await
        .expect_err("workspace discovery requires a configured host");
    assert!(
        workspaces.contains(ACTION_GET_AUTHORIZED_TEAMS_WORKSPACES),
        "{workspaces}"
    );

    let tasks = provider
        .fetch_tasks(
            &ctx,
            &TaskFetchFilter {
                team_id: Some("team-1".into()),
                assignee_is_me: false,
                ..Default::default()
            },
        )
        .await
        .expect_err("task fetch requires a configured host");
    assert!(tasks.contains(ACTION_GET_FILTERED_TEAM_TASKS), "{tasks}");
}

#[test]
fn task_normalization_maps_wrapped_fields_and_rejects_missing_ids() {
    let task = normalize_clickup_task(&json!({
        "data": {
            "task_id": "task-7",
            "description": "Ship deterministic tests",
            "url": "https://app.clickup.com/t/task-7",
            "status": {"status": "in progress"},
            "assignees": [{"username": "alice"}],
            "due_date": "1700000000000",
            "priority": {"priority": "high"},
            "dateUpdated": "1690000000000"
        }
    }))
    .expect("wrapped task normalizes");
    assert_eq!(task.external_id, "task-7");
    assert_eq!(task.title, "ClickUp task task-7");
    assert_eq!(task.kind, TaskKind::Generic);
    assert_eq!(task.body.as_deref(), Some("Ship deterministic tests"));
    assert_eq!(task.assignee.as_deref(), Some("alice"));
    assert_eq!(task.status.as_deref(), Some("in progress"));
    assert_eq!(task.priority.as_deref(), Some("high"));
    assert_eq!(task.labels, Vec::<String>::new());
    assert!(normalize_clickup_task(&json!({"name": "unroutable"})).is_none());
}
