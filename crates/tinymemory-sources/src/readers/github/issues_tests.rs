//! Offline behavioral tests for issue and pull-request list/read orchestration.

use super::*;
use crate::readers::github::api::with_test_responses;
use crate::readers::github::types::LIST_CACHE;

fn issue_json(number: u64) -> serde_json::Value {
    serde_json::json!({
        "number": number,
        "title": "Broken widget",
        "body": "Steps to reproduce",
        "state": "open",
        "user": {"login": "alice"},
        "labels": [{"name": "bug"}, {"name": "urgent"}],
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-02T03:04:05Z",
        "pull_request": null
    })
}

fn pr_json(number: u64) -> serde_json::Value {
    serde_json::json!({
        "number": number,
        "title": "Fix widget",
        "body": "Implements the fix",
        "state": "closed",
        "user": {"login": "bob"},
        "labels": [{"name": "ready"}],
        "created_at": "2026-01-03T00:00:00Z",
        "updated_at": "2026-01-04T00:00:00Z",
        "merged_at": "2026-01-05T00:00:00Z"
    })
}

#[tokio::test]
async fn lists_cache_and_render_issues_and_pull_requests_without_network() {
    LIST_CACHE.lock().expect("list cache").clear();
    let disguised_pr = serde_json::json!({
        "number": 99,
        "title": "PR returned by issues endpoint",
        "body": null,
        "state": "open",
        "user": null,
        "labels": [],
        "created_at": null,
        "updated_at": null,
        "pull_request": {"url":"https://example.invalid/pr/99"}
    });
    let listed_issues = with_test_responses(
        vec![Ok(
            serde_json::json!([issue_json(7), disguised_pr]).to_string()
        )],
        list_issues("acme", "widget", 10, false),
    )
    .await
    .expect("list issues");
    assert_eq!(listed_issues.len(), 1);
    assert_eq!(listed_issues[0].id, "issue:7");
    assert_eq!(listed_issues[0].title, "#7 Broken widget");
    assert_eq!(listed_issues[0].updated_at_ms, Some(1_767_323_045_000));

    let issue = with_test_responses(
        vec![Ok(serde_json::json!([
            {
                "user":{"login":"carol"},
                "body":"Confirmed",
                "created_at":"2026-01-02T04:00:00Z"
            },
            {"user":null,"body":null,"created_at":null}
        ])
        .to_string())],
        read_issue("acme", "widget", 7, false),
    )
    .await
    .expect("read cached issue");
    assert_eq!(issue.id, "issue:7");
    assert_eq!(issue.title, "#7 Broken widget");
    assert!(issue.body.contains("**Participants:** @alice @carol"));
    assert!(issue.body.contains("**Labels:** bug, urgent"));
    assert!(issue.body.contains("### @carol (2026-01-02T04:00:00Z)"));
    assert!(issue.body.contains("### @unknown (unknown)"));
    assert_eq!(issue.metadata["state"], "open");

    let listed_prs = with_test_responses(
        vec![Ok(serde_json::json!([pr_json(8)]).to_string())],
        list_prs("acme", "widget", 10, true),
    )
    .await
    .expect("list pull requests");
    assert_eq!(listed_prs[0].id, "pr:8");
    assert_eq!(listed_prs[0].title, "PR #8 Fix widget");

    let pr = with_test_responses(
        vec![Ok("not valid comments JSON".into())],
        read_pr("acme", "widget", 8, true),
    )
    .await
    .expect("read cached pull request despite malformed comments");
    assert!(pr
        .body
        .contains("**State:** closed (merged at 2026-01-05T00:00:00Z)"));
    assert!(pr.body.contains("**Participants:** @bob"));
    assert!(!pr.body.contains("## Comments"));
    assert_eq!(pr.metadata["merged"], true);
    assert!(LIST_CACHE.lock().expect("list cache").is_empty());

    assert_uncached_reads_and_failures().await;
}

async fn assert_uncached_reads_and_failures() {
    LIST_CACHE.lock().expect("list cache").clear();
    let issue = with_test_responses(
        vec![
            Ok(issue_json(11).to_string()),
            Err("comments unavailable".into()),
        ],
        read_issue("acme", "widget", 11, false),
    )
    .await
    .expect("uncached issue read");
    assert_eq!(issue.id, "issue:11");
    assert!(!issue.body.contains("## Comments"));

    let transport_error = with_test_responses(
        vec![Err("offline".into())],
        list_issues("acme", "widget", 10, false),
    )
    .await
    .expect_err("transport failure must propagate");
    assert_eq!(transport_error, "offline");

    let parse_error = with_test_responses(
        vec![Ok("{}".into())],
        list_issues("acme", "widget", 10, false),
    )
    .await
    .expect_err("malformed list must fail");
    assert!(parse_error.contains("parse issues page 1"));

    let read_error =
        with_test_responses(vec![Ok("[]".into())], read_pr("acme", "widget", 12, false))
            .await
            .expect_err("malformed pull request must fail");
    assert!(read_error.contains("parse PR"));

    let exhausted = with_test_responses(Vec::new(), list_prs("acme", "widget", 1, false))
        .await
        .expect_err("empty fixture must not reach network");
    assert!(exhausted.contains("no deterministic GitHub response queued"));
}
