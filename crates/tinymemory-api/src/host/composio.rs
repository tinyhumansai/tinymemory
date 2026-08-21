//! Composio value types — connections, capabilities, execute responses.
//!
//! Moved here from the host's `integrations::composio::types` because the
//! extracted memory sync pipelines read these fields directly on every run, and
//! a trait accessor per field would be absurd. They are inert serde data with
//! no behaviour and no dependencies beyond `serde`, so the contract crate's
//! dependency-light guarantee is unaffected.
//!
//! The Composio *client* deliberately did not come with them — see
//! `tinymemory_core::composio_host`. Its `Direct` variant wraps a host agent
//! tool, and mode dispatch is host policy.
//!
//! Domain types for the Composio integration.
//!
//! These mirror the response envelopes emitted by the openhuman backend under
//! `/agent-integrations/composio/*`. See:
//!   - `src/routes/agentIntegrations/composio.ts`
//!   - `src/controllers/agentIntegrations/composio/*.ts`
//!     in the backend repo for the authoritative shapes.

use serde::{Deserialize, Deserializer, Serialize};

/// Accepts either a JSON string or an object whose first matching field
/// (`slug`/`id`/`name`/`key`) is a string. Lets us tolerate upstream
/// shape drift where a previously-stringy field is now nested in an
/// object — e.g. `"toolkit": {"slug": "gmail", "logo": "…"}`.
fn de_string_or_object<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    use serde::de::Error;
    let v = serde_json::Value::deserialize(d)?;
    match v {
        serde_json::Value::String(s) => Ok(s),
        serde_json::Value::Object(map) => {
            for key in ["slug", "id", "name", "key"] {
                if let Some(serde_json::Value::String(s)) = map.get(key) {
                    return Ok(s.clone());
                }
            }
            Err(D::Error::custom(
                "expected string or object with slug/id/name/key field",
            ))
        }
        other => Err(D::Error::custom(format!(
            "expected string, got {}",
            match other {
                serde_json::Value::Null => "null",
                serde_json::Value::Bool(_) => "bool",
                serde_json::Value::Number(_) => "number",
                serde_json::Value::Array(_) => "array",
                _ => "unknown",
            }
        ))),
    }
}

/// Like [`de_string_or_object`] but optional and resilient: missing /
/// null / unrecognized object shapes return `None` instead of erroring.
fn de_opt_string_or_object<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    let v = Option::<serde_json::Value>::deserialize(d)?;
    Ok(match v {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(s)) => Some(s),
        Some(serde_json::Value::Object(map)) => {
            let mut found = None;
            for key in ["state", "value", "slug", "id", "name", "key"] {
                if let Some(serde_json::Value::String(s)) = map.get(key) {
                    found = Some(s.clone());
                    break;
                }
            }
            found
        }
        _ => None,
    })
}

// ── Toolkits ────────────────────────────────────────────────────────

/// One toolkit from the live Composio catalog, forwarded verbatim from the
/// backend (`GET /agent-integrations/composio/toolkits`).
///
/// The core does not interpret these fields — it passes them straight through
/// to the desktop UI so the app no longer hardcodes toolkit display metadata
/// (see the workspace `COMPOSIO_DYNAMIC_CATALOG_PLAN.md`). Everything except
/// `slug` is best-effort; backends predating the dynamic catalog omit the
/// whole `catalog` array, in which case the UI falls back to local metadata.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ComposioToolkitCatalogEntry {
    /// Toolkit slug as Composio emits it, e.g. `"googlecalendar"`.
    pub slug: String,
    /// Human-readable name, e.g. `"Google Calendar"`.
    #[serde(default)]
    pub name: String,
    /// Composio-hosted logo URL (`meta.logo`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logo: Option<String>,
    /// Short description (`meta.description`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Composio category names (`meta.categories`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub categories: Vec<String>,
    /// Whether the user can connect/use this toolkit (passed the backend gate).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

/// Response body of `GET /agent-integrations/composio/toolkits`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ComposioToolkitsResponse {
    /// Server-enforced toolkit allowlist, e.g. `["gmail", "notion"]`.
    #[serde(default)]
    pub toolkits: Vec<String>,
    /// Rich render model from the live Composio catalog. Optional — empty when
    /// the backend predates the dynamic catalog. Forwarded as-is to the UI.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub catalog: Vec<ComposioToolkitCatalogEntry>,
}

/// One row in OpenHuman's local Composio capability matrix.
///
/// Unlike `ComposioToolkitsResponse`, this is not tied to a signed-in
/// backend/direct Composio session. It describes what this core build knows
/// how to do for each toolkit: whether the toolkit has a native provider
/// implementation, a curated tool catalog, profile/sync hooks, and memory
/// ingestion support.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposioCapability {
    pub toolkit: String,
    pub description: String,
    pub native_provider: bool,
    pub curated_tools: bool,
    pub curated_tool_count: usize,
    pub tool_execution: bool,
    pub user_profile: bool,
    pub initial_sync: bool,
    pub periodic_sync: bool,
    pub sync_interval_secs: Option<u64>,
    pub trigger_webhooks: bool,
    pub memory_ingest: bool,
}

/// Response body of `composio.list_capabilities`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ComposioCapabilitiesResponse {
    #[serde(default)]
    pub capabilities: Vec<ComposioCapability>,
}

/// Response body of `composio.list_agent_ready_toolkits`.
///
/// Sorted slugs that have a curated agent catalog — the frontend
/// uses this to decide whether to label a connected toolkit as
/// "preview / agent integration coming soon". See #2283.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ComposioAgentReadyToolkitsResponse {
    #[serde(default)]
    pub toolkits: Vec<String>,
}

// ── Connections ─────────────────────────────────────────────────────

/// One connected Composio account (OAuth integration instance).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposioConnection {
    /// Composio connection id (what you DELETE to disconnect).
    pub id: String,
    /// Toolkit slug, e.g. `"gmail"`.
    pub toolkit: String,
    /// Connection status — `"ACTIVE"`, `"CONNECTED"`, `"PENDING"`, …
    pub status: String,
    /// ISO timestamp (backend passes this through from Composio).
    #[serde(rename = "createdAt", default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    /// Account email — populated from the cached provider profile when
    /// the toolkit reports an email address (e.g. Gmail, Google Calendar,
    /// Google Sheets). Lets the UI picker show "Gmail · user@example.com"
    /// instead of a generic "Account N" label.
    #[serde(
        rename = "accountEmail",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub account_email: Option<String>,
    /// Workspace or team display name — populated for workspace-based
    /// services (e.g. Slack: user display name / team name, Notion: workspace
    /// name). Used by the picker when no email is available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// Screen name or handle — populated for username-based services
    /// (e.g. GitHub login, Twitter handle). Used by the picker as a
    /// last-resort identity hint after email and workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
}

impl ComposioConnection {
    /// Return the toolkit slug in the canonical form used by provider
    /// lookup, prompt injection, and tool-action prefix matching.
    pub fn normalized_toolkit(&self) -> String {
        self.toolkit.trim().to_ascii_lowercase()
    }

    /// Whether this row represents a usable connection.
    ///
    /// The web UI already treats status case-insensitively. Keep the
    /// core-side chat/runtime filters aligned so a backend spelling such
    /// as `connected` cannot display as connected in Settings while
    /// disappearing from the agent's integration surface.
    pub fn is_active(&self) -> bool {
        let status = self.status.trim();
        status.eq_ignore_ascii_case("ACTIVE") || status.eq_ignore_ascii_case("CONNECTED")
    }
}

/// Response body of `GET /agent-integrations/composio/connections`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ComposioConnectionsResponse {
    #[serde(default)]
    pub connections: Vec<ComposioConnection>,
}

/// Response body of `POST /agent-integrations/composio/authorize`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposioAuthorizeResponse {
    /// Composio-hosted OAuth URL the user opens in a browser.
    #[serde(rename = "connectUrl")]
    pub connect_url: String,
    /// Composio connection id created by this authorize call.
    #[serde(rename = "connectionId")]
    pub connection_id: String,
}

/// Response body of `DELETE /agent-integrations/composio/connections/:id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposioDeleteResponse {
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub memory_chunks_deleted: usize,
}

// ── Tools ───────────────────────────────────────────────────────────

/// OpenAI function-calling schema returned by the backend for each tool.
///
/// The backend wraps Composio's upstream shape; we keep the `type` +
/// `function` envelope so callers can forward directly into an LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposioToolSchema {
    #[serde(rename = "type", default = "default_function_type")]
    pub kind: String,
    pub function: ComposioToolFunction,
}

fn default_function_type() -> String {
    "function".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposioToolFunction {
    /// Composio action slug, e.g. `"GMAIL_SEND_EMAIL"`.
    pub name: String,
    /// Human-readable description shown to the model.
    #[serde(default)]
    pub description: Option<String>,
    /// JSON schema for the tool's INPUT parameters.
    #[serde(default)]
    pub parameters: Option<serde_json::Value>,
    /// JSON schema describing the tool's OUTPUT/return-value shape, when the
    /// upstream listing publishes one. Composio's v3 `/tools` endpoint calls
    /// this `output_parameters` — documented as "Schema definition of return
    /// values from the tool"
    /// (<https://docs.composio.dev/reference/api-reference/tools/getTools>) —
    /// alongside `input_parameters`. `None` means "unknown" (not "empty"):
    /// the backend-proxied `/agent-integrations/composio/tools` path is
    /// opaque to this crate and may not forward it, and not every Composio
    /// action publishes an output schema.
    #[serde(default)]
    pub output_parameters: Option<serde_json::Value>,
}

/// Response body of `GET /agent-integrations/composio/tools`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ComposioToolsResponse {
    #[serde(default)]
    pub tools: Vec<ComposioToolSchema>,
}

// ── Execute ─────────────────────────────────────────────────────────

/// Response body of `POST /agent-integrations/composio/execute`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposioExecuteResponse {
    /// Raw result from the upstream provider.
    #[serde(default)]
    pub data: serde_json::Value,
    /// Did the provider report success?
    #[serde(default)]
    pub successful: bool,
    /// Provider error message if any.
    #[serde(default)]
    pub error: Option<String>,
    /// Amount charged to the caller (base + margin) in USD.
    #[serde(rename = "costUsd", default)]
    pub cost_usd: f64,
    /// Backend-rendered compact markdown for known tools (set by
    /// backend PR tinyhumansai/backend#683). When present and non-empty
    /// callers should prefer this over `data` for LLM/CLI consumption.
    #[serde(rename = "markdownFormatted", default)]
    pub markdown_formatted: Option<String>,
}

// ── GitHub repos + triggers ─────────────────────────────────────────

/// One repository returned by `GET /agent-integrations/composio/github/repos`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposioGithubRepo {
    pub owner: String,
    pub repo: String,
    #[serde(rename = "fullName")]
    pub full_name: String,
    #[serde(default)]
    pub private: Option<bool>,
    #[serde(rename = "defaultBranch", default)]
    pub default_branch: Option<String>,
    #[serde(rename = "htmlUrl", default)]
    pub html_url: Option<String>,
}

/// Response body of `GET /agent-integrations/composio/github/repos`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposioGithubReposResponse {
    #[serde(rename = "connectionId")]
    pub connection_id: String,
    #[serde(default, rename = "repositories")]
    pub repositories: Vec<ComposioGithubRepo>,
}

/// Response body of `POST /agent-integrations/composio/triggers`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposioCreateTriggerResponse {
    #[serde(rename = "triggerId")]
    pub trigger_id: String,
    #[serde(default)]
    pub status: Option<String>,
}

// ── Trigger management (catalog + active list + enable/disable) ─────

/// Per-repo descriptor used by GitHub-scoped available triggers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposioAvailableTriggerRepo {
    pub owner: String,
    pub repo: String,
}

/// One entry in `GET /agent-integrations/composio/triggers/available`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposioAvailableTrigger {
    pub slug: String,
    /// `"static"` or `"github_repo"`.
    pub scope: String,
    #[serde(
        rename = "defaultConfig",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub default_config: Option<serde_json::Value>,
    #[serde(
        rename = "requiredConfigKeys",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub required_config_keys: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<ComposioAvailableTriggerRepo>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ComposioAvailableTriggersResponse {
    #[serde(default)]
    pub triggers: Vec<ComposioAvailableTrigger>,
}

/// One entry in `GET /agent-integrations/composio/triggers`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposioActiveTrigger {
    #[serde(deserialize_with = "de_string_or_object")]
    pub id: String,
    #[serde(deserialize_with = "de_string_or_object")]
    pub slug: String,
    #[serde(deserialize_with = "de_string_or_object")]
    pub toolkit: String,
    #[serde(rename = "connectionId", deserialize_with = "de_string_or_object")]
    pub connection_id: String,
    #[serde(
        rename = "triggerConfig",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub trigger_config: Option<serde_json::Value>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "de_opt_string_or_object"
    )]
    pub state: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ComposioActiveTriggersResponse {
    #[serde(default)]
    pub triggers: Vec<ComposioActiveTrigger>,
}

/// Response body of `POST /agent-integrations/composio/triggers` (enable).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposioEnableTriggerResponse {
    #[serde(rename = "triggerId")]
    pub trigger_id: String,
    pub slug: String,
    #[serde(rename = "connectionId")]
    pub connection_id: String,
}

/// Response body of `DELETE /agent-integrations/composio/triggers/:id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposioDisableTriggerResponse {
    #[serde(default)]
    pub deleted: bool,
}

// ── Triggers ────────────────────────────────────────────────────────

/// Payload of the `composio:trigger` Socket.IO event emitted by the backend
/// when a Composio webhook is received, HMAC-verified, and delivered to the
/// user's active sockets.
///
/// See `src/controllers/agentIntegrations/composio/handleWebhook.ts` in the
/// backend repo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposioTriggerEvent {
    /// Toolkit slug, e.g. `"gmail"`.
    #[serde(default)]
    pub toolkit: String,
    /// Trigger slug, e.g. `"GMAIL_NEW_GMAIL_MESSAGE"`.
    #[serde(default)]
    pub trigger: String,
    /// Trigger-specific payload (provider-defined shape).
    #[serde(default)]
    pub payload: serde_json::Value,
    /// Metadata the backend attaches: `{ id, uuid }`.
    #[serde(default)]
    pub metadata: ComposioTriggerMetadata,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ComposioTriggerMetadata {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub uuid: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposioTriggerHistoryEntry {
    /// Unix timestamp in milliseconds when the trigger reached the core.
    pub received_at_ms: u64,
    /// Toolkit slug, e.g. `"gmail"`.
    pub toolkit: String,
    /// Trigger slug, e.g. `"GMAIL_NEW_GMAIL_MESSAGE"`.
    pub trigger: String,
    /// Backend metadata id for this event.
    pub metadata_id: String,
    /// Backend metadata UUID for this event.
    pub metadata_uuid: String,
    /// Raw provider payload as forwarded by the backend socket event.
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposioTriggerHistoryResult {
    /// Directory containing daily JSONL archives.
    pub archive_dir: String,
    /// Today's JSONL file path.
    pub current_day_file: String,
    /// Recent triggers, newest first.
    pub entries: Vec<ComposioTriggerHistoryEntry>,
}

#[cfg(test)]
#[path = "composio_tests.rs"]
mod tests;
