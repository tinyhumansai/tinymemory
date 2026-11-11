//! The inputs and outputs of each lifecycle step, and the recall policy.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tinymemory_api::{Error, ObservedActor, Result, StoreReceipt, ToolCallRef, Turn};

use crate::background::BackgroundJob;
use crate::recall::ContextPack;

/// Default token budget of a turn's context pack.
pub const DEFAULT_TURN_BUDGET_TOKENS: usize = 1_200;

/// How an [`crate::AgentMemory`] fills its packs and when it asks for
/// belief builds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecallPolicy {
    /// The most tokens a pack's block may take.
    pub budget_tokens: usize,
    /// The most learnings a pack shows.
    pub learnings_limit: usize,
    /// The most brain documents a pack shows.
    pub brain_limit: usize,
    /// The most of this agent's past turns a pack shows.
    pub history_limit: usize,
    /// The most other agents' turns a pack shows; `0` leaves the team's
    /// conversations out.
    pub team_limit: usize,
    /// Ask for a belief build of this agent's conversations after every this
    /// many turns (by `turn_index + 1`); `None` never asks, and neither does
    /// an engine that rebuilds beliefs on its own
    /// ([`tinymemory_api::Consolidation::Automatic`]).
    pub build_beliefs_every: Option<u32>,
}

/// Host settings for recall and the memory lifecycle's wait and build delays.
///
/// Every field defaults so a host can load older or partial configuration.
/// `enabled` controls whether the host identity recalls at all; it is kept
/// separate from [`RecallPolicy`], which describes the sections and budgets
/// of a pack.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RecallSettings {
    /// Whether the host identity recalls a pack before each turn.
    pub enabled: bool,
    /// The pack's size in tokens.
    pub budget_tokens: u32,
    /// Learnings and built beliefs per pack.
    pub learnings_limit: u32,
    /// Brain documents per pack.
    pub brain_limit: u32,
    /// This agent's earlier turns per pack.
    pub history_limit: u32,
    /// Other agents' turns under the same root per pack.
    pub team_limit: u32,
    /// Turns between belief builds; zero disables them.
    pub build_beliefs_every: u32,
    /// How long a turn waits for recall before proceeding without a pack.
    pub pre_turn_timeout_ms: u64,
    /// Whether to use a model call to identify dates in each turn.
    pub date_hint: bool,
    /// How long compaction waits for recalled context.
    pub compaction_timeout_ms: u64,
    /// How long queued belief builds wait for newer writes.
    pub build_delay_secs: u64,
}

impl Default for RecallSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            budget_tokens: 1_200,
            learnings_limit: 8,
            brain_limit: 6,
            history_limit: 6,
            team_limit: 3,
            build_beliefs_every: 10,
            pre_turn_timeout_ms: 10_000,
            date_hint: false,
            compaction_timeout_ms: 8_000,
            build_delay_secs: 300,
        }
    }
}

impl RecallSettings {
    /// Validates the settings without changing them.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRequest`] when a host policy value is outside its
    /// supported range or `build_delay_secs` cannot be represented by the
    /// supported date range.
    pub fn validate(&self) -> Result<()> {
        validate_range("budget_tokens", self.budget_tokens, 100, 16_000)?;
        validate_range("learnings_limit", self.learnings_limit, 0, 50)?;
        validate_range("brain_limit", self.brain_limit, 0, 50)?;
        validate_range("history_limit", self.history_limit, 0, 50)?;
        validate_range("team_limit", self.team_limit, 0, 50)?;
        validate_range("build_beliefs_every", self.build_beliefs_every, 0, 1_000)?;
        validate_range("pre_turn_timeout_ms", self.pre_turn_timeout_ms, 100, 30_000)?;
        validate_range(
            "compaction_timeout_ms",
            self.compaction_timeout_ms,
            100,
            30_000,
        )?;
        validate_build_delay_secs(self.build_delay_secs)?;
        Ok(())
    }
}

impl From<&RecallSettings> for RecallPolicy {
    fn from(settings: &RecallSettings) -> Self {
        let limit = |value: u32| usize::try_from(value).unwrap_or(usize::MAX);
        Self {
            budget_tokens: limit(settings.budget_tokens).max(1),
            learnings_limit: limit(settings.learnings_limit),
            brain_limit: limit(settings.brain_limit),
            history_limit: limit(settings.history_limit),
            team_limit: limit(settings.team_limit),
            build_beliefs_every: (settings.build_beliefs_every > 0)
                .then_some(settings.build_beliefs_every),
        }
    }
}

fn validate_range<T: PartialOrd + std::fmt::Display + Copy>(
    name: &str,
    value: T,
    min: T,
    max: T,
) -> Result<()> {
    if value < min || value > max {
        return Err(Error::InvalidRequest(format!(
            "{name} must be between {min} and {max}"
        )));
    }
    Ok(())
}

fn validate_build_delay_secs(seconds: u64) -> Result<()> {
    let delay = i64::try_from(seconds)
        .ok()
        .and_then(chrono::Duration::try_seconds);
    if delay
        .and_then(|delay| chrono::Utc::now().checked_sub_signed(delay))
        .is_none()
    {
        return Err(Error::InvalidRequest(
            "build_delay_secs must fit within the supported date range".into(),
        ));
    }
    Ok(())
}

/// How an agent-memory preview will be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreviewMode {
    /// A query-ranked pack for a turn.
    Turn,
    /// The newest context for a session start or resume.
    Session,
}

/// The pack and mode returned by [`crate::AgentMemory::preview`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Preview {
    /// Whether the pack is for a turn or a session.
    pub mode: PreviewMode,
    /// The context pack.
    pub pack: ContextPack,
}

/// An agent with logged conversation turns under a memory layout root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCount {
    /// The agent id stored with its turns.
    pub agent_id: String,
    /// Number of conversation items found for this agent.
    pub turns: u64,
}

impl Default for RecallPolicy {
    fn default() -> Self {
        Self {
            budget_tokens: DEFAULT_TURN_BUDGET_TOKENS,
            learnings_limit: 8,
            brain_limit: 6,
            history_limit: 6,
            team_limit: 3,
            build_beliefs_every: Some(10),
        }
    }
}

impl RecallPolicy {
    /// Keeps conversation logging enabled while omitting every policy-owned
    /// recall section. A host that configured extra [`crate::CoreScope`]s
    /// must clear those separately with [`crate::AgentMemory::with_core`].
    ///
    /// The budget and belief-build cadence stay configured; only retrieval
    /// limits are cleared.
    #[must_use]
    pub fn log_only(&self) -> Self {
        Self {
            learnings_limit: 0,
            brain_limit: 0,
            history_limit: 0,
            team_limit: 0,
            ..self.clone()
        }
    }
}

/// A session starting or resuming.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStart {
    /// The thread being resumed, if any: its own past turns come first in
    /// this agent's history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    /// What the session is about, when known: ranks every section. Without
    /// one, each section shows its newest items.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus: Option<String>,
}

/// A user turn about to be answered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreTurn {
    /// The conversation thread.
    pub thread_id: String,
    /// This turn's position in the thread, from `0`.
    pub turn_index: u32,
    /// What the user said.
    pub user_text: String,
    /// Whether this is the first turn after a compaction. A resumed turn gets
    /// the earlier turns of this thread at the head of its memory pack.
    ///
    /// This public field is additive for serialized requests (missing values
    /// default to `false`), but it requires struct-literal callers to add the
    /// field or use `..PreTurn::new(...)`.
    #[serde(default)]
    pub resumed: bool,
    /// The messaging channel that owns this thread, when it came from one.
    /// Struct-literal callers can use `..PreTurn::new(...)` to keep the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// The first turn of this thread still in the host's prompt; turns from
    /// here on are left out of the pack, since the model already sees them.
    /// `0` (the default) leaves the whole thread out.
    #[serde(default)]
    pub in_prompt_from: u32,
    /// When the user spoke, if the host knows: orders history newest first,
    /// and leads the turn's bullet in later packs, so a reader can tell a
    /// corrected value from its correction. Pass the message's own time (not
    /// the time of the call), so a retried turn stays a replay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<DateTime<Utc>>,
    /// Who said it, when that is someone other than the memory's owner: a
    /// channel's sender (`user:+15551234567`). Logged as the turn's
    /// [`tinymemory_api::MemoryMeta::observed_actor`], which an engine that
    /// attributes may send; it is not part of the turn's id, so a retried
    /// turn stays a replay with or without it. `None` (the default) is the
    /// owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_actor: Option<ObservedActor>,
}

impl PreTurn {
    /// A turn of `thread_id` at `turn_index` saying `user_text`, with the
    /// whole thread in the prompt.
    #[must_use]
    pub fn new(
        thread_id: impl Into<String>,
        turn_index: u32,
        user_text: impl Into<String>,
    ) -> Self {
        Self {
            thread_id: thread_id.into(),
            turn_index,
            user_text: user_text.into(),
            resumed: false,
            channel: None,
            in_prompt_from: 0,
            at: None,
            observed_actor: None,
        }
    }
}

/// What [`crate::AgentMemory::pre_turn`] produced.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TurnContext {
    /// The context to inject before the model runs.
    pub pack: ContextPack,
    /// The logged user turn's receipt; `None` when logging failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logged: Option<StoreReceipt>,
    /// Why logging failed, when it did. The pack is still good.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_error: Option<String>,
}

/// A reply the model just gave.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostTurn {
    /// The conversation thread.
    pub thread_id: String,
    /// This reply's position in the thread.
    pub turn_index: u32,
    /// What the assistant said.
    pub assistant_text: String,
    /// The messaging channel that owns this thread, when it came from one.
    /// Struct-literal callers can use `..PostTurn::new(...)` to keep the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// The tool calls the reply made.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallRef>,
    /// When the reply was given, if the host knows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<DateTime<Utc>>,
}

impl PostTurn {
    /// A reply of `thread_id` at `turn_index` saying `assistant_text`, with
    /// no tool calls.
    #[must_use]
    pub fn new(
        thread_id: impl Into<String>,
        turn_index: u32,
        assistant_text: impl Into<String>,
    ) -> Self {
        Self {
            thread_id: thread_id.into(),
            turn_index,
            assistant_text: assistant_text.into(),
            channel: None,
            tool_calls: Vec::new(),
            at: None,
        }
    }
}

/// What [`crate::AgentMemory::post_turn`] did.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PostTurnReport {
    /// The logged reply's receipt.
    pub receipt: StoreReceipt,
    /// Work to run off the turn (a belief build, per the policy).
    pub jobs: Vec<BackgroundJob>,
}

/// Turns about to leave the host's prompt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Compaction {
    /// The conversation thread.
    pub thread_id: String,
    /// The turns being dropped, oldest first. They are already stored; they
    /// steer what is recalled.
    pub dropped: Vec<Turn>,
    /// What the conversation is about now, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus: Option<String>,
}
