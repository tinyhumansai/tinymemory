//! Tests for the surrounding module.

use super::*;
use crate::test_env_lock::TEST_ENV_LOCK as ENV_LOCK;
use tempfile::tempdir;

#[test]
fn tick_seconds_is_sane_default() {
    // Sanity check: don't accidentally ship a 1-second tick.
    const _: () = assert!(TICK_SECONDS >= 30);
    const _: () = assert!(TICK_SECONDS <= 3600);
}

#[test]
fn effective_interval_none_falls_back_to_default() {
    // No user choice → 24h default, floored at the provider default.
    assert_eq!(
        effective_interval_secs(15 * 60, None),
        Some(DEFAULT_MEMORY_SYNC_INTERVAL_SECS)
    );
}

#[test]
fn effective_interval_manual_disables_sync() {
    // Some(0) is the "Manual only" sentinel — periodic sync is skipped.
    assert_eq!(effective_interval_secs(15 * 60, Some(0)), None);
}

#[test]
fn effective_interval_override_is_floored_at_provider_default() {
    // A user cadence longer than the provider default is honoured as-is.
    assert_eq!(
        effective_interval_secs(15 * 60, Some(4 * 3600)),
        Some(4 * 3600)
    );
    // A user cadence shorter than the provider default is clamped up to it
    // so we never sync more often than the provider intends.
    assert_eq!(effective_interval_secs(30 * 60, Some(60)), Some(30 * 60));
    // Exactly equal stays equal.
    assert_eq!(effective_interval_secs(1800, Some(1800)), Some(1800));
}

#[test]
fn effective_interval_default_is_floored_at_a_longer_provider_default() {
    // If a provider ever defaults to longer than 24h, that wins under None.
    let long = DEFAULT_MEMORY_SYNC_INTERVAL_SECS + 3600;
    assert_eq!(effective_interval_secs(long, None), Some(long));
}

#[test]
fn connection_is_due_compares_elapsed_against_interval() {
    let interval = 4 * 3600;
    // Never synced this run → always due.
    assert!(connection_is_due(interval, None));
    // Synced more recently than the interval → not due.
    assert!(!connection_is_due(
        interval,
        Some(Duration::from_secs(3600))
    ));
    // Synced exactly at the interval boundary → due.
    assert!(connection_is_due(
        interval,
        Some(Duration::from_secs(interval))
    ));
    // Synced longer ago than the interval → due.
    assert!(connection_is_due(
        interval,
        Some(Duration::from_secs(interval + 1))
    ));
}

/// Build a minimal Composio `MemorySourceEntry` for the per-source gate
/// tests — only the fields `decide_periodic_source` reads are meaningful.
fn composio_source(
    enabled: bool,
    max_items: Option<u32>,
    sync_depth_days: Option<u32>,
) -> MemorySourceEntry {
    MemorySourceEntry {
        id: "src_test".to_string(),
        kind: SourceKind::Composio,
        label: "test".to_string(),
        enabled,
        toolkit: Some("gmail".to_string()),
        connection_id: Some("cmp-1".to_string()),
        path: None,
        glob: None,
        url: None,
        branch: None,
        paths: Vec::new(),
        max_commits: None,
        max_issues: None,
        max_prs: None,
        query: None,
        since_days: None,
        max_items,
        selector: None,
        max_tokens_per_sync: None,
        max_cost_per_sync_usd: None,
        sync_depth_days,
    }
}

/// #2831 row 2: a source explicitly toggled **off** must be skipped by the
/// background loop — this is the leak the gate closes.
#[test]
fn decide_periodic_source_skips_disabled_source() {
    let src = composio_source(false, Some(100), Some(30));
    assert_eq!(
        decide_periodic_source(Some(&src), "gmail"),
        PeriodicSourceDecision::Skip
    );
}

/// An enabled source syncs with exactly its configured caps (no defaulting).
#[test]
fn decide_periodic_source_uses_enabled_source_caps() {
    let src = composio_source(true, Some(42), Some(7));
    assert_eq!(
        decide_periodic_source(Some(&src), "gmail"),
        PeriodicSourceDecision::Sync {
            max_items: Some(42),
            sync_depth_days: Some(7),
        }
    );
}

/// A connection with no registry row yet (pre-reconcile window) syncs with
/// the conservative per-toolkit defaults — **bounded**, never uncapped, and
/// never skipped. This is the safe-direction fallback for a missing match.
#[test]
fn decide_periodic_source_defaults_caps_when_no_row() {
    let (want_items, want_depth) = memory_sync_defaults_for_toolkit("gmail");
    assert_eq!(
        decide_periodic_source(None, "gmail"),
        PeriodicSourceDecision::Sync {
            max_items: want_items,
            sync_depth_days: want_depth,
        }
    );
    // The defaults are bounded for a known toolkit (regression guard against
    // an accidental return to uncapped background fetches).
    assert!(want_items.is_some());
}

/// Multi-account regression (#3443 added multiple account connections per
/// toolkit): two live connections of the *same* toolkit must be gated
/// **independently** by their own per-`connection_id` source rows. This
/// pins the loop's connection-id keying — re-keying the lookup by toolkit
/// would collapse the two accounts and is the regression this guards.
#[test]
fn per_connection_gate_is_independent_across_accounts_of_same_toolkit() {
    // gmail account A: enabled with caps; gmail account B: disabled.
    let mut a = composio_source(true, Some(10), Some(5));
    a.connection_id = Some("conn-A".to_string());
    a.toolkit = Some("gmail".to_string());
    let mut b = composio_source(false, Some(99), Some(99));
    b.connection_id = Some("conn-B".to_string());
    b.toolkit = Some("gmail".to_string());

    // Build the same connection_id → entry index the live tick builds.
    let index: HashMap<String, MemorySourceEntry> = [a, b]
        .into_iter()
        .filter_map(|s| s.connection_id.clone().map(|id| (id, s)))
        .collect();

    // Account A (enabled) syncs with its own caps...
    assert_eq!(
        decide_periodic_source(index.get("conn-A"), "gmail"),
        PeriodicSourceDecision::Sync {
            max_items: Some(10),
            sync_depth_days: Some(5),
        }
    );
    // ...account B (disabled) is skipped, even though it shares the toolkit.
    assert_eq!(
        decide_periodic_source(index.get("conn-B"), "gmail"),
        PeriodicSourceDecision::Skip
    );
    // A third, not-yet-registered account of the same toolkit falls back to
    // bounded defaults (never skipped, never uncapped).
    let (def_items, def_depth) = memory_sync_defaults_for_toolkit("gmail");
    assert_eq!(
        decide_periodic_source(index.get("conn-C"), "gmail"),
        PeriodicSourceDecision::Sync {
            max_items: def_items,
            sync_depth_days: def_depth,
        }
    );
}

/// End-to-end simulation of the scheduler's per-connection decision: prove
/// that **changing the global setting changes when the next sync fires**
/// (issue #3302 acceptance criterion). We drive the same two pure helpers
/// the live tick uses (`effective_interval_secs` → `connection_is_due`)
/// across realistic last-sync ages, so no clock or network is needed.
#[test]
fn scheduler_decision_honors_the_global_setting() {
    // A chatty provider that natively wants to sync every 15 minutes.
    let provider_default = 15 * 60;

    // Helper mirroring the live loop: returns whether the connection would
    // fire right now, or `None` for "Manual only" (skipped entirely).
    let decide = |global: Option<u64>, since: Option<Duration>| -> Option<bool> {
        effective_interval_secs(provider_default, global)
            .map(|interval| connection_is_due(interval, since))
    };

    let one_hour_ago = Some(Duration::from_secs(3600));
    let five_hours_ago = Some(Duration::from_secs(5 * 3600));

    // Baseline (no global override): with only the 15m provider default, a
    // connection synced an hour ago is already overdue and WOULD fire.
    // (This is the behavior the feature is reining in.)
    assert!(connection_is_due(provider_default, one_hour_ago));

    // User picks "every 4h": now that same hour-old connection must NOT
    // fire — the global cadence (not the 15m default) governs the gap…
    assert_eq!(decide(Some(4 * 3600), one_hour_ago), Some(false));
    // …but once 5h have passed it fires again.
    assert_eq!(decide(Some(4 * 3600), five_hours_ago), Some(true));

    // User picks "Manual only" (0): never auto-fires, no matter how stale.
    assert_eq!(decide(Some(0), five_hours_ago), None);
    assert_eq!(decide(Some(0), None), None);

    // Unset (None) → 24h default: the hour-old connection is not yet due,
    // confirming the default is far more conservative than the 15m native
    // cadence.
    assert_eq!(decide(None, one_hour_ago), Some(false));
    assert_eq!(
        decide(None, Some(Duration::from_secs(25 * 3600))),
        Some(true)
    );

    // A never-synced connection fires on any non-manual setting (the
    // restart-recovery path).
    assert_eq!(decide(Some(4 * 3600), None), Some(true));
}

fn audit_entry(
    connection_id: &str,
    scope: &str,
    success: bool,
    ts: DateTime<Utc>,
) -> SyncAuditEntry {
    SyncAuditEntry {
        timestamp: ts,
        source_id: connection_id.to_string(),
        source_kind: "composio".to_string(),
        scope: scope.to_string(),
        items_fetched: 1,
        batches: 0,
        input_tokens: 0,
        output_tokens: 0,
        estimated_cost_usd: 0.0,
        composio_actions_called: 1,
        composio_cost_usd: 0.0,
        actual_charged_usd: None,
        duration_ms: 10,
        success,
        error: None,
    }
}

#[test]
fn index_last_success_keeps_latest_success_and_ignores_failures() {
    let now = Utc::now();
    let older = now - chrono::Duration::hours(6);
    let newer = now - chrono::Duration::hours(1);
    let entries = vec![
        audit_entry("cmp-1", "gmail:cmp-1", true, older),
        audit_entry("cmp-1", "gmail:cmp-1", true, newer), // newer success wins
        audit_entry("cmp-1", "gmail:cmp-1", false, now),  // failure ignored
        audit_entry("cmp-2", "slack:cmp-2", false, now),  // only-failure → absent
    ];
    let idx = index_last_success_by_connection(&entries);
    assert_eq!(idx.get("cmp-1"), Some(&newer));
    assert!(
        !idx.contains_key("cmp-2"),
        "a connection with only failed syncs is not indexed"
    );
}

#[test]
fn index_last_success_falls_back_to_source_id_without_scope_suffix() {
    let now = Utc::now();
    // A non-composio kind is skipped entirely.
    let entries = vec![
        SyncAuditEntry {
            source_kind: "github_repo".to_string(),
            ..audit_entry("ignored", "github:org/repo", true, now)
        },
        // Composio entry whose scope has no ':' → key by source_id.
        audit_entry("cmp-3", "noscope", true, now),
    ];
    let idx = index_last_success_by_connection(&entries);
    assert!(idx.contains_key("cmp-3"));
    assert!(!idx.contains_key("ignored"));
}

#[test]
fn persisted_since_last_sync_computes_and_saturates() {
    let now = Utc::now();
    let mut idx = HashMap::new();
    idx.insert("cmp-1".to_string(), now - chrono::Duration::hours(3));
    idx.insert("future".to_string(), now + chrono::Duration::hours(2));

    let elapsed = persisted_since_last_sync(&idx, "cmp-1", now).unwrap();
    // ~3h, allow a small window for test execution time.
    assert!(elapsed >= Duration::from_secs(3 * 3600 - 5));
    assert!(elapsed <= Duration::from_secs(3 * 3600 + 5));
    // Clock skew (future timestamp) saturates to zero, not a huge value.
    assert_eq!(
        persisted_since_last_sync(&idx, "future", now),
        Some(Duration::ZERO)
    );
    // Unknown connection → None (treated as never synced).
    assert_eq!(persisted_since_last_sync(&idx, "unknown", now), None);
}

/// The cadence must survive a restart: with the in-memory map cold, the
/// persisted audit timestamp drives the due-check so a connection synced
/// 1h ago does NOT re-fire under a 4h setting, but one synced 5h ago does.
#[test]
fn cadence_survives_restart_via_persisted_audit() {
    let now = Utc::now();
    let mut idx = HashMap::new();
    idx.insert("cmp-1".to_string(), now - chrono::Duration::hours(1));
    idx.insert("cmp-2".to_string(), now - chrono::Duration::hours(5));

    let interval = effective_interval_secs(15 * 60, Some(4 * 3600)).unwrap();

    // cmp-1 (synced 1h ago) — in-memory cold, persisted fallback says NOT due.
    let cmp1 = None.or_else(|| persisted_since_last_sync(&idx, "cmp-1", now));
    assert!(!connection_is_due(interval, cmp1));

    // cmp-2 (synced 5h ago) — persisted fallback says due.
    let cmp2 = None.or_else(|| persisted_since_last_sync(&idx, "cmp-2", now));
    assert!(connection_is_due(interval, cmp2));

    // A connection with no persisted record still fires (truly fresh).
    let fresh = None.or_else(|| persisted_since_last_sync(&idx, "cmp-new", now));
    assert!(connection_is_due(interval, fresh));
}

#[test]
fn audit_failure_is_unavailable_and_unknown_cadence_is_skipped() {
    let (index, available) =
        composio_audit_state(Err(anyhow::anyhow!("simulated audit I/O failure")));
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
fn readable_empty_audit_preserves_first_sync_behavior() {
    let (index, available) = composio_audit_state(Ok(Vec::new()));
    assert!(index.is_empty());
    assert!(available);

    let cadence = cadence_from_audit(None, available, None)
        .expect("readable empty audit keeps the source eligible");
    assert!(connection_is_due(3600, cadence));
}

/// A successful periodic tick produces a Composio-kind audit entry that
/// carries the billable-action tally + cost and zeroes the LLM-cost
/// columns (summarisation happens later in the job worker). Pins the
/// shape the Sync History panel reads (#3111 follow-up).
#[test]
fn periodic_audit_entry_records_composio_cost_on_success() {
    let usage = ComposioUsage {
        actions_called: 3,
        cost_usd: 0.042,
    };
    let entry = build_periodic_audit_entry("gmail", "cmp-123", &usage, 17, 1234, None);

    assert_eq!(entry.source_kind, "composio");
    assert_eq!(entry.source_id, "cmp-123");
    assert_eq!(entry.scope, "gmail:cmp-123");
    assert_eq!(entry.items_fetched, 17);
    assert_eq!(entry.composio_actions_called, 3);
    assert!((entry.composio_cost_usd - 0.042).abs() < f64::EPSILON);
    assert!(entry.success);
    assert!(entry.error.is_none());
    // Periodic fetch does no summarisation — LLM cost columns stay zero,
    // and the Composio spend is the whole combined cost.
    assert_eq!(entry.input_tokens, 0);
    assert_eq!(entry.estimated_cost_usd, 0.0);
    assert!((entry.combined_cost_usd() - 0.042).abs() < f64::EPSILON);
}

/// A failed periodic tick still records the partial billable cost it
/// incurred before erroring (the fetch may have fired actions), with
/// `success = false` and the error message preserved.
#[test]
fn periodic_audit_entry_preserves_partial_cost_on_failure() {
    let usage = ComposioUsage {
        actions_called: 1,
        cost_usd: 0.01,
    };
    let entry = build_periodic_audit_entry(
        "notion",
        "cmp-9",
        &usage,
        0,
        500,
        Some("fetch timed out".to_string()),
    );

    assert!(!entry.success);
    assert_eq!(entry.error.as_deref(), Some("fetch timed out"));
    assert_eq!(entry.items_fetched, 0);
    // The billable action it managed to fire before failing is still
    // recorded so cost isn't under-reported on failures.
    assert_eq!(entry.composio_actions_called, 1);
    assert!((entry.composio_cost_usd - 0.01).abs() < f64::EPSILON);
}

#[test]
fn record_sync_success_stores_timestamp_keyed_by_toolkit_and_connection() {
    // Use unique keys so this test doesn't collide with other tests
    // writing into the process-wide map.
    let toolkit = "test_periodic_toolkit_a";
    let conn = "test-conn-a";
    record_sync_success(toolkit, conn);
    let map = last_sync_map();
    let guard = map.lock().expect("lock");
    let ts = guard
        .get(&(toolkit.to_string(), conn.to_string()))
        .expect("entry recorded");
    // Just-recorded timestamps should be very recent.
    assert!(ts.elapsed() < Duration::from_secs(5));
}

#[test]
fn record_sync_success_overwrites_previous_timestamp() {
    let toolkit = "test_periodic_toolkit_b";
    let conn = "test-conn-b";
    record_sync_success(toolkit, conn);
    let first = last_sync_map()
        .lock()
        .expect("lock")
        .get(&(toolkit.to_string(), conn.to_string()))
        .copied()
        .expect("first entry");
    // Second call must replace (not keep the older) timestamp.
    std::thread::sleep(Duration::from_millis(5));
    record_sync_success(toolkit, conn);
    let second = last_sync_map()
        .lock()
        .expect("lock")
        .get(&(toolkit.to_string(), conn.to_string()))
        .copied()
        .expect("second entry");
    assert!(
        second >= first,
        "record_sync_success should advance the stored Instant"
    );
}

// The `_guard` below is held deliberately across the `run_one_tick().await`
// in this test: it's a std::sync::Mutex used purely as a test-isolation
// gate around the process-global `OPENHUMAN_WORKSPACE` env var, not an
// async resource lock guarding shared runtime state. Dropping it before
// the await would let a sibling test mutate the env var mid-tick,
// defeating the isolation this guard exists to provide.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn run_one_tick_returns_ok_when_no_client() {
    // Isolate the workspace/env so config loading doesn't contend with
    // sibling tests mutating OPENHUMAN_WORKSPACE in parallel.
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempdir().expect("tempdir");
    unsafe {
        std::env::set_var("OPENHUMAN_WORKSPACE", tmp.path());
    }

    // With no session stored in the isolated workspace,
    // `build_composio_client` returns None and the tick should
    // silently skip (returning Ok). This covers the early-return
    // path that's otherwise only hit in production.
    let inner = tokio::time::timeout(Duration::from_secs(5), run_one_tick())
        .await
        .expect("run_one_tick should not hang indefinitely during tests");
    assert!(
        inner.is_ok(),
        "run_one_tick should return Ok when no client is available: {inner:?}"
    );

    unsafe {
        std::env::remove_var("OPENHUMAN_WORKSPACE");
    }
}

#[tokio::test]
async fn start_periodic_sync_is_idempotent() {
    // First call installs the scheduler via the OnceLock; subsequent
    // calls must be cheap no-ops without panicking. `tokio::spawn`
    // needs an ambient runtime, so this test runs under `tokio::test`.
    start_periodic_sync();
    start_periodic_sync();
    assert!(SCHEDULER_STARTED.get().is_some());
}

#[test]
fn record_sync_success_distinguishes_connections() {
    let toolkit = "test_periodic_toolkit_c";
    record_sync_success(toolkit, "conn-1");
    record_sync_success(toolkit, "conn-2");
    let map = last_sync_map();
    let guard = map.lock().expect("lock");
    assert!(guard
        .get(&(toolkit.to_string(), "conn-1".to_string()))
        .is_some());
    assert!(guard
        .get(&(toolkit.to_string(), "conn-2".to_string()))
        .is_some());
    // Unrelated key should be absent.
    assert!(guard
        .get(&(toolkit.to_string(), "conn-3".to_string()))
        .is_none());
}

/// In unit tests `scheduler_gate::STATE` is never initialised, so
/// `current_policy()` returns `Policy::Normal` and the helper must
/// return `None` — i.e. the tick is allowed to proceed. This pins the
/// happy-path wiring; an accidental "always pause" regression in the
/// helper would break every `run_one_tick`-driven test that follows it.
///
/// (The redundant "does-not-short-circuit" tick-level test that was
/// here in the first review pass was dropped per @oxoxDev's
/// [#2825 review](https://github.com/tinyhumansai/openhuman/pull/2825):
/// it duplicated `run_one_tick_returns_ok_when_no_client` because
/// both exited at the same `create_composio_client` no-client branch,
/// so neither actually proved the new gate-check arm fired in the
/// right direction. Asserting log-line absence via `tracing-test`
/// would prove it but adds a new dev-dependency for one assertion —
/// the helper-level test below already pins the wiring.)
#[test]
fn periodic_pause_reason_returns_none_when_gate_not_initialised() {
    // Calling without `scheduler_gate::init_global(...)` exercises the
    // OnceLock-uninitialised branch in `current_policy`, which is the
    // realistic test-environment state.
    assert!(
        periodic_pause_reason().is_none(),
        "expected None (i.e. tick proceeds) when scheduler_gate is in default Normal state, \
         got {:?}",
        periodic_pause_reason()
    );
}

#[test]
fn synthesized_periodic_source_is_enabled_scoped_and_uncapped() {
    let source = periodic_source("gmail", "connection-42");
    assert_eq!(source.id, "composio:connection-42");
    assert_eq!(source.kind, SourceKind::Composio);
    assert_eq!(source.label, "gmail");
    assert!(source.enabled);
    assert_eq!(source.toolkit.as_deref(), Some("gmail"));
    assert_eq!(source.connection_id.as_deref(), Some("connection-42"));
    assert!(source.path.is_none());
    assert!(source.glob.is_none());
    assert!(source.url.is_none());
    assert!(source.branch.is_none());
    assert!(source.paths.is_empty());
    assert!(source.max_commits.is_none());
    assert!(source.max_issues.is_none());
    assert!(source.max_prs.is_none());
    assert!(source.query.is_none());
    assert!(source.since_days.is_none());
    assert!(source.max_items.is_none());
    assert!(source.selector.is_none());
    assert!(source.max_tokens_per_sync.is_none());
    assert!(source.max_cost_per_sync_usd.is_none());
    assert!(source.sync_depth_days.is_none());
}
