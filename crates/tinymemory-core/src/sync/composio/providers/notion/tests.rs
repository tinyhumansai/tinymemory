//! Unit tests for the Notion provider.

use super::normalization::{extract_notion_cursor, extract_page_title, extract_results};
use super::NotionProvider;
use crate::sync::composio::providers::{
    ComposioProvider, ComposioUsageHandle, ProviderContext, TaskFetchFilter,
};
use serde_json::json;
use std::sync::Arc;

fn context(connection_id: Option<&str>) -> ProviderContext {
    ProviderContext {
        config: Arc::new(tinymemory_api::host::test_support::TestHostConfig::default())
            as Arc<crate::Config>,
        toolkit: "notion".into(),
        connection_id: connection_id.map(str::to_string),
        usage: ComposioUsageHandle::default(),
        max_items: None,
        sync_depth_days: None,
    }
}

#[test]
fn extract_results_walks_common_shapes() {
    let v1 = json!({ "data": { "results": [{"id": "p1"}] } });
    let v2 = json!({ "results": [{"id": "p2"}, {"id": "p3"}] });
    let v3 = json!({ "data": {} });
    assert_eq!(extract_results(&v1).len(), 1);
    assert_eq!(extract_results(&v2).len(), 2);
    assert_eq!(extract_results(&v3).len(), 0);
}

#[test]
fn extract_notion_cursor_finds_nested() {
    let v = json!({ "data": { "next_cursor": "abc123" } });
    assert_eq!(extract_notion_cursor(&v), Some("abc123".to_string()));
}

#[test]
fn extract_notion_cursor_none_when_missing() {
    let v = json!({ "data": { "has_more": false } });
    assert_eq!(extract_notion_cursor(&v), None);
}

#[test]
fn extract_page_title_from_properties() {
    let page = json!({
        "id": "page-1",
        "properties": {
            "Name": {
                "type": "title",
                "title": [
                    { "plain_text": "My " },
                    { "plain_text": "Page Title" }
                ]
            }
        }
    });
    assert_eq!(extract_page_title(&page), Some("My Page Title".to_string()));
}

#[test]
fn extract_page_title_fallback_to_top_level() {
    let page = json!({ "title": "Fallback Title" });
    assert_eq!(
        extract_page_title(&page),
        Some("Fallback Title".to_string())
    );
}

#[test]
fn extract_page_title_returns_none_when_missing() {
    let page = json!({ "id": "p1" });
    assert_eq!(extract_page_title(&page), None);
}

#[test]
fn provider_metadata_is_stable() {
    let p = NotionProvider::new();
    assert_eq!(p.toolkit_slug(), "notion");
    assert_eq!(p.sync_interval_secs(), Some(30 * 60));
}

#[test]
fn default_impl_matches_new() {
    let _a = NotionProvider::new();
    let _b = <NotionProvider as Default>::default();
}

// ── parse_database_results (list_databases parser) ───────────────────────────

#[test]
fn parse_database_results_keeps_databases_and_extracts_title() {
    use super::provider::parse_database_results;
    let data = json!({
        "results": [
            {
                "object": "database",
                "id": "db-1",
                "title": [{ "plain_text": "Engineering " }, { "plain_text": "Tasks" }]
            },
            // A page hit must be filtered out — list_databases is databases only.
            { "object": "page", "id": "pg-9", "title": [{ "plain_text": "Some page" }] },
            // Newest API exposes databases as `data_source`.
            { "object": "data_source", "id": "db-2", "title": [{ "plain_text": "Roadmap" }] },
            // Untitled database falls back to a synthesized label.
            { "object": "database", "id": "db-3", "title": [] }
        ]
    });
    let dbs = parse_database_results(&data);
    assert_eq!(
        dbs.len(),
        3,
        "two named databases + one data_source, page dropped"
    );
    assert_eq!(dbs[0].id, "db-1");
    assert_eq!(dbs[0].title, "Engineering Tasks");
    assert_eq!(dbs[1].id, "db-2");
    assert_eq!(dbs[1].title, "Roadmap");
    assert_eq!(dbs[2].title, "Notion database db-3");
}

#[test]
fn parse_database_results_handles_data_wrapper_and_empty() {
    use super::provider::parse_database_results;
    let wrapped = json!({ "data": { "results": [
        { "object": "database", "id": "x", "title": [{ "plain_text": "Wrapped" }] }
    ] } });
    let dbs = parse_database_results(&wrapped);
    assert_eq!(dbs.len(), 1);
    assert_eq!(dbs[0].title, "Wrapped");

    assert!(parse_database_results(&json!({ "results": [] })).is_empty());
}

#[tokio::test]
async fn provider_io_methods_fail_with_action_context_without_host() {
    let provider = NotionProvider::new();
    assert!(provider
        .fetch_user_profile(&context(Some("connection-1")))
        .await
        .unwrap_err()
        .contains("NOTION_GET_ABOUT_ME"));
    assert!(provider
        .fetch_tasks(
            &context(Some("connection-1")),
            &TaskFetchFilter {
                database_id: Some(" database-1 ".into()),
                max: 4,
                extra: json!({"archived": false}),
                ..Default::default()
            },
        )
        .await
        .unwrap_err()
        .contains("NOTION_QUERY_DATABASE"));
    assert!(provider
        .fetch_tasks(&context(Some("connection-1")), &TaskFetchFilter::default())
        .await
        .unwrap_err()
        .contains("NOTION_FETCH_DATA"));
    assert!(provider
        .list_databases(&context(Some("connection-1")))
        .await
        .unwrap_err()
        .contains("NOTION_SEARCH_NOTION_PAGE"));
    assert!(provider
        .on_trigger(&context(None), "PAGE_UPDATED", &json!({}))
        .await
        .unwrap_err()
        .contains("missing connection_id"));
}

#[test]
fn page_normalization_covers_properties_and_fallbacks() {
    use super::provider::normalize_notion_page;

    assert!(normalize_notion_page(&json!({"title": "missing id"})).is_none());
    let page = normalize_notion_page(&json!({
        "pageId": "page-7",
        "properties": {
            "Status": {"status": {"name": "In progress"}},
            "Assignee": {"people": [{"name": "Alice"}]},
            "Due": {"date": {"start": "2026-08-21"}},
            "Priority": {"select": {"name": "High"}}
        },
        "lastEditedTime": "2026-08-20T12:00:00Z"
    }))
    .unwrap();
    assert_eq!(page.external_id, "page-7");
    assert_eq!(page.title, "Notion page page-7");
    assert_eq!(page.status.as_deref(), Some("In progress"));
    assert_eq!(page.assignee.as_deref(), Some("Alice"));
    assert_eq!(page.due.as_deref(), Some("2026-08-21"));
    assert_eq!(page.priority.as_deref(), Some("High"));
}
