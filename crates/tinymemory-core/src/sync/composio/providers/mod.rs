//! Provider-specific code for Composio toolkits.
//!
//! Each Composio toolkit (gmail, notion, slack, …) can register a
//! [`ComposioProvider`] implementation that knows how to:
//!
//!   * Fetch a normalized **user profile** for a connected account.
//!   * Run an **initial / periodic sync** that pulls fresh data from the
//!     upstream service via the backend-proxied
//!     `ComposioClient`.
//!   * React to **trigger webhooks** that arrive over the
//!     `composio:trigger` Socket.IO bridge.
//!   * React to **OAuth handoff completion** so the very first sync can
//!     run as soon as a user connects an account.
//!
//! Providers are pure Rust — there is no JS sandbox involved. They are
//! the native counterpart to the QuickJS skill bundles in
//! `tinyhumansai/openhuman-skills`, but specialized for Composio's API
//! surface and run inside the core process directly.
//!
//! ## Registry & dispatch
//!
//! The [`registry`] module owns a process-global `HashMap<toolkit_slug,
//! Arc<dyn ComposioProvider>>`. The composio event bus subscriber
//! (`super::bus::ComposioTriggerSubscriber`) and the periodic sync
//! task both look up providers by toolkit slug and call into them.
//!
//! ## Why a trait, not a giant `match`
//!
//! Each provider has provider-specific shapes (gmail returns
//! emailAddress + messagesTotal, notion returns workspaces + pages, …)
//! and a different idea of what "sync" means. A trait keeps each
//! provider's implementation isolated, individually testable, and
//! easy to add without touching the dispatch layer.

mod descriptions;
pub(crate) mod helpers;
mod scope_lookup;
pub mod tool_scope;
mod traits;
mod types;
pub mod user_scopes;

pub mod catalogs;
pub mod catalogs_business;
pub mod catalogs_google;
pub mod catalogs_messaging;
pub mod catalogs_microsoft;
pub mod catalogs_productivity;
pub mod catalogs_social_media;
pub mod clickup;
pub mod github;
pub mod gmail;
pub mod linear;
pub mod notion;
pub mod profile;
pub mod profile_md;
pub mod registry;
pub mod slack;
pub mod sync_state;

use crate::composio_host::ComposioCapability;

const CAPABILITY_TOOLKITS: &[&str] = &[
    "gmail",
    "notion",
    "slack",
    "clickup",
    "github",
    "discord",
    "googlecalendar",
    "googledrive",
    "googledocs",
    "googlesheets",
    "outlook",
    "microsoft_teams",
    "linear",
    "jira",
    "trello",
    "asana",
    "dropbox",
    "twitter",
    "spotify",
    "telegram",
    "whatsapp",
    "shopify",
    "stripe",
    "hubspot",
    "salesforce",
    "airtable",
    "figma",
    "youtube",
    "one_drive",
    "excel",
    "todoist",
];

fn native_provider_sync_interval(toolkit: &str) -> Option<u64> {
    match toolkit {
        "gmail" => Some(gmail::GmailProvider::new().sync_interval_secs()),
        "notion" => Some(notion::NotionProvider::new().sync_interval_secs()),
        "slack" => Some(slack::SlackProvider::new().sync_interval_secs()),
        "clickup" => Some(clickup::ClickUpProvider::new().sync_interval_secs()),
        "github" => Some(github::GitHubProvider::new().sync_interval_secs()),
        "linear" => Some(linear::LinearProvider::new().sync_interval_secs()),
        _ => None,
    }
    .flatten()
}

fn has_native_provider(toolkit: &str) -> bool {
    matches!(
        toolkit,
        "gmail" | "notion" | "slack" | "clickup" | "github" | "linear"
    )
}

/// Static overview of the Composio integrations supported by this core build.
///
/// This deliberately does not consult the live Composio backend/direct tenant:
/// it is an observability surface for OpenHuman's own capability tiers. Use
/// `composio_list_toolkits` / `composio_list_connections` when callers need
/// the currently signed-in user's allowlist or OAuth state.
pub fn capability_matrix() -> Vec<ComposioCapability> {
    CAPABILITY_TOOLKITS
        .iter()
        .map(|toolkit| {
            let native_provider = has_native_provider(toolkit);
            let catalog = catalog_for_toolkit(toolkit);
            let sync_interval_secs = native_provider_sync_interval(toolkit);
            ComposioCapability {
                toolkit: (*toolkit).to_string(),
                description: toolkit_description(toolkit).to_string(),
                native_provider,
                curated_tools: catalog.is_some(),
                curated_tool_count: catalog.map_or(0, <[CuratedTool]>::len),
                tool_execution: catalog.is_some(),
                user_profile: native_provider,
                initial_sync: native_provider,
                periodic_sync: sync_interval_secs.is_some(),
                sync_interval_secs,
                trigger_webhooks: native_provider,
                memory_ingest: native_provider,
            }
        })
        .collect()
}

/// Static toolkit → curated catalog map.
///
/// This is consulted by the meta-tool layer alongside any registered
/// provider's [`ComposioProvider::curated_tools`]. It lets toolkits
/// without a full native provider still benefit from curated
/// whitelisting.
///
/// Lookup key is the lowercased prefix returned by
/// [`toolkit_from_slug`] applied to the action slug — e.g.
/// `GOOGLECALENDAR_CREATE_EVENT` → `"googlecalendar"`. Multi-segment
/// prefixes like `MICROSOFT_TEAMS_*` return their known toolkit slug.
/// Synchronous visibility check for a Composio action slug given a
/// pre-loaded user scope preference.
///
/// Returns `true` if the action should appear in the agent's tool
/// surface — i.e. it's in the toolkit's curated whitelist (or the
/// toolkit has no curation) **and** the user's scope pref allows its
/// classification. Falls back to [`classify_unknown`] for un-curated
/// toolkits.
///
/// Use this when the user pref has already been loaded for the
/// toolkit (typical inside a `for slug in toolkits {...}` loop where
/// awaiting once per toolkit is cheaper than once per action).
pub fn is_action_visible_with_pref(slug: &str, pref: &UserScopePref) -> bool {
    let Some(toolkit) = toolkit_from_slug(slug) else {
        return true;
    };
    let catalog = get_provider(&toolkit)
        .and_then(|p| p.curated_tools())
        .or_else(|| catalog_for_toolkit(&toolkit));
    match catalog {
        Some(catalog) => match find_curated(catalog, slug) {
            Some(curated) => pref.allows(curated.scope),
            None => false,
        },
        None => pref.allows(classify_unknown(slug)),
    }
}

pub fn catalog_for_toolkit(toolkit: &str) -> Option<&'static [CuratedTool]> {
    match toolkit.trim().to_ascii_lowercase().as_str() {
        // Native providers
        "gmail" => Some(gmail::GMAIL_CURATED),
        "notion" => Some(notion::NOTION_CURATED),
        "github" => Some(github::GITHUB_CURATED),
        "linear" => Some(linear::LINEAR_CURATED),
        // Catalog-only toolkits
        "slack" => Some(catalogs::SLACK_CURATED),
        "discord" => Some(catalogs::DISCORD_CURATED),
        "googlecalendar" | "google_calendar" => Some(catalogs::GOOGLECALENDAR_CURATED),
        "googledrive" | "google_drive" => Some(catalogs::GOOGLEDRIVE_CURATED),
        "googledocs" | "google_docs" => Some(catalogs::GOOGLEDOCS_CURATED),
        "googlesheets" | "google_sheets" => Some(catalogs::GOOGLESHEETS_CURATED),
        "outlook" => Some(catalogs::OUTLOOK_CURATED),
        // Keep the legacy "microsoft" alias while toolkit_from_slug now
        // returns the precise "microsoft_teams" slug for Teams actions.
        "microsoft" | "microsoft_teams" => Some(catalogs::MICROSOFT_TEAMS_CURATED),
        "jira" => Some(catalogs::JIRA_CURATED),
        "trello" => Some(catalogs::TRELLO_CURATED),
        "asana" => Some(catalogs::ASANA_CURATED),
        "clickup" => Some(clickup::CLICKUP_CURATED),
        "dropbox" => Some(catalogs::DROPBOX_CURATED),
        "twitter" => Some(catalogs::TWITTER_CURATED),
        "spotify" => Some(catalogs::SPOTIFY_CURATED),
        "telegram" => Some(catalogs::TELEGRAM_CURATED),
        "whatsapp" => Some(catalogs::WHATSAPP_CURATED),
        "shopify" => Some(catalogs::SHOPIFY_CURATED),
        "stripe" => Some(catalogs::STRIPE_CURATED),
        "hubspot" => Some(catalogs::HUBSPOT_CURATED),
        "salesforce" => Some(catalogs::SALESFORCE_CURATED),
        "airtable" => Some(catalogs::AIRTABLE_CURATED),
        "figma" => Some(catalogs::FIGMA_CURATED),
        "youtube" => Some(catalogs::YOUTUBE_CURATED),
        // ONE_DRIVE_* slugs extract to "one" via toolkit_from_slug;
        // alias both the prefix and the canonical UI/backend slugs.
        "one" | "one_drive" | "onedrive" => Some(catalogs::ONE_DRIVE_CURATED),
        "excel" => Some(catalogs::EXCEL_CURATED),
        "todoist" => Some(catalogs::TODOIST_CURATED),
        _ => None,
    }
}

/// All toolkit slugs that have a curated agent-ready catalog.
///
/// Source of truth for the UI "preview / agent integration coming
/// soon" badge: any connected toolkit whose slug is NOT in this list
/// can be authorized but lacks a curated tool surface, so the agent
/// can't use it productively.
///
/// Returned in sorted order to keep the RPC response stable across
/// builds.
pub fn agent_ready_toolkits() -> Vec<&'static str> {
    let mut slugs: Vec<&'static str> = vec![
        // Native providers
        "gmail",
        "notion",
        "github",
        // Catalog-only toolkits
        "slack",
        "discord",
        "googlecalendar",
        "googledrive",
        "googledocs",
        "googlesheets",
        "outlook",
        "microsoft_teams",
        "linear",
        "jira",
        "trello",
        "asana",
        "dropbox",
        "twitter",
        "spotify",
        "telegram",
        "whatsapp",
        "shopify",
        "stripe",
        "hubspot",
        "salesforce",
        "airtable",
        "figma",
        "youtube",
        "one_drive",
        "excel",
        "todoist",
    ];
    slugs.sort_unstable();
    slugs
}

pub use descriptions::toolkit_description;
pub(crate) use helpers::{first_array_str, merge_extra};
// `pick_str` is a provider payload normaliser and lives in tinycortex; it is
// re-exported here so the ~40 in-tree call sites keep resolving unchanged.
// Note this is deliberately NOT `providers::common::pick_str`, which coerces
// numbers to strings — see the doc comments on both definitions.
pub use registry::{
    all_providers, get_provider, init_default_providers, register_provider, ProviderArc,
};
pub use scope_lookup::{curated_scope_for, toolkit_has_scope};
pub(crate) use tinymemory_sync::helpers::pick_str;
pub use tool_scope::{classify_unknown, find_curated, toolkit_from_slug, CuratedTool, ToolScope};
pub use traits::{resolve_sync_interval_secs, sync_interval_env_var, ComposioProvider};
pub use types::{
    ComposioUsage, ComposioUsageHandle, GithubFetchMode, NormalizedTask, ProviderContext,
    ProviderUserProfile, SyncOutcome, SyncReason, TaskContainer, TaskFetchFilter, TaskKind,
};
pub use user_scopes::{load_or_default as load_user_scope_or_default, UserScopePref};

#[cfg(test)]
#[path = "providers_tests.rs"]
mod tests;
