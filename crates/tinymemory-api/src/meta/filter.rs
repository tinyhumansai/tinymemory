//! [`MetaFilter`]: the metadata query every fetch, list, recall and forget
//! takes.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{MemoryMeta, SourceKind, TurnRange};
use crate::item::ItemKind;
use crate::namespace::{Namespace, Reach};

/// Which items an operation applies to.
///
/// Every set field must match; an empty filter matches everything. Scalar
/// fields are exact matches, except `folder` and `file_path`, which also match
/// a path prefix on a `/` boundary (`/a/b` matches `/a/b/c.rs`, not `/a/bc`).
/// `tool_call` matches the tool's name. The list fields match when the item's
/// value is in the list (`kinds`, `sources`) or shares one tag (`tags_any`); an
/// empty list does not constrain. The window is `observed_after <= observed_at
/// < observed_before`, and an item with no `observed_at` never matches a
/// window. `reach` admits only items whose namespace is in reach; unset, it
/// admits every namespace except a service sandbox ([`Reach::admitted_by`]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MetaFilter {
    /// The namespaces read; `None` reads every namespace except a service
    /// sandbox, as [`Reach::subtree`] of the root does.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reach: Option<Reach>,
    /// Exact workspace.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// Folder, exact or as a path prefix.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    /// File path, exact or as a path prefix.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    /// Exact language.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Exact repository.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Exact commit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Exact URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Exact thread id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    /// Exact channel.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// Exact turn range.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turns: Option<TurnRange>,
    /// Exact agent id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Exact tool name of the producing tool call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call: Option<String>,
    /// Exact source id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    /// Item kinds to include; empty means all.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<ItemKind>,
    /// Source kinds to include; empty means all.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<SourceKind>,
    /// Match an item carrying any one of these tags; empty means no constraint.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags_any: Vec<String>,
    /// Inclusive lower bound on `observed_at`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_after: Option<DateTime<Utc>>,
    /// Exclusive upper bound on `observed_at`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_before: Option<DateTime<Utc>>,
}

impl MetaFilter {
    /// A filter restricted to items carrying any of the given tags.
    #[must_use]
    pub fn tags_any(tags: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            tags_any: tags.into_iter().map(Into::into).collect(),
            ..Self::default()
        }
    }

    /// A filter restricted to the given item kinds.
    #[must_use]
    pub fn kinds(kinds: impl IntoIterator<Item = ItemKind>) -> Self {
        Self {
            kinds: kinds.into_iter().collect(),
            ..Self::default()
        }
    }

    /// Whether the filter constrains nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Whether the filter admits `kind` (ignoring every metadata field).
    #[must_use]
    pub fn admits_kind(&self, kind: ItemKind) -> bool {
        self.kinds.is_empty() || self.kinds.contains(&kind)
    }

    /// Whether the filter's reach admits `namespace` (ignoring every other
    /// field).
    fn admits_namespace(&self, namespace: &Namespace) -> bool {
        Reach::admitted_by(self.reach.as_ref(), namespace)
    }

    /// Whether an item of `kind` carrying `meta` matches every set field.
    #[must_use]
    pub fn matches(&self, kind: ItemKind, meta: &MemoryMeta) -> bool {
        self.admits_kind(kind)
            && self.admits_namespace(&meta.namespace)
            && exact(self.workspace.as_deref(), meta.workspace.as_deref())
            && path_prefix(self.folder.as_deref(), meta.folder.as_deref())
            && path_prefix(self.file_path.as_deref(), meta.file_path.as_deref())
            && exact(self.language.as_deref(), meta.language.as_deref())
            && exact(self.repo.as_deref(), meta.repo.as_deref())
            && exact(self.commit.as_deref(), meta.commit.as_deref())
            && exact(self.url.as_deref(), meta.url.as_deref())
            && exact(self.thread_id.as_deref(), meta.thread_id.as_deref())
            && exact(self.channel.as_deref(), meta.channel.as_deref())
            && self.turns.is_none_or(|turns| meta.turns == Some(turns))
            && exact(self.agent_id.as_deref(), meta.agent_id.as_deref())
            && exact(
                self.tool_call.as_deref(),
                meta.tool_call.as_ref().map(|call| call.name.as_str()),
            )
            && exact(self.source_id.as_deref(), meta.source.id.as_deref())
            && (self.sources.is_empty() || self.sources.contains(&meta.source.kind))
            && (self.tags_any.is_empty() || self.tags_any.iter().any(|t| meta.tags.contains(t)))
            && self.in_window(meta.observed_at)
    }

    fn in_window(&self, observed_at: Option<DateTime<Utc>>) -> bool {
        if self.observed_after.is_none() && self.observed_before.is_none() {
            return true;
        }
        let Some(at) = observed_at else {
            return false;
        };
        self.observed_after.is_none_or(|after| at >= after)
            && self.observed_before.is_none_or(|before| at < before)
    }
}

fn exact(wanted: Option<&str>, held: Option<&str>) -> bool {
    wanted.is_none_or(|wanted| held == Some(wanted))
}

fn path_prefix(wanted: Option<&str>, held: Option<&str>) -> bool {
    let Some(wanted) = wanted else {
        return true;
    };
    let Some(held) = held else {
        return false;
    };
    if held == wanted {
        return true;
    }
    let base = wanted.trim_end_matches('/');
    held.strip_prefix(base)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

#[cfg(test)]
#[path = "filter_tests.rs"]
mod tests;
