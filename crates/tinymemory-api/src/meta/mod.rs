//! Typed metadata carried by every stored item.
//!
//! [`MemoryMeta`] says where an item came from and what it is about: the
//! workspace, folder and file it was read from, the code language, the
//! repository and commit, the conversation thread and turns, the agent and tool
//! call that produced it, and the [`SourceRef`] that names its reader. Every
//! field is optional except the source, so a reader fills in what it knows.
//!
//! [`MetaFilter`] is the matching query side: the same fields as exact
//! matches (with `folder` and `file_path` also matching as a path prefix), plus
//! item kinds, source kinds, a tag set and an observation window.

mod filter;

pub use filter::MetaFilter;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::namespace::Namespace;

/// Where an item came from and what it is about.
///
/// Fields are added as the contract grows, so build one with
/// `..MemoryMeta::default()` for the fields you do not set. A literal that
/// names every field stops compiling when a field is added, which is a
/// major release.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryMeta {
    /// The memory node the item belongs to; the root (shared by every
    /// agent) by default. See [`crate::namespace`].
    #[serde(skip_serializing_if = "Namespace::is_root")]
    pub namespace: Namespace,
    /// Absolute path or logical workspace id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// Containing folder, absolute or workspace-relative.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    /// The file the item was read from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    /// Code language (`rust`, `python`) or natural-language tag (`en`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Repository, as `owner/name` or a remote URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Commit the item was read at.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// URL the item was read from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Conversation thread the item belongs to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    /// Channel the conversation or message came from, such as `telegram`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// Which turns of the thread the item covers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turns: Option<TurnRange>,
    /// Agent that produced the item.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Tool call that produced the item.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call: Option<ToolCallRef>,
    /// The reader or producer that supplied the item.
    pub source: SourceRef,
    /// Free-form tags.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// When the underlying fact was observed, as opposed to when it was stored.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<DateTime<Utc>>,
    /// Whether an engine that derives layers from what it stores (facts,
    /// beliefs, concepts) may derive them from this item. `Some(false)`
    /// stores and indexes the item, so it stays searchable, but derives
    /// nothing from it: runtime state, tool output, machine-written
    /// summaries. `None`, the default, leaves it to the engine; engines that
    /// derive nothing ignore it. Unset, it is not serialized, so an item's
    /// fingerprint is the same as before the field existed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub derive: Option<bool>,
    /// Who actually said or did what the item records, when that is someone
    /// other than the person whose memory it is: the sender of a synced
    /// email. `None`, the default, means the memory's owner. An engine that
    /// can attribute an event to its actor may send it as such; any other
    /// ignores it. Unset, it is not serialized, and it is never part of an
    /// item's fingerprint, so setting it does not change the item's id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_actor: Option<ObservedActor>,
}

/// A person (or system) an item is attributed to: [`MemoryMeta::observed_actor`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedActor {
    /// `type:id`, as CortexDB names actors: `user:priya@acme.com`.
    pub id: String,
    /// The display name, when the source gives one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl ObservedActor {
    /// Builds an actor from the sender of a channel message.
    ///
    /// Phone numbers on phone-addressed channels become `user:<dial digits>`;
    /// email addresses become lower-cased `user:<email>` ids; other single
    /// token senders become `<channel>:<sender>`. The supported phone channel
    /// vocabulary is kept in this API so memory does not depend on a host's
    /// channel runtime. A blank channel or sender, or a non-phone sender
    /// containing whitespace, has no actor id and returns `None`.
    #[must_use]
    pub fn for_channel_sender(channel: &str, sender: &str, name: Option<&str>) -> Option<Self> {
        let name = name
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string);
        Some(Self {
            id: channel_actor_id(channel, sender)?,
            name,
        })
    }
}

/// Phone-addressed channels. This vocabulary stays local to TinyMemory to
/// avoid a dependency from the memory contract on any channel host crate.
const PHONE_CHANNELS: &[&str] = &["whatsapp", "signal", "imessage", "linq", "sms"];

/// `user:<phone or email>` or `<channel>:<sender>`; `None` when either is
/// blank or the sender is not one token.
fn channel_actor_id(channel: &str, sender: &str) -> Option<String> {
    let channel = channel.trim().to_ascii_lowercase();
    let sender = sender.trim();
    if channel.is_empty() || channel.contains(char::is_whitespace) || sender.is_empty() {
        return None;
    }
    if PHONE_CHANNELS.contains(&channel.as_str()) && looks_like_phone(sender) {
        return Some(format!("user:{}", dial_digits(sender)));
    }
    if sender.contains(char::is_whitespace) {
        return None;
    }
    if is_email(sender) {
        return Some(format!("user:{}", sender.to_ascii_lowercase()));
    }
    Some(format!("{channel}:{sender}"))
}

/// Only phone characters, with seven or more digits.
fn looks_like_phone(text: &str) -> bool {
    text.chars()
        .all(|c| c.is_ascii_digit() || "+-(). ".contains(c))
        && text.chars().filter(char::is_ascii_digit).count() >= 7
}

/// `text`'s digits, with its leading `+` when it has one.
fn dial_digits(text: &str) -> String {
    let digits: String = text.chars().filter(char::is_ascii_digit).collect();
    if text.starts_with('+') {
        format!("+{digits}")
    } else {
        digits
    }
}

/// One `@`, a non-empty local part and a dotted domain: a WhatsApp
/// `<lid>@lid` is not an email address.
fn is_email(text: &str) -> bool {
    text.split_once('@').is_some_and(|(local, domain)| {
        !local.is_empty() && domain.contains('.') && !domain.contains('@')
    })
}

impl MemoryMeta {
    /// Metadata naming only its source.
    #[must_use]
    pub fn from_source(kind: SourceKind, id: Option<String>) -> Self {
        Self {
            source: SourceRef { kind, id },
            ..Self::default()
        }
    }

    /// Overlays values known by the host onto this metadata.
    ///
    /// Set optional fields from `host` replace their values here. The host's
    /// namespace and source always replace these values, including the root
    /// namespace and default source. Host tags append after caller tags, with
    /// duplicates omitted while preserving order.
    #[must_use]
    pub fn overlay(mut self, mut host: Self) -> Self {
        macro_rules! overlay_options {
            ($($field:ident),* $(,)?) => {
                $(
                    if host.$field.is_some() {
                        self.$field = host.$field.take();
                    }
                )*
            };
        }

        overlay_options!(
            workspace,
            folder,
            file_path,
            language,
            repo,
            commit,
            url,
            thread_id,
            channel,
            turns,
            agent_id,
            tool_call,
            observed_at,
            derive,
            observed_actor
        );
        self.namespace = host.namespace;
        self.source = host.source;
        for tag in host.tags {
            if !self.tags.contains(&tag) {
                self.tags.push(tag);
            }
        }
        self
    }
}

/// An inclusive range of conversation turns, zero-based.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnRange {
    /// First turn covered.
    pub first: u32,
    /// Last turn covered, inclusive.
    pub last: u32,
}

/// A reference to one tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallRef {
    /// The tool's name.
    pub name: String,
    /// The call's id, when the producer assigned one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

/// Which reader or producer supplied an item.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRef {
    /// The kind of source.
    pub kind: SourceKind,
    /// The source's own id (a configured source id, a feed URL, a thread id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

/// The kind of reader or producer an item came from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// A local folder.
    Folder,
    /// A single file.
    File,
    /// A web page.
    Link,
    /// A GitHub repository.
    Github,
    /// An RSS or Atom feed.
    Rss,
    /// A host conversation.
    Conversation,
    /// Written by an agent directly; the default.
    #[default]
    Agent,
    /// Imported from a legacy store.
    ///
    /// The retired `composio` kind decodes to this variant, so items stored
    /// under it stay readable.
    #[serde(alias = "composio")]
    Import,
}

impl SourceKind {
    /// Every kind, in declaration order.
    pub const ALL: [Self; 8] = [
        Self::Folder,
        Self::File,
        Self::Link,
        Self::Github,
        Self::Rss,
        Self::Conversation,
        Self::Agent,
        Self::Import,
    ];

    /// The stable snake_case wire string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Folder => "folder",
            Self::File => "file",
            Self::Link => "link",
            Self::Github => "github",
            Self::Rss => "rss",
            Self::Conversation => "conversation",
            Self::Agent => "agent",
            Self::Import => "import",
        }
    }
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
