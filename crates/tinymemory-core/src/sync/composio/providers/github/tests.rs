//! Unit tests for the GitHub Composio provider.

use super::normalization::{
    extract_issue_id, extract_issue_title, extract_issue_updated_at, extract_issues,
    extract_user_login,
};
use super::provider::github_env_token;
use super::provider::{
    build_fetch_query, github_search_arg_pairs, normalize_github_issue,
    normalize_github_repo_filter, ACTION_GET_AUTHENTICATED_USER, ACTION_SEARCH_ISSUES,
};
use super::tools::GITHUB_CURATED;
use super::GitHubProvider;
use crate::sync::composio::providers::ComposioProvider;
use crate::sync::composio::providers::{
    ComposioUsageHandle, GithubFetchMode, ProviderContext, TaskFetchFilter, TaskKind,
};
use serde_json::json;
use std::sync::Arc;

// ── extract_issues ───────────────────────────────────────────────────────────

#[test]
fn extract_issues_walks_data_items_shape() {
    let data = json!({ "data": { "items": [{"id": 1u64}] } });
    assert_eq!(extract_issues(&data).len(), 1);
}

#[test]
fn extract_issues_walks_top_level_items_shape() {
    let data = json!({ "items": [{"id": 1u64}, {"id": 2u64}] });
    assert_eq!(extract_issues(&data).len(), 2);
}

#[test]
fn extract_issues_returns_empty_when_no_items_key() {
    let data = json!({ "foo": "bar" });
    assert!(extract_issues(&data).is_empty());
}

#[test]
fn extract_issues_handles_data_data_nesting() {
    let data = json!({ "data": { "data": { "items": [{"id": 9u64}] } } });
    assert_eq!(extract_issues(&data).len(), 1);
}

// ── extract_issue_id ─────────────────────────────────────────────────────────

#[test]
fn extract_issue_id_from_numeric_id() {
    let issue = json!({ "id": 123456789u64, "title": "Fix race" });
    assert_eq!(extract_issue_id(&issue), Some("123456789".to_string()));
}

#[test]
fn extract_issue_id_from_wrapped_data() {
    let issue = json!({ "data": { "id": 42u64 } });
    assert_eq!(extract_issue_id(&issue), Some("42".to_string()));
}

#[test]
fn extract_issue_id_falls_back_to_html_url_path() {
    let issue = json!({
        "html_url": "https://github.com/owner/repo/issues/7"
    });
    assert_eq!(extract_issue_id(&issue), Some("owner/repo#7".to_string()));
}

#[test]
fn extract_issue_id_none_when_no_id_or_url() {
    let issue = json!({ "title": "orphan" });
    assert!(extract_issue_id(&issue).is_none());
}

// ── extract_issue_title ──────────────────────────────────────────────────────

#[test]
fn extract_issue_title_builds_prefixed_title() {
    let issue = json!({
        "id": 1u64,
        "title": "Fix race condition",
        "html_url": "https://github.com/acme/core/issues/99"
    });
    assert_eq!(
        extract_issue_title(&issue),
        Some("GitHub: acme/core#99: Fix race condition".to_string())
    );
}

#[test]
fn extract_issue_title_pr_url_also_works() {
    let issue = json!({
        "id": 2u64,
        "title": "Add feature",
        "html_url": "https://github.com/org/repo/pull/101"
    });
    assert_eq!(
        extract_issue_title(&issue),
        Some("GitHub: org/repo#101: Add feature".to_string())
    );
}

#[test]
fn extract_issue_title_returns_raw_title_when_no_url() {
    let issue = json!({ "title": "Bare title" });
    assert_eq!(extract_issue_title(&issue), Some("Bare title".to_string()));
}

#[test]
fn extract_issue_title_none_when_no_title() {
    let issue = json!({ "id": 1u64 });
    assert!(extract_issue_title(&issue).is_none());
}

// ── extract_issue_updated_at ─────────────────────────────────────────────────

#[test]
fn extract_issue_updated_at_from_top_level() {
    let issue = json!({ "updated_at": "2024-05-21T15:30:00Z" });
    assert_eq!(
        extract_issue_updated_at(&issue),
        Some("2024-05-21T15:30:00Z".to_string())
    );
}

#[test]
fn extract_issue_updated_at_from_data_wrapper() {
    let issue = json!({ "data": { "updated_at": "2023-01-01T00:00:00Z" } });
    assert_eq!(
        extract_issue_updated_at(&issue),
        Some("2023-01-01T00:00:00Z".to_string())
    );
}

#[test]
fn extract_issue_updated_at_none_when_missing() {
    let issue = json!({ "id": 1u64 });
    assert!(extract_issue_updated_at(&issue).is_none());
}

// ── extract_user_login ───────────────────────────────────────────────────────

#[test]
fn extract_user_login_from_top_level() {
    let data = json!({ "login": "octocat" });
    assert_eq!(extract_user_login(&data), Some("octocat".to_string()));
}

#[test]
fn extract_user_login_from_data_wrapper() {
    let data = json!({ "data": { "login": "monalisa" } });
    assert_eq!(extract_user_login(&data), Some("monalisa".to_string()));
}

#[test]
fn extract_user_login_none_when_missing() {
    let data = json!({ "id": 1u64 });
    assert!(extract_user_login(&data).is_none());
}

// ── provider metadata ────────────────────────────────────────────────────────

#[test]
fn provider_metadata_is_stable() {
    let p = GitHubProvider::new();
    assert_eq!(p.toolkit_slug(), "github");
    assert_eq!(p.sync_interval_secs(), Some(30 * 60));
    assert!(p.curated_tools().is_some());
}

#[test]
fn curated_tools_contains_core_actions() {
    let p = GitHubProvider::new();
    let curated = p.curated_tools().expect("GITHUB_CURATED is registered");
    let slugs: Vec<&str> = curated.iter().map(|t| t.slug).collect();
    assert!(slugs.contains(&"GITHUB_GET_THE_AUTHENTICATED_USER"));
    assert!(slugs.contains(&"GITHUB_SEARCH_ISSUES_AND_PULL_REQUESTS"));
    assert!(slugs.contains(&"GITHUB_LIST_REPOSITORY_ISSUES"));
    assert!(slugs.contains(&"GITHUB_LIST_REPOSITORIES_FOR_THE_AUTHENTICATED_USER"));
    assert!(slugs.contains(&"GITHUB_CREATE_A_REPOSITORY_FOR_THE_AUTHENTICATED_USER"));
    // DELETE_A_REFERENCE replaces DELETE_A_BRANCH (Composio v3 rename).
    assert!(slugs.contains(&"GITHUB_DELETE_A_REFERENCE"));
    // CLOSE_AN_ISSUE was removed — callers must use UPDATE_AN_ISSUE with state:"closed".
    assert!(
        !slugs.contains(&"GITHUB_CLOSE_AN_ISSUE"),
        "GITHUB_CLOSE_AN_ISSUE was removed — use GITHUB_UPDATE_AN_ISSUE with state:closed"
    );
}

#[test]
fn default_impl_matches_new() {
    let a = GitHubProvider::new();
    let b = <GitHubProvider as Default>::default();
    assert_eq!(a.toolkit_slug(), b.toolkit_slug());
    assert_eq!(a.sync_interval_secs(), b.sync_interval_secs());
    assert_eq!(
        a.curated_tools().map(<[_]>::len),
        b.curated_tools().map(<[_]>::len),
    );
}

#[test]
fn build_fetch_query_scopes_repo_labels_state_and_assignee() {
    let query = build_fetch_query(&TaskFetchFilter {
        repo: Some("tinyhumansai/openhuman".to_string()),
        labels: vec!["bug".to_string(), "agent harness".to_string()],
        state: Some("open".to_string()),
        assignee_is_me: true,
        ..Default::default()
    });

    assert_eq!(
        query,
        "repo:tinyhumansai/openhuman label:\"bug\" label:\"agent harness\" assignee:@me state:open"
    );
}

#[test]
fn build_fetch_query_normalizes_github_repo_urls() {
    let query = build_fetch_query(&TaskFetchFilter {
        repo: Some("https://github.com/tinyhumansai/openhuman/pull/3267".to_string()),
        state: Some("open".to_string()),
        ..Default::default()
    });

    assert_eq!(query, "repo:tinyhumansai/openhuman state:open");
}

#[test]
fn normalize_github_repo_filter_accepts_common_repo_inputs() {
    assert_eq!(
        normalize_github_repo_filter("tinyhumansai/openhuman"),
        "tinyhumansai/openhuman"
    );
    assert_eq!(
        normalize_github_repo_filter("https://github.com/tinyhumansai/openhuman.git"),
        "tinyhumansai/openhuman"
    );
    assert_eq!(
        normalize_github_repo_filter("git@github.com:tinyhumansai/openhuman.git"),
        "tinyhumansai/openhuman"
    );
}

#[test]
fn build_fetch_query_falls_back_to_involves_me_when_unscoped() {
    // No scoping and no explicit state: fall back to `involves:@me` and bias
    // toward open items so closed issues / merged PRs aren't even fetched.
    assert_eq!(
        build_fetch_query(&TaskFetchFilter::default()),
        "involves:@me is:open"
    );
}

#[test]
fn build_fetch_query_appends_is_open_when_no_explicit_state() {
    // Scoped by repo but no explicit state — `is:open` is appended.
    let query = build_fetch_query(&TaskFetchFilter {
        repo: Some("tinyhumansai/openhuman".to_string()),
        ..Default::default()
    });
    assert_eq!(query, "repo:tinyhumansai/openhuman is:open");
}

#[test]
fn build_fetch_query_respects_explicit_state_without_double_open() {
    // Explicit `state` is respected verbatim and `is:open` is NOT added.
    let query = build_fetch_query(&TaskFetchFilter {
        repo: Some("tinyhumansai/openhuman".to_string()),
        state: Some("closed".to_string()),
        ..Default::default()
    });
    assert_eq!(query, "repo:tinyhumansai/openhuman state:closed");
    assert!(!query.contains("is:open"));
}

#[test]
fn github_search_arg_pairs_render_cli_and_rest_params() {
    let args = json!({
        "q": "repo:tinyhumansai/openhuman state:open",
        "sort": "updated",
        "order": "desc",
        "per_page": 25,
        "page": 1,
        "include_prs": true,
        "skip": null
    });

    let pairs = github_search_arg_pairs(&args).expect("pairs");
    assert!(pairs.contains(&(
        "q".to_string(),
        "repo:tinyhumansai/openhuman state:open".to_string()
    )));
    assert!(pairs.contains(&("per_page".to_string(), "25".to_string())));
    assert!(pairs.contains(&("include_prs".to_string(), "true".to_string())));
    assert!(!pairs.iter().any(|(key, _)| key == "skip"));
}

// ── slug regression tests (#2768) ───────────────────────────────────────────
//
// Guard the current Composio action slug values used by the GitHub provider.
// Outdated slugs (e.g. GITHUB_USERS_GET_AUTHENTICATED, GITHUB_LIST_REPOS,
// GITHUB_LIST_ISSUES) were previously scattered across tests; these assertions
// pin the correct values in one place so a slug rename is caught immediately.

#[test]
fn action_get_authenticated_user_slug_is_current() {
    // The Composio v3 slug is GITHUB_GET_THE_AUTHENTICATED_USER.
    // Regression: was mistakenly referenced as GITHUB_USERS_GET_AUTHENTICATED
    // in tests (see issue #2768).
    assert_eq!(
        ACTION_GET_AUTHENTICATED_USER, "GITHUB_GET_THE_AUTHENTICATED_USER",
        "slug must match Composio v3 catalog; old slug GITHUB_USERS_GET_AUTHENTICATED is retired"
    );
}

#[test]
fn action_search_issues_slug_is_current() {
    assert_eq!(
        ACTION_SEARCH_ISSUES, "GITHUB_SEARCH_ISSUES_AND_PULL_REQUESTS",
        "slug must match Composio v3 catalog"
    );
}

#[test]
fn curated_list_does_not_contain_retired_slugs() {
    // Guard against re-introducing removed slugs that no longer exist in the
    // Composio v3 GitHub app catalog.
    const RETIRED: &[&str] = &[
        "GITHUB_USERS_GET_AUTHENTICATED", // replaced by GITHUB_GET_THE_AUTHENTICATED_USER
        "GITHUB_LIST_REPOS", // replaced by GITHUB_LIST_REPOSITORIES_FOR_THE_AUTHENTICATED_USER
        "GITHUB_LIST_ISSUES", // replaced by GITHUB_LIST_REPOSITORY_ISSUES
        "GITHUB_COMMIT_MULTIPLE_FILES", // removed from Composio catalog
        "GITHUB_CLOSE_AN_ISSUE", // removed; use GITHUB_UPDATE_AN_ISSUE with state=closed
        "GITHUB_DELETE_A_BRANCH", // removed; use GITHUB_DELETE_A_REFERENCE
    ];

    let slugs: Vec<&str> = GITHUB_CURATED.iter().map(|t| t.slug).collect();
    for retired in RETIRED {
        assert!(
            !slugs.contains(retired),
            "curated list must not contain retired slug {retired} (see #2768)"
        );
    }
}

#[test]
fn curated_list_contains_current_read_slugs() {
    // Verify that the primary read-tier actions are present with their correct
    // v3 slug names (not the old v1/v2 names).
    let slugs: Vec<&str> = GITHUB_CURATED.iter().map(|t| t.slug).collect();
    let required = [
        "GITHUB_GET_THE_AUTHENTICATED_USER",
        "GITHUB_LIST_REPOSITORIES_FOR_THE_AUTHENTICATED_USER",
        "GITHUB_LIST_REPOSITORY_ISSUES",
        "GITHUB_SEARCH_ISSUES_AND_PULL_REQUESTS",
        "GITHUB_LIST_PULL_REQUESTS",
        "GITHUB_GET_A_PULL_REQUEST",
    ];
    for slug in required {
        assert!(
            slugs.contains(&slug),
            "curated list must contain current slug {slug} (see #2768)"
        );
    }
}

#[test]
fn curated_list_contains_current_write_slugs() {
    let slugs: Vec<&str> = GITHUB_CURATED.iter().map(|t| t.slug).collect();
    let required = [
        "GITHUB_CREATE_AN_ISSUE",
        "GITHUB_UPDATE_AN_ISSUE",
        "GITHUB_CREATE_A_PULL_REQUEST",
        "GITHUB_MERGE_A_PULL_REQUEST",
    ];
    for slug in required {
        assert!(
            slugs.contains(&slug),
            "curated list must contain current write slug {slug} (see #2768)"
        );
    }
}

// ── GithubFetchMode (#3279) ─────────────────────────────────────────────────
//
// The fetch-mode selector makes the local `gh`/REST path a *true fallback*
// (default `Auto`) instead of a hard Composio replacement. These tests pin the
// default and the serde wire contract the UI persists into a source's
// `FilterSpec::Github { fetch_mode }`.

#[test]
fn github_fetch_mode_defaults_to_auto() {
    // `Auto` must be the default so shipped Composio users keep working and
    // local/dev setups still get the fallback — neither side regresses.
    assert_eq!(GithubFetchMode::default(), GithubFetchMode::Auto);
}

#[test]
fn task_fetch_filter_default_uses_auto_fetch_mode() {
    // A filter built with no explicit mode (the common path) carries `Auto`.
    let filter = TaskFetchFilter::default();
    assert_eq!(filter.github_fetch_mode, GithubFetchMode::Auto);
}

#[test]
fn github_fetch_mode_serializes_snake_case() {
    assert_eq!(
        serde_json::to_value(GithubFetchMode::Auto).expect("ser auto"),
        json!("auto")
    );
    assert_eq!(
        serde_json::to_value(GithubFetchMode::Composio).expect("ser composio"),
        json!("composio")
    );
    assert_eq!(
        serde_json::to_value(GithubFetchMode::Local).expect("ser local"),
        json!("local")
    );
}

#[test]
fn github_fetch_mode_deserializes_each_variant() {
    let auto: GithubFetchMode = serde_json::from_value(json!("auto")).expect("de auto");
    let composio: GithubFetchMode = serde_json::from_value(json!("composio")).expect("de composio");
    let local: GithubFetchMode = serde_json::from_value(json!("local")).expect("de local");
    assert_eq!(auto, GithubFetchMode::Auto);
    assert_eq!(composio, GithubFetchMode::Composio);
    assert_eq!(local, GithubFetchMode::Local);
}

#[test]
fn github_fetch_mode_round_trips_through_json() {
    for mode in [
        GithubFetchMode::Auto,
        GithubFetchMode::Composio,
        GithubFetchMode::Local,
    ] {
        let json = serde_json::to_string(&mode).expect("ser");
        let back: GithubFetchMode = serde_json::from_str(&json).expect("de");
        assert_eq!(back, mode, "round-trip must preserve {mode:?}");
    }
}

#[test]
fn github_fetch_mode_rejects_unknown_variant() {
    let parsed: Result<GithubFetchMode, _> = serde_json::from_value(json!("remote"));
    assert!(parsed.is_err(), "unknown mode strings must fail to parse");
}

// ── github_search_arg_pairs edge cases (#3279) ──────────────────────────────

#[test]
fn github_search_arg_pairs_skips_null_and_empty_string_values() {
    // Null values are dropped entirely; whitespace-only / empty strings are
    // trimmed to empty and also dropped, so they never reach the gh CLI / REST
    // query as blank params.
    let args = json!({
        "q": "involves:@me",
        "empty": "",
        "blank": "   ",
        "missing": null,
        "page": 1,
    });
    let pairs = github_search_arg_pairs(&args).expect("pairs");
    assert!(pairs.contains(&("q".to_string(), "involves:@me".to_string())));
    assert!(pairs.contains(&("page".to_string(), "1".to_string())));
    assert!(!pairs.iter().any(|(k, _)| k == "empty"));
    assert!(!pairs.iter().any(|(k, _)| k == "blank"));
    assert!(!pairs.iter().any(|(k, _)| k == "missing"));
}

#[test]
fn github_search_arg_pairs_errors_when_not_an_object() {
    // A non-object value (array, scalar) is a programmer error — surface it
    // rather than silently producing an empty arg set.
    let err = github_search_arg_pairs(&json!(["not", "an", "object"]))
        .expect_err("array args must error");
    assert!(err.contains("JSON object"), "got: {err}");
}

// ── github_env_token (#3279) ────────────────────────────────────────────────

#[test]
fn github_env_token_reads_env_and_is_none_when_unset() {
    // Env-mutation test: this whole suite is the only reader of GH_TOKEN /
    // GITHUB_TOKEN, but cargo runs tests in parallel threads sharing the
    // process env. Hold a process-wide lock so concurrent token reads don't
    // race, and restore the original values on exit.
    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let prev_gh = std::env::var("GH_TOKEN").ok();
    let prev_github = std::env::var("GITHUB_TOKEN").ok();

    // Neither set → None.
    std::env::remove_var("GH_TOKEN");
    std::env::remove_var("GITHUB_TOKEN");
    assert_eq!(github_env_token(), None, "no token vars → None");

    // GH_TOKEN takes precedence; surrounding whitespace is trimmed.
    std::env::set_var("GH_TOKEN", "  gh-pat-123  ");
    assert_eq!(github_env_token().as_deref(), Some("gh-pat-123"));

    // Falls back to GITHUB_TOKEN when GH_TOKEN is absent.
    std::env::remove_var("GH_TOKEN");
    std::env::set_var("GITHUB_TOKEN", "github-pat-456");
    assert_eq!(github_env_token().as_deref(), Some("github-pat-456"));

    // A blank token is treated as unset.
    std::env::set_var("GH_TOKEN", "   ");
    std::env::remove_var("GITHUB_TOKEN");
    assert_eq!(github_env_token(), None, "blank token → None");

    // Restore original env.
    match prev_gh {
        Some(v) => std::env::set_var("GH_TOKEN", v),
        None => std::env::remove_var("GH_TOKEN"),
    }
    match prev_github {
        Some(v) => std::env::set_var("GITHUB_TOKEN", v),
        None => std::env::remove_var("GITHUB_TOKEN"),
    }
}

// ── issue vs pull-request kind detection ─────────────────────────────────────

#[test]
fn normalize_tags_pull_request_when_pull_request_object_present() {
    // GitHub's issues-and-PRs search marks a PR hit with a `pull_request` object.
    let pr = json!({
        "id": 42,
        "title": "Add retry to fetch",
        "state": "open",
        "html_url": "https://github.com/o/r/pull/42",
        "pull_request": { "url": "https://api.github.com/repos/o/r/pulls/42" }
    });
    let nt = normalize_github_issue(&pr).expect("normalizes");
    assert_eq!(nt.kind, TaskKind::PullRequest);
}

#[test]
fn normalize_tags_issue_when_no_pull_request_object() {
    let issue = json!({
        "id": 7,
        "title": "Login throws on empty password",
        "state": "open",
        "html_url": "https://github.com/o/r/issues/7"
    });
    let nt = normalize_github_issue(&issue).expect("normalizes");
    assert_eq!(nt.kind, TaskKind::Issue);
}

#[test]
fn normalize_tags_issue_when_pull_request_is_null() {
    // The REST issue payload carries `pull_request: null` for plain issues.
    let issue = json!({
        "id": 8,
        "title": "Docs typo",
        "state": "open",
        "html_url": "https://github.com/o/r/issues/8",
        "pull_request": null
    });
    let nt = normalize_github_issue(&issue).expect("normalizes");
    assert_eq!(nt.kind, TaskKind::Issue);
}

// ── skip merged PRs / closed issues ──────────────────────────────────────────

#[test]
fn normalize_skips_closed_issue() {
    // A closed issue is already-done work — drop it.
    let issue = json!({
        "id": 100,
        "title": "Old bug",
        "state": "closed",
        "html_url": "https://github.com/o/r/issues/100"
    });
    assert!(
        normalize_github_issue(&issue).is_none(),
        "closed issue must be skipped"
    );
}

#[test]
fn normalize_skips_merged_or_closed_pull_request() {
    // A merged/closed PR also reports `state == "closed"` — drop it too.
    let pr = json!({
        "id": 101,
        "title": "Shipped feature",
        "state": "closed",
        "html_url": "https://github.com/o/r/pull/101",
        "pull_request": { "url": "https://api.github.com/repos/o/r/pulls/101" }
    });
    assert!(
        normalize_github_issue(&pr).is_none(),
        "merged/closed PR must be skipped"
    );
}

#[test]
fn normalize_keeps_open_item() {
    // An open item is kept and tagged with its kind.
    let issue = json!({
        "id": 102,
        "title": "Active work",
        "state": "open",
        "html_url": "https://github.com/o/r/issues/102"
    });
    let nt = normalize_github_issue(&issue).expect("open item is kept");
    assert_eq!(nt.kind, TaskKind::Issue);
    assert_eq!(nt.status.as_deref(), Some("open"));
}

#[cfg(unix)]
#[test]
fn local_fetch_uses_gh_expands_me_and_normalizes_open_work() {
    use std::os::unix::fs::PermissionsExt;

    let _env = crate::test_env_lock::TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let previous_path = std::env::var_os("PATH");
    let previous_gh = std::env::var_os("GH_TOKEN");
    let previous_github = std::env::var_os("GITHUB_TOKEN");
    struct RestoreEnv {
        path: Option<std::ffi::OsString>,
        gh: Option<std::ffi::OsString>,
        github: Option<std::ffi::OsString>,
    }
    impl Drop for RestoreEnv {
        fn drop(&mut self) {
            match self.path.take() {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }
            match self.gh.take() {
                Some(value) => std::env::set_var("GH_TOKEN", value),
                None => std::env::remove_var("GH_TOKEN"),
            }
            match self.github.take() {
                Some(value) => std::env::set_var("GITHUB_TOKEN", value),
                None => std::env::remove_var("GITHUB_TOKEN"),
            }
        }
    }
    let _restore = RestoreEnv {
        path: previous_path.clone(),
        gh: previous_gh,
        github: previous_github,
    };

    let temp = tempfile::tempdir().expect("fake gh directory");
    let gh = temp.path().join("gh");
    std::fs::write(
        &gh,
        r##"#!/bin/sh
if [ "$1" = "api" ] && [ "$2" = "user" ]; then
  printf '%s\n' 'octocat'
  exit 0
fi
case "$*" in
  *'assignee:octocat'*)
    printf '%s\n' '{"items":[{"id":1,"title":"Closed","state":"closed","html_url":"https://github.com/o/r/issues/1"},{"id":2,"title":"Open issue","state":"open","body":"Issue body","html_url":"https://github.com/o/r/issues/2","labels":[{"name":"bug"}]},{"id":3,"title":"Open PR","state":"open","html_url":"https://github.com/o/r/pull/3","pull_request":{"url":"https://api.github.com/repos/o/r/pulls/3"}},{"id":4,"title":"Beyond max","state":"open","html_url":"https://github.com/o/r/issues/4"}]}'
    exit 0
    ;;
  *)
    printf '%s\n' 'query did not expand @me' >&2
    exit 17
    ;;
esac
"##,
    )
    .expect("write fake gh");
    let mut permissions = std::fs::metadata(&gh)
        .expect("fake gh metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&gh, permissions).expect("make fake gh executable");
    let mut path = temp.path().as_os_str().to_os_string();
    if let Some(previous) = previous_path {
        path.push(":");
        path.push(previous);
    }
    std::env::set_var("PATH", path);
    std::env::remove_var("GH_TOKEN");
    std::env::remove_var("GITHUB_TOKEN");

    let ctx = ProviderContext {
        config: Arc::new(tinymemory_api::host::test_support::TestHostConfig::default())
            as Arc<crate::Config>,
        toolkit: "github".into(),
        connection_id: Some("connection-1".into()),
        usage: ComposioUsageHandle::default(),
        max_items: None,
        sync_depth_days: None,
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let tasks = runtime
        .block_on(GitHubProvider::new().fetch_tasks(
            &ctx,
            &TaskFetchFilter {
                assignee_is_me: true,
                max: 2,
                github_fetch_mode: GithubFetchMode::Local,
                extra: json!({"custom": true}),
                ..Default::default()
            },
        ))
        .expect("local GitHub fetch");
    assert_eq!(tasks.len(), 2);
    assert_eq!(tasks[0].external_id, "2");
    assert_eq!(tasks[0].kind, TaskKind::Issue);
    assert_eq!(tasks[0].labels, vec!["bug"]);
    assert_eq!(tasks[1].external_id, "3");
    assert_eq!(tasks[1].kind, TaskKind::PullRequest);
}

#[tokio::test]
async fn composio_provider_failures_name_the_action_without_network() {
    use tinymemory_api::host::test_support::TestHostConfig;

    let temp = tempfile::tempdir().expect("config directory");
    let mut config = TestHostConfig::default();
    config.config_path = temp.path().join("config.toml");
    config.workspace_dir = temp.path().join("workspace");
    config.secrets_encrypt = false;
    tinymemory_api::host::MemoryHostConfig::save(&config)
        .await
        .expect("save unsigned config");
    let ctx = ProviderContext {
        config: Arc::new(config) as Arc<crate::Config>,
        toolkit: "github".into(),
        connection_id: Some("connection-1".into()),
        usage: ComposioUsageHandle::default(),
        max_items: None,
        sync_depth_days: None,
    };
    let provider = GitHubProvider::new();
    let profile_error = provider
        .fetch_user_profile(&ctx)
        .await
        .expect_err("signed-out profile fetch");
    assert!(profile_error.contains(ACTION_GET_AUTHENTICATED_USER));
    let task_error = provider
        .fetch_tasks(
            &ctx,
            &TaskFetchFilter {
                github_fetch_mode: GithubFetchMode::Composio,
                ..Default::default()
            },
        )
        .await
        .expect_err("signed-out task fetch");
    assert!(task_error.contains(ACTION_SEARCH_ISSUES));
}
