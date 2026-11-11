//! [`AgentMemory`]: one agent's memory lifecycle over any engine.
//!
//! A host calls it at fixed points of the agent loop. Every read is a
//! [`crate::recall`] over the [`MemoryLayout`]'s scopes; every write is one
//! turn at this agent's node; every slow step is a [`BackgroundJob`] handed
//! back for the host to run.
//!
//! | When | Call | Engine work |
//! | --- | --- | --- |
//! | session start, before any user turn | [`AgentMemory::start_session`] | reads only |
//! | user turn, before the model | [`AgentMemory::pre_turn`] | logs the turn (accepted, not indexed) while fetching the pack |
//! | first user turn after a compaction | [`AgentMemory::pre_turn_resumed`] | as `pre_turn`, with the thread's earlier turns leading the same pack |
//! | after the reply | [`AgentMemory::post_turn`] | logs the reply; may return a belief build (never on an engine that builds on its own) |
//! | prompt truncated | [`AgentMemory::recall_for_compaction`] | an answered summary of the thread, plus related memory |
//! | any time | [`AgentMemory::recall`] | a pre-turn pack without logging |
//! | off the turn | [`AgentMemory::run_background`] | the job |
//!
//! The hot path — `pre_turn` and `post_turn` — never waits for indexing and
//! never runs a model: writes use [`tinymemory_api::WaitFor::Accepted`] and the pack is
//! ranked retrieval ([`SectionQuery::Fetch`]). Logging runs concurrently
//! with the read, and the pack never contains the turn being logged or the
//! part of the thread still in the prompt.
//!
//! A pack's sections, highest priority first (budget trimming takes from the
//! last): **Learnings**, one section per [`CoreScope`], **Brain**, **This
//! agent's history**, and **Team conversations** (other agents' turns). An
//! item appears once, in the first section that found it.
//!
//! Core scopes share memory above the layout, such as a hive-wide core or a
//! company brain. They are empty by default; [`AgentMemory::with_core`] sets
//! them, and because the memory is cheap to clone a host can override them
//! for one call: `memory.clone().with_core(scopes)?.pre_turn(turn)`. Only the
//! host writes there, through [`AgentMemory::promote`]; the model's tools
//! never choose a node.
//!
//! Each turn is stored as its own one-turn conversation item carrying the
//! thread id, the turn's index, the agent id and the `conversation` source,
//! so a retried call with the same input is a replay, not a duplicate.
//!
//! # Example
//!
//! ```
//! use std::sync::Arc;
//! use tinymemory_api::conformance::ReferenceEngine;
//! use tinymemory_tools::{
//!     AgentMemory, Brain, BrainDocument, BrainSource, MemoryLayout, PostTurn, PreTurn,
//! };
//!
//! # let runtime = tokio::runtime::Builder::new_current_thread().build()?;
//! # runtime.block_on(async {
//! let engine = Arc::new(ReferenceEngine::new());
//! let layout = MemoryLayout::default();
//! Brain::new(engine.clone(), layout.clone())
//!     .ingest(BrainDocument::new(BrainSource::Markdown, "Refunds take five business days."))
//!     .await?;
//!
//! let memory = AgentMemory::new(engine, layout, "support-01")?;
//! let turn = memory
//!     .pre_turn(PreTurn::new("thread-1", 0, "How long do refunds take?"))
//!     .await?;
//! assert!(turn.pack.markdown.contains("Refunds take five business days."));
//! assert!(turn.logged.is_some());
//!
//! let report = memory
//!     .post_turn(PostTurn::new("thread-1", 1, "Five business days."))
//!     .await?;
//! assert!(!report.receipt.replayed);
//! # Ok::<(), tinymemory_api::Error>(())
//! # })?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod types;

pub mod date_hint;
pub mod turn;
pub mod window;

use std::future::Future;
use std::sync::Arc;

use futures::future::join;
use tinymemory_api::{
    ConsolidateRequest, Error, ExploreRequest, Facet, ItemKind, MemoryEngine, MemoryMeta,
    MetaFilter, Namespace, ObservedActor, Reach, Result, Role, SourceKind, SourceRef, StoreItem,
    StoreReceipt, TimeHint, Turn, TurnRange, WriteOptions,
};

use crate::background::{BackgroundJob, BackgroundRunner, JobReport, builds_on_its_own};
use crate::brain::Brain;
use crate::layout::{CoreScope, MemoryLayout};
use crate::recall::{
    ContextPack, HolisticRecall, LateHint, ScopeSection, SectionQuery, ThreadWindow,
};
use crate::tools::MemoryTools;

pub use types::{
    AgentCount, Compaction, DEFAULT_TURN_BUDGET_TOKENS, PostTurn, PostTurnReport, PreTurn, Preview,
    PreviewMode, RecallPolicy, RecallSettings, SessionStart, TurnContext,
};

/// Heading of the learnings section.
pub const LEARNINGS_HEADING: &str = "Learnings";
/// Heading of the brain section.
pub const BRAIN_HEADING: &str = "Brain";
/// Heading of this agent's own past conversations.
pub const HISTORY_HEADING: &str = "This agent's history";
/// Heading of the other agents' conversations.
pub const TEAM_HEADING: &str = "Team conversations";
/// The most brain scopes a turn's pack reads: the ones its query names
/// first, then the most recently written
/// ([`tinymemory_api::FetchRequest::max_scopes`]). Each scope is a separate
/// ranking on an engine that keeps scopes apart, so a brain split per
/// connector and repository would otherwise cost a read per scope on every
/// turn.
pub const BRAIN_SCOPES_PER_TURN: usize = 4;
/// Heading of a resumed thread's own turns.
pub const THREAD_HEADING: &str = "Earlier in this thread";
/// Heading of a compaction's summary.
pub const SUMMARY_HEADING: &str = "Earlier in this conversation";

/// Title of every pack.
const PACK_TITLE: &str = "Memory";

/// Longest the gist of dropped turns used as a query may be, in characters.
const MAX_GIST_CHARS: usize = 600;

/// The fewest characters of each dropped turn the gist keeps.
const MIN_GIST_SHARE: usize = 40;

/// The most turns a compaction's summary reads.
const MAX_SUMMARY_TURNS: usize = 24;

/// The question a compaction's summary answers.
const SUMMARY_QUESTION: &str = "What was discussed, decided and left open earlier in this \
     conversation?";

/// Guidance for a compaction's summary.
const SUMMARY_INSTRUCTIONS: &str = "Summarise briefly as markdown bullets: facts the user \
     gave, decisions made, and open questions. State only what the stored turns support.";

/// One agent's memory: its layout, its node, its engine and its policy.
/// Cheap to clone.
#[derive(Clone)]
pub struct AgentMemory {
    engine: Arc<dyn MemoryEngine>,
    layout: MemoryLayout,
    agent_id: String,
    node: Namespace,
    policy: RecallPolicy,
    core: Vec<CoreScope>,
}

impl std::fmt::Debug for AgentMemory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentMemory")
            .field("engine", &self.engine.descriptor().id)
            .field("agent_id", &self.agent_id)
            .field("node", &self.node.to_string())
            .field("policy", &self.policy)
            .field("core", &self.core)
            .finish()
    }
}

impl AgentMemory {
    /// The memory of `agent_id` in `layout` on `engine`, with the default
    /// [`RecallPolicy`].
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRequest`] for a blank agent id.
    pub fn new(
        engine: Arc<dyn MemoryEngine>,
        layout: MemoryLayout,
        agent_id: &str,
    ) -> Result<Self> {
        let agent_id = agent_id.trim();
        if agent_id.is_empty() {
            return Err(Error::InvalidRequest(
                "an agent's memory needs an agent id".to_string(),
            ));
        }
        Ok(Self {
            node: layout.conversations(agent_id)?,
            engine,
            layout,
            agent_id: agent_id.to_string(),
            policy: RecallPolicy::default(),
            core: Vec::new(),
        })
    }

    /// The same memory under `policy`.
    #[must_use]
    pub fn with_policy(mut self, policy: RecallPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// The same memory recalling `core` as well, replacing any core scopes it
    /// had; an empty list drops them. Clone first to override them for one
    /// call.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRequest`] for a scope that is not a strict ancestor of
    /// the layout root ([`MemoryLayout::admits_core`]), a blank heading, or a
    /// node named twice.
    pub fn with_core(mut self, core: Vec<CoreScope>) -> Result<Self> {
        for (index, scope) in core.iter().enumerate() {
            self.layout.admits_core(&scope.at)?;
            if scope.heading.trim().is_empty() {
                return Err(Error::InvalidRequest(format!(
                    "the core scope `{}` needs a heading",
                    scope.at
                )));
            }
            if core[..index].iter().any(|earlier| earlier.at == scope.at) {
                return Err(Error::InvalidRequest(format!(
                    "the core scope `{}` is named twice",
                    scope.at
                )));
            }
        }
        self.core = core;
        Ok(self)
    }

    /// The core scopes recalled alongside the layout, in section order.
    #[must_use]
    pub fn core(&self) -> &[CoreScope] {
        &self.core
    }

    /// Writes `item` into the core scope at `scope`, for every agent below it
    /// to recall: a hive-wide learning or a company brain document. The
    /// item's namespace is set to `scope` whatever it was. Waits as
    /// [`MemoryEngine::store`] does.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRequest`] for a node that is not one of this memory's
    /// core scopes, or a conversation (turns stay at the agent's node); the
    /// engine's failure to store.
    pub async fn promote(&self, scope: &Namespace, mut item: StoreItem) -> Result<StoreReceipt> {
        let Some(core) = self.core.iter().find(|core| &core.at == scope) else {
            return Err(Error::InvalidRequest(format!(
                "`{scope}` is not a core scope of this memory"
            )));
        };
        let kind = item.kind();
        if kind == ItemKind::Conversation {
            return Err(Error::InvalidRequest(
                "only learnings and documents are promoted to a core scope".to_string(),
            ));
        }
        if !core.kinds.is_empty() && !core.kinds.contains(&kind) {
            return Err(Error::InvalidRequest(format!(
                "the core scope `{scope}` does not read {kind:?} items"
            )));
        }
        item.meta_mut().namespace = scope.clone();
        self.engine.store(item).await
    }

    /// A belief build of exactly the core scope at `scope`.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRequest`] for a node that is not one of this memory's
    /// core scopes.
    pub fn core_build(&self, scope: &Namespace) -> Result<BackgroundJob> {
        if !self.core.iter().any(|core| &core.at == scope) {
            return Err(Error::InvalidRequest(format!(
                "`{scope}` is not a core scope of this memory"
            )));
        }
        Ok(BackgroundJob::BuildBeliefs {
            request: ConsolidateRequest::new(Reach::exact(scope.clone())),
        })
    }

    /// The agent's id.
    #[must_use]
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// The node the agent's turns are stored at.
    #[must_use]
    pub fn namespace(&self) -> &Namespace {
        &self.node
    }

    /// The layout.
    #[must_use]
    pub fn layout(&self) -> &MemoryLayout {
        &self.layout
    }

    /// The policy.
    #[must_use]
    pub fn policy(&self) -> &RecallPolicy {
        &self.policy
    }

    /// The engine.
    #[must_use]
    pub fn engine(&self) -> &Arc<dyn MemoryEngine> {
        &self.engine
    }

    /// The layout's brain, on the same engine.
    #[must_use]
    pub fn brain(&self) -> Brain {
        Brain::new(self.engine.clone(), self.layout.clone())
    }

    /// A runner for the jobs this memory hands back.
    #[must_use]
    pub fn background(&self) -> BackgroundRunner {
        BackgroundRunner::new(self.engine.clone(), self.layout.clone())
    }

    /// The pack a turn or session would receive, without writing a turn.
    ///
    /// A nonblank `query` previews a query-ranked turn and ignores `thread_id`.
    /// A missing or blank query previews a session start or resume; a blank
    /// `thread_id` is treated as absent. Engine failures remain available as
    /// typed skipped-section reasons in the returned pack.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRequest`] when the preview request or policy is
    /// invalid.
    pub async fn preview(
        &self,
        query: Option<&str>,
        thread_id: Option<&str>,
    ) -> Result<types::Preview> {
        let query = query.filter(|query| !query.trim().is_empty());
        let (mode, pack) = match query {
            Some(query) => (types::PreviewMode::Turn, self.recall(query).await?),
            None => {
                let thread_id = thread_id
                    .filter(|thread_id| !thread_id.trim().is_empty())
                    .map(str::to_string);
                (
                    types::PreviewMode::Session,
                    self.start_session(SessionStart {
                        thread_id,
                        focus: None,
                    })
                    .await?,
                )
            }
        };
        Ok(types::Preview { mode, pack })
    }

    /// Agents with conversations below this memory layout's root, most turns
    /// first.
    ///
    /// # Errors
    ///
    /// Any error returned by the memory engine's agent-facet exploration.
    pub async fn agents(&self) -> Result<Vec<types::AgentCount>> {
        let page = self
            .engine
            .explore(ExploreRequest {
                facet: Facet::Agent,
                filter: self.layout.conversations_filter(None),
                limit: 200,
                scan_limit: 20_000,
            })
            .await?;
        Ok(page
            .buckets
            .into_iter()
            .map(|bucket| types::AgentCount {
                agent_id: bucket.value,
                turns: bucket.count,
            })
            .collect())
    }

    /// The model-facing memory tools for this agent: writes land at its node,
    /// reads see its node and every node above it ([`Reach::of`]), core
    /// scopes included.
    #[must_use]
    pub fn tools(&self) -> MemoryTools {
        MemoryTools::new(self.engine.clone()).placed_at(self.node.clone())
    }

    /// Context for a session starting or resuming: a resumed thread's own
    /// turns first, then the standard sections, ranked for `focus` or newest
    /// first without one. Reads only.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRequest`] for a blank thread id or a policy with no
    /// budget. Engine failures leave sections out instead (see
    /// [`crate::recall`]).
    pub async fn start_session(&self, start: SessionStart) -> Result<ContextPack> {
        let mut sections = Vec::new();
        if let Some(thread_id) = &start.thread_id {
            let thread_id = non_blank(thread_id, "thread id")?;
            sections.push(self.thread_section(thread_id, self.policy.history_limit.max(1)));
        }
        sections.extend(self.standard_sections());
        self.read(start.focus, sections).await
    }

    /// Recalls the context for a turn without logging it. A resumed turn gets
    /// its earlier thread history, while `in_prompt_from` excludes turns the
    /// host already included in the prompt.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRequest`] for a blank thread id or user text. Engine
    /// failures never fail the call.
    pub(crate) async fn pre_turn_read(&self, turn: &PreTurn) -> Result<ContextPack> {
        let thread_id = non_blank(&turn.thread_id, "thread id")?;
        let text = non_blank(&turn.user_text, "user text")?;
        let mut sections = Vec::new();
        if turn.resumed && self.policy.history_limit > 0 {
            sections.push(self.thread_section(thread_id, self.policy.history_limit));
        }
        sections.extend(self.standard_sections());
        let mut request = self.request(Some(text.to_string()), sections);
        request.exclude_thread = Some(ThreadWindow {
            thread_id: thread_id.to_string(),
            from_turn: turn.in_prompt_from,
        });
        crate::recall::run(
            self.engine.as_ref(),
            &request,
            None,
            None,
            self.team_exclusion(&request),
        )
        .await
    }

    /// Logs the user's turn and, concurrently, recalls the context to inject
    /// before the model runs.
    ///
    /// Logging waits only for the engine to accept the turn. A failed log
    /// does not fail the call: the pack is still returned, with
    /// [`TurnContext::log_error`] set.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRequest`] for a blank thread id or text. Engine
    /// failures never fail the call.
    pub async fn pre_turn(&self, turn: PreTurn) -> Result<TurnContext> {
        let resumed = turn.resumed;
        self.pre_turn_with(turn, resumed, None).await
    }

    /// [`AgentMemory::pre_turn`] for a session that resumes this thread after
    /// a compaction: the one pack leads with the thread's earlier turns (those
    /// before [`PreTurn::in_prompt_from`]), sharing the turn's budget and the
    /// cross-section dedupe, instead of a separate
    /// [`AgentMemory::start_session`] pack pasted beside it. A policy with
    /// `history_limit == 0` gets no thread section, as everywhere else.
    ///
    /// # Errors
    ///
    /// As [`AgentMemory::pre_turn`].
    pub async fn pre_turn_resumed(&self, turn: PreTurn) -> Result<TurnContext> {
        self.pre_turn_with(
            PreTurn {
                resumed: true,
                ..turn
            },
            true,
            None,
        )
        .await
    }

    /// [`AgentMemory::pre_turn`] (or, `resumed`,
    /// [`AgentMemory::pre_turn_resumed`]) for a turn whose date is still being
    /// worked out: the pack's fetch sections put hits from the days `hint`
    /// resolves to first, when it resolves before the reads finish waiting
    /// for it (see [`crate::recall::holistic_recall_dated`]). `hint` must
    /// carry the host's own deadline; it is never waited for past it.
    ///
    /// # Errors
    ///
    /// As [`AgentMemory::pre_turn`].
    pub async fn pre_turn_dated(
        &self,
        turn: PreTurn,
        resumed: bool,
        hint: impl Future<Output = Option<TimeHint>> + Send,
    ) -> Result<TurnContext> {
        self.pre_turn_with(PreTurn { resumed, ..turn }, resumed, Some(Box::pin(hint)))
            .await
    }

    async fn pre_turn_with(
        &self,
        turn: PreTurn,
        resumed: bool,
        hint: Option<LateHint<'_>>,
    ) -> Result<TurnContext> {
        let thread_id = non_blank(&turn.thread_id, "thread id")?;
        let text = non_blank(&turn.user_text, "user text")?;
        let item = self.turn_item(
            thread_id,
            turn.turn_index,
            Turn {
                at: turn.at,
                ..Turn::new(Role::User, text)
            },
            turn.observed_actor.clone(),
            turn.channel.clone(),
        );
        let id = tinymemory_api::ItemId::new(item.fingerprint());
        let window = ThreadWindow {
            thread_id: thread_id.to_string(),
            from_turn: turn.in_prompt_from,
        };
        let mut sections = Vec::new();
        if resumed && self.policy.history_limit > 0 {
            sections.push(self.thread_section(thread_id, self.policy.history_limit));
        }
        sections.extend(self.standard_sections());
        let mut request = self.request(Some(text.to_string()), sections);
        request.exclude_ids = vec![id];
        request.exclude_thread = Some(window);
        let (logged, pack) = join(
            self.engine.store_with(item, WriteOptions::accepted()),
            crate::recall::run(
                self.engine.as_ref(),
                &request,
                None,
                hint,
                self.team_exclusion(&request),
            ),
        )
        .await;
        let pack = pack?;
        let (logged, log_error) = match logged {
            Ok(receipt) => (Some(receipt), None),
            Err(error) => {
                log::warn!(
                    "[lifecycle] user turn not logged agent={} thread={thread_id} error={error}",
                    self.agent_id
                );
                (None, Some(error.to_string()))
            }
        };
        Ok(TurnContext {
            pack,
            logged,
            log_error,
        })
    }

    /// Logs the assistant's reply, and returns the belief build the policy
    /// asks for at this turn, if any. An engine that rebuilds beliefs on its
    /// own ([`tinymemory_api::Consolidation::Automatic`]) gets none;
    /// [`AgentMemory::history_build`] still asks for one at any time.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRequest`] for a blank thread id or text, and the
    /// engine's failure to accept the turn.
    pub async fn post_turn(&self, turn: PostTurn) -> Result<PostTurnReport> {
        let thread_id = non_blank(&turn.thread_id, "thread id")?;
        let text = non_blank(&turn.assistant_text, "assistant text")?;
        let item = self.turn_item(
            thread_id,
            turn.turn_index,
            Turn {
                at: turn.at,
                tool_calls: turn.tool_calls.clone(),
                ..Turn::new(Role::Assistant, text)
            },
            None,
            turn.channel.clone(),
        );
        let receipt = self
            .engine
            .store_with(item, WriteOptions::accepted())
            .await?;
        let due = !builds_on_its_own(self.engine.as_ref())
            && self
                .policy
                .build_beliefs_every
                .is_some_and(|every| every > 0 && (turn.turn_index + 1).is_multiple_of(every));
        let jobs = if due {
            vec![self.history_build()]
        } else {
            Vec::new()
        };
        Ok(PostTurnReport { receipt, jobs })
    }

    /// Context to carry across a compaction: an answered summary of the
    /// thread so far (falling back to its most relevant turns when the engine
    /// cannot answer), then the standard sections, ranked for `focus` or the
    /// dropped turns' gist.
    ///
    /// Runs a model on engines that answer with one; call it off the hot
    /// path.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRequest`] for a blank thread id. Engine failures leave
    /// sections out instead.
    pub async fn recall_for_compaction(&self, compaction: Compaction) -> Result<ContextPack> {
        let thread_id = non_blank(&compaction.thread_id, "thread id")?;
        let gist = gist(&compaction.dropped);
        let query = compaction
            .focus
            .filter(|focus| !focus.trim().is_empty())
            .or(gist);
        let summary = ScopeSection {
            heading: SUMMARY_HEADING.to_string(),
            filter: MetaFilter {
                thread_id: Some(thread_id.to_string()),
                ..self.layout.conversations_filter(Some(&self.agent_id))
            },
            // Every dropped turn is a candidate, not only the policy's few.
            limit: self
                .policy
                .history_limit
                .max(compaction.dropped.len())
                .clamp(1, MAX_SUMMARY_TURNS),
            query: SectionQuery::Answer {
                question: SUMMARY_QUESTION.to_string(),
                instructions: Some(SUMMARY_INSTRUCTIONS.to_string()),
                fallback_to_fetch: true,
            },
            max_scopes: None,
        };
        let mut sections = vec![summary];
        sections.extend(self.standard_sections());
        self.read(query, sections).await
    }

    /// A pre-turn pack for `query`, without logging anything: for a tool, a
    /// sub-agent, or a mid-turn refresh.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRequest`] for a policy with no budget.
    pub async fn recall(&self, query: &str) -> Result<ContextPack> {
        let query = (!query.trim().is_empty()).then(|| query.to_string());
        self.read(query, self.standard_sections()).await
    }

    /// Runs one job this memory (or its brain) handed back.
    ///
    /// # Errors
    ///
    /// As [`BackgroundRunner::run`].
    pub async fn run_background(&self, job: BackgroundJob) -> Result<JobReport> {
        self.background().run(job).await
    }

    /// A belief build of this agent's conversations.
    #[must_use]
    pub fn history_build(&self) -> BackgroundJob {
        BackgroundJob::BuildBeliefs {
            request: ConsolidateRequest::new(Reach::exact(self.node.clone()))
                .kinds([ItemKind::Conversation]),
        }
    }

    /// The thread's own latest turns, up to `limit`.
    fn thread_section(&self, thread_id: &str, limit: usize) -> ScopeSection {
        ScopeSection::latest(
            THREAD_HEADING,
            MetaFilter {
                thread_id: Some(thread_id.to_string()),
                ..self.layout.conversations_filter(Some(&self.agent_id))
            },
            limit,
        )
    }

    /// Learnings, each core scope, brain (at most
    /// [`BRAIN_SCOPES_PER_TURN`] scopes), this agent's history, then other
    /// agents' turns. A zero limit leaves a section out. In a pooled layout,
    /// history and team read the same node with complementary agent filters.
    fn standard_sections(&self) -> Vec<ScopeSection> {
        let policy = &self.policy;
        let learnings = (
            LEARNINGS_HEADING,
            self.layout.learnings_filter(),
            policy.learnings_limit,
            None,
        );
        let core = self
            .core
            .iter()
            .map(|scope| (scope.heading.as_str(), scope.filter(), scope.limit, None));
        let layout = [
            (
                BRAIN_HEADING,
                self.layout.brain_filter(None),
                policy.brain_limit,
                Some(BRAIN_SCOPES_PER_TURN),
            ),
            (
                HISTORY_HEADING,
                self.layout.conversations_filter(Some(&self.agent_id)),
                policy.history_limit,
                None,
            ),
            (
                TEAM_HEADING,
                self.layout.conversations_filter(None),
                policy.team_limit,
                None,
            ),
        ];
        std::iter::once(learnings)
            .chain(core)
            .chain(layout)
            .filter(|(_, _, limit, _)| *limit > 0)
            .map(|(heading, filter, limit, max_scopes)| {
                let section = ScopeSection::fetch(heading, filter, limit);
                match max_scopes {
                    Some(scopes) => section.with_max_scopes(scopes),
                    None => section,
                }
            })
            .collect()
    }

    fn request(&self, query: Option<String>, sections: Vec<ScopeSection>) -> HolisticRecall {
        HolisticRecall {
            budget_tokens: self.policy.budget_tokens,
            title: PACK_TITLE.to_string(),
            ..HolisticRecall::new(query, sections)
        }
    }

    /// The team section is the last section with its heading and shared
    /// conversation filter; only lifecycle recall applies this exclusion.
    fn team_exclusion<'a>(&'a self, request: &HolisticRecall) -> Option<(usize, &'a str)> {
        if self.policy.team_limit == 0 {
            return None;
        }
        let team_filter = self.layout.conversations_filter(None);
        request
            .sections
            .iter()
            .rposition(|section| section.heading == TEAM_HEADING && section.filter == team_filter)
            .map(|index| (index, self.agent_id.as_str()))
    }

    async fn read(
        &self,
        query: Option<String>,
        sections: Vec<ScopeSection>,
    ) -> Result<ContextPack> {
        let request = self.request(query, sections);
        crate::recall::run(
            self.engine.as_ref(),
            &request,
            None,
            None,
            self.team_exclusion(&request),
        )
        .await
    }

    /// One turn of `thread_id` as a one-turn conversation at this agent's
    /// node, observed from `observed_actor` when someone other than the
    /// owner said it.
    fn turn_item(
        &self,
        thread_id: &str,
        index: u32,
        turn: Turn,
        observed_actor: Option<ObservedActor>,
        channel: Option<String>,
    ) -> StoreItem {
        StoreItem::Conversation {
            meta: MemoryMeta {
                namespace: self.node.clone(),
                thread_id: Some(thread_id.to_string()),
                turns: Some(TurnRange {
                    first: index,
                    last: index,
                }),
                agent_id: Some(self.agent_id.clone()),
                source: SourceRef {
                    kind: SourceKind::Conversation,
                    id: Some(thread_id.to_string()),
                },
                channel,
                observed_at: turn.at,
                observed_actor,
                ..MemoryMeta::default()
            },
            turns: vec![turn],
        }
    }
}

/// `value` trimmed, refused when blank.
fn non_blank<'a>(value: &'a str, what: &str) -> Result<&'a str> {
    let value = value.trim();
    if value.is_empty() {
        Err(Error::InvalidRequest(format!(
            "the {what} must not be blank"
        )))
    } else {
        Ok(value)
    }
}

/// The dropped turns' text within [`MAX_GIST_CHARS`], oldest first: each
/// turn keeps the start of its text, an equal share of the budget (at least
/// [`MIN_GIST_SHARE`]), so a fact stated early in a long thread still steers
/// the query. `None` when the turns say nothing.
fn gist(turns: &[Turn]) -> Option<String> {
    let texts: Vec<String> = turns
        .iter()
        .map(|turn| turn.text.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|text| !text.is_empty())
        .collect();
    if texts.is_empty() {
        return None;
    }
    let share = (MAX_GIST_CHARS / texts.len()).max(MIN_GIST_SHARE);
    let joined = texts
        .iter()
        .map(|text| text.chars().take(share).collect::<String>())
        .collect::<Vec<_>>()
        .join(" ");
    Some(joined.chars().take(MAX_GIST_CHARS).collect())
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "turn_tests.rs"]
mod turn_tests;

#[cfg(test)]
#[path = "turn_runtime_tests.rs"]
mod turn_runtime_tests;
