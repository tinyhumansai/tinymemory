//! Shared types for Composio provider implementations.

use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

// Test-only: the tests below build a `TestHostConfig` and call
// `MemoryHostConfig` methods on it directly. Production code in this module
// goes through `crate::Config`, so neither name is needed in a non-test build.
#[cfg(test)]
use tinymemory_api::host::{test_support::TestHostConfig, MemoryHostConfig};

use crate::composio_host::{self, ComposioExecuteResponse};
use crate::config_loader as config_rpc;
use crate::Config;

/// Reason a sync was triggered. Providers can use this to decide
/// whether to do a full backfill or an incremental pull.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncReason {
    /// First sync immediately after an OAuth handoff completes.
    ConnectionCreated,
    /// Periodic background sync from the scheduler.
    Periodic,
    /// Explicit user-driven sync from RPC / UI.
    Manual,
}

impl SyncReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            SyncReason::ConnectionCreated => "connection_created",
            SyncReason::Periodic => "periodic",
            SyncReason::Manual => "manual",
        }
    }
}

/// What kind of work an ingested task implies. GitHub's issues-and-PRs
/// search returns both shapes, and the job differs fundamentally —
/// *resolve* an issue vs *review* a pull request — so providers tag each
/// task and the `task_sources` enrichment phrases the objective / agent
/// prompt accordingly (the triage LLM then knows what to do). Providers
/// that don't distinguish (notion, linear, clickup) leave this `Generic`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    /// No issue/PR distinction — the default for non-code providers.
    #[default]
    Generic,
    /// A tracker issue: the job is to resolve / implement it.
    Issue,
    /// A pull request: the job is to review it (read the diff, give feedback).
    PullRequest,
}

impl TaskKind {
    /// Stable lowercase tag, mirrored into the card's `source_metadata`.
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskKind::Generic => "generic",
            TaskKind::Issue => "issue",
            TaskKind::PullRequest => "pull_request",
        }
    }
}

/// Normalized user profile shape returned by every provider.
///
/// The shared fields (`display_name`, `email`, `username`, `avatar_url`,
/// `profile_url`)
/// cover what the desktop UI actually needs to render a connected
/// account card. Anything provider-specific (Gmail's `messagesTotal`,
/// Notion's workspace ids, …) goes into [`extras`](Self::extras) so
/// callers don't have to widen the shape every time a new toolkit
/// lands.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderUserProfile {
    pub toolkit: String,
    pub connection_id: Option<String>,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub username: Option<String>,
    pub avatar_url: Option<String>,
    pub profile_url: Option<String>,
    /// Provider-specific extras (raw JSON object).
    #[serde(default)]
    pub extras: serde_json::Value,
}

/// Result of a provider sync run. Mostly used for logging + UI status.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncOutcome {
    pub toolkit: String,
    pub connection_id: Option<String>,
    pub reason: String,
    pub items_ingested: usize,
    pub started_at_ms: u64,
    pub finished_at_ms: u64,
    pub summary: String,
    /// Provider-specific extras (raw JSON object).
    #[serde(default)]
    pub details: serde_json::Value,
}

impl SyncOutcome {
    pub fn elapsed_ms(&self) -> u64 {
        self.finished_at_ms.saturating_sub(self.started_at_ms)
    }
}

/// A provider-agnostic, structured work item produced by
/// [`super::ComposioProvider::fetch_tasks`].
///
/// Unlike the `sync()` path — which persists upstream items into the
/// memory store as passive context — `fetch_tasks` *returns* normalized
/// tasks so the `task_sources` domain can enrich them and route them
/// onto the agent's todo board. Every native task provider (github,
/// notion, linear, clickup) maps its upstream payload shape into this
/// common envelope.
///
/// `source_id` is left empty by providers and stamped by the
/// `task_sources` pipeline with the originating `TaskSource.id` — a
/// provider has no knowledge of which configured source asked for the
/// fetch.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NormalizedTask {
    /// Upstream provider's stable id for the item (issue/task/page id).
    pub external_id: String,
    /// The `TaskSource.id` that produced this task. Empty until the
    /// pipeline stamps it.
    #[serde(default)]
    pub source_id: String,
    /// Toolkit slug, e.g. `"github"`.
    pub provider: String,
    /// Whether this task is an issue, a pull request, or undifferentiated.
    /// Drives intent-aware objective / prompt phrasing in enrichment.
    #[serde(default)]
    pub kind: TaskKind,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    /// Due date as an ISO-8601 string, when the provider exposes one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    /// Last-updated ISO-8601 timestamp — used for cursor advancement and
    /// edit-aware dedup (`{external_id}@{updated_at}`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    /// The raw upstream payload, retained for enrichment / debugging.
    #[serde(default)]
    pub raw: serde_json::Value,
}

/// A selectable upstream task container (board / database / list) used to
/// populate a picker so the user chooses from a list instead of pasting a
/// raw id. Today this is a Notion database, later a Linear team or ClickUp
/// list. Surfaced to the task-source UI as `{ id, title }`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TaskContainer {
    /// Provider-native id (e.g. a Notion database id) used as the filter id.
    pub id: String,
    /// Human-readable label for the picker.
    pub title: String,
}

/// Provider-agnostic filter passed into
/// [`super::ComposioProvider::fetch_tasks`].
///
/// The `task_sources` domain builds this from a user-configured,
/// per-provider `FilterSpec`. Each provider reads only the fields that
/// apply to it (github reads `repo`/`labels`; notion reads
/// `database_id`; linear/clickup read `team_id`; …) and ignores the
/// rest. `extra` is a free-form escape hatch surfaced in the UI for
/// advanced provider-native query fragments.
/// How the GitHub task-source fetch reaches GitHub. Shipped desktop users
/// connect GitHub via Composio OAuth (no `gh` on PATH, no `GITHUB_TOKEN`),
/// while local dev / self-host setups often have the reverse. `Auto` does the
/// right thing for both; `Composio` / `Local` force a path when the user wants.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GithubFetchMode {
    /// Try the connected Composio account first; fall back to local `gh`/REST
    /// only when Composio is unavailable. The safe default — no regression for
    /// shipped users, still a true fallback for local/dev.
    #[default]
    Auto,
    /// Force the connected Composio account (classic shipped-app behaviour).
    Composio,
    /// Force local `gh` CLI / REST with a `GH_TOKEN`/`GITHUB_TOKEN` env token.
    Local,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TaskFetchFilter {
    /// Scope to items assigned to (or involving) the authenticated user.
    #[serde(default)]
    pub assignee_is_me: bool,
    /// GitHub fetch path selector (Composio vs local `gh`/REST). Default `Auto`.
    #[serde(default)]
    pub github_fetch_mode: GithubFetchMode,
    /// GitHub `owner/name` repository scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// GitHub label filter.
    #[serde(default)]
    pub labels: Vec<String>,
    /// Issue/task state filter (e.g. `"open"`, `"todo"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// Notion database (board) id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database_id: Option<String>,
    /// Notion status property filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Linear / ClickUp team (workspace) id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_id: Option<String>,
    /// ClickUp list id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list_id: Option<String>,
    /// Free-form provider-native filter fragment (advanced).
    #[serde(default)]
    pub extra: serde_json::Value,
    /// Hard cap on how many tasks a single fetch returns.
    #[serde(default)]
    pub max: u32,
}

impl TaskFetchFilter {
    /// Effective per-fetch item cap, defaulting to a safe bound when the
    /// caller leaves `max` unset (0).
    pub fn effective_max(&self) -> usize {
        if self.max == 0 {
            25
        } else {
            self.max as usize
        }
    }
}

/// Per-call context handed to provider methods.
///
/// `connection_id` is `None` when a method runs in a "no specific
/// connection" mode (e.g. an across-the-board periodic sync that
/// already iterated). For per-connection paths it is always populated.
///
/// **Mode-aware dispatch (#1710)**: pre-fix, `ProviderContext` cached a
/// pre-baked `ComposioClient` built once at construction time. Toggling
/// `composio.mode = "direct"` mid-session left provider syncs still
/// routing through the backend tinyhumans tenant. The current shape
/// keeps an [`Arc<Config>`] and resolves the underlying client per call
/// through [`ProviderContext::execute`], mirroring the agent-tool
/// migration in the host's `integrations::composio::tools::ComposioExecuteTool`.
/// Per-sync accumulator for Composio billable-action usage.
///
/// Lives behind a shared handle on [`ProviderContext`] so the single
/// `execute` chokepoint can tally every action a provider fires during one
/// sync run, regardless of which provider (gmail / slack / github / notion /
/// linear / clickup) or how many pages it paginates.
/// [`crate::sync::composio::run_connection_sync`] returns
/// the final tally alongside the [`SyncOutcome`] for the sync audit log
/// (#3111).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ComposioUsage {
    /// Count of `execute` calls that returned a response this run.
    pub actions_called: u32,
    /// Sum of each response's backend-reported `cost_usd`.
    pub cost_usd: f64,
}

/// Shared, interior-mutable handle to a [`ComposioUsage`] tally. Cloning a
/// [`ProviderContext`] shares the same underlying counter, so the count is
/// stable no matter how the context is passed around within a sync.
pub type ComposioUsageHandle = Arc<Mutex<ComposioUsage>>;

#[derive(Clone)]
pub struct ProviderContext {
    pub config: Arc<Config>,
    pub toolkit: String,
    pub connection_id: Option<String>,
    /// Accumulates Composio billable-action usage across this context's
    /// lifetime. Defaulted at every construction site; only the sync path
    /// (`run_connection_sync`) reads it back. Non-sync callers (agent tools,
    /// task-source fetches) leave it at zero — harmless.
    pub usage: ComposioUsageHandle,
    /// Maximum items to fetch in a single sync pass.
    ///
    /// Set from the corresponding `MemorySourceEntry.max_items` field at
    /// sync-dispatch time. `None` means no cap beyond the provider's own
    /// internal upper bounds.
    pub max_items: Option<u32>,
    /// Maximum sync depth window in days.
    ///
    /// Set from `MemorySourceEntry.sync_depth_days`. When `Some(n)`, the
    /// provider only fetches items from the last `n` days. `None` means
    /// no additional depth restriction beyond the provider's cursor.
    pub sync_depth_days: Option<u32>,
}

impl ProviderContext {
    /// Build a context from the current config + a toolkit slug.
    ///
    /// Returns `None` only when we want to short-circuit early on the
    /// "user clearly not signed in" path. In the post-#1710 shape this
    /// is determined by attempting a factory resolve via
    /// [`composio_host::is_available`] and treating a `false` there as
    /// "skip silently" — the same UX as the pre-fix
    /// `build_composio_client(...).is_some()` probe, but routed
    /// through the mode-aware factory so direct-mode users (no backend
    /// session token, BYO key in keychain) aren't falsely treated as
    /// signed-out.
    pub fn from_config(
        config: Arc<Config>,
        toolkit: impl Into<String>,
        connection_id: Option<String>,
    ) -> Option<Self> {
        // Probe the factory: any successful resolve (Backend OR Direct)
        // means the user has *some* viable Composio client. Direct-mode
        // users typically have no backend session token, which would
        // make a `build_composio_client` probe return None and falsely
        // skip them.
        if composio_host::is_available(&*config) {
            Some(Self {
                config,
                toolkit: toolkit.into(),
                connection_id,
                usage: ComposioUsageHandle::default(),
                max_items: None,
                sync_depth_days: None,
            })
        } else {
            tracing::debug!(
                "[composio:provider_context] from_config: no viable Composio client; \
                 treating as not-signed-in"
            );
            None
        }
    }

    /// Resolve the underlying composio client via the mode-aware
    /// factory and dispatch a single action. This is the canonical
    /// way for provider implementations to execute a Composio action
    /// — going through here ensures the live `composio.mode` toggle is
    /// honoured on every call (#1710).
    ///
    /// Returns the same [`ComposioExecuteResponse`] shape that
    /// `ComposioClient::execute_tool` used to return so existing
    /// provider call-sites can swap `ctx.client.execute_tool(...)` for
    /// `ctx.execute(...)` with no other changes.
    pub async fn execute(
        &self,
        action: &str,
        arguments: Option<serde_json::Value>,
    ) -> anyhow::Result<ComposioExecuteResponse> {
        // [#1710 Wave 4] Reload config fresh per execute so a mid-session
        // `composio.mode` toggle takes effect at the very next call. The
        // Arc<Config> snapshot held by `self` was taken at agent-init time
        // and is otherwise stale relative to subsequent set_api_key /
        // clear_api_key RPCs.
        //
        // Use `reload_config_snapshot_with_timeout` (anchored to the snapshot's
        // `config_path`) rather than `load_config_with_timeout` (which
        // re-resolves `OPENHUMAN_WORKSPACE` from the process env). The config
        // path is stable for the lifetime of a `ProviderContext` — it is set
        // at context creation from the agent's scoped config — so reading from
        // it always reaches the correct user workspace and avoids a data-race
        // in tests that share the process env.
        let live_config = config_rpc::reload_config_snapshot_with_timeout(&*self.config)
            .await
            .map_err(|e| {
                tracing::warn!(
                    action = %action,
                    toolkit = %self.toolkit,
                    error = %e,
                    "[composio:provider_context] execute: reload_config failed"
                );
                anyhow::anyhow!("composio provider_context: failed to reload live config: {e}")
            })?;
        // Mode dispatch (backend tenant vs the user's own direct v3 tenant)
        // lives in the host's `ComposioHost` impl — this side just asks.
        let result = composio_host::execute(
            &*live_config,
            action,
            arguments,
            &live_config.composio().entity_id,
            self.connection_id.as_deref(),
        )
        .await
        .map_err(|e| anyhow::anyhow!(e));

        // Tally billable-action usage at the single chokepoint every provider
        // routes through (#3111). We count any *completed* round-trip — even a
        // provider-reported failure (`successful == false`) is a billable call
        // — and sum the backend-reported `cost_usd`. Transport errors (the
        // `Err` arm) never reached Composio, so they don't count. The lock is
        // held only for the increment, never across an `.await`.
        if let Ok(ref resp) = result {
            if let Ok(mut usage) = self.usage.lock() {
                usage.actions_called = usage.actions_called.saturating_add(1);
                usage.cost_usd += resp.cost_usd;
            }
        }
        result
    }

    /// Memory client handle if the global memory singleton is ready.
    /// Used by providers that want to persist sync snapshots.
    ///
    /// Under `cfg(test)` the global singleton is not booted, so build a
    /// workspace-scoped client directly instead.
    /// Memory client handle if the global memory singleton is ready.
    /// Used by providers that want to persist sync snapshots.
    #[cfg(not(test))]
    pub fn memory_client(&self) -> Option<crate::store::MemoryClientRef> {
        crate::global::client_if_ready()
    }
}

#[cfg(test)]
#[path = "types_test_support.rs"]
mod test_support;

#[cfg(test)]
#[path = "types_tests.rs"]
mod tests;
