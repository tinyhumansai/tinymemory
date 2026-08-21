//! Unit tests for the Linear provider.

use super::normalization::{
    extract_issue_title, extract_issue_updated, extract_issues, extract_pagination_cursor,
    extract_viewer, extract_viewer_id,
};
use super::LinearProvider;
use crate::sync::composio::providers::{
    ComposioProvider, ComposioUsageHandle, ProviderContext, TaskFetchFilter,
};
use serde_json::json;
use std::sync::Arc;

fn context() -> ProviderContext {
    ProviderContext {
        config: Arc::new(tinymemory_api::host::test_support::TestHostConfig::default())
            as Arc<crate::Config>,
        toolkit: "linear".into(),
        connection_id: Some("connection-1".into()),
        usage: ComposioUsageHandle::default(),
        max_items: None,
        sync_depth_days: None,
    }
}

// ── extract_issues ───────────────────────────────────────────────────

#[test]
fn extract_issues_walks_common_shapes() {
    let v1 = json!({ "data": { "nodes": [{"id": "i1"}] } });
    let v2 = json!({ "nodes": [{"id": "i2"}, {"id": "i3"}] });
    let v3 = json!({ "data": { "issues": { "nodes": [{"id": "i4"}] } } });
    let v4 = json!({ "foo": "bar" });
    assert_eq!(extract_issues(&v1).len(), 1);
    assert_eq!(extract_issues(&v2).len(), 2);
    assert_eq!(extract_issues(&v3).len(), 1);
    assert_eq!(extract_issues(&v4).len(), 0);
}

// ── extract_issue_title ──────────────────────────────────────────────

#[test]
fn extract_issue_title_finds_title_field() {
    let issue = json!({ "id": "i1", "title": "Fix the login bug" });
    assert_eq!(
        extract_issue_title(&issue),
        Some("Fix the login bug".into())
    );
}

#[test]
fn extract_issue_title_falls_back_to_wrapped_data() {
    let issue = json!({ "data": { "title": "Wrapped issue" } });
    assert_eq!(extract_issue_title(&issue), Some("Wrapped issue".into()));
}

#[test]
fn extract_issue_title_falls_back_to_identifier() {
    let issue = json!({ "identifier": "ENG-99" });
    assert_eq!(extract_issue_title(&issue), Some("ENG-99".into()));
}

// ── extract_issue_updated ────────────────────────────────────────────

#[test]
fn extract_issue_updated_handles_camel_case() {
    let issue = json!({ "updatedAt": "2026-03-01T12:00:00.000Z" });
    assert_eq!(
        extract_issue_updated(&issue),
        Some("2026-03-01T12:00:00.000Z".to_string())
    );
}

#[test]
fn extract_issue_updated_handles_wrapped_data() {
    let issue = json!({ "data": { "updatedAt": "2026-01-15T08:30:00.000Z" } });
    assert_eq!(
        extract_issue_updated(&issue),
        Some("2026-01-15T08:30:00.000Z".to_string())
    );
}

// ── extract_viewer ───────────────────────────────────────────────────

#[test]
fn extract_viewer_finds_first_node() {
    let data = json!({ "data": { "nodes": [{ "id": "usr_1", "email": "a@b.com" }] } });
    let v = extract_viewer(&data).expect("viewer found");
    assert_eq!(v["id"], "usr_1");
}

#[test]
fn extract_viewer_from_top_level_nodes() {
    let data = json!({ "nodes": [{ "id": "usr_2" }] });
    let v = extract_viewer(&data).expect("viewer found");
    assert_eq!(v["id"], "usr_2");
}

#[test]
fn extract_viewer_fallback_direct_object() {
    let data = json!({ "id": "usr_direct", "name": "Alice" });
    let v = extract_viewer(&data).expect("viewer found");
    assert_eq!(v["id"], "usr_direct");
}

#[test]
fn extract_viewer_returns_none_when_absent() {
    let data = json!({ "foo": "bar" });
    assert!(extract_viewer(&data).is_none());
}

// ── extract_pagination_cursor ────────────────────────────────────────

#[test]
fn extract_pagination_cursor_returns_cursor_on_has_next_page() {
    let data = json!({
        "data": {
            "pageInfo": { "hasNextPage": true, "endCursor": "abc123" }
        }
    });
    assert_eq!(extract_pagination_cursor(&data), Some("abc123".to_string()));
}

#[test]
fn extract_pagination_cursor_returns_none_on_last_page() {
    let data = json!({
        "pageInfo": { "hasNextPage": false, "endCursor": "xyz" }
    });
    assert!(extract_pagination_cursor(&data).is_none());
}

#[test]
fn extract_pagination_cursor_returns_none_when_absent() {
    let data = json!({ "nodes": [{"id": "i1"}] });
    assert!(extract_pagination_cursor(&data).is_none());
}

// ── extract_viewer_id ────────────────────────────────────────────────

#[test]
fn extract_viewer_id_from_data_nodes() {
    let data = json!({ "data": { "nodes": [{ "id": "usr_abc" }] } });
    assert_eq!(extract_viewer_id(&data), Some("usr_abc".to_string()));
}

#[test]
fn extract_viewer_id_returns_none_when_absent() {
    let data = json!({ "foo": "bar" });
    assert!(extract_viewer_id(&data).is_none());
}

// ── provider metadata ────────────────────────────────────────────────

#[test]
fn provider_metadata_is_stable() {
    let p = LinearProvider::new();
    assert_eq!(p.toolkit_slug(), "linear");
    assert_eq!(p.sync_interval_secs(), Some(30 * 60));
    assert!(p.curated_tools().is_some());
}

#[test]
fn curated_tools_contains_core_sync_surface() {
    let p = LinearProvider::new();
    let curated = p.curated_tools().expect("LINEAR_CURATED is registered");
    let slugs: Vec<&str> = curated.iter().map(|t| t.slug).collect();
    assert!(
        slugs.contains(&"LINEAR_LIST_LINEAR_USERS"),
        "LINEAR_LIST_LINEAR_USERS must be in curated catalog"
    );
    assert!(
        slugs.contains(&"LINEAR_LIST_LINEAR_ISSUES"),
        "LINEAR_LIST_LINEAR_ISSUES must be in curated catalog"
    );
}

#[test]
fn default_impl_matches_new() {
    let a = LinearProvider::new();
    let b = <LinearProvider as Default>::default();
    assert_eq!(a.toolkit_slug(), b.toolkit_slug());
    assert_eq!(a.sync_interval_secs(), b.sync_interval_secs());
    assert_eq!(
        a.curated_tools().map(<[_]>::len),
        b.curated_tools().map(<[_]>::len),
    );
}

#[tokio::test]
async fn provider_calls_fail_contextually_without_a_composio_host() {
    let provider = LinearProvider::new();
    let profile = provider.fetch_user_profile(&context()).await.unwrap_err();
    assert!(profile.contains("LINEAR_LIST_LINEAR_USERS"));
    let tasks = provider
        .fetch_tasks(
            &context(),
            &TaskFetchFilter {
                assignee_is_me: false,
                team_id: Some(" team-1 ".into()),
                extra: json!({"includeArchived": false}),
                max: 3,
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(tasks.contains("LINEAR_LIST_LINEAR_ISSUES"));
}

#[test]
fn issue_normalization_covers_fallbacks_labels_and_missing_id() {
    use super::provider::{extract_linear_labels, normalize_linear_issue};

    assert!(normalize_linear_issue(&json!({"title": "missing id"})).is_none());
    let task = normalize_linear_issue(&json!({
        "identifier": "ENG-7",
        "description": "body",
        "state": {"name": "Started"},
        "labels": {"nodes": [{"name": "bug"}, {}, {"name": "urgent"}]}
    }))
    .unwrap();
    assert_eq!(task.external_id, "ENG-7");
    assert_eq!(task.title, "ENG-7");
    assert_eq!(task.labels, vec!["bug", "urgent"]);
    assert!(extract_linear_labels(&json!({})).is_empty());
}
