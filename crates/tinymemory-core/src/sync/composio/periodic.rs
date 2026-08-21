//! Periodic sync scheduler for the Composio domain.
//!
//! Spawned once at startup. The scheduler walks every active Composio
//! connection on a fixed tick, looks up the matching native provider,
//! and dispatches the matching tinycortex pipeline if enough time
//! has elapsed since that connection's last sync (per the provider's
//! `sync_interval_secs`).
//!
//! ## Direct mode (`[composio-direct]`)
//!
//! As of #1710 Wave 1, the scheduler is **mode-aware**: it resolves the
//! client via `create_composio_client` each tick so a direct-mode
//! user's personal Composio v3 tenant gets walked (via
//! `direct_list_connections`) instead of returning an empty list from
//! the tinyhumans tenant. The per-connection sync calls go through
//! `ProviderContext::execute` which is itself mode-aware.
//!
//! Real-time trigger webhooks (`composio:trigger` socket.io events
//! fanned out from `wss://api.tinyhumans.ai`) still do not reach the
//! core when `config.composio().mode == "direct"`, because the backend
//! HMAC-verifies the Composio webhook and pushes it down a per-user
//! socket — direct-mode users see synchronous tool execution and
//! periodic poll-based sync, but not async trigger pushes in this
//! release. See the `composio.direct_mode_triggers_gap` capability
//! entry in `about_app/catalog.rs` for the user-visible status.
//!
//! Design notes:
//!
//!   * One global tick (5min) drives every provider — we don't spawn a
//!     task per connection, because the number of connections per user
//!     is small and a single tick keeps the bookkeeping trivial.
//!   * Per-connection state (last sync timestamp) lives in a
//!     process-global `Arc<Mutex<HashMap>>` keyed by `(toolkit,
//!     connection_id)`. The map is shared with event-driven sync paths
//!     (bus subscribers, `on_connection_created`) via
//!     [`record_sync_success`] so a recent non-periodic sync prevents
//!     the scheduler from redundantly re-firing. The map is rebuilt on
//!     restart; to keep a user-configured cadence (e.g. "Sync every 24h",
//!     #3302) from re-firing on every cold start, the due-check falls back
//!     to the **persisted** sync-audit timestamp (`read_audit_log`) when
//!     the in-memory record is absent — see `persisted_since_last_sync`.
//!   * Errors are logged and swallowed; the scheduler must never panic
//!     out of its loop or periodic sync stops silently for the rest of
//!     the process lifetime.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tokio::time::interval;

use crate::config_loader as config_rpc;
use crate::scheduler_gate::PauseReason;
use crate::scheduler_gate::{current_policy, resume_notify};
use crate::sources::{memory_sync_defaults_for_toolkit, MemorySourceEntry, SourceKind};
use tinymemory_api::host::DEFAULT_MEMORY_SYNC_INTERVAL_SECS;

use super::providers::{get_provider, ComposioUsage};
use crate::composio_host;
use crate::sync::audit::{append_audit_entry, read_audit_log, SyncAuditEntry};
use chrono::{DateTime, Utc};

/// How often the scheduler wakes up to look for due syncs. Independent
/// from per-provider `sync_interval_secs` — this just bounds how long
/// past a provider's interval we might fire.
///
/// 20 min trades a little staleness for noticeably less foreground load:
/// each tick triggers an HTTP fetch + DB write per due connection, and
/// for users with several connected providers the old 60s cadence kept
/// the laptop visibly busy. Per-provider `sync_interval_secs` still
/// caps the *minimum* delay between actual syncs — this only loosens
/// the upper bound.
const TICK_SECONDS: u64 = 1200;

/// Process-wide guard so the scheduler is only started once even
/// when both `start_channels` and `bootstrap_core_runtime` call into
/// us during startup. Without this we'd end up with two parallel tick
/// loops competing for the same connections.
static SCHEDULER_STARTED: OnceLock<()> = OnceLock::new();

/// Process-wide map of `(toolkit, connection_id) → last successful sync
/// instant`. Shared between the periodic scheduler loop and event-driven
/// sync paths (e.g. `ComposioConnectionCreatedSubscriber`,
/// `on_connection_created`) so that a recent non-periodic sync prevents
/// the scheduler from firing immediately on the next tick.
type SyncTimestampMap = Arc<Mutex<HashMap<(String, String), Instant>>>;

static LAST_SYNC_AT: OnceLock<SyncTimestampMap> = OnceLock::new();

/// Get (or lazily initialise) the shared last-sync-at map.
fn last_sync_map() -> SyncTimestampMap {
    LAST_SYNC_AT
        .get_or_init(|| Arc::new(Mutex::new(HashMap::new())))
        .clone()
}

/// Record a successful sync for the given `(toolkit, connection_id)` key.
/// Called by the periodic scheduler after a successful sync and by
/// event-driven paths (bus subscribers, `on_connection_created`) so the
/// periodic ticker respects recent non-periodic syncs.
pub fn record_sync_success(toolkit: &str, connection_id: &str) {
    if let Ok(mut map) = last_sync_map().lock() {
        map.insert(
            (toolkit.to_string(), connection_id.to_string()),
            Instant::now(),
        );
    }
}

/// Resolve the effective periodic sync interval (seconds) for one connection,
/// combining the provider's own default with the user's global
/// memory-sync cadence ([`Config::memory_sync_interval_secs`], #3302).
///
/// - `global == Some(0)` → `None`: "Manual only" — the scheduler skips this
///   source entirely (manual sync still works).
/// - `global == Some(n)` → `Some(max(n, provider_default))`: the user's
///   cadence overrides the provider default but is floored at it, so we never
///   sync *more* often than the provider intended.
/// - `global == None` → `Some(max(DEFAULT, provider_default))`: no explicit
///   user choice, so fall back to the 24h default cadence (also floored at the
///   provider default).
pub(crate) fn effective_interval_secs(provider_default: u64, global: Option<u64>) -> Option<u64> {
    match global {
        Some(0) => None,
        Some(n) => Some(n.max(provider_default)),
        None => Some(DEFAULT_MEMORY_SYNC_INTERVAL_SECS.max(provider_default)),
    }
}

/// Decide whether a connection is due for a periodic sync right now, given the
/// effective interval and how long ago it last synced this run.
///
/// `since_last_sync == None` means we have no record of a sync this process
/// lifetime, so we fire immediately (the restart-recovery path). Kept pure so
/// the due-check can be simulated without driving the real `Instant` clock.
pub(crate) fn connection_is_due(interval_secs: u64, since_last_sync: Option<Duration>) -> bool {
    match since_last_sync {
        Some(elapsed) => elapsed >= Duration::from_secs(interval_secs),
        None => true,
    }
}

/// Build an index of `connection_id → most recent successful Composio sync
/// timestamp` from the persisted sync audit log (#3302).
///
/// The periodic loop writes audit entries with `source_id = connection_id` and
/// `scope = "{toolkit}:{connection_id}"`; we accept either shape and key by the
/// connection id. Only successful syncs count — matching the in-memory
/// [`record_sync_success`] semantics, which never records a failed tick so the
/// next tick retries. This is the wall-clock record that lets the cadence
/// survive restarts (the in-memory monotonic map cannot).
fn index_last_success_by_connection(entries: &[SyncAuditEntry]) -> HashMap<String, DateTime<Utc>> {
    let mut idx: HashMap<String, DateTime<Utc>> = HashMap::new();
    for e in entries {
        if e.source_kind != "composio" || !e.success {
            continue;
        }
        let connection_id = e
            .scope
            .rsplit_once(':')
            .map(|(_, c)| c.to_string())
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| e.source_id.clone());
        idx.entry(connection_id)
            .and_modify(|t| {
                if e.timestamp > *t {
                    *t = e.timestamp;
                }
            })
            .or_insert(e.timestamp);
    }
    idx
}

/// Wall-clock elapsed since a connection's last persisted successful sync, if
/// any. Saturates at zero for a future timestamp (clock skew), so a skewed
/// record never reads as "wildly overdue". Returns `None` when the connection
/// has no persisted sync — letting the caller treat it as never-synced.
fn persisted_since_last_sync(
    idx: &HashMap<String, DateTime<Utc>>,
    connection_id: &str,
    now: DateTime<Utc>,
) -> Option<Duration> {
    idx.get(connection_id).map(|ts| {
        let secs = (now - *ts).num_seconds().max(0) as u64;
        Duration::from_secs(secs)
    })
}

/// Outcome of consulting the per-source registry for one Composio connection
/// during a periodic tick (#2831).
#[derive(Debug, PartialEq, Eq)]
enum PeriodicSourceDecision {
    /// The user toggled this source **off** — skip background sync entirely.
    /// Manual `memory_sources_sync` still works (it has its own `enabled`
    /// guard); only the automatic loop honours this here.
    Skip,
    /// Sync this connection with the given caps (`None` = uncapped for that
    /// dimension).
    Sync {
        max_items: Option<u32>,
        sync_depth_days: Option<u32>,
    },
}

/// Decide whether — and with what caps — to periodically sync one connection,
/// honouring the per-source `enabled` toggle (#2831). Pure so the three
/// branches can be unit-tested without async/registry I/O.
///
/// - **disabled row** → [`PeriodicSourceDecision::Skip`]: the background loop
///   must not sync a source the user switched off (this was the row-2 leak —
///   previously the loop only read the registry for caps and synced disabled
///   sources anyway, uncapped).
/// - **enabled row** → `Sync` with the row's caps.
/// - **no row yet** → `Sync` with conservative per-toolkit defaults
///   ([`memory_sync_defaults_for_toolkit`]). `reconcile` normally backfills an
///   enabled, capped row for every connection; this covers the brief
///   pre-reconcile window. The no-match path defaults to *sync-bounded*, never
///   *skip* and never *uncapped*, so a missing/mismatched row degrades safely
///   (data keeps flowing, just capped) instead of silently going dark.
fn decide_periodic_source(
    source: Option<&MemorySourceEntry>,
    toolkit: &str,
) -> PeriodicSourceDecision {
    match source {
        Some(s) if !s.enabled => PeriodicSourceDecision::Skip,
        Some(s) => PeriodicSourceDecision::Sync {
            max_items: s.max_items,
            sync_depth_days: s.sync_depth_days,
        },
        None => {
            let (max_items, sync_depth_days) = memory_sync_defaults_for_toolkit(toolkit);
            PeriodicSourceDecision::Sync {
                max_items,
                sync_depth_days,
            }
        }
    }
}

/// Spawn the periodic sync background task. Idempotent: only the
/// first call actually spawns the loop, every subsequent call is a
/// cheap no-op (logged at `debug` so it's visible during startup
/// tracing without spamming `info`).
pub fn start_periodic_sync() {
    if SCHEDULER_STARTED.get().is_some() {
        tracing::debug!("[composio:periodic] scheduler already running, skipping start");
        return;
    }
    // Race-safe: only the thread that wins `set` runs the spawn body.
    if SCHEDULER_STARTED.set(()).is_err() {
        tracing::debug!("[composio:periodic] scheduler already running (race), skipping start");
        return;
    }

    tokio::spawn(async move {
        tracing::info!(
            tick_seconds = TICK_SECONDS,
            "[composio:periodic] scheduler starting"
        );
        run_loop().await;
        // run_loop only returns on a fatal error in the bus — log it
        // so the silent stop is at least visible in the trace.
        tracing::error!("[composio:periodic] scheduler loop exited");
    });
}

/// Inner loop, broken out so it's easy to mock-replace in tests if we
/// ever want to drive ticks deterministically.
///
/// Each iteration waits on whichever comes first (#2831):
///   * the 20-min `ticker` — the steady-state cadence, or
///   * the scheduler-gate **resume** notify — fired when the user toggles
///     Memory Tree back on or signs back in.
///
/// On a resume wake we run a tick **immediately** (so sync restarts within
/// seconds, not at the next ≤20-min boundary) and `reset()` the ticker so the
/// *next* scheduled tick is a full `TICK_SECONDS` out. The reset is what stops
/// rapid off-on toggling from bunch-firing: many wakes collapse into at most
/// one extra tick (the `Notify` stores a single permit), and the cadence
/// re-bases from the last actual tick.
async fn run_loop() {
    let mut ticker = interval(Duration::from_secs(TICK_SECONDS));
    let resume = resume_notify();
    // Skip the immediate-fire tick so startup isn't slammed before the
    // user even has time to sign in.
    ticker.tick().await;

    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = resume.notified() => {
                // Woke early on a resume transition. Re-base the cadence so the
                // next scheduled tick is TICK_SECONDS from now, then fall
                // through and run the tick immediately.
                ticker.reset();
            }
        }
        if let Err(e) = run_one_tick().await {
            tracing::warn!(
                error = %e,
                "[composio:periodic] tick failed (continuing)"
            );
        }
    }
}

/// Inspect the scheduler-gate policy and decide whether this tick should
/// fire at all. Returns `Some(reason)` for paused states so the caller can
/// log a single, attributable line instead of doing the work and discovering
/// per-LLM-call later that everything's gated.
///
/// Covers two reasons the memory subsystem treats as "do no background
/// work":
/// - [`PauseReason::UserDisabled`] — user flipped the Memory Tree toggle off
///   in Settings (#1856 Part 1). The 20-min Composio fetch loop honouring
///   this flag is the explicit follow-up listed in the #2719 PR body.
/// - [`PauseReason::SignedOut`] — no live session; periodic work would just
///   401-loop against the backend.
///
/// Other [`PauseReason`] variants:
/// - `OnBattery` / `CpuPressure` (future, per #1073) — intentionally **not**
///   gated here; periodic Composio fetch is network-light, so battery / CPU
///   pressure shouldn't stop the user's data flowing in. Those signals
///   already throttle LLM-bound work through the regular gate.
/// - `Unknown` — documented in `scheduler_gate::policy` as a safe fallback;
///   `Policy::pause_reason()` returns it only when the gate state is in a
///   transitional / not-yet-resolved condition. Letting the tick proceed
///   here keeps periodic sync running through brief transitions instead of
///   pausing on stale unresolved state.
pub(crate) fn periodic_pause_reason() -> Option<PauseReason> {
    // Delegate the `Policy::Paused { .. }` → `PauseReason` extraction to
    // the existing `Policy::pause_reason()` helper (avoids re-implementing
    // the same destructure twice). The allow-list below is the only thing
    // this site has to own — future `PauseReason` variants stay opt-in.
    let reason = current_policy().pause_reason()?;
    matches!(reason, PauseReason::UserDisabled | PauseReason::SignedOut).then_some(reason)
}

/// Process-level "was the last tick paused?" tracker for transition logging.
///
/// We want `info!` *once* when the periodic loop crosses the pause boundary
/// (so fleet operators investigating "why is Composio not syncing?" see a
/// breadcrumb at default log level), without spamming `info` every 20 min
/// while the user has the toggle off. `Relaxed` ordering is fine because
/// the only consumer is the inside of `run_one_tick`, which is serialised
/// by the singleton scheduler loop.
static LAST_TICK_WAS_PAUSED: AtomicBool = AtomicBool::new(false);

/// Run a single scheduler tick. Public-ish (`pub(crate)`) so the test
/// module can drive ticks without spinning up the real `interval`.
pub(crate) async fn run_one_tick() -> Result<(), String> {
    // Step 0: scheduler-gate check. When the user has paused Memory Tree
    // via the Settings toggle, every subsequent tick should be a cheap
    // no-op — no `list_connections` call, no provider walk, no API budget
    // burn. The check runs **before** config load + auth-client build so
    // a paused session never even resolves the API token.
    //
    // Transition logging: emit `info!` once when the loop crosses the
    // pause boundary in either direction; stay at `debug!` for the
    // already-paused / already-running steady state. Without this, fleet
    // operators investigating "why is Composio not syncing?" see nothing
    // at default log level.
    if let Some(reason) = periodic_pause_reason() {
        let was_paused = LAST_TICK_WAS_PAUSED.swap(true, Ordering::Relaxed);
        if was_paused {
            tracing::debug!(
                reason = reason.as_str(),
                "[composio:periodic] scheduler-gate paused — skipping tick"
            );
        } else {
            tracing::info!(
                reason = reason.as_str(),
                "[composio:periodic] scheduler-gate paused — pausing periodic Composio sync"
            );
        }
        return Ok(());
    } else {
        let was_paused = LAST_TICK_WAS_PAUSED.swap(false, Ordering::Relaxed);
        if was_paused {
            tracing::info!(
                "[composio:periodic] scheduler-gate resumed — periodic Composio sync re-enabled"
            );
        }
    }

    // Step 1: load config (also gives us the auth token via the
    // shared integrations client builder).
    let config = config_rpc::load_config_with_timeout()
        .await
        .map_err(|e| format!("load_config: {e}"))?;

    // Step 2: list active connections — mode-aware. Backend mode walks
    // the tinyhumans tenant; direct mode walks the user's personal
    // Composio v3 tenant. Mirrors `ops::composio_list_connections` so
    // direct-mode users get periodic sync against their own connections
    // instead of seeing an empty list (#1710).
    // Mode dispatch lives in the host's `ComposioHost` impl, and so does the
    // 401 classification that used to happen here: the direct-mode v3
    // `/connected_accounts` 401 shape is a property of the client, not of this
    // loop, and the host reports it through the same observability classifier
    // the UI poll uses. A failure here means "not signed in / no direct key" as
    // often as it means a real fault, so the tick skips rather than erroring.
    let connections = match composio_host::list_connections(&*config).await {
        Ok(connections) => connections,
        Err(e) => {
            tracing::debug!(
                error = %e,
                "[composio:periodic] no connections (not signed in? no direct key?), skipping tick"
            );
            return Ok(());
        }
    };

    let sync_map = last_sync_map();

    // Global, user-configurable memory-sync cadence (#3302). Applied to every
    // opted-in source as a floor/override over the provider's own default; a
    // value of `Some(0)` disables periodic auto-sync ("Manual only").
    let global_interval = config.memory_sync_interval_secs();

    // Persisted last-sync fallback (#3302). The in-memory `LAST_SYNC_AT` map is
    // rebuilt empty on every launch, so without this a cold start would re-fire
    // every connection on the first tick — silently breaking the configured
    // "Sync every 24h" gap across app restarts. We index the persisted sync
    // audit log (wall-clock timestamps that survive restarts) and use it as the
    // due-check fallback whenever the in-memory monotonic record is absent.
    let (audit_index, audit_available) =
        composio_audit_state(read_audit_log(config.workspace_dir()));
    if !audit_available {
        tracing::warn!(
            "[memory_sync:periodic] audit unavailable; sources without in-memory cadence will be skipped"
        );
    }
    let now = Utc::now();

    // Per-source registry snapshot (#2831). The periodic loop gates on the
    // per-source `enabled` toggle so a source the user switched off stops
    // syncing in the background — matching the manual paths
    // (`memory_sources::sync_source`, `memory_sources_sync_all`), which already
    // early-return on `!enabled`. Index every Composio source (enabled and
    // disabled) by connection id; the per-connection branch below resolves
    // skip/caps via `decide_periodic_source`.
    //
    // Built from the **already-loaded** `config` snapshot (Step 1), not a second
    // `list_sources()` read. A separate read whose error we swallowed to an
    // empty map would make every disabled source fall through to the
    // `decide_periodic_source(None, ..)` default-caps path — silently
    // re-enabling background sync for sources the user switched off on a
    // transient config-read failure. Reusing the tick's snapshot is fail-closed
    // (a disabled row stays disabled) and avoids the extra read entirely.
    let composio_sources: HashMap<String, MemorySourceEntry> =
        crate::sources::decode_memory_sources(&*config)
            .iter()
            .filter(|s| s.kind == SourceKind::Composio)
            .filter_map(|s| s.connection_id.clone().map(|id| (id, s.clone())))
            .collect();

    let mut considered = 0usize;
    let mut fired = 0usize;
    for conn in connections {
        considered += 1;

        // Skip connections that aren't actually live yet.
        if !conn.is_active() {
            continue;
        }

        let toolkit = conn.normalized_toolkit();
        let Some(provider) = get_provider(&toolkit) else {
            // No provider registered for this toolkit — that's fine,
            // we just don't have native code for it. Tools still work
            // through `composio_execute`.
            continue;
        };

        let Some(provider_default) = provider.sync_interval_secs() else {
            // Provider opted out of periodic sync entirely.
            continue;
        };

        let Some(interval_secs) = effective_interval_secs(provider_default, global_interval) else {
            // User selected "Manual only" — skip auto-sync for this source.
            // Manual `memory_sources_sync` still works.
            tracing::debug!(
                toolkit = %toolkit,
                connection_id = %conn.id,
                "[composio:periodic] manual-only mode — skipping periodic sync"
            );
            continue;
        };

        let key = (toolkit.clone(), conn.id.clone());
        // Prefer the in-memory monotonic record (most accurate within this run);
        // fall back to the persisted audit timestamp so the configured cadence
        // is honoured across restarts instead of re-firing on every cold start.
        let in_memory_since = {
            let map = sync_map.lock().unwrap_or_else(|e| e.into_inner());
            map.get(&key).map(|when| when.elapsed())
        };
        let Some(since_last_sync) = cadence_from_audit(
            in_memory_since,
            audit_available,
            persisted_since_last_sync(&audit_index, &conn.id, now),
        ) else {
            tracing::debug!(
                toolkit = %toolkit,
                "[composio:periodic] source has unknown cadence while audit is unavailable; skipping"
            );
            continue;
        };
        if !connection_is_due(interval_secs, since_last_sync) {
            continue;
        }

        // Per-source gate + caps from the memory_sources registry (#2831).
        // A disabled source is skipped here (the background-sync half of the
        // toggle); enabled sources sync with their caps; a connection with no
        // registry row yet syncs with conservative per-toolkit defaults.
        let (src_max_items, src_sync_depth_days) =
            match decide_periodic_source(composio_sources.get(&conn.id), &toolkit) {
                PeriodicSourceDecision::Skip => {
                    tracing::debug!(
                        toolkit = %toolkit,
                        connection_id = %conn.id,
                        "[composio:periodic] source disabled — skipping periodic sync"
                    );
                    continue;
                }
                PeriodicSourceDecision::Sync {
                    max_items,
                    sync_depth_days,
                } => (max_items, sync_depth_days),
            };

        tracing::debug!(
            toolkit = %toolkit,
            connection_id = %conn.id,
            max_items = ?src_max_items,
            sync_depth_days = ?src_sync_depth_days,
            "[composio:periodic] caps from registry"
        );

        let mut source = composio_sources
            .get(&conn.id)
            .cloned()
            .unwrap_or_else(|| periodic_source(&toolkit, &conn.id));
        source.max_items = src_max_items;
        source.sync_depth_days = src_sync_depth_days;

        tracing::debug!(
            toolkit = %conn.toolkit,
            connection_id = %conn.id,
            interval_secs,
            "[composio:periodic] firing sync"
        );
        let sync_started = Instant::now();
        let result = crate::sync::pipelines::host::run_composio_connection_with_caps(
            &toolkit,
            &conn.id,
            &*config,
            crate::sync::pipelines::host::SourceCaps::from_source(&source),
        )
        .await;
        let duration_ms = sync_started.elapsed().as_millis() as u64;

        match result {
            Ok(outcome) => {
                let usage = ComposioUsage {
                    actions_called: outcome.actions_called,
                    cost_usd: outcome.provider_cost_usd,
                };
                tracing::debug!(
                    toolkit = %conn.toolkit,
                    connection_id = %conn.id,
                    items = outcome.records_ingested,
                    composio_actions = usage.actions_called,
                    "[composio:periodic] sync ok"
                );
                let entry = build_periodic_audit_entry(
                    &toolkit,
                    &conn.id,
                    &usage,
                    outcome.records_ingested as usize,
                    duration_ms,
                    None,
                );
                if let Err(error) = append_audit_entry(config.workspace_dir(), &entry) {
                    tracing::warn!(%error, "[memory_sync:audit] append failed");
                }
                record_sync_success(&conn.toolkit, &conn.id);
                fired += 1;
            }
            Err(e) => {
                let usage = ComposioUsage {
                    actions_called: e.actions_called,
                    cost_usd: e.provider_cost_usd,
                };
                tracing::warn!(
                    toolkit = %conn.toolkit,
                    connection_id = %conn.id,
                    error = %e,
                    "[composio:periodic] sync failed (will retry next tick)"
                );
                // A failed tick may still have fired billable fetch actions
                // before erroring — audit the partial cost so it isn't lost.
                let entry = build_periodic_audit_entry(
                    &toolkit,
                    &conn.id,
                    &usage,
                    0,
                    duration_ms,
                    Some(e.to_string()),
                );
                if let Err(error) = append_audit_entry(config.workspace_dir(), &entry) {
                    tracing::warn!(%error, "[memory_sync:audit] append failed");
                }
                // Intentionally do NOT update last_sync_at on failure
                // so the next tick retries immediately.
            }
        }
    }

    tracing::debug!(considered, fired, "[composio:periodic] tick complete");
    Ok(())
}

fn composio_audit_state(
    read: anyhow::Result<Vec<SyncAuditEntry>>,
) -> (HashMap<String, DateTime<Utc>>, bool) {
    match read {
        Ok(entries) => (index_last_success_by_connection(&entries), true),
        Err(error) => {
            tracing::warn!(%error, "[memory_sync:periodic] audit read failed");
            (HashMap::new(), false)
        }
    }
}

fn cadence_from_audit(
    in_memory_since: Option<Duration>,
    audit_available: bool,
    persisted_since: Option<Duration>,
) -> Option<Option<Duration>> {
    match in_memory_since {
        Some(since) => Some(Some(since)),
        None if audit_available => Some(persisted_since),
        None => None,
    }
}

fn periodic_source(toolkit: &str, connection_id: &str) -> MemorySourceEntry {
    MemorySourceEntry {
        id: format!("composio:{connection_id}"),
        kind: SourceKind::Composio,
        label: toolkit.to_string(),
        enabled: true,
        toolkit: Some(toolkit.to_string()),
        connection_id: Some(connection_id.to_string()),
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
        max_items: None,
        selector: None,
        max_tokens_per_sync: None,
        max_cost_per_sync_usd: None,
        sync_depth_days: None,
    }
}

/// Build a [`SyncAuditEntry`] for one periodic Composio sync tick (#3111
/// follow-up).
///
/// Periodic syncs only fetch + ingest; summarisation runs later in the async
/// job worker, so the LLM-cost columns (tokens, estimated / actual charge)
/// are zero here. The meaningful spend is the Composio billable actions the
/// fetch fired, carried in `usage`. `scope` is `{toolkit}:{connection_id}` to
/// match the owner shape the per-source memory-tree ingest uses, and
/// `source_kind` is `"composio"` so the Sync History panel groups periodic
/// rows alongside the manual-sync rows the dispatcher already writes.
fn build_periodic_audit_entry(
    toolkit: &str,
    connection_id: &str,
    usage: &ComposioUsage,
    items_ingested: usize,
    duration_ms: u64,
    error: Option<String>,
) -> SyncAuditEntry {
    SyncAuditEntry {
        timestamp: chrono::Utc::now(),
        source_id: connection_id.to_string(),
        source_kind: "composio".to_string(),
        scope: format!("{toolkit}:{connection_id}"),
        items_fetched: items_ingested as u32,
        batches: 0,
        input_tokens: 0,
        output_tokens: 0,
        estimated_cost_usd: 0.0,
        composio_actions_called: usage.actions_called,
        composio_cost_usd: usage.cost_usd,
        actual_charged_usd: None,
        duration_ms,
        success: error.is_none(),
        error,
    }
}

#[cfg(test)]
#[path = "periodic_tests.rs"]
mod tests;
