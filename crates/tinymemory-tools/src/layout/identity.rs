//! Resolve an acting agent against the host's plain-data memory pins.
//!
//! Hosts adapt their configuration into [`IdentityPins`], then resolve the
//! current [`Identity`] once for the work they perform. Pin precedence is
//! first match wins:
//!
//! | Source | Memory agent id | Layout root |
//! | --- | --- | --- |
//! | Host pin | `agent_id` | `root` |
//! | Definition pin | definition's `agent_id` | definition's `root` |
//! | Team member | definition id | `team:<team>` |
//! | Default | definition id, else `assistant` | namespace root |
//!
//! When `IdentityPins::v3` is set, conversations are pooled at the absolute
//! `ws:main` child of the resolved root. Identity is supplied by the host;
//! this module does not read model arguments or process state.

use std::collections::HashMap;

use tinymemory_api::{Namespace, Segment, SegmentKind};

use super::MemoryLayout;

/// The fallback memory agent id for work with no agent definition.
pub const DEFAULT_AGENT_ID: &str = "assistant";

/// The workspace id of the pooled conversation node used when
/// [`IdentityPins::v3`] is enabled.
pub const POOLED_CONVERSATIONS_WORKSPACE: &str = "main";

/// A host's per-definition identity settings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentPin {
    /// An alternate id used to label this definition's conversation turns.
    pub agent_id: Option<String>,
    /// The namespace root for this definition's memory.
    pub root: Option<String>,
    /// Whether this definition receives a context pack. `None` inherits the
    /// host's [`IdentityPins::recall`] setting.
    pub recall: Option<bool>,
}

/// Plain host settings used to resolve an acting identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityPins {
    /// An id shared by all work under the host binding.
    pub agent_id: Option<String>,
    /// A namespace root shared by all work under the host binding.
    pub root: Option<String>,
    /// Per-definition overrides, keyed by the definition id.
    pub agents: HashMap<String, AgentPin>,
    /// Whether an agent receives a context pack unless its definition pin
    /// overrides this value.
    pub recall: bool,
    /// Whether every agent's conversations share one pooled node.
    pub v3: bool,
}

impl Default for IdentityPins {
    fn default() -> Self {
        Self {
            agent_id: None,
            root: None,
            agents: HashMap::new(),
            recall: true,
            v3: false,
        }
    }
}

/// Who is acting on memory: an agent definition and, optionally, its team.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Identity {
    /// The agent definition id, or `None` for host work outside an agent.
    pub agent_id: Option<String>,
    /// The team containing the acting agent.
    pub team: Option<String>,
}

impl Identity {
    /// Host work outside an agent definition.
    #[must_use]
    pub fn root() -> Self {
        Self::default()
    }

    /// Work performed by a top-level agent definition.
    #[must_use]
    pub fn agent(agent_id: &str) -> Self {
        Self {
            agent_id: non_blank(agent_id),
            team: None,
        }
    }

    /// Work performed by an agent definition that belongs to a team.
    #[must_use]
    pub fn team_member(team: &str, agent_id: &str) -> Self {
        Self {
            agent_id: non_blank(agent_id),
            team: non_blank(team),
        }
    }

    /// Work performed by a child agent in the same team as this identity.
    #[must_use]
    pub fn child(&self, agent_id: &str) -> Self {
        Self {
            agent_id: non_blank(agent_id),
            team: self.team.clone(),
        }
    }

    /// Resolve this identity against host pins using the documented
    /// first-match-wins precedence.
    #[must_use]
    pub fn resolve(&self, pins: &IdentityPins) -> ResolvedIdentity {
        let definition = self.agent_id.as_deref().and_then(|id| pins.agents.get(id));
        let agent_id = non_blank_opt(pins.agent_id.as_deref())
            .or_else(|| non_blank_opt(definition.and_then(|pin| pin.agent_id.as_deref())))
            .or_else(|| self.agent_id.clone())
            .unwrap_or_else(|| DEFAULT_AGENT_ID.to_string());
        let root = parse_root(pins.root.as_deref())
            .or_else(|| parse_root(definition.and_then(|pin| pin.root.as_deref())))
            .or_else(|| self.team.as_deref().map(team_root))
            .unwrap_or(Namespace::ROOT);
        let layout = match MemoryLayout::new(root) {
            Ok(layout) => layout,
            Err(error) => {
                log::warn!("memory identity root is too deep; using the default root: {error}");
                MemoryLayout::default()
            }
        };
        let layout = if pins.v3 {
            pool_conversations(layout)
        } else {
            layout
        };
        let recall = definition.and_then(|pin| pin.recall).unwrap_or(pins.recall);

        ResolvedIdentity {
            layout,
            agent_id,
            recall,
        }
    }
}

/// An identity resolved to its memory layout, turn-label id, and recall policy.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ResolvedIdentity {
    /// The layout root and conversation placement for this identity.
    pub layout: MemoryLayout,
    /// The id used to label this identity's conversation turns.
    pub agent_id: String,
    /// Whether this identity receives a context pack.
    pub recall: bool,
}

impl ResolvedIdentity {
    /// The layout root for this identity.
    #[must_use]
    pub fn root(&self) -> &Namespace {
        self.layout.root()
    }
}

fn pool_conversations(layout: MemoryLayout) -> MemoryLayout {
    let node = chat_node(&layout);
    layout
        .clone()
        .with_pooled_conversations(&node)
        .unwrap_or_else(|error| {
            log::warn!("memory conversations could not be pooled: {error}");
            layout
        })
}

/// The absolute chat node below `layout`'s root (`ws:main`).
#[must_use]
pub fn chat_node(layout: &MemoryLayout) -> Namespace {
    chat_node_at(layout.root())
}

fn chat_node_at(root: &Namespace) -> Namespace {
    root.child(Segment::sanitized(
        SegmentKind::Workspace,
        POOLED_CONVERSATIONS_WORKSPACE,
    ))
    .unwrap_or_else(|_| root.clone())
}

/// Whether `root` names a valid memory-layout root.
///
/// # Errors
///
/// Returns the parse or layout-depth error when the root is invalid.
pub fn validate_root(root: &str) -> Result<(), String> {
    let root: Namespace = root
        .trim()
        .parse()
        .map_err(|error: tinymemory_api::Error| error.to_string())?;
    MemoryLayout::new(root)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// The `org:<id>` spelling of a valid 24-hex-digit account id.
#[must_use]
pub fn account_root(id: &str) -> Option<String> {
    let account = id.len() == 24 && id.bytes().all(|byte| byte.is_ascii_hexdigit());
    account.then(|| format!("org:{}", id.to_ascii_lowercase()))
}

/// Converts a recorded local `user:` root to its current `org:` spelling.
#[must_use]
pub fn org_of_recorded(recorded: &str) -> String {
    match recorded.strip_prefix("user:") {
        Some(id) => format!("org:{id}"),
        None => recorded.to_string(),
    }
}

/// The `user:` actor spelling corresponding to a person's `org:` root.
#[must_use]
pub fn actor_of_root(root: &str) -> String {
    match root
        .strip_prefix("org")
        .and_then(|rest| rest.strip_prefix(':'))
    {
        Some(id) => format!("user:{id}"),
        None => root.to_string(),
    }
}

fn team_root(team: &str) -> Namespace {
    Namespace::ROOT
        .child(Segment::sanitized(SegmentKind::Team, team))
        .unwrap_or(Namespace::ROOT)
}

fn parse_root(raw: Option<&str>) -> Option<Namespace> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    match raw.parse::<Namespace>() {
        Ok(root) => Some(root),
        Err(error) => {
            log::warn!("ignoring invalid memory identity root {raw:?}: {error}");
            None
        }
    }
}

fn non_blank(value: &str) -> Option<String> {
    non_blank_opt(Some(value))
}

fn non_blank_opt(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
#[path = "identity_tests.rs"]
mod tests;
