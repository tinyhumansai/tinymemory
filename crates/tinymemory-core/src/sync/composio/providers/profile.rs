//! Profile persistence — maps [`ProviderUserProfile`] (and provider-specific
//! `extras`) into [`IdentityKind`]-tagged facet rows so the self-identity
//! matcher can join directly against the memory tree's `EntityKind` and the
//! structural sender field on chunks.
//!
//! Schema: `user_profile.facet_type='skill'`,
//! `key = "skill:{toolkit}:{conn_id}:{identity_kind}"`, `value` =
//! canonicalized identifier. Confidence is set per-kind so the matcher can
//! refuse to auto-promote weak signals (display_name) to `is_self`.
//!
//! One [`ProviderUserProfile`] expands to multiple rows — including
//! identifiers carried in `extras` that the previous fixed-fields shape
//! dropped on the floor (e.g. Slack screen-name handle).
//!
//! Callers invoke [`persist_provider_profile`] after every successful
//! `fetch_user_profile` call — from `on_connection_created`, periodic syncs,
//! and the `composio_get_user_profile` / `composio_refresh_all_identities`
//! RPC ops.

use super::ProviderUserProfile;
use crate::learning_candidate::{
    self as learning_candidate, CueFamily, EvidenceRef, FacetClass, LearningCandidate,
};
use crate::store::profile::FacetType;
use serde_json::Value;
use std::collections::BTreeMap;

// ────────────────────────────────────────────────────────────────────────
// IdentityKind — the matching axis
// ────────────────────────────────────────────────────────────────────────

/// Shape of an identifier persisted against a connection. Mirrors the
/// matching dimensions of the memory tree's
/// `crate::tree::score::extract::EntityKind` so the
/// self-check is a direct `(toolkit, kind, value)` lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityKind {
    /// Platform-canonical immutable id — Slack `U123ABC`, Notion UUID.
    UserId,
    Email,
    /// `@`-style screen name, canonicalised without the leading `@`.
    Handle,
    /// E.164 phone number.
    Phone,
    /// Human display label. Weak signal — never auto-promotes to is_self.
    DisplayName,
    /// Not for matching; kept for UI / prompt rendering.
    AvatarUrl,
    /// Not for matching; kept for UI / prompt rendering.
    ProfileUrl,
}

impl IdentityKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UserId => "user_id",
            Self::Email => "email",
            Self::Handle => "handle",
            Self::Phone => "phone",
            Self::DisplayName => "display_name",
            Self::AvatarUrl => "avatar_url",
            Self::ProfileUrl => "profile_url",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "user_id" => Self::UserId,
            "email" => Self::Email,
            "handle" => Self::Handle,
            "phone" => Self::Phone,
            "display_name" => Self::DisplayName,
            "avatar_url" => Self::AvatarUrl,
            "profile_url" => Self::ProfileUrl,
            _ => return None,
        })
    }

    /// Confidence the matcher records on the row. Hard kinds auto-promote
    /// a chunk to `is_self`; weak kinds require corroboration.
    pub fn confidence(self) -> f64 {
        match self {
            Self::UserId | Self::Phone => 1.00,
            Self::Email => 0.95,
            Self::Handle => 0.70,
            Self::DisplayName => 0.40,
            Self::AvatarUrl | Self::ProfileUrl => 0.50,
        }
    }

    /// True if this kind is a real identity signal worth running through
    /// the matcher (vs. UI-only fields).
    pub fn is_matchable(self) -> bool {
        matches!(
            self,
            Self::UserId | Self::Email | Self::Handle | Self::Phone | Self::DisplayName
        )
    }
}

/// Canonicalize a raw value for storage and lookup. The same routine runs
/// on the entity side at match time, so equality of canonical forms is the
/// matcher's only test — no `COLLATE NOCASE`, no per-call lowercasing.
pub fn canonicalize(kind: IdentityKind, raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(match kind {
        IdentityKind::Email => trimmed.to_lowercase(),
        IdentityKind::Handle => trimmed.trim_start_matches('@').to_lowercase(),
        IdentityKind::Phone => trimmed
            .chars()
            .filter(|c| c.is_ascii_digit() || *c == '+')
            .collect(),
        IdentityKind::DisplayName => trimmed.split_whitespace().collect::<Vec<_>>().join(" "),
        IdentityKind::UserId | IdentityKind::AvatarUrl | IdentityKind::ProfileUrl => {
            trimmed.to_string()
        }
    })
}

// ────────────────────────────────────────────────────────────────────────
// Persist
// ────────────────────────────────────────────────────────────────────────

/// Persist a provider profile as one facet row per (kind, value). Returns
/// the number of rows written. Silently no-ops if the memory client isn't
/// ready (startup race / unauthenticated CLI).
pub fn persist_provider_profile(profile: &ProviderUserProfile) -> usize {
    let Some(client) = crate::global::client_if_ready() else {
        tracing::debug!(
            toolkit = %profile.toolkit,
            "[composio:profile] memory client not ready, skipping persist"
        );
        return 0;
    };
    let store = client.profile_store();

    let now = now_secs();
    let toolkit = normalize_token(&profile.toolkit);
    let identifier = profile
        .connection_id
        .as_deref()
        .map(normalize_token)
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "default".to_string());

    let rows = expand_identity_rows(&toolkit, profile);

    let mut written = 0usize;
    for (kind, value) in rows {
        let key = format!("skill:{toolkit}:{identifier}:{}", kind.as_str());
        let facet_id = format!("skill-{toolkit}-{identifier}-{}", kind.as_str());

        if let Err(e) = store.upsert_provider_facet(
            &facet_id,
            &FacetType::Workflow,
            &key,
            &value,
            kind.confidence(),
            None,
            now,
        ) {
            tracing::warn!(
                toolkit = %toolkit,
                identifier = %identifier,
                kind = kind.as_str(),
                error = %e,
                "[composio:profile] profile_upsert failed (non-fatal)"
            );
            continue;
        }

        // Phase 3 (#566): also emit a LearningCandidate so the stability detector
        // can score provider data alongside other evidence on the next rebuild.
        // We use the `identity/` key prefix for provider identity fields.
        if kind.is_matchable() {
            let identity_key = format!("{}:{}", normalize_token(&toolkit), kind.as_str());
            let candidate = LearningCandidate {
                class: FacetClass::Identity,
                key: identity_key,
                value: value.clone(),
                cue_family: CueFamily::Structural,
                evidence: EvidenceRef::Provider {
                    toolkit: toolkit.clone(),
                    connection_id: identifier.clone(),
                    field: kind.as_str().to_string(),
                },
                initial_confidence: kind.confidence(),
                observed_at: now,
            };
            learning_candidate::global().push(candidate);
        }

        written += 1;
    }

    if written > 0 {
        tracing::debug!(
            toolkit = %toolkit,
            identifier = %identifier,
            rows_written = written,
            "[composio:profile] persisted identity rows (+ emitted Identity candidates)"
        );
    }
    written
}

/// Expand a [`ProviderUserProfile`] (and provider-specific `extras`) into
/// the canonical (kind, value) rows. **All per-toolkit quirks live here**;
/// the matcher only sees normalized tuples.
fn expand_identity_rows(
    toolkit: &str,
    profile: &ProviderUserProfile,
) -> Vec<(IdentityKind, String)> {
    let mut rows: Vec<(IdentityKind, String)> = Vec::new();
    let mut push = |kind: IdentityKind, raw: Option<&str>| {
        if let Some(v) = raw.and_then(|s| canonicalize(kind, s)) {
            rows.push((kind, v));
        }
    };

    push(IdentityKind::DisplayName, profile.display_name.as_deref());
    push(IdentityKind::Email, profile.email.as_deref());
    push(IdentityKind::AvatarUrl, profile.avatar_url.as_deref());
    push(IdentityKind::ProfileUrl, profile.profile_url.as_deref());

    match toolkit {
        "slack" => {
            // After the auth.test + users.info fix in slack/provider.rs:
            //   profile.username == Slack user_id (e.g. U123ABC)
            //   extras.handle    == Slack screen_name (e.g. "cyrus")
            //   extras.team_*    → workspace context, not identity
            push(IdentityKind::UserId, profile.username.as_deref());
            push(IdentityKind::Handle, json_str(&profile.extras, "handle"));
        }
        "notion" => {
            // Notion's `username` is the user UUID
            // (`data.bot.owner.user.id` per notion/provider.rs).
            push(IdentityKind::UserId, profile.username.as_deref());
        }
        "gmail" => {
            // Email + display_name only — no platform user_id worth matching.
        }
        _ => {
            // Unknown toolkit: best-effort. If `username` is set treat it
            // as a handle so weak-match logic (medium confidence) applies.
            push(IdentityKind::Handle, profile.username.as_deref());
        }
    }

    rows
}

fn json_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str())
}

// ────────────────────────────────────────────────────────────────────────
// Read paths
// ────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnectedIdentity {
    pub source: String,
    pub identifier: String,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub handle: Option<String>,
    pub phone: Option<String>,
    pub user_id: Option<String>,
    pub avatar_url: Option<String>,
    pub profile_url: Option<String>,
}

/// Load all provider-sourced identities, grouped by `(source, conn_id)`.
/// Rows whose last segment is not a known [`IdentityKind`] are silently
/// skipped — that includes legacy `username` rows from before the rewrite.
pub fn load_connected_identities() -> Vec<ConnectedIdentity> {
    let Some(client) = crate::global::client_if_ready() else {
        tracing::debug!("[composio:profile] load_connected_identities: memory client not ready");
        return Vec::new();
    };
    let facets = match client.profile_store().facets_by_type(&FacetType::Workflow) {
        Ok(f) => f,
        Err(error) => {
            tracing::warn!(
                error = %error,
                "[composio:profile] load_connected_identities: profile_facets_by_type failed"
            );
            return Vec::new();
        }
    };

    let mut grouped: BTreeMap<(String, String), ConnectedIdentity> = BTreeMap::new();
    for facet in facets {
        let Some((source, identifier, kind_str)) = parse_skill_identity_key(&facet.key) else {
            continue;
        };
        let Some(kind) = IdentityKind::parse(&kind_str) else {
            continue;
        };
        let entry = grouped
            .entry((source.clone(), identifier.clone()))
            .or_insert_with(|| ConnectedIdentity {
                source,
                identifier,
                ..Default::default()
            });
        match kind {
            IdentityKind::DisplayName => entry.display_name = Some(facet.value),
            IdentityKind::Email => entry.email = Some(facet.value),
            IdentityKind::Handle => entry.handle = Some(facet.value),
            IdentityKind::Phone => entry.phone = Some(facet.value),
            IdentityKind::UserId => entry.user_id = Some(facet.value),
            IdentityKind::AvatarUrl => entry.avatar_url = Some(facet.value),
            IdentityKind::ProfileUrl => entry.profile_url = Some(facet.value),
        }
    }
    grouped.into_values().collect()
}

/// Direct self-check for the entity matcher and the chunk-build hook.
/// Returns true if any connection of `toolkit` has a row with this
/// `(kind, value)` after canonicalization. Non-matchable kinds
/// (avatar_url, profile_url) always return false.
pub fn is_self_identity(toolkit: &str, kind: IdentityKind, raw_value: &str) -> bool {
    if !kind.is_matchable() {
        return false;
    }
    let Some(canonical) = canonicalize(kind, raw_value) else {
        return false;
    };
    let Some(client) = crate::global::client_if_ready() else {
        return false;
    };
    let key_pattern = format!("skill:{}:%:{}", normalize_token(toolkit), kind.as_str());
    client
        .profile_store()
        .skill_identity_matches(&key_pattern, &canonical)
}

/// Cross-toolkit variant — matches against every connected provider's
/// rows of this kind. Used for marking memory-tree entity rows: an email
/// in a Slack message that matches the user's Gmail address is still
/// "me," regardless of which source produced the chunk.
pub fn is_self_identity_any_toolkit(kind: IdentityKind, raw_value: &str) -> bool {
    if !kind.is_matchable() {
        return false;
    }
    let Some(canonical) = canonicalize(kind, raw_value) else {
        return false;
    };
    let Some(client) = crate::global::client_if_ready() else {
        return false;
    };
    let key_pattern = format!("skill:%:%:{}", kind.as_str());
    client
        .profile_store()
        .skill_identity_matches(&key_pattern, &canonical)
}

/// Render a compact section for prompt injection. Skips `user_id` (not
/// human-readable), prefixes `handle` with `@`.
pub fn render_connected_identities_section(identities: &[ConnectedIdentity]) -> String {
    if identities.is_empty() {
        return String::new();
    }
    let mut out = String::from("## Connected Identities\n\n");
    for id in identities {
        let mut fields = Vec::<String>::new();
        if let Some(v) = id.display_name.as_deref() {
            let v = sanitize_prompt_value(v);
            if !v.is_empty() {
                fields.push(v);
            }
        }
        if let Some(v) = id.email.as_deref() {
            let v = sanitize_prompt_value(v);
            if !v.is_empty() {
                fields.push(v);
            }
        }
        if let Some(v) = id.handle.as_deref() {
            let v = sanitize_prompt_value(v);
            if !v.is_empty() {
                fields.push(format!("@{v}"));
            }
        }
        if let Some(v) = id.profile_url.as_deref() {
            let v = sanitize_prompt_value(v);
            if !v.is_empty() {
                fields.push(v);
            }
        }
        if fields.is_empty() {
            continue;
        }
        let identifier = sanitize_prompt_value(&id.identifier);
        out.push_str(&format!(
            "- {} ({}): {}\n",
            title_case(&id.source),
            identifier,
            fields.join(" | ")
        ));
    }
    if out.trim() == "## Connected Identities" {
        return String::new();
    }
    out
}

/// Delete every row for a `(source, conn_id)` pair — used on disconnect.
pub fn delete_connected_identity_facets(source: &str, identifier: &str) -> usize {
    // `persist_provider_profile` writes keys with `normalize_token`-applied
    // segments; compare against the same normalized form here so a caller
    // passing the raw toolkit/connection_id still matches stored rows
    // (otherwise rows would survive disconnect and the user-tagger would
    // keep treating the removed account as the user — #1381 review).
    let source = normalize_token(source);
    let identifier = normalize_token(identifier);
    let Some(client) = crate::global::client_if_ready() else {
        tracing::debug!(
            source = %source,
            identifier = %identifier,
            "[composio:profile] delete_connected_identity_facets: memory client not ready"
        );
        return 0;
    };
    let store = client.profile_store();
    let Ok(facets) = store.facets_by_type(&FacetType::Workflow) else {
        return 0;
    };
    let mut deleted = 0usize;
    for facet in facets {
        let Some((s, i, _kind)) = parse_skill_identity_key(&facet.key) else {
            continue;
        };
        if s == source && i == identifier {
            // Same swallow as before: a disconnect must not fail because one
            // row was already gone.
            if store.delete_by_facet_id(&facet.facet_id).unwrap_or(false) {
                deleted += 1;
            }
        }
    }
    deleted
}

// ────────────────────────────────────────────────────────────────────────
// Helpers
// ────────────────────────────────────────────────────────────────────────

fn parse_skill_identity_key(key: &str) -> Option<(String, String, String)> {
    let mut parts = key.split(':');
    let prefix = parts.next()?;
    let source = parts.next()?;
    let identifier = parts.next()?;
    let kind = parts.next()?;
    if prefix != "skill" || parts.next().is_some() {
        return None;
    }
    Some((source.to_string(), identifier.to_string(), kind.to_string()))
}

fn normalize_token(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() || lower == '-' || lower == '_' {
            out.push(lower);
        } else {
            out.push('_');
        }
    }
    out.trim_matches('_').to_string()
}

pub fn normalize_connection_identifier(raw: &str) -> String {
    normalize_token(raw)
}

fn title_case(raw: &str) -> String {
    let mut chars = raw.chars();
    match chars.next() {
        Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

fn sanitize_prompt_value(raw: &str) -> String {
    let replaced = raw.replace(['\n', '\r', '\t'], " ").replace('|', "/");
    replaced.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn now_secs() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

// ────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "profile_tests.rs"]
mod tests;
