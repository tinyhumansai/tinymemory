//! Tests for the surrounding module.

use super::*;

fn entry(source_id: &str, kind: &str, success: bool, ts: DateTime<Utc>) -> SyncAuditEntry {
    SyncAuditEntry {
        timestamp: ts,
        source_id: source_id.to_string(),
        source_kind: kind.to_string(),
        scope: format!("{kind}:{source_id}"),
        items_fetched: 1,
        batches: 0,
        input_tokens: 0,
        output_tokens: 0,
        estimated_cost_usd: 0.0,
        composio_actions_called: 0,
        composio_cost_usd: 0.0,
        actual_charged_usd: None,
        duration_ms: 10,
        success,
        error: None,
    }
}

#[test]
fn workspace_kinds_are_scheduled_composio_is_not() {
    assert!(is_workspace_synced_kind(&SourceKind::GithubRepo));
    assert!(is_workspace_synced_kind(&SourceKind::Folder));
    assert!(is_workspace_synced_kind(&SourceKind::RssFeed));
    assert!(is_workspace_synced_kind(&SourceKind::WebPage));
    assert!(!is_workspace_synced_kind(&SourceKind::Composio));
    assert!(!is_workspace_synced_kind(&SourceKind::Conversation));
    assert!(!is_workspace_synced_kind(&SourceKind::TwitterQuery));
}

#[test]
fn audit_index_keeps_latest_workspace_success_and_skips_others() {
    let now = Utc::now();
    let older = now - chrono::Duration::hours(30);
    let newer = now - chrono::Duration::hours(2);
    let entries = vec![
        entry("src_gh", "github_repo", true, older),
        entry("src_gh", "github_repo", true, newer), // newest success wins
        entry("src_gh", "github_repo", false, now),  // failure ignored
        entry("conn_1", "composio", true, now),      // composio kind ignored
    ];
    let idx = index_last_success_by_source_id(&entries);
    assert_eq!(idx.get("src_gh"), Some(&newer));
    assert!(!idx.contains_key("conn_1"));
}

/// The headline regression: a GitHub source that synced once long ago
/// must read as DUE under the default 24h cadence — before this loop
/// existed, nothing ever consulted that staleness, so the source went
/// permanently dark after its first manual sync.
#[test]
fn stale_github_source_is_due_fresh_one_is_not() {
    let now = Utc::now();
    let mut idx = HashMap::new();
    idx.insert("src_stale".to_string(), now - chrono::Duration::days(5));
    idx.insert("src_fresh".to_string(), now - chrono::Duration::hours(1));

    let interval =
        effective_interval_secs(DEFAULT_MEMORY_SYNC_INTERVAL_SECS, None).expect("interval");

    let stale = persisted_since_last_sync(&idx, "src_stale", now);
    assert!(connection_is_due(interval, stale), "5-day-old sync is due");

    let fresh = persisted_since_last_sync(&idx, "src_fresh", now);
    assert!(
        !connection_is_due(interval, fresh),
        "1h-old sync is not due"
    );

    // Never-synced source fires immediately.
    let never = persisted_since_last_sync(&idx, "src_new", now);
    assert!(connection_is_due(interval, never));
}

#[test]
fn manual_only_global_setting_disables_the_loop() {
    assert_eq!(
        effective_interval_secs(DEFAULT_MEMORY_SYNC_INTERVAL_SECS, Some(0)),
        None
    );
}

#[test]
fn persisted_since_last_sync_saturates_clock_skew() {
    let now = Utc::now();
    let mut idx = HashMap::new();
    idx.insert("future".to_string(), now + chrono::Duration::hours(2));
    assert_eq!(
        persisted_since_last_sync(&idx, "future", now),
        Some(Duration::ZERO)
    );
    assert_eq!(persisted_since_last_sync(&idx, "missing", now), None);
}

#[test]
fn audit_failure_is_unavailable_and_unknown_cadence_is_excluded() {
    let (index, available) =
        workspace_audit_state(Err(anyhow::anyhow!("simulated audit I/O failure")));
    assert!(index.is_empty());
    assert!(!available);
    assert_eq!(cadence_from_audit(None, available, None), None);

    let known = Duration::from_secs(60);
    assert_eq!(
        cadence_from_audit(Some(known), available, None),
        Some(Some(known))
    );
}

#[test]
fn readable_empty_audit_keeps_never_synced_workspace_source_due() {
    let (index, available) = workspace_audit_state(Ok(Vec::new()));
    assert!(index.is_empty());
    assert!(available);

    let cadence = cadence_from_audit(None, available, None)
        .expect("readable empty audit keeps the source eligible");
    assert!(connection_is_due(
        DEFAULT_MEMORY_SYNC_INTERVAL_SECS,
        cadence
    ));
}

#[tokio::test]
async fn start_workspace_periodic_sync_is_idempotent() {
    start_workspace_periodic_sync();
    start_workspace_periodic_sync();
    assert!(SCHEDULER_STARTED.get().is_some());
}
