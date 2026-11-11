//! The three kinds of item a host stores: documents, conversations and
//! learnings.
//!
//! A [`StoreItem`] is the unit of [`crate::MemoryEngine::store`]. Each variant
//! carries its own [`MemoryMeta`]. [`StoreItem::fingerprint`] is the
//! content-derived identity an engine uses so that storing an identical item
//! twice is a replay ([`StoreReceipt::replayed`]) rather than a duplicate.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::meta::{MemoryMeta, ToolCallRef};

/// An engine-assigned item id. Opaque to the host.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ItemId(pub String);

impl ItemId {
    /// Wraps an id.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ItemId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for ItemId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl From<String> for ItemId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// Which of the three item kinds a stored item is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    /// A document: a file, a page, a payload converted to markdown.
    Document,
    /// A conversation: an ordered list of turns.
    Conversation,
    /// A learning: one distilled statement about the user or the world.
    Learning,
}

impl ItemKind {
    /// Every kind, in declaration order.
    pub const ALL: [Self; 3] = [Self::Document, Self::Conversation, Self::Learning];

    /// The stable snake_case wire string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Document => "document",
            Self::Conversation => "conversation",
            Self::Learning => "learning",
        }
    }
}

/// One item to store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StoreItem {
    /// A document.
    Document {
        /// Title, when the source has one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        /// The body; must be [`DocumentBody::Text`] by the time it reaches an
        /// engine.
        body: DocumentBody,
        /// The body's MIME type, when known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime: Option<String>,
        /// Metadata.
        #[serde(default)]
        meta: MemoryMeta,
    },
    /// A conversation.
    Conversation {
        /// The turns, in order.
        turns: Vec<Turn>,
        /// Metadata.
        #[serde(default)]
        meta: MemoryMeta,
    },
    /// A learning.
    Learning {
        /// The statement.
        text: String,
        /// What kind of statement it is.
        kind: LearningKind,
        /// Confidence in `0.0..=1.0`.
        confidence: f32,
        /// What supports it, when recorded.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        evidence: Option<String>,
        /// Metadata.
        #[serde(default)]
        meta: MemoryMeta,
    },
}

impl StoreItem {
    /// A text document with no title or MIME type.
    #[must_use]
    pub fn document(text: impl Into<String>, meta: MemoryMeta) -> Self {
        Self::Document {
            title: None,
            body: DocumentBody::Text(text.into()),
            mime: None,
            meta,
        }
    }

    /// A learning with no evidence.
    #[must_use]
    pub fn learning(
        text: impl Into<String>,
        kind: LearningKind,
        confidence: f32,
        meta: MemoryMeta,
    ) -> Self {
        Self::Learning {
            text: text.into(),
            kind,
            confidence,
            evidence: None,
            meta,
        }
    }

    /// The item's kind.
    #[must_use]
    pub fn kind(&self) -> ItemKind {
        match self {
            Self::Document { .. } => ItemKind::Document,
            Self::Conversation { .. } => ItemKind::Conversation,
            Self::Learning { .. } => ItemKind::Learning,
        }
    }

    /// A learning's confidence; `None` for documents and conversations.
    #[must_use]
    pub fn confidence(&self) -> Option<f32> {
        match self {
            Self::Learning { confidence, .. } => Some(*confidence),
            Self::Document { .. } | Self::Conversation { .. } => None,
        }
    }

    /// The item's metadata.
    #[must_use]
    pub fn meta(&self) -> &MemoryMeta {
        match self {
            Self::Document { meta, .. }
            | Self::Conversation { meta, .. }
            | Self::Learning { meta, .. } => meta,
        }
    }

    /// The item's metadata, mutably.
    pub fn meta_mut(&mut self) -> &mut MemoryMeta {
        match self {
            Self::Document { meta, .. }
            | Self::Conversation { meta, .. }
            | Self::Learning { meta, .. } => meta,
        }
    }

    /// The item as one readable text: a document's title and body, a
    /// conversation's turns as `role: text` lines, a learning's statement.
    #[must_use]
    pub fn render_text(&self) -> String {
        match self {
            Self::Document { title, body, .. } => {
                let body = match body {
                    DocumentBody::Text(text) | DocumentBody::Uri(text) => text.as_str(),
                };
                match title {
                    Some(title) if !title.trim().is_empty() => format!("# {title}\n\n{body}"),
                    _ => body.to_string(),
                }
            }
            Self::Conversation { turns, .. } => turns
                .iter()
                .map(Turn::render)
                .collect::<Vec<_>>()
                .join("\n"),
            Self::Learning { text, .. } => text.clone(),
        }
    }

    /// Checks the item is storable.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRequest`] for a document whose body is empty or still a
    /// [`DocumentBody::Uri`], a conversation with no turns or an empty turn, a
    /// blank learning, or a confidence outside `0.0..=1.0`.
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Document { body, .. } => match body {
                DocumentBody::Text(text) if text.trim().is_empty() => Err(Error::InvalidRequest(
                    "document body must not be empty".to_string(),
                )),
                DocumentBody::Text(_) => Ok(()),
                DocumentBody::Uri(_) => Err(Error::InvalidRequest(
                    "document body is an unresolved uri; resolve it through a source first"
                        .to_string(),
                )),
            },
            Self::Conversation { turns, .. } => {
                if turns.is_empty() {
                    return Err(Error::InvalidRequest(
                        "conversation must have at least one turn".to_string(),
                    ));
                }
                if turns.iter().any(|turn| turn.text.trim().is_empty()) {
                    return Err(Error::InvalidRequest(
                        "conversation turns must not be empty".to_string(),
                    ));
                }
                Ok(())
            }
            Self::Learning {
                text, confidence, ..
            } => {
                if text.trim().is_empty() {
                    return Err(Error::InvalidRequest(
                        "learning text must not be empty".to_string(),
                    ));
                }
                if !(0.0..=1.0).contains(confidence) {
                    return Err(Error::InvalidRequest(format!(
                        "learning confidence {confidence} is outside 0.0..=1.0"
                    )));
                }
                Ok(())
            }
        }
    }

    /// A stable hex digest of the whole item, metadata included, except
    /// `meta.observed_at`, `meta.observed_actor`, `meta.tool_call.id`, and a
    /// conversation's `meta.channel`.
    ///
    /// Two items with the same fingerprint are the same item: an engine
    /// derives its idempotency from this, so an identical retry is a replay.
    /// `observed_at` records *when* the item was seen, not *what* it is: a
    /// host stamps it on every store, so hashing it would make a retried
    /// learning, or an unchanged file re-synced, a new item each time.
    /// `observed_actor` says who said it, which an engine sends only when
    /// attribution is on: hashing it would make the same email a second
    /// item once attribution is turned on.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        self.fingerprint_with_channel(false)
    }

    /// The former conversation fingerprint, which included its channel.
    ///
    /// Engines use this only to find items persisted before conversation
    /// fingerprints became channel-neutral. New writes must use
    /// [`Self::fingerprint`]. Returns `None` unless this is a conversation
    /// with a channel.
    #[doc(hidden)]
    #[must_use]
    pub fn legacy_conversation_fingerprint(&self) -> Option<String> {
        matches!(self, Self::Conversation { meta, .. } if meta.channel.is_some())
            .then(|| self.fingerprint_with_channel(true))
    }

    fn fingerprint_with_channel(&self, include_channel: bool) -> String {
        let mut identity = self.clone();
        identity.meta_mut().observed_at = None;
        identity.meta_mut().observed_actor = None;
        // The provider assigns a fresh invocation id on every call. It is
        // provenance, not learning content: retrying a successful write must
        // replay the same item even when the model issues a new tool call.
        if let Some(call) = identity.meta_mut().tool_call.as_mut() {
            call.id = None;
        }
        // Channel identifies where a conversation turn was observed, not the
        // thread/turn itself. Adding a channel to an existing log must not
        // create a second copy of the same turn.
        if !include_channel && let Self::Conversation { meta, .. } = &mut identity {
            meta.channel = None;
        }
        // Serialising a struct cannot fail: every field is a plain string,
        // number, enum or timestamp. The fallback keeps the function total.
        let bytes =
            serde_json::to_vec(&identity).unwrap_or_else(|_| format!("{identity:?}").into_bytes());
        let digest = Sha256::digest(&bytes);
        digest
            .iter()
            .take(20)
            .fold(String::with_capacity(40), |mut out, byte| {
                out.push_str(&format!("{byte:02x}"));
                out
            })
    }
}

/// A document's body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentBody {
    /// The text itself, normally markdown.
    Text(String),
    /// Where the text lives. Sources resolve it to [`DocumentBody::Text`]
    /// before store; an engine refuses it.
    Uri(String),
}

/// One conversation turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Turn {
    /// Who spoke.
    pub role: Role,
    /// What was said.
    pub text: String,
    /// When, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<DateTime<Utc>>,
    /// Tool calls the turn made.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallRef>,
}

impl Turn {
    /// A turn with no timestamp and no tool calls.
    #[must_use]
    pub fn new(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            text: text.into(),
            at: None,
            tool_calls: Vec::new(),
        }
    }

    /// The turn as one `role: text` line, followed by the tool calls it made
    /// as ` [tools: name (id), …]` (the id only when one was assigned) so they
    /// stay visible and searchable in fetch and list results.
    #[must_use]
    pub fn render(&self) -> String {
        let line = format!("{}: {}", self.role.as_str(), self.text);
        if self.tool_calls.is_empty() {
            return line;
        }
        let calls = self
            .tool_calls
            .iter()
            .map(|call| match &call.id {
                Some(id) => format!("{} ({id})", call.name),
                None => call.name.clone(),
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("{line} [tools: {calls}]")
    }
}

/// Who spoke a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The human.
    User,
    /// The assistant.
    Assistant,
    /// A system message.
    System,
    /// A tool result.
    Tool,
}

impl Role {
    /// The stable snake_case wire string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::System => "system",
            Self::Tool => "tool",
        }
    }
}

/// What kind of statement a learning is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LearningKind {
    /// How the user likes things done.
    Preference,
    /// Something true about the user or the world.
    Fact,
    /// How to do something.
    Procedure,
    /// A correction of an earlier mistake.
    Correction,
    /// Anything else.
    Other,
}

impl LearningKind {
    /// The stable snake_case wire string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Preference => "preference",
            Self::Fact => "fact",
            Self::Procedure => "procedure",
            Self::Correction => "correction",
            Self::Other => "other",
        }
    }
}

/// What a store returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreReceipt {
    /// The stored item's id.
    pub id: ItemId,
    /// Whether the engine already held this exact item and wrote nothing.
    pub replayed: bool,
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
