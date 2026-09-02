//! The curated Composio tool catalogs, and the lookups over them.
//!
//! Composio publishes 60+ actions per toolkit; most are noise for an agent's
//! planning loop. Each toolkit gets a hand-curated `&'static [CuratedTool]`
//! slice that pares the surface down to a useful subset and tags every action
//! with a [`ToolScope`], so per-user scope preferences can gate execution.
//!
//! # Why the catalogs are here and not in the engine crate
//!
//! [`super`]'s module docs used to end with "the curated catalogs and the
//! provider registry ... are the engine's", and the catalogs half of that is no
//! longer true. The registry still is — it is a process-global map of trait
//! objects that reach `reqwest` and the chunk store, and none of that may enter
//! this crate.
//!
//! The catalogs are the opposite: several thousand `&'static str` action slugs
//! and a `match` over them, with no dependency of any kind. And the *host* is
//! their heaviest reader — it filters the agent's visible tool list, renders
//! the `gated_tools` unlock hints, and decides which connected toolkits get the
//! "agent-ready" badge. While they lived in the engine crate, every one of
//! those host reads was a compile-time link to `tinymemory-core`, which is the
//! link OpenHuman#5560 removes. Same argument [`super::scopes`] already makes
//! for the verdict functions: the *same answer* has to be reachable on both
//! sides of the module boundary, so the data has to be nameable from the
//! contract.
//!
//! # `get_provider(..).curated_tools()` is not a separate source
//!
//! The lookups here consult [`catalog_for_toolkit`] alone. The engine's
//! versions walked the registered provider's `curated_tools()` first and fell
//! back to the static map, which read as two sources of truth; it was one.
//! Every native provider's `curated_tools()` returns exactly the slice
//! `catalog_for_toolkit` returns for the same toolkit — verified against all
//! six (`gmail`, `notion`, `github`, `linear`, `clickup`, `slack`) — so the
//! provider hop was pure indirection, and dropping it is what lets a host
//! answer "may this action run" without a provider registry at all.

pub mod business;
pub mod clickup;
pub mod descriptions;
pub mod github;
pub mod gmail;
pub mod google;
pub mod linear;
pub mod messaging;
pub mod microsoft;
pub mod notion;
pub mod productivity;
pub mod social_media;

use super::scopes::{
    classify_unknown, find_curated, toolkit_from_slug, CuratedTool, ToolScope, UserScopePref,
};

pub use descriptions::{toolkit_description, toolkit_result_notes};

/// Every toolkit the capability surface reports on, in display order.
pub const CAPABILITY_TOOLKITS: &[&str] = &[
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

/// Toolkits with a native `ComposioProvider` in the engine, and the
/// compile-time default periodic-sync interval each one ships.
///
/// The provider impls are the engine's, but *which* toolkits have one and how
/// often they run are facts a host reports in its capability surface, so the
/// table is contract data. Each provider still calls its own
/// `resolve_sync_interval_secs` with the same default, and
/// `native_provider_sync_interval_secs` below resolves the identical env
/// override — so the two cannot disagree without this table being edited.
pub const NATIVE_PROVIDERS: &[(&str, u64)] = &[
    ("gmail", 15 * 60),
    ("notion", 30 * 60),
    ("slack", 15 * 60),
    ("clickup", 30 * 60),
    ("github", 30 * 60),
    ("linear", 30 * 60),
];

/// Does `toolkit` have a native provider implementation?
#[must_use]
pub fn has_native_provider(toolkit: &str) -> bool {
    NATIVE_PROVIDERS.iter().any(|(slug, _)| *slug == toolkit)
}

/// The env var read to override a toolkit's periodic sync interval.
///
/// Exposed so tests and `.env.example` stay in lockstep with the runtime
/// lookup without re-implementing the casing.
#[must_use]
pub fn sync_interval_env_var(toolkit: &str) -> String {
    format!(
        "OPENHUMAN_COMPOSIO_{}_SYNC_INTERVAL_SECS",
        toolkit.to_ascii_uppercase()
    )
}

/// Apply a raw env-var override to a default interval.
///
/// Split out from the env read so both sides can share the rule and differ on
/// what they do about a bad value: a provider warns once, an observability read
/// stays silent. `0` is never honoured — it would burn the scheduler in a tight
/// loop — so a non-positive or unparseable value yields `None` and the caller
/// keeps its default.
#[must_use]
pub fn parse_sync_interval_override(raw: &str) -> Option<u64> {
    raw.trim().parse::<u64>().ok().filter(|n| *n >= 1)
}

/// The effective periodic sync interval for a native provider, honouring the
/// `OPENHUMAN_COMPOSIO_<TOOLKIT>_SYNC_INTERVAL_SECS` override.
///
/// `None` for a toolkit with no native provider.
#[must_use]
pub fn native_provider_sync_interval_secs(toolkit: &str) -> Option<u64> {
    let default_secs = NATIVE_PROVIDERS
        .iter()
        .find(|(slug, _)| *slug == toolkit)
        .map(|(_, secs)| *secs)?;
    let resolved = std::env::var(sync_interval_env_var(toolkit))
        .ok()
        .and_then(|raw| parse_sync_interval_override(&raw))
        .unwrap_or(default_secs);
    Some(resolved)
}

/// Static toolkit → curated catalog map.
///
/// The lookup key is the lowercased prefix [`toolkit_from_slug`] returns for an
/// action slug — `GOOGLECALENDAR_CREATE_EVENT` → `"googlecalendar"`.
/// Multi-segment prefixes like `MICROSOFT_TEAMS_*` return their known toolkit
/// slug.
#[must_use]
pub fn catalog_for_toolkit(toolkit: &str) -> Option<&'static [CuratedTool]> {
    match toolkit.trim().to_ascii_lowercase().as_str() {
        // Toolkits with a native provider. Each provider's `curated_tools()`
        // returns this same slice.
        "gmail" => Some(gmail::GMAIL_CURATED),
        "notion" => Some(notion::NOTION_CURATED),
        "github" => Some(github::GITHUB_CURATED),
        "linear" => Some(linear::LINEAR_CURATED),
        "clickup" => Some(clickup::CLICKUP_CURATED),
        "slack" => Some(messaging::SLACK_CURATED),
        // Catalog-only toolkits.
        "discord" => Some(messaging::DISCORD_CURATED),
        "googlecalendar" | "google_calendar" => Some(google::GOOGLECALENDAR_CURATED),
        "googledrive" | "google_drive" => Some(google::GOOGLEDRIVE_CURATED),
        "googledocs" | "google_docs" => Some(google::GOOGLEDOCS_CURATED),
        "googlesheets" | "google_sheets" => Some(google::GOOGLESHEETS_CURATED),
        "outlook" => Some(productivity::OUTLOOK_CURATED),
        // The legacy "microsoft" alias stays while `toolkit_from_slug` returns
        // the precise "microsoft_teams" slug for Teams actions.
        "microsoft" | "microsoft_teams" => Some(messaging::MICROSOFT_TEAMS_CURATED),
        "jira" => Some(productivity::JIRA_CURATED),
        "trello" => Some(productivity::TRELLO_CURATED),
        "asana" => Some(productivity::ASANA_CURATED),
        "dropbox" => Some(productivity::DROPBOX_CURATED),
        "twitter" => Some(social_media::TWITTER_CURATED),
        "spotify" => Some(social_media::SPOTIFY_CURATED),
        "telegram" => Some(messaging::TELEGRAM_CURATED),
        "whatsapp" => Some(messaging::WHATSAPP_CURATED),
        "shopify" => Some(business::SHOPIFY_CURATED),
        "stripe" => Some(business::STRIPE_CURATED),
        "hubspot" => Some(business::HUBSPOT_CURATED),
        "salesforce" => Some(business::SALESFORCE_CURATED),
        "airtable" => Some(business::AIRTABLE_CURATED),
        "figma" => Some(business::FIGMA_CURATED),
        "youtube" => Some(social_media::YOUTUBE_CURATED),
        // `ONE_DRIVE_*` slugs extract to "one" via `toolkit_from_slug`; alias
        // both the prefix and the canonical UI/backend slugs.
        "one" | "one_drive" | "onedrive" => Some(microsoft::ONE_DRIVE_CURATED),
        "excel" => Some(microsoft::EXCEL_CURATED),
        "todoist" => Some(productivity::TODOIST_CURATED),
        _ => None,
    }
}

/// Should this action slug appear in the agent's tool surface, given an
/// already-loaded user scope preference?
///
/// `true` when the action is in its toolkit's curated whitelist (or the toolkit
/// has no curation) **and** the preference allows its classification. Falls
/// back to [`classify_unknown`] for uncurated toolkits.
///
/// Takes a pre-loaded preference because the typical caller loops over
/// toolkits, where awaiting once per toolkit is cheaper than once per action.
#[must_use]
pub fn is_action_visible_with_pref(slug: &str, pref: &UserScopePref) -> bool {
    let Some(toolkit) = toolkit_from_slug(slug) else {
        return true;
    };
    match catalog_for_toolkit(&toolkit) {
        Some(catalog) => match find_curated(catalog, slug) {
            Some(curated) => pref.allows(curated.scope),
            None => false,
        },
        None => pref.allows(classify_unknown(slug)),
    }
}

/// The curated scope `slug` requires, if it appears in any catalog.
///
/// `None` for a genuinely uncurated slug — a caller wanting a defensible
/// heuristic for those should reach for [`classify_unknown`] explicitly.
///
/// Sibling of [`is_action_visible_with_pref`]: that one answers "visible?",
/// this one answers "what scope is required?", so a caller can render an unlock
/// hint without redoing the catalog walk.
#[must_use]
pub fn curated_scope_for(slug: &str) -> Option<ToolScope> {
    let toolkit = toolkit_from_slug(slug)?;
    let catalog = catalog_for_toolkit(&toolkit)?;
    find_curated(catalog, slug).map(|c| c.scope)
}

/// Does any curated action for `toolkit` require `scope`?
///
/// Useful whenever the question is "would flipping the {scope} bit unlock
/// anything here?" — a UI hint that greys out a toggle with no effect.
#[must_use]
pub fn toolkit_has_scope(toolkit: &str, scope: ToolScope) -> bool {
    catalog_for_toolkit(toolkit).is_some_and(|cat| cat.iter().any(|t| t.scope == scope))
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
