use super::*;
use crate::raw_kind::RawKind;
use crate::readers::SourceReader;

fn github_source(url: Option<&str>) -> MemorySourceEntry {
    MemorySourceEntry {
        id: "github".into(),
        kind: SourceKind::GithubRepo,
        label: "GitHub".into(),
        enabled: true,
        toolkit: None,
        connection_id: None,
        path: None,
        glob: None,
        url: url.map(str::to_string),
        branch: None,
        paths: Vec::new(),
        max_commits: Some(10),
        max_issues: Some(0),
        max_prs: Some(0),
        query: None,
        since_days: None,
        max_items: None,
        selector: None,
        max_tokens_per_sync: None,
        max_cost_per_sync_usd: None,
        sync_depth_days: None,
    }
}

fn local_git(cwd: &std::path::Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("spawn git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[tokio::test]
async fn reader_lists_and_reads_a_cached_local_repository_without_network() {
    let workspace = tempfile::tempdir().expect("workspace");
    let source_repo = workspace.path().join("source");
    std::fs::create_dir_all(&source_repo).expect("source directory");
    local_git(&source_repo, &["init", "-q"]);
    local_git(&source_repo, &["config", "user.email", "test@example.com"]);
    local_git(&source_repo, &["config", "user.name", "Test"]);
    std::fs::write(source_repo.join("README.md"), "hello").expect("write file");
    local_git(&source_repo, &["add", "."]);
    local_git(&source_repo, &["commit", "-qm", "local activity"]);

    let cache = git::git_cache_dir(workspace.path(), "local", "fixture");
    std::fs::create_dir_all(cache.parent().expect("cache parent")).expect("cache parent");
    local_git(
        workspace.path(),
        &[
            "clone",
            "--bare",
            "-q",
            source_repo.to_str().expect("source path"),
            cache.to_str().expect("cache path"),
        ],
    );

    let source = github_source(Some("https://github.com/local/fixture"));
    let reader = GithubReader;
    assert_eq!(reader.kind(), SourceKind::GithubRepo);
    let items = reader
        .list_items(&source, workspace.path())
        .await
        .expect("list local cached activity");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].title, "local activity");

    let content = reader
        .read_item(&source, &items[0].id, workspace.path())
        .await
        .expect("read cached commit");
    assert_eq!(content.title, "local activity");
    assert!(content.body.contains("Test <test@example.com>"));
}

#[tokio::test]
async fn reader_rejects_missing_urls_and_malformed_item_ids_before_network() {
    let workspace = tempfile::tempdir().expect("workspace");
    let reader = GithubReader;
    let missing = github_source(None);
    assert!(reader.list_items(&missing, workspace.path()).await.is_err());
    assert!(reader
        .read_item(&missing, "commit:abc", workspace.path())
        .await
        .is_err());

    let configured = github_source(Some("https://github.com/local/fixture"));
    for item_id in ["unknown", "issue:not-a-number", "pr:not-a-number"] {
        assert!(reader
            .read_item(&configured, item_id, workspace.path())
            .await
            .is_err());
    }
}

fn issue_json(number: u64) -> serde_json::Value {
    serde_json::json!({
        "number": number,
        "title": "Reader issue",
        "body": "Issue body",
        "state": "open",
        "user": {"login": "alice"},
        "labels": [{"name": "coverage"}],
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-02T00:00:00Z",
        "pull_request": null
    })
}

fn pr_json(number: u64) -> serde_json::Value {
    serde_json::json!({
        "number": number,
        "title": "Reader PR",
        "body": "PR body",
        "state": "closed",
        "user": {"login": "bob"},
        "labels": [],
        "created_at": "2026-01-03T00:00:00Z",
        "updated_at": "2026-01-04T00:00:00Z",
        "merged_at": null
    })
}

#[tokio::test]
async fn reader_orchestrates_issue_and_pr_cache_lifecycles_without_network() {
    let workspace = tempfile::tempdir().expect("workspace");
    let mut source = github_source(Some("https://github.com/local/fixture"));
    source.max_commits = Some(0);
    source.max_issues = Some(5);
    source.max_prs = Some(5);
    let reader = GithubReader;

    let items = api::with_test_responses(
        vec![
            Ok(serde_json::json!([issue_json(7)]).to_string()),
            Ok(serde_json::json!([pr_json(8)]).to_string()),
        ],
        reader.list_items(&source, workspace.path()),
    )
    .await
    .expect("list issue and PR through reader");
    assert_eq!(
        items
            .iter()
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>(),
        ["issue:7", "pr:8"]
    );

    let issue = api::with_test_responses(
        vec![Ok("[]".into())],
        reader.read_item(&source, "issue:7", workspace.path()),
    )
    .await
    .expect("read cached issue");
    assert_eq!(issue.title, "#7 Reader issue");

    let pr = api::with_test_responses(
        vec![Ok("[]".into())],
        reader.read_item(&source, "pr:8", workspace.path()),
    )
    .await
    .expect("read cached PR");
    assert_eq!(pr.title, "PR #8 Reader PR");
}

#[tokio::test]
async fn reader_reports_when_every_configured_github_family_fails() {
    let workspace = tempfile::tempdir().expect("workspace");
    let mut source = github_source(Some("https://github.com/local/fixture"));
    source.max_commits = Some(0);
    source.max_issues = Some(1);
    source.max_prs = Some(1);

    let error = api::with_test_responses(
        vec![Err("issues offline".into()), Err("prs offline".into())],
        GithubReader.list_items(&source, workspace.path()),
    )
    .await
    .expect_err("all configured families failed");
    assert!(error.to_string().contains("all GitHub API calls failed"));
    assert!(error.to_string().contains("issues offline"));
    assert!(error.to_string().contains("prs offline"));
}

fn commit_json(sha: &str, message: &str, login: Option<&str>) -> String {
    let committed_at = if sha == "new" {
        "2026-02-02T00:00:00Z"
    } else {
        "2026-01-02T00:00:00Z"
    };
    let author = login
        .map(|value| serde_json::json!({ "login": value }))
        .unwrap_or(serde_json::Value::Null);
    serde_json::json!({
        "sha": sha,
        "commit": {
            "message": message,
            "author": {
                "name": "Test Author",
                "email": "author@example.com",
                "date": "2026-01-01T00:00:00Z"
            },
            "committer": {
                "name": "Test Committer",
                "email": "committer@example.com",
                "date": committed_at
            }
        },
        "author": author
    })
    .to_string()
}

#[tokio::test]
async fn api_commit_fallback_lists_merges_and_renders_without_network() {
    let older = commit_json("old", "older commit\nbody", None);
    let newer = commit_json("new", "newer commit\nbody", Some("octocat"));
    let listed = api::with_test_responses(
        vec![Ok(format!("[{older}]")), Ok(format!("[{newer},{older}]"))],
        api::list_commits_api(
            "owner",
            "repo",
            10,
            false,
            Some("main"),
            &["docs/".into(), "src/".into()],
        ),
    )
    .await
    .expect("list commits from deterministic API pages");
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].id, "commit:new");
    assert_eq!(listed[1].id, "commit:old");

    let content = api::with_test_responses(
        vec![Ok(commit_json(
            "new",
            "newer commit\nfull body",
            Some("octocat"),
        ))],
        api::read_commit_api("owner", "repo", "new", false),
    )
    .await
    .expect("read deterministic commit");
    assert_eq!(content.title, "newer commit");
    assert!(content
        .body
        .contains("Test Author <author@example.com> (@octocat)"));
    assert_eq!(content.metadata["author_handle"], "octocat");
}

#[tokio::test]
async fn api_commit_fallback_reports_transport_and_parse_failures_without_network() {
    let transport = api::with_test_responses(
        vec![Err("offline".into())],
        api::list_commits_api("owner", "repo", 1, false, None, &[]),
    )
    .await
    .expect_err("transport error");
    assert_eq!(transport, "offline");

    let list_parse = api::with_test_responses(
        vec![Ok("not json".into())],
        api::list_commits_api("owner", "repo", 1, false, None, &[]),
    )
    .await
    .expect_err("list parse error");
    assert!(list_parse.contains("parse commits page 1"));

    let read_parse = api::with_test_responses(
        vec![Ok("{}".into())],
        api::read_commit_api("owner", "repo", "bad", false),
    )
    .await
    .expect_err("commit parse error");
    assert!(read_parse.contains("parse commit"));

    let exhausted = api::with_test_responses(
        Vec::new(),
        api::read_commit_api("owner", "repo", "missing", false),
    )
    .await
    .expect_err("fixture exhaustion fails closed");
    assert!(exhausted.contains("no deterministic GitHub response queued"));
}

#[tokio::test]
async fn reader_falls_back_from_a_broken_local_cache_to_the_api_without_network() {
    let workspace = tempfile::tempdir().expect("workspace");
    let cache = git::git_cache_dir(workspace.path(), "owner", "repo");
    std::fs::create_dir_all(&cache).expect("cache directory");
    std::fs::write(cache.join("HEAD"), "not a git repository").expect("broken cache marker");

    let mut source = github_source(Some("https://github.com/owner/repo"));
    source.max_commits = Some(5);
    source.max_issues = Some(0);
    source.max_prs = Some(0);
    let reader = GithubReader;

    let listed = api::with_test_responses(
        vec![Ok(format!(
            "[{}]",
            commit_json("fallback", "API fallback commit", Some("octocat"))
        ))],
        reader.list_items(&source, workspace.path()),
    )
    .await
    .expect("broken git cache falls back to API list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, "commit:fallback");

    let content = api::with_test_responses(
        vec![Ok(commit_json(
            "fallback",
            "API fallback commit\nfull body",
            Some("octocat"),
        ))],
        reader.read_item(&source, "commit:fallback", workspace.path()),
    )
    .await
    .expect("broken git cache falls back to API read");
    assert_eq!(content.id, "commit:fallback");
    assert!(content.body.contains("full body"));
    assert_eq!(content.metadata["author_handle"], "octocat");
}

#[tokio::test]
async fn reader_keeps_successful_families_when_commit_transports_fail() {
    let workspace = tempfile::tempdir().expect("workspace");
    let cache = git::git_cache_dir(workspace.path(), "owner", "repo");
    std::fs::create_dir_all(&cache).expect("cache directory");
    std::fs::write(cache.join("HEAD"), "not a git repository").expect("broken cache marker");

    let mut source = github_source(Some("https://github.com/owner/repo"));
    source.max_commits = Some(1);
    source.max_issues = Some(1);
    source.max_prs = Some(0);

    let items = api::with_test_responses(
        vec![
            Err("commit API offline".into()),
            Ok(serde_json::json!([issue_json(7)]).to_string()),
        ],
        GithubReader.list_items(&source, workspace.path()),
    )
    .await
    .expect("a successful issue family makes the partial result usable");

    assert_eq!(items.len(), 1);
    assert_eq!(items[0].id, "issue:7");
    assert_eq!(items[0].title, "#7 Reader issue");
}

#[test]
fn git_log_args_default_to_head_without_branch() {
    // With no branch configured the walk must stay on the bare clone's HEAD
    // (the default branch), matching the REST fallback's default-branch scope
    // rather than walking every ref.
    let args = git::log_args(50, None, &[]);
    assert_eq!(
        args,
        vec![
            "log".to_string(),
            "HEAD".to_string(),
            "--max-count=50".to_string(),
            "--format=%H\t%s\t%aI".to_string(),
        ]
    );
}

#[test]
fn git_log_args_restrict_to_branch_and_paths() {
    let args = git::log_args(
        50,
        Some("main"),
        &["src/lib.rs".to_string(), "docs/".to_string()],
    );
    assert_eq!(
        args,
        vec![
            "log".to_string(),
            "main".to_string(),
            "--max-count=50".to_string(),
            "--format=%H\t%s\t%aI".to_string(),
            "--".to_string(),
            "src/lib.rs".to_string(),
            "docs/".to_string(),
        ]
    );
    // Empty/whitespace branch falls back to HEAD, never an empty ref.
    let args = git::log_args(1, Some(""), &[]);
    assert_eq!(args[1], "HEAD");
}

#[test]
fn commit_list_queries_carry_branch_and_path_filters() {
    // No filters → a single empty query (plain pagination).
    assert_eq!(api::commit_list_queries(None, &[]), vec![String::new()]);
    // Branch only → `sha=<branch>`.
    assert_eq!(
        api::commit_list_queries(Some("main"), &[]),
        vec![String::from("sha=main")]
    );
    // One path → `path=<p>`.
    assert_eq!(
        api::commit_list_queries(None, &["src/".to_string()]),
        vec![String::from("path=src/")]
    );
    // Branch + one path → `sha=<branch>&path=<p>`.
    assert_eq!(
        api::commit_list_queries(Some("main"), &["src/lib.rs".to_string()]),
        vec![String::from("sha=main&path=src/lib.rs")]
    );
    // Multiple paths → one query per path, dedup happens in the caller.
    assert_eq!(
        api::commit_list_queries(Some("main"), &["a/".to_string(), "b/".to_string()]),
        vec![
            String::from("sha=main&path=a/"),
            String::from("sha=main&path=b/")
        ]
    );
    // Empty branch is treated as unset.
    assert_eq!(
        api::commit_list_queries(Some(""), &["a/".to_string()]),
        vec![String::from("path=a/")]
    );
}

#[test]
fn commit_list_queries_percent_encode_special_chars() {
    // `&`, `#`, `=` and spaces inside a branch or path value would be parsed
    // as query syntax and corrupt the filter; they must be percent-encoded.
    // `/` is left intact (legal in a query component, and GitHub's commits
    // `path` filter expects the common `path=src/` shape unencoded).
    assert_eq!(
        api::commit_list_queries(Some("feature/one&two"), &["src/#1.rs".to_string()]),
        vec![String::from("sha=feature/one%26two&path=src/%231.rs")]
    );
    // Unreserved values are unchanged.
    assert_eq!(
        api::commit_list_queries(Some("main"), &["docs/".to_string()]),
        vec![String::from("sha=main&path=docs/")]
    );
}

#[tokio::test]
async fn fetch_all_pages_keeps_page_size_constant_and_truncates() {
    // Regression: the page size must not shrink mid-walk. With `max = 150`
    // (not a multiple of 100), a shrinking `per_page` would re-window the
    // offsets — page 2 at per_page=50 returns items 51-100 again, skipping
    // 101-150. A constant page size walks page 1 and page 2 both at
    // per_page=100 and truncates the 200 collected rows to 150.
    let mut requested: Vec<String> = Vec::new();
    let pages = api::collect_pages::<u64, _, _>("commits", 150, |page| {
        let url = format!("per_page=100&page={page}");
        requested.push(url);
        // 100 rows per page, all full (never a short page before the cap).
        let rows: Vec<String> = (1..=100)
            .map(|i| format!("{}", (page - 1) * 100 + i))
            .collect();
        async move { Ok(format!("[{}]", rows.join(","))) }
    })
    .await
    .unwrap();

    assert_eq!(
        requested,
        vec![
            "per_page=100&page=1".to_string(),
            "per_page=100&page=2".to_string(),
        ]
    );
    assert_eq!(pages.len(), 150);
    // No overlap: the second page is the next window (101..), not 51..100.
    assert_eq!(pages[0], 1);
    assert_eq!(pages[100], 101);
    assert_eq!(pages[149], 150);
}

#[tokio::test]
async fn fetch_all_pages_stops_at_a_short_page() {
    // A short page (fewer than GH_PAGE_SIZE rows) is the last page; the walk
    // must not request page 2 after it.
    let mut requested: Vec<u32> = Vec::new();
    let pages = crate::readers::github::api::collect_pages::<u64, _, _>("commits", 1000, |page| {
        requested.push(page);
        async move {
            // Page 1 is short (3 rows) — stop after it even though max is large.
            Ok("[1,2,3]".to_string())
        }
    })
    .await
    .unwrap();

    assert_eq!(requested, vec![1]);
    assert_eq!(pages, vec![1, 2, 3]);
}

/// Build a synthetic `GhCommit` for merge tests.
fn gh_commit(sha: &str, subject: &str, ts: &str) -> types::GhCommit {
    types::GhCommit {
        sha: sha.into(),
        commit: types::GhCommitInner {
            message: subject.into(),
            author: None,
            committer: Some(types::GhAuthor {
                name: None,
                email: None,
                date: Some(ts.into()),
            }),
        },
        author: None,
    }
}

#[test]
fn merge_commit_batches_walks_every_path_before_truncating() {
    // Two configured paths: the first returns two commits, the second one.
    // The pre-fix code stopped after the first path once `out` reached `max`,
    // silently dropping the `src` commit even though it is newer than the
    // second `docs` commit.
    let docs = vec![gh_commit("a", "docs first", "2024-01-01T00:00:00Z")];
    let src = vec![gh_commit("b", "src newer", "2024-02-01T00:00:00Z")];

    let merged = api::merge_commit_batches(vec![docs, src], 3);
    let ids: Vec<&str> = merged.iter().map(|i| i.id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["commit:b", "commit:a"],
        "newest-first, both paths kept"
    );
}

#[test]
fn merge_commit_batches_dedups_by_sha_and_truncates_globally() {
    // A commit touching both paths appears in both batches but only once.
    let docs = vec![
        gh_commit("a", "docs first", "2024-01-01T00:00:00Z"),
        gh_commit("shared", "touches both", "2024-02-01T00:00:00Z"),
    ];
    let src = vec![gh_commit("shared", "touches both", "2024-02-01T00:00:00Z")];

    let merged = api::merge_commit_batches(vec![docs, src], 1);
    let ids: Vec<&str> = merged.iter().map(|i| i.id.as_str()).collect();
    assert_eq!(ids, vec!["commit:shared"], "deduped and truncated to max");
}

#[test]
fn parse_github_url_extracts_owner_and_repo() {
    let (owner, repo) = parse_github_url("https://github.com/openai/tiktoken").unwrap();
    assert_eq!(owner, "openai");
    assert_eq!(repo, "tiktoken");
}

#[test]
fn parse_github_url_handles_trailing_slash_and_git() {
    let (owner, repo) = parse_github_url("https://github.com/org/repo.git/").unwrap();
    assert_eq!(owner, "org");
    assert_eq!(repo, "repo");
}

#[test]
fn parse_github_url_rejects_non_repo_paths() {
    // Deep links like /tree/main must not silently extract the wrong
    // owner/repo. Bare host or non-github URLs also rejected.
    assert!(parse_github_url("https://github.com/org/repo/tree/main").is_err());
    assert!(parse_github_url("https://gitlab.com/org/repo").is_err());
    assert!(parse_github_url("https://github.com/org").is_err());
    assert!(parse_github_url("not-a-url").is_err());
}

#[test]
fn item_kind_round_trips() {
    let cases = [
        ("commit:abc123", ItemKind::Commit, "abc123"),
        ("issue:42", ItemKind::Issue, "42"),
        ("pr:99", ItemKind::PullRequest, "99"),
    ];
    for (id, expected_kind, expected_ref) in cases {
        let (kind, ref_id) = ItemKind::from_id(id).unwrap();
        assert_eq!(kind, expected_kind);
        assert_eq!(ref_id, expected_ref);
    }
}

#[test]
fn item_kind_rejects_invalid() {
    assert!(ItemKind::from_id("unknown:123").is_none());
    assert!(ItemKind::from_id("noprefix").is_none());
}

#[test]
fn repo_archive_source_id_slugs_to_repo_folder() {
    // `github.com/<owner>/<repo>` → slugify → `github-com-<owner>-<repo>`.
    assert_eq!(
        repo_archive_source_id("https://github.com/tinyhumansai/openhuman").as_deref(),
        Some("github.com/tinyhumansai/openhuman")
    );
    assert!(repo_archive_source_id("not-a-url").is_none());
}

#[test]
fn chunk_source_id_is_clean_and_per_item() {
    assert_eq!(
        chunk_source_id("https://github.com/org/repo", "commit:abc123").as_deref(),
        Some("github:org/repo:commit:abc123")
    );
    assert_eq!(
        chunk_source_id("https://github.com/org/repo", "pr:42").as_deref(),
        Some("github:org/repo:pr:42")
    );
}

#[test]
fn unique_handles_dedups_and_skips_unknown() {
    assert_eq!(
        unique_handles(["alice", "bob", "alice", "unknown", ""].into_iter()),
        "@alice @bob"
    );
    assert_eq!(unique_handles(["unknown", ""].into_iter()), "none");
    assert_eq!(unique_handles(std::iter::empty()), "none");
}

#[test]
fn raw_archive_coords_maps_kind_and_uid() {
    assert_eq!(
        raw_archive_coords("commit:deadbeef"),
        Some((RawKind::Commit, "deadbeef".to_string()))
    );
    assert_eq!(
        raw_archive_coords("issue:7"),
        Some((RawKind::Issue, "7".to_string()))
    );
    assert_eq!(
        raw_archive_coords("pr:99"),
        Some((RawKind::PullRequest, "99".to_string()))
    );
    assert!(raw_archive_coords("bogus:1").is_none());
}
