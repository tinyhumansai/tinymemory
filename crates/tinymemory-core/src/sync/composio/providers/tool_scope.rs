//! Per-action scope classification (read / write / admin) plus the
//! [`CuratedTool`] catalog type that providers use to whitelist the
//! actions they want surfaced to the agent.
//!
//! Composio publishes 60+ actions per toolkit; most are noise for the
//! agent's planning loop. Each provider exports a hand-curated
//! [`CuratedTool`] slice via [`super::ComposioProvider::curated_tools`]
//! that pares the surface down to a useful subset and tags every action
//! with a [`ToolScope`] so per-user scope preferences can gate execution.

use serde::{Deserialize, Serialize};

/// Classification of how invasive an action is.
///
/// Used both to filter the agent's visible tool list and to enforce
/// per-user scope preferences at execution time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolScope {
    /// Pure reads — `GET` / `FETCH` / `LIST` / `SEARCH` / `GET_PROFILE`.
    Read,
    /// Side-effectful actions that create or mutate user data —
    /// `SEND` / `CREATE` / `UPDATE` / `REPLY` / `APPEND`.
    Write,
    /// Destructive or permission-changing actions — `DELETE` / `TRASH` /
    /// `REMOVE` / `MODIFY_LABELS` / `SHARE`.
    Admin,
}

impl ToolScope {
    pub fn as_str(self) -> &'static str {
        match self {
            ToolScope::Read => "read",
            ToolScope::Write => "write",
            ToolScope::Admin => "admin",
        }
    }
}

/// One curated entry in a provider's tool catalog.
///
/// `slug` is the Composio action slug as returned by `composio_list_tools`
/// (e.g. `"GMAIL_SEND_EMAIL"`). `scope` controls whether the action is
/// gated by the user's read / write / admin preference.
#[derive(Debug, Clone, Copy)]
pub struct CuratedTool {
    pub slug: &'static str,
    pub scope: ToolScope,
}

/// Heuristic fallback when we need to gate a tool that isn't in any
/// provider's curated list. Prefer the curated classification when
/// available; only call this when [`super::ComposioProvider::curated_tools`]
/// returned `None` or didn't include the slug.
pub fn classify_unknown(slug: &str) -> ToolScope {
    let upper = slug.to_ascii_uppercase();
    // Admin verbs are checked first so e.g. `MODIFY_LABELS` doesn't slip
    // into the Write bucket on the `UPDATE`-substring rule.
    const ADMIN: &[&str] = &[
        "DELETE",
        "TRASH",
        "REMOVE",
        "MODIFY_LABELS",
        "SHARE",
        "REVOKE",
        "DESTROY",
    ];
    const WRITE: &[&str] = &[
        "SEND", "CREATE", "UPDATE", "REPLY", "APPEND", "INSERT", "ADD", "POST", "PATCH", "WRITE",
        "DRAFT",
    ];
    if ADMIN.iter().any(|kw| upper.contains(kw)) {
        return ToolScope::Admin;
    }
    if WRITE.iter().any(|kw| upper.contains(kw)) {
        return ToolScope::Write;
    }
    ToolScope::Read
}

/// Look up a slug inside a curated catalog.
pub fn find_curated<'a>(catalog: &'a [CuratedTool], slug: &str) -> Option<&'a CuratedTool> {
    catalog.iter().find(|t| t.slug.eq_ignore_ascii_case(slug))
}

/// Extract the toolkit slug from a Composio action slug.
///
/// Most Composio action slugs follow `<TOOLKIT>_<VERB>_…`
/// (e.g. `GMAIL_SEND_EMAIL` → `gmail`). A few toolkit identifiers contain
/// underscores themselves; those need known-prefix handling so connected
/// toolkit checks do not drop actions such as `ZOHO_MAIL_*`.
pub fn toolkit_from_slug(slug: &str) -> Option<String> {
    let trimmed = slug.trim();
    if trimmed.is_empty() {
        return None;
    }
    const MULTI_SEGMENT_TOOLKIT_PREFIXES: &[(&str, &str)] = &[
        ("MICROSOFT_TEAMS_", "microsoft_teams"),
        ("ONE_DRIVE_", "one_drive"),
        ("ZOHO_MAIL_", "zoho_mail"),
    ];
    let upper = trimmed.to_ascii_uppercase();
    for (prefix, toolkit) in MULTI_SEGMENT_TOOLKIT_PREFIXES {
        if upper.starts_with(prefix) {
            return Some((*toolkit).to_string());
        }
    }
    let prefix = trimmed.split('_').next()?;
    if prefix.is_empty() {
        None
    } else {
        Some(prefix.to_ascii_lowercase())
    }
}

#[cfg(test)]
#[path = "tool_scope_tests.rs"]
mod tests;
