//! Requests and responses for recall, fetch, list and forget.
//!
//! - **Recall** ([`RecallRequest`] → [`RecallAnswer`]) asks a question and gets
//!   a synthesised answer with [`Citation`]s, or, in [`RecallMode::Evidence`],
//!   the evidence for the caller's own model to answer from.
//! - **Fetch** ([`FetchRequest`] → [`FetchPage`]) is raw retrieval in a
//!   [`FetchMode`], filtered by metadata.
//! - **List** ([`ListRequest`] → [`ListPage`]) pages through stored items
//!   with no query.
//! - **Forget** ([`ForgetTarget`] → [`ForgetReport`]) removes items by id or by
//!   a non-empty filter.
//!
//! Each request has a `validate` method engines call first, so every engine
//! refuses the same malformed requests the same way.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::item::{ItemId, ItemKind, StoreItem};
use crate::meta::{MemoryMeta, MetaFilter};
use crate::namespace::Reach;

mod time;
pub use time::TimeHint;

/// A question for [`crate::MemoryEngine::recall`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecallRequest {
    /// The question.
    pub question: String,
    /// Which items the answer may draw on.
    #[serde(default)]
    pub filter: MetaFilter,
    /// Most citations to gather; must be positive.
    pub limit: usize,
    /// Extra instructions for how to answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// The days the question is about: memories from them rank first. A
    /// ranking hint, never a filter (see [`TimeHint`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refers_to: Option<TimeHint>,
    /// Conversation turns never cited: the calling thread's, which its
    /// prompt already holds (see [`ThreadWindow`]). Read-only, so it can
    /// never narrow a forget or an erase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_thread: Option<ThreadWindow>,
    /// Whether the engine writes the answer ([`RecallMode::Answer`], the
    /// default) or returns the evidence for the caller to answer from.
    #[serde(default, skip_serializing_if = "RecallMode::is_answer")]
    pub mode: RecallMode,
}

/// What [`crate::MemoryEngine::recall`] puts in [`RecallAnswer::answer`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecallMode {
    /// An answer the engine writes from what it recalled (an engine may run
    /// a model for it).
    #[default]
    Answer,
    /// The recalled evidence itself, as text, for a caller whose own model
    /// writes the answer: the engine runs no answer step, and
    /// [`RecallAnswer::model`] is `None`. The citations are the items the
    /// evidence holds.
    Evidence,
}

impl RecallMode {
    /// Whether this is [`RecallMode::Answer`] (serde's skip test).
    #[must_use]
    pub fn is_answer(&self) -> bool {
        matches!(self, Self::Answer)
    }
}

impl RecallRequest {
    /// A question over everything, gathering at most `limit` citations.
    #[must_use]
    pub fn new(question: impl Into<String>, limit: usize) -> Self {
        Self {
            question: question.into(),
            filter: MetaFilter::default(),
            limit,
            instructions: None,
            refers_to: None,
            exclude_thread: None,
            mode: RecallMode::Answer,
        }
    }

    /// Checks the request is answerable.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRequest`] for a blank question, a zero limit, an
    /// invalid [`TimeHint`] or a blank excluded thread.
    pub fn validate(&self) -> Result<()> {
        non_blank("recall question", &self.question)?;
        positive("recall limit", self.limit)?;
        self.exclude_thread
            .as_ref()
            .map_or(Ok(()), ThreadWindow::validate)?;
        self.refers_to.as_ref().map_or(Ok(()), TimeHint::validate)
    }
}

/// A thread's turns a read leaves out: every turn of `thread_id` from
/// `from_turn` on. A caller reading inside a thread sets it to the turns its
/// prompt already holds, so the message being answered (logged before the
/// read) does not come back as a memory; `from_turn: 0` leaves the whole
/// thread out. Engines apply it to what they return ([`ThreadWindow::excludes`]:
/// conversation turns only); an engine whose store cannot exclude server side
/// drops the matching items from its results.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadWindow {
    /// The thread.
    pub thread_id: String,
    /// The first turn index still in the prompt; turns from here on are
    /// left out.
    #[serde(default)]
    pub from_turn: u32,
}

impl ThreadWindow {
    /// The whole of `thread_id`.
    #[must_use]
    pub fn whole(thread_id: impl Into<String>) -> Self {
        Self {
            thread_id: thread_id.into(),
            from_turn: 0,
        }
    }

    /// Whether `hit` lies inside the window.
    #[must_use]
    pub fn covers(&self, hit: &Hit) -> bool {
        self.covers_meta(&hit.meta)
    }

    /// Whether an item carrying `meta` lies inside the window: it belongs
    /// to the thread, and its turns (when it records them) reach
    /// `from_turn`.
    #[must_use]
    pub fn covers_meta(&self, meta: &MemoryMeta) -> bool {
        meta.thread_id.as_deref() == Some(self.thread_id.as_str())
            && meta
                .turns
                .as_ref()
                .is_none_or(|turns| turns.last >= self.from_turn)
    }

    /// Whether a read's `exclude_thread` leaves out an item of `kind`
    /// carrying `meta`: only **conversation** turns are left out. A learning
    /// or a document stamped with the thread (a host records where it was
    /// learned) stays readable, since the prompt holds the thread's turns,
    /// not what was learned from them.
    #[must_use]
    pub fn excludes(&self, kind: ItemKind, meta: &MemoryMeta) -> bool {
        kind == ItemKind::Conversation && self.covers_meta(meta)
    }

    /// Checks the window names a thread.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRequest`] for a blank thread id.
    pub fn validate(&self) -> Result<()> {
        non_blank("excluded thread id", &self.thread_id)
    }
}

/// A synthesised answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecallAnswer {
    /// The answer text; in [`RecallMode::Evidence`], the recalled evidence
    /// as text (empty when nothing was recalled).
    pub answer: String,
    /// The items it drew on.
    #[serde(default)]
    pub citations: Vec<Citation>,
    /// The model that answered, when the engine reports it; `None` in
    /// [`RecallMode::Evidence`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// One item an answer drew on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Citation {
    /// The cited item's id, resolvable through [`crate::MemoryEngine::list`].
    pub id: ItemId,
    /// The cited item's kind.
    pub kind: ItemKind,
    /// The relevant excerpt.
    pub snippet: String,
    /// The cited item's metadata.
    pub meta: MemoryMeta,
    /// Relevance, when the engine scores.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f32>,
}

/// How [`crate::MemoryEngine::fetch`] ranks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FetchMode {
    /// Lexical match.
    Keyword,
    /// Embedding similarity.
    Vector,
    /// The engine's blend of both.
    Hybrid,
}

impl FetchMode {
    /// Every mode, in declaration order.
    pub const ALL: [Self; 3] = [Self::Keyword, Self::Vector, Self::Hybrid];

    /// The stable snake_case wire string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Keyword => "keyword",
            Self::Vector => "vector",
            Self::Hybrid => "hybrid",
        }
    }
}

/// A raw retrieval.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FetchRequest {
    /// The query text.
    pub query: String,
    /// How to rank.
    pub mode: FetchMode,
    /// Which items to search.
    #[serde(default)]
    pub filter: MetaFilter,
    /// Page size; must be positive.
    pub limit: usize,
    /// Continue from a previous page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// Also return up to this many beliefs from what the fetch read, in
    /// [`FetchPage::beliefs`]: the beliefs an engine keeps apart from its
    /// items (see [`crate::MemoryEngine::beliefs`]), ranked for the same
    /// query, from the same reads. `0`, the default, asks for none; an
    /// engine that keeps no beliefs apart returns none.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub beliefs: usize,
    /// The days the query is about: hits from them rank first. A ranking
    /// hint, never a filter (see [`TimeHint`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refers_to: Option<TimeHint>,
    /// Read at most this many of the scopes the filter reaches; `None`, the
    /// default, reads them all. For a per-turn read over a tree that holds
    /// many scopes (one per connector or repository), where each scope is a
    /// separate ranking: an engine that keeps scopes apart reads the ones the
    /// query names first, then the most recently written; an engine without
    /// scopes ignores it. Must be positive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_scopes: Option<usize>,
    /// Conversation turns never returned: the calling thread's (see
    /// [`ThreadWindow`]). Read-only, so it can never narrow a forget or an
    /// erase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_thread: Option<ThreadWindow>,
}

/// Whether `n` is zero (serde's skip test).
fn is_zero(n: &usize) -> bool {
    *n == 0
}

impl FetchRequest {
    /// A first-page fetch over everything.
    #[must_use]
    pub fn new(query: impl Into<String>, mode: FetchMode, limit: usize) -> Self {
        Self {
            query: query.into(),
            mode,
            filter: MetaFilter::default(),
            limit,
            cursor: None,
            beliefs: 0,
            refers_to: None,
            max_scopes: None,
            exclude_thread: None,
        }
    }

    /// Checks the request is answerable.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRequest`] for a blank query, a zero limit, a zero
    /// [`FetchRequest::max_scopes`], an invalid [`TimeHint`] or a blank
    /// excluded thread.
    pub fn validate(&self) -> Result<()> {
        non_blank("fetch query", &self.query)?;
        positive("fetch limit", self.limit)?;
        if let Some(scopes) = self.max_scopes {
            positive("fetch max_scopes", scopes)?;
        }
        self.exclude_thread
            .as_ref()
            .map_or(Ok(()), ThreadWindow::validate)?;
        self.refers_to.as_ref().map_or(Ok(()), TimeHint::validate)
    }
}

/// One page of fetch results, best first.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FetchPage {
    /// The hits.
    pub hits: Vec<Hit>,
    /// Pass back as [`FetchRequest::cursor`] for the next page; `None` at the
    /// end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// The beliefs [`FetchRequest::beliefs`] asked for, best first: learning
    /// hits tagged [`crate::BELIEF_TAG`] within the filter's reach. Not
    /// items of the page, and not paged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub beliefs: Vec<Hit>,
}

/// One stored item as a read returns it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hit {
    /// The item's id.
    pub id: ItemId,
    /// The item's kind.
    pub kind: ItemKind,
    /// The item's text ([`crate::StoreItem::render_text`] form).
    pub text: String,
    /// The item's metadata.
    pub meta: MemoryMeta,
    /// Relevance; `0.0` in a listing.
    pub score: f32,
    /// A learning's confidence; `None` for documents and conversations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
}

/// A query-free listing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ListRequest {
    /// Which items to list.
    #[serde(default)]
    pub filter: MetaFilter,
    /// Page size; must be positive.
    pub limit: usize,
    /// Continue from a previous page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

impl ListRequest {
    /// A first page of `limit` items matching `filter`.
    #[must_use]
    pub fn new(filter: MetaFilter, limit: usize) -> Self {
        Self {
            filter,
            limit,
            cursor: None,
        }
    }

    /// Checks the request is answerable.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRequest`] for a zero limit.
    pub fn validate(&self) -> Result<()> {
        positive("list limit", self.limit)
    }
}

/// One page of a listing. Every hit's score is `0.0`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ListPage {
    /// The items.
    pub items: Vec<Hit>,
    /// Pass back as [`ListRequest::cursor`] for the next page; `None` at the
    /// end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// One stored item exactly as it was stored, for moving it elsewhere (another
/// engine, or another node of the same one).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Exported {
    /// The id the engine holds it under.
    pub id: ItemId,
    /// The item, whole: storing it again where it was is a replay.
    pub item: StoreItem,
}

/// One page of an export (see [`crate::MemoryEngine::export`]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExportPage {
    /// The items, whole.
    pub items: Vec<Exported>,
    /// Items the engine holds only part of (a chunked document missing a
    /// piece), so cannot hand back whole. Named rather than dropped, so a
    /// caller moving memory knows what it could not move.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub incomplete: Vec<ItemId>,
    /// Pass back as [`ListRequest::cursor`] for the next page; `None` at the
    /// end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// What [`crate::MemoryEngine::erase`] removes: every item of `kinds` (all
/// kinds when empty) at the nodes `reach` names.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EraseRequest {
    /// The nodes erased: `at`, and everything below it when `descendants`.
    /// `inherit` is ignored: an erasure never reaches up.
    pub reach: Reach,
    /// The kinds erased; empty erases every kind.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<ItemKind>,
    /// Must be set to erase the whole tree (the root with its descendants
    /// and every kind): an interlock, because nothing erased comes back.
    #[serde(default)]
    pub whole_tree: bool,
}

impl EraseRequest {
    /// Erases `reach`'s nodes, every kind.
    #[must_use]
    pub fn new(reach: Reach) -> Self {
        Self {
            reach,
            kinds: Vec::new(),
            whole_tree: false,
        }
    }

    /// Checks that the request cannot erase the whole tree by accident.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRequest`] for the root with its descendants and every
    /// kind, unless `whole_tree` is set.
    pub fn validate(&self) -> Result<()> {
        let everything = self.reach.at.is_root() && self.reach.descendants && self.kinds.is_empty();
        if everything && !self.whole_tree {
            return Err(Error::InvalidRequest(
                "erasing the whole tree needs whole_tree: true".to_string(),
            ));
        }
        Ok(())
    }
}

/// What an erasure did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EraseReport {
    /// How many of the engine's scopes (kind stores) were erased.
    pub erased_scopes: usize,
    /// The engine's receipts for them, when it keeps any (CortexDB's erasure
    /// ids), for an audit trail.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub receipts: Vec<String>,
}

/// What [`crate::MemoryEngine::forget`] removes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(
    clippy::large_enum_variant,
    reason = "the contract names the filter by value; a target is built once per call"
)]
pub enum ForgetTarget {
    /// These items, wherever they live: ids are not scoped by namespace, and
    /// an engine may look for them in every node it holds. A caller confined
    /// to a [`crate::Reach`] forgets with
    /// [`crate::MemoryEngine::forget_within`] instead, which looks only
    /// inside that reach.
    Ids(Vec<ItemId>),
    /// Every item matching the filter, which must not be empty.
    Filter(MetaFilter),
}

impl ForgetTarget {
    /// Checks the target cannot mean "everything".
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRequest`] for no ids or an empty filter.
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Ids(ids) if ids.is_empty() => Err(Error::InvalidRequest(
                "forget needs at least one id".to_string(),
            )),
            Self::Filter(filter) if filter.is_empty() => Err(Error::InvalidRequest(
                "forget refuses an empty filter, which would mean everything".to_string(),
            )),
            _ => Ok(()),
        }
    }
}

/// What a forget removed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgetReport {
    /// How many items were removed. Ids that named nothing are not counted.
    pub forgotten: usize,
}

fn non_blank(what: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(Error::InvalidRequest(format!("{what} must not be empty")));
    }
    Ok(())
}

fn positive(what: &str, value: usize) -> Result<()> {
    if value == 0 {
        return Err(Error::InvalidRequest(format!("{what} must be positive")));
    }
    Ok(())
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
