//! Storing past chats: the turn builder.
//!
//! The lifecycle logs each turn as it happens ([`crate::lifecycle`]). A host
//! that wants conversations from before that stored the same way walks its
//! thread store and builds the items with this module: one conversation item
//! per message, user message `2n` and its reply `2n + 1`, at the main agent's
//! node, plus a [`BACKFILL_TAG`] so they can be told apart.
//!
//! - **Turns.** A thread's messages become turns the way a chat shows them:
//!   each user message opens a turn and the replies after it, up to the next
//!   user message, are its answer. Replies before any user message form a
//!   turn with no user side ([`turns_of`]).
//! - **No overlap with live logging.** A thread's turns from the first one the
//!   lifecycle logged on are already in memory; [`first_live_turn`] asks the
//!   engine where logging began and [`pending_range`] stops short of it.
//! - **Offline status.** [`Backfiller::status_without_engine`] reports pending
//!   turns from the saved counters and thread source when no engine is bound;
//!   unlike engine-backed status, it cannot query live logging cutoffs.
//! - **Repeatable.** A re-sent item is a replay (its id is a content digest),
//!   never a duplicate, so a host may resume from any [`pending_range`] start.
//!
//! Reading the thread store, remembering progress, asking for consent and
//! spawning the run stay with the host: the module links no executor and does
//! no I/O.

use chrono::{DateTime, Utc};
use tinymemory_api::{
    ItemKind, ListRequest, MemoryEngine, MemoryMeta, MetaFilter, Result, Role, SourceKind,
    SourceRef, StoreItem, Turn, TurnRange,
};

use crate::layout::MemoryLayout;

mod driver;

pub use driver::{
    BackfillFile, BackfillPhase, BackfillRun, BackfillState, BackfillView, Backfiller,
    CounterStore, ThreadSource,
};

/// The tag every backfilled conversation item carries.
pub const BACKFILL_TAG: &str = "backfill";

/// The agent definition past chats belong to: the main chat agent.
pub const MAIN_AGENT: &str = "orchestrator";

/// Conversation items a host stores per bulk call; whole turns go together,
/// so a group is [`TURNS_PER_GROUP`] turns.
pub const STORE_GROUP: usize = 50;

/// Turns per bulk store (two items each).
pub const TURNS_PER_GROUP: usize = STORE_GROUP / 2;

/// Items a thread lookup lists to find where live logging began.
const LIVE_SCAN_LIMIT: usize = 200;

/// One stored chat message, as the host's thread store holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadMessage {
    /// Who wrote it: `user` (any case) opens a turn, anything else replies.
    pub sender: String,
    /// The message text.
    pub content: String,
    /// When it was written, RFC 3339; an unparsable time is ignored.
    pub created_at: String,
}

impl ThreadMessage {
    /// A message from `sender` at `created_at`.
    #[must_use]
    pub fn new(
        sender: impl Into<String>,
        content: impl Into<String>,
        created_at: impl Into<String>,
    ) -> Self {
        Self {
            sender: sender.into(),
            content: content.into(),
            created_at: created_at.into(),
        }
    }
}

/// One past turn: what the user said and what came back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PastTurn {
    /// The user's message; empty for replies before any user message.
    pub user: String,
    /// The replies, joined.
    pub assistant: String,
    /// When the turn last changed.
    pub at: DateTime<Utc>,
}

fn parse_time(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

/// A thread's messages as turns, oldest first. Blank messages are dropped.
#[must_use]
pub fn turns_of(messages: &[ThreadMessage]) -> Vec<PastTurn> {
    let mut turns: Vec<PastTurn> = Vec::new();
    for message in messages {
        let text = message.content.trim();
        if text.is_empty() {
            continue;
        }
        let at = parse_time(&message.created_at);
        if message.sender.eq_ignore_ascii_case("user") {
            turns.push(PastTurn {
                user: text.to_string(),
                assistant: String::new(),
                at: at.unwrap_or_else(Utc::now),
            });
            continue;
        }
        if turns.is_empty() {
            turns.push(PastTurn {
                user: String::new(),
                assistant: String::new(),
                at: at.unwrap_or_else(Utc::now),
            });
        }
        if let Some(turn) = turns.last_mut() {
            if !turn.assistant.is_empty() {
                turn.assistant.push_str("\n\n");
            }
            turn.assistant.push_str(text);
            if let Some(at) = at {
                turn.at = at;
            }
        }
    }
    turns
}

/// Which turns of a thread with `total` turns the backfill should store:
/// from what it already stored up to (not including) the first turn live
/// logging took, if any.
#[must_use]
pub fn pending_range(
    total: usize,
    first_live: Option<usize>,
    already_stored: u32,
) -> std::ops::Range<usize> {
    let end = first_live.map_or(total, |first| first.min(total));
    let start = (already_stored as usize).min(end);
    start..end
}

/// `range` cut into groups of whole turns to store together
/// ([`TURNS_PER_GROUP`] each), so a stored-turn counter never splits a turn.
#[must_use]
pub fn turn_groups(range: std::ops::Range<usize>) -> Vec<Vec<usize>> {
    range
        .collect::<Vec<_>>()
        .chunks(TURNS_PER_GROUP)
        .map(<[usize]>::to_vec)
        .collect()
}

/// The conversation items for turn `index` of `thread_id`, shaped as the
/// lifecycle logs them (one per message) at `agent_id`'s node of `layout`,
/// tagged [`BACKFILL_TAG`]. Empty when the node cannot be built.
#[must_use]
pub fn turn_items(
    layout: &MemoryLayout,
    agent_id: &str,
    thread_id: &str,
    index: usize,
    turn: &PastTurn,
) -> Vec<StoreItem> {
    let Ok(node) = layout.conversations(agent_id) else {
        return Vec::new();
    };
    let base = u32::try_from(index.saturating_mul(2)).unwrap_or(u32::MAX - 1);
    [
        (Role::User, &turn.user, base),
        (Role::Assistant, &turn.assistant, base + 1),
    ]
    .into_iter()
    .filter(|(_, text, _)| !text.trim().is_empty())
    .map(|(role, text, at_index)| StoreItem::Conversation {
        meta: MemoryMeta {
            namespace: node.clone(),
            thread_id: Some(thread_id.to_string()),
            turns: Some(TurnRange {
                first: at_index,
                last: at_index,
            }),
            agent_id: Some(agent_id.to_string()),
            source: SourceRef {
                kind: SourceKind::Conversation,
                id: Some(thread_id.to_string()),
            },
            observed_at: Some(turn.at),
            tags: vec![BACKFILL_TAG.to_string()],
            ..MemoryMeta::default()
        },
        turns: vec![Turn {
            at: Some(turn.at),
            ..Turn::new(role, text.as_str())
        }],
    })
    .collect()
}

/// The items of one [`turn_groups`] group of `turns` (`group` holds turn
/// indices), in order.
#[must_use]
pub fn group_items(
    layout: &MemoryLayout,
    agent_id: &str,
    thread_id: &str,
    turns: &[PastTurn],
    group: &[usize],
) -> Vec<StoreItem> {
    group
        .iter()
        .filter_map(|&index| turns.get(index).map(|turn| (index, turn)))
        .flat_map(|(index, turn)| turn_items(layout, agent_id, thread_id, index, turn))
        .collect()
}

/// The first turn of `thread_id` the lifecycle logged (not a backfill), as a
/// turn number; `None` when none was, or the lookup failed (the backfill then
/// stores everything, and a replay is harmless).
pub async fn first_live_turn(engine: &dyn MemoryEngine, thread_id: &str) -> Option<usize> {
    let page = list_thread(engine, thread_id)
        .await
        .map_err(|error| {
            log::debug!("[memory:backfill] live turn lookup failed: {error}");
        })
        .ok()?;
    page.items
        .iter()
        .filter(|hit| !hit.meta.tags.iter().any(|tag| tag == BACKFILL_TAG))
        .filter_map(|hit| hit.meta.turns.map(|turns| turns.first as usize / 2))
        .min()
}

async fn list_thread(
    engine: &dyn MemoryEngine,
    thread_id: &str,
) -> Result<tinymemory_api::ListPage> {
    engine
        .list(ListRequest {
            filter: MetaFilter {
                thread_id: Some(thread_id.to_string()),
                ..MetaFilter::kinds([ItemKind::Conversation])
            },
            limit: LIVE_SCAN_LIMIT,
            cursor: None,
        })
        .await
}

#[cfg(test)]
#[path = "backfill_tests.rs"]
mod tests;
