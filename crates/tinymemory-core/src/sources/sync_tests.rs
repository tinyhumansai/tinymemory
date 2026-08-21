//! Tests for the surrounding module.

use super::*;

/// The two GitHub coordinate helpers are re-exported from `tinymemory-sources` and
/// they deliberately differ: `tree_scope` slugifies to
/// `github-tinyhumansai-openhuman` while `archive_source_id` slugifies to
/// `github-com-tinyhumansai-openhuman`. Swapping the two still compiles and
/// still type-checks — it just makes reconcile scan an empty directory at
/// runtime. Pin both spellings.
#[test]
fn derive_scopes_keeps_github_tree_and_archive_ids_distinct() {
    let source: MemorySourceEntry = serde_json::from_value(serde_json::json!({
        "id": "gh-scope",
        "kind": "github_repo",
        "label": "Repo",
        "url": "https://github.com/tinyhumansai/openhuman",
    }))
    .expect("github source entry");

    let scopes = derive_scopes(&source, &TestHostConfig::default());

    assert_eq!(scopes.len(), 1);
    assert_eq!(scopes[0].tree_scope, "github:tinyhumansai/openhuman");
    assert_eq!(
        scopes[0].archive_source_id,
        "github.com/tinyhumansai/openhuman"
    );
}

fn source(kind: &str, id: &str, fields: serde_json::Value) -> MemorySourceEntry {
    let mut value = serde_json::json!({
        "id": id,
        "kind": kind,
        "label": format!("{kind} source"),
    });
    value
        .as_object_mut()
        .expect("source object")
        .extend(fields.as_object().expect("fields object").clone());
    serde_json::from_value(value).expect("valid source fixture")
}

#[tokio::test]
async fn disabled_source_is_rejected_without_spawning() {
    let mut disabled = source(
        "twitter_query",
        "disabled-twitter",
        serde_json::json!({"query": "rust"}),
    );
    disabled.enabled = false;
    let error = sync_source(
        disabled,
        tinymemory_api::host::MemoryHostConfig::to_arc(&TestHostConfig::default()),
    )
    .await
    .expect_err("disabled sources fail closed");
    assert_eq!(error, "source 'disabled-twitter' is disabled");
    assert!(!ACTIVE_SYNCS
        .lock()
        .expect("active sync lock")
        .contains("disabled-twitter"));
}

#[tokio::test]
async fn duplicate_active_source_returns_without_spawning() {
    let id = "already-active-twitter";
    ACTIVE_SYNCS
        .lock()
        .expect("active sync lock")
        .insert(id.into());
    let result = sync_source(
        source("twitter_query", id, serde_json::json!({"query": "rust"})),
        tinymemory_api::host::MemoryHostConfig::to_arc(&TestHostConfig::default()),
    )
    .await;
    ACTIVE_SYNCS.lock().expect("active sync lock").remove(id);
    assert_eq!(result, Ok(()));
}

#[tokio::test]
async fn twitter_failure_is_audited_and_releases_the_active_lock() {
    let workspace = tempfile::tempdir().expect("workspace");
    let id = "twitter-audit-failure";
    let mut config = TestHostConfig::default();
    config.workspace_dir = workspace.path().join("memory");
    let config = tinymemory_api::host::MemoryHostConfig::to_arc(&config);

    sync_source(
        source(
            "twitter_query",
            id,
            serde_json::json!({"query": "deterministic"}),
        ),
        config.clone(),
    )
    .await
    .expect("queue Twitter failure path");
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if !ACTIVE_SYNCS.lock().expect("active sync lock").contains(id) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("sync task releases its active lock");

    let audit =
        crate::sync::audit::read_audit_log(config.workspace_dir()).expect("read failed sync audit");
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].source_id, id);
    assert_eq!(audit[0].source_kind, "twitter_query");
    assert!(!audit[0].success);
    assert!(audit[0]
        .error
        .as_deref()
        .is_some_and(|error| error.contains("Twitter sync not yet configured")));

    sync_source(
        source(
            "twitter_query",
            id,
            serde_json::json!({"query": "deterministic"}),
        ),
        config,
    )
    .await
    .expect("released source can be queued again");
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while ACTIVE_SYNCS.lock().expect("active sync lock").contains(id) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("second sync also releases its active lock");
}

#[test]
fn derive_scopes_fails_closed_and_reads_only_valid_gmail_archives() {
    let workspace = tempfile::tempdir().expect("workspace");
    let mut config = TestHostConfig::default();
    config.workspace_dir = workspace.path().join("memory");

    assert!(derive_scopes(
        &source("github_repo", "missing-url", serde_json::json!({})),
        &config
    )
    .is_empty());
    assert!(derive_scopes(
        &source(
            "github_repo",
            "bad-url",
            serde_json::json!({"url": "https://example.com/not-github"}),
        ),
        &config
    )
    .is_empty());
    assert!(derive_scopes(
        &source(
            "composio",
            "slack",
            serde_json::json!({"toolkit": "slack", "connection_id": "one"}),
        ),
        &config
    )
    .is_empty());
    assert!(derive_scopes(
        &source("folder", "folder", serde_json::json!({"path": "."}),),
        &config
    )
    .is_empty());

    let raw = config.workspace_dir.join("memory_tree/content/raw");
    std::fs::create_dir_all(raw.join("gmail-valid")).expect("valid archive directory");
    std::fs::write(
        raw.join("gmail-valid/_source.md"),
        "---\nscope: \"gmail:alice-example-com\"\n---\n",
    )
    .expect("valid source metadata");
    std::fs::create_dir_all(raw.join("gmail-missing")).expect("missing metadata directory");
    std::fs::create_dir_all(raw.join("gmail-malformed")).expect("malformed metadata directory");
    std::fs::write(raw.join("gmail-malformed/_source.md"), "no scope here")
        .expect("malformed source metadata");
    std::fs::create_dir_all(raw.join("slack-ignored")).expect("ignored archive directory");

    let gmail = source(
        "composio",
        "gmail",
        serde_json::json!({"toolkit": "GMAIL", "connection_id": "one"}),
    );
    assert_eq!(
        derive_scopes(&gmail, &config),
        vec![SourceScope {
            tree_scope: "gmail:alice-example-com".into(),
            archive_source_id: "gmail:alice-example-com".into(),
        }]
    );
}

#[tokio::test]
async fn rebuild_check_is_a_noop_for_sources_without_archive_scopes() {
    let config = TestHostConfig::default();
    check_and_rebuild_tree(
        &source(
            "folder",
            "folder-no-rebuild",
            serde_json::json!({"path": "."}),
        ),
        &config,
    )
    .await;
}
