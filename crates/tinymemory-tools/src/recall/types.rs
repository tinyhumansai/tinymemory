//! The request and result types of a holistic recall.

use serde::{Deserialize, Serialize};
use tinymemory_api::{Error, Hit, ItemId, MetaFilter, Result, TimeHint};

/// Default token budget of a context pack.
pub const DEFAULT_PACK_BUDGET_TOKENS: usize = 1_200;

/// Default heading of a context pack's markdown.
pub const DEFAULT_PACK_TITLE: &str = "Memory";

/// How one section is filled.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "by", rename_all = "snake_case")]
pub enum SectionQuery {
    /// Ranked retrieval ([`tinymemory_api::MemoryEngine::fetch`]) for the
    /// pack's query, or for `query` when the section names its own. With no
    /// query at all the section reads [`SectionQuery::Latest`] instead. The
    /// hot-path choice: no model runs.
    Fetch {
        /// This section's own query, overriding the pack's.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query: Option<String>,
    },
    /// A synthesised answer ([`tinymemory_api::MemoryEngine::recall`]) to a
    /// fixed question. Slower: an engine may run a model.
    Answer {
        /// The question.
        question: String,
        /// Extra guidance for the answer.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instructions: Option<String>,
        /// When the answer fails, fetch for the question instead of skipping
        /// the section.
        #[serde(default)]
        fallback_to_fetch: bool,
    },
    /// The newest items, then the most confident, with no query: what a
    /// learnings list or a recent-history section wants.
    Latest,
}

/// One section of a context pack: a heading, the scope it reads, and how it
/// is filled.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScopeSection {
    /// The section's `##` heading.
    pub heading: String,
    /// Which items the section reads: its reach and kinds are its scope.
    #[serde(default)]
    pub filter: MetaFilter,
    /// The most hits (or citations) the section gathers.
    pub limit: usize,
    /// How the section is filled.
    pub query: SectionQuery,
    /// Read at most this many of the scopes the filter reaches (see
    /// [`tinymemory_api::FetchRequest::max_scopes`]); `None` reads them all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_scopes: Option<usize>,
}

impl ScopeSection {
    /// This section, reading at most `scopes` of the scopes it reaches.
    #[must_use]
    pub fn with_max_scopes(mut self, scopes: usize) -> Self {
        self.max_scopes = Some(scopes);
        self
    }

    /// A section of ranked hits for the pack's query.
    #[must_use]
    pub fn fetch(heading: impl Into<String>, filter: MetaFilter, limit: usize) -> Self {
        Self {
            heading: heading.into(),
            filter,
            limit,
            query: SectionQuery::Fetch { query: None },
            max_scopes: None,
        }
    }

    /// A section answering `question`.
    #[must_use]
    pub fn answer(
        heading: impl Into<String>,
        question: impl Into<String>,
        filter: MetaFilter,
        limit: usize,
    ) -> Self {
        Self {
            heading: heading.into(),
            filter,
            limit,
            query: SectionQuery::Answer {
                question: question.into(),
                instructions: None,
                fallback_to_fetch: false,
            },
            max_scopes: None,
        }
    }

    /// A section of the newest items.
    #[must_use]
    pub fn latest(heading: impl Into<String>, filter: MetaFilter, limit: usize) -> Self {
        Self {
            heading: heading.into(),
            filter,
            limit,
            query: SectionQuery::Latest,
            max_scopes: None,
        }
    }

    fn validate(&self) -> Result<()> {
        if self.heading.trim().is_empty() {
            return Err(Error::InvalidRequest(
                "every recall section needs a heading".to_string(),
            ));
        }
        if self.limit == 0 {
            return Err(Error::InvalidRequest(format!(
                "recall section `{}` has a zero limit",
                self.heading
            )));
        }
        if self.max_scopes == Some(0) {
            return Err(Error::InvalidRequest(format!(
                "recall section `{}` reads zero scopes",
                self.heading
            )));
        }
        if let SectionQuery::Answer { question, .. } = &self.query
            && question.trim().is_empty()
        {
            return Err(Error::InvalidRequest(format!(
                "recall section `{}` answers a blank question",
                self.heading
            )));
        }
        Ok(())
    }
}

/// The turns a recall leaves out; defined beside the read requests that
/// also carry it ([`tinymemory_api::RecallRequest::exclude_thread`]).
pub use tinymemory_api::ThreadWindow;

/// A recall across several scopes at once, rendered into one budgeted
/// markdown block: the single read every lifecycle step is built from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HolisticRecall {
    /// The query [`SectionQuery::Fetch`] sections rank for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// The sections, highest priority first: budget trimming takes from the
    /// last.
    pub sections: Vec<ScopeSection>,
    /// The most tokens the rendered block may take, four characters per
    /// token.
    pub budget_tokens: usize,
    /// The block's `#` heading.
    pub title: String,
    /// Items never included (the turn just logged, say).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_ids: Vec<ItemId>,
    /// Conversation turns never included because the prompt holds them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_thread: Option<ThreadWindow>,
    /// The days the query is about, when known before the read: fetch
    /// sections rank their hits from those days first (see
    /// [`tinymemory_api::TimeHint`]). A date that arrives later goes to
    /// [`crate::recall::holistic_recall_dated`] instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refers_to: Option<TimeHint>,
}

impl HolisticRecall {
    /// A recall of `sections` for `query`, with the default budget and title.
    #[must_use]
    pub fn new(query: Option<String>, sections: Vec<ScopeSection>) -> Self {
        Self {
            query,
            sections,
            budget_tokens: DEFAULT_PACK_BUDGET_TOKENS,
            title: DEFAULT_PACK_TITLE.to_string(),
            exclude_ids: Vec::new(),
            exclude_thread: None,
            refers_to: None,
        }
    }

    /// Checks the request.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRequest`] for a zero budget, a blank title, or a
    /// section with a blank heading, a zero limit or a blank question.
    pub fn validate(&self) -> Result<()> {
        if self.budget_tokens == 0 {
            return Err(Error::InvalidRequest(
                "a recall budget must be positive".to_string(),
            ));
        }
        if self.title.trim().is_empty() {
            return Err(Error::InvalidRequest(
                "a recall block needs a title".to_string(),
            ));
        }
        self.sections.iter().try_for_each(ScopeSection::validate)?;
        self.refers_to.as_ref().map_or(Ok(()), TimeHint::validate)
    }

    /// Whether `hit` is left out by [`HolisticRecall::exclude_ids`] or
    /// [`HolisticRecall::exclude_thread`].
    pub(crate) fn excludes(&self, hit: &Hit) -> bool {
        self.exclude_ids.contains(&hit.id)
            || self
                .exclude_thread
                .as_ref()
                .is_some_and(|window| window.covers(hit))
    }
}

/// What one section gathered, before budget trimming.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SectionHits {
    /// The section's heading.
    pub heading: String,
    /// The synthesised answer, for an answered section.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    /// The hits, best first (an answered section's citations as hits).
    pub hits: Vec<Hit>,
}

/// A section that contributed nothing, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "error", rename_all = "snake_case")]
pub enum SkipReason {
    /// The section read succeeded but returned no hits.
    Empty,
    /// The engine refused or failed the section read.
    Engine(Error),
}

/// A section that contributed nothing, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkippedSection {
    /// The section's heading.
    pub heading: String,
    /// Typed reason, retaining the engine error category and cause.
    pub reason: SkipReason,
}

/// A rendered holistic recall.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ContextPack {
    /// The block to inject into a prompt; empty when nothing was found.
    pub markdown: String,
    /// Its estimated tokens.
    pub tokens: usize,
    /// Every item the block cites, in order of first citation.
    pub refs: Vec<ItemId>,
    /// Everything each section gathered, in section order, including what
    /// the budget trimmed from the block.
    pub sections: Vec<SectionHits>,
    /// Sections that contributed nothing.
    pub skipped: Vec<SkippedSection>,
    /// The id of the engine read.
    pub engine: String,
}

impl ContextPack {
    /// Whether the block is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.markdown.is_empty()
    }
}
