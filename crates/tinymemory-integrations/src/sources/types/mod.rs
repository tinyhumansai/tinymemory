//! Core types for memory sources.
//!
//! A *memory source* answers the question "what feeds my memory?". Each
//! configured source is a [`MemorySourceEntry`], which the host persists
//! wherever it keeps configuration. The [`SourceKind`] discriminator selects
//! which kind-specific fields are required; [`MemorySourceEntry::validate`]
//! checks them.
//!
//! Reader output contracts ([`SourceItem`], [`SourceContent`], [`ContentType`])
//! are shared across every reader implementation so the host can ingest source
//! payloads uniformly regardless of where they came from; [`crate::sources::items`]
//! turns them into `StoreItem`s.
//!
//! Wire strings are snake_case and are part of the persisted contract — do not
//! rename them when porting from OpenHuman.

#[cfg(feature = "schema")]
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::sources::error::{Error, Result};

pub(crate) fn default_true() -> bool {
    true
}

/// The kind of a configured memory source.
///
/// The wire representation is snake_case (`github_repo`, `rss_feed`, …) and is
/// persisted by hosts; it must stay stable across versions. Each maps
/// onto one [`tinymemory_api::SourceKind`] through [`SourceKind::api_kind`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// Local agent conversation transcripts stored in the workspace.
    Conversation,
    /// A local folder of files matched by an optional glob.
    Folder,
    /// A single local file.
    File,
    /// A GitHub repository's project activity (commits, issues, PRs).
    GithubRepo,
    /// An RSS/Atom feed.
    RssFeed,
    /// A single web page, optionally narrowed by a CSS selector.
    WebPage,
}

impl SourceKind {
    /// Every kind, in declaration order.
    pub const ALL: [Self; 6] = [
        Self::Conversation,
        Self::Folder,
        Self::File,
        Self::GithubRepo,
        Self::RssFeed,
        Self::WebPage,
    ];

    /// The stable snake_case wire string for this kind.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            SourceKind::Conversation => "conversation",
            SourceKind::Folder => "folder",
            SourceKind::File => "file",
            SourceKind::GithubRepo => "github_repo",
            SourceKind::RssFeed => "rss_feed",
            SourceKind::WebPage => "web_page",
        }
    }

    /// The contract's [`tinymemory_api::SourceKind`] for items this kind of
    /// source produces: a web page is a `Link`, a GitHub repository `Github`,
    /// an RSS feed `Rss`; the rest keep their name.
    #[must_use]
    pub fn api_kind(&self) -> tinymemory_api::SourceKind {
        use tinymemory_api::SourceKind as Api;
        match self {
            SourceKind::Conversation => Api::Conversation,
            SourceKind::Folder => Api::Folder,
            SourceKind::File => Api::File,
            SourceKind::GithubRepo => Api::Github,
            SourceKind::RssFeed => Api::Rss,
            SourceKind::WebPage => Api::Link,
        }
    }
}

/// The kinds accepted by OpenHuman's `[[memory.sources]]` configuration.
///
/// This is narrower than [`SourceKind`]: conversation sources are a reader
/// capability, not a user-configured source. The snake_case names are stable
/// persisted values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum MemorySourceKind {
    /// A local directory, walked recursively.
    Folder,
    /// One local file.
    File,
    /// One web page.
    Link,
    /// A GitHub repository (`owner/repo` or URL).
    Github,
    /// An RSS or Atom feed.
    Rss,
}

impl MemorySourceKind {
    /// Every configured kind, in display order.
    pub const ALL: [Self; 5] = [
        Self::Folder,
        Self::File,
        Self::Link,
        Self::Github,
        Self::Rss,
    ];

    /// The stable wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Folder => "folder",
            Self::File => "file",
            Self::Link => "link",
            Self::Github => "github",
            Self::Rss => "rss",
        }
    }

    /// Parses a wire name, ignoring surrounding whitespace and ASCII case.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str().eq_ignore_ascii_case(raw.trim()))
    }

    /// The API source kind emitted by this configured reader.
    #[must_use]
    pub fn source_kind(self) -> tinymemory_api::SourceKind {
        match self {
            Self::Folder => tinymemory_api::SourceKind::Folder,
            Self::File => tinymemory_api::SourceKind::File,
            Self::Link => tinymemory_api::SourceKind::Link,
            Self::Github => tinymemory_api::SourceKind::Github,
            Self::Rss => tinymemory_api::SourceKind::Rss,
        }
    }
}

/// One configured source and its schedule, without runtime sync state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct MemorySourceConfig {
    /// Stable id.
    pub id: String,
    /// What the source reads.
    pub kind: MemorySourceKind,
    /// Path, URL, `owner/repo` or feed URL.
    pub target: String,
    /// Display label; missing labels default to the empty string on the wire.
    #[serde(default)]
    pub label: String,
    /// Minutes between scheduled syncs; unset means on demand only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule_mins: Option<u32>,
    /// Optional namespace root for this source's documents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

/// A configured memory source entry.
///
/// All kind-specific fields are flattened onto the struct as `Option`s. The
/// [`kind`](MemorySourceEntry::kind) discriminator determines which fields are
/// required; [`MemorySourceEntry::validate`] checks them.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct MemorySourceEntry {
    /// Stable unique id (e.g. `src_<uuid>`).
    pub id: String,
    /// Discriminator selecting the kind-specific fields below.
    pub kind: SourceKind,
    /// Human-readable label shown in UIs.
    pub label: String,
    /// Whether this source participates in sync. Defaults to `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,

    // ── Folder / File ──
    /// Filesystem path of the folder or file to read. Required for `folder`
    /// and `file`; a relative path is anchored on the workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Optional glob applied under a folder's `path`. When absent the folder
    /// reader takes markdown, plain-text and source-code files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glob: Option<String>,

    // ── GithubRepo / RssFeed / WebPage (shared) ──
    /// Source URL. Required for `github_repo`, `rss_feed`, and `web_page`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,

    // ── GithubRepo ──
    /// Branch to read (defaults to the repo default when absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Optional path filters within the repo.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    /// Max commits to pull per sync (default 1000 when absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_commits: Option<u32>,
    /// Max issues to pull per sync (default 1000 when absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_issues: Option<u32>,
    /// Max pull requests to pull per sync (default 1000 when absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_prs: Option<u32>,

    // ── RssFeed ──
    /// Max feed items to pull per sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_items: Option<u32>,

    // ── WebPage ──
    /// Optional CSS selector to narrow extracted content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,

    // ── Sync Budget (all source kinds) ──
    /// Maximum tokens to consume per sync run. Sync stops once this budget is hit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens_per_sync: Option<u64>,
    /// Maximum cost in USD per sync run. Refuses LLM calls once reached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_per_sync_usd: Option<f64>,
    /// Sync depth in days — only fetch items from the last N days.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_depth_days: Option<u32>,
}

impl MemorySourceEntry {
    /// An enabled entry of `kind` with every optional field unset.
    #[must_use]
    pub fn new(id: impl Into<String>, kind: SourceKind, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            kind,
            label: label.into(),
            enabled: true,
            path: None,
            glob: None,
            url: None,
            branch: None,
            paths: Vec::new(),
            max_commits: None,
            max_issues: None,
            max_prs: None,
            max_items: None,
            selector: None,
            max_tokens_per_sync: None,
            max_cost_per_sync_usd: None,
            sync_depth_days: None,
        }
    }

    /// Validate the fields this entry's [`SourceKind`] requires.
    ///
    /// `id` and `label` are required for every kind, and `id` must not contain
    /// `:` or control characters. Folders and files need `path`; GitHub repositories, RSS
    /// feeds and web pages need `url`. An empty string counts as missing.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] naming the first failing rule.
    pub fn validate(&self) -> Result<()> {
        if self.id.trim().is_empty() {
            return Err(Error::Invalid("id is required".to_string()));
        }
        if self.id.contains(':') || self.id.chars().any(char::is_control) {
            return Err(Error::Invalid(
                "id must not contain ':' or control characters".to_string(),
            ));
        }
        if self.label.is_empty() {
            return Err(Error::Invalid("label is required".to_string()));
        }
        match self.kind {
            SourceKind::Conversation => Ok(()),
            SourceKind::Folder | SourceKind::File => require_field(&self.path, "path"),
            SourceKind::GithubRepo | SourceKind::RssFeed | SourceKind::WebPage => {
                require_field(&self.url, "url")
            }
        }
    }
}

/// Require that `value` is present and non-empty, naming it `name` in errors.
fn require_field(value: &Option<String>, name: &str) -> Result<()> {
    match value {
        Some(v) if !v.is_empty() => Ok(()),
        _ => Err(Error::Invalid(format!(
            "{name} is required for this source kind"
        ))),
    }
}

/// One item listed from a source reader.
///
/// `id` is reader-scoped (e.g. a folder-relative path or a thread id) and is
/// stable enough to pass back into `SourceReader::read_item`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct SourceItem {
    /// Reader-scoped item id.
    pub id: String,
    /// Human-readable title.
    pub title: String,
    /// Last-modified time in epoch milliseconds, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at_ms: Option<i64>,
}

/// The rendered content type of a [`SourceContent`] body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ContentType {
    /// Markdown body.
    Markdown,
    /// Raw HTML body.
    Html,
    /// Plain text body.
    Plaintext,
}

/// Content read from a single source item.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct SourceContent {
    /// Reader-scoped item id (matches the [`SourceItem::id`] it was read from).
    pub id: String,
    /// Human-readable title.
    pub title: String,
    /// The item body, rendered as [`content_type`](SourceContent::content_type).
    pub body: String,
    /// How [`body`](SourceContent::body) should be interpreted.
    pub content_type: ContentType,
    /// Reader-specific metadata (JSON object).
    #[serde(default)]
    pub metadata: serde_json::Value,
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
