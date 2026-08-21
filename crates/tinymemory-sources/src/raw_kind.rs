//! Which sub-directory of the raw archive an item belongs in.
//!
//! Moved with the readers (#18 §B4) rather than imported, because importing it
//! would mean depending on the engine for one dependency-free enum — the exact
//! coupling this move removes.
//!
//! This is a deliberate second definition, and it is safe because it never
//! crosses a boundary: `raw_archive_coords` is the only consumer, nothing
//! outside this crate calls it, and the engine keeps its own copy for its
//! storage layer. If a caller ever needs to exchange one, that is the moment to
//! lift it into the contract instead.

/// Category of a raw item, selecting the per-kind subdirectory under
/// `raw/<source_slug>/<kind>/`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum RawKind {
    /// Email messages (Gmail, Outlook, …).
    Email,
    /// Chat / DM messages (Slack, Telegram, WhatsApp, Discord, …).
    Chat,
    /// Standalone documents — Notion pages, Drive files, attachments.
    Document,
    /// One file per person reachable via this source.
    Contact,
    /// Long-form posts — LinkedIn posts, tweets, blog entries.
    Post,
    /// Git commits (one file per commit) — GitHub repo sources.
    Commit,
    /// Issues with their conversation + metadata — GitHub repo sources.
    Issue,
    /// Pull requests with their body + metadata — GitHub repo sources.
    PullRequest,
}

impl RawKind {
    /// Directory name used on disk for this kind (plural).
    pub const fn as_dir(&self) -> &'static str {
        match self {
            Self::Email => "emails",
            Self::Chat => "chats",
            Self::Document => "documents",
            Self::Contact => "contacts",
            Self::Post => "posts",
            Self::Commit => "commits",
            Self::Issue => "issues",
            Self::PullRequest => "prs",
        }
    }
}

#[cfg(test)]
#[path = "raw_kind_tests.rs"]
mod tests;
