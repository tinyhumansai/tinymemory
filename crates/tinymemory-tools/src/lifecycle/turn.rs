//! Runtime-neutral hooks around a model turn.
//!
//! `TurnHooks` owns timeout and degradation decisions while the host owns
//! engine resolution, executor scheduling, model inference, and delivery
//! sinks. Its returned futures are ordinary owned futures: a host may pass
//! `pre_turn_work` or `pre_turn_background_work` to the existing
//! [`crate::prefetch::Prefetcher`] (whose [`crate::prefetch::PrefetchRuntime`]
//! owns worker spawning and cancellation), or await `pre_turn` directly. The
//! background work methods let a host cache results that finish after the
//! hook's turn deadline. This module starts no tasks and depends on no async
//! runtime.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::future::{BoxFuture, Either, select};
use serde::Serialize;
use tinymemory_api::{Error, Hit, TimeHint, ToolCallRef};

use super::date_hint::{self, DateHintModel};
use super::{AgentMemory, Compaction, PostTurn, PreTurn};
use crate::background::BackgroundJob;

/// Opening tag of an injected memory pack.
pub const OPEN_TAG: &str = "<memory-context>";
/// Closing tag of an injected memory pack.
pub const CLOSE_TAG: &str = "</memory-context>";
/// Maximum number of Unicode scalar values shown for one tool result.
pub const MAX_TOOL_LINE_CHARS: usize = 240;
const MIN_TIMEOUT: Duration = Duration::from_millis(1);
const DEFAULT_HINT_MARGIN: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeadlinePolicy {
    Bounded(Duration),
    Background,
}

impl DeadlinePolicy {
    fn deadline_from(self, started_at: Duration) -> Option<Duration> {
        match self {
            Self::Bounded(timeout) => Some(started_at.saturating_add(timeout.max(MIN_TIMEOUT))),
            Self::Background => None,
        }
    }
}

/// A host clock whose values share one monotonic time base.
pub trait Clock: Send + Sync {
    /// The current monotonic tick, represented as an elapsed duration.
    fn now(&self) -> Duration;

    /// Completes once this clock reaches `deadline`.
    fn sleep_until(&self, deadline: Duration) -> BoxFuture<'static, ()>;
}

/// Resolves the configured engine and identity to their lifecycle instance.
pub trait EngineResolver: Send + Sync {
    /// Returns the configured memory instance for this host hook.
    ///
    /// # Errors
    ///
    /// Returns the host's engine or identity resolution failure.
    fn resolve(&self) -> tinymemory_api::Result<AgentMemory>;
}

/// Receives background jobs returned by a successful post-turn log.
#[async_trait]
pub trait JobsSink: Send + Sync {
    /// Queues the jobs once. The hook does not also return this batch.
    async fn enqueue(&self, jobs: Vec<BackgroundJob>);
}

/// Receives generic TinyMemory hits for a pack the host actually shows.
pub trait CitationSink: Send + Sync {
    /// Delivers the pack's citation hits. The sink is scoped by the host to
    /// the conversation or surface that owns the citations.
    fn deliver(&self, citations: Vec<Hit>);
}

/// Per-call options for a pre-turn hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreTurnOptions {
    /// Whether to log the user turn.
    pub logging: bool,
    /// Whether to read memory for this turn.
    pub recall: bool,
    /// The whole bounded pre-turn work budget. Background pre-turn work uses
    /// this only to bound date-hint inference. Values below one millisecond
    /// clamp to one millisecond.
    pub timeout: Duration,
    /// Whether to ask the model for a date hint alongside logging and recall.
    pub date_hint: bool,
    /// The user's IANA time zone; absent zones use UTC.
    pub zone: Option<String>,
    /// Time reserved from the same turn deadline for the date-hint model call.
    pub date_hint_margin: Duration,
}

impl Default for PreTurnOptions {
    fn default() -> Self {
        Self {
            logging: true,
            recall: true,
            timeout: Duration::from_millis(1_500),
            date_hint: false,
            zone: None,
            date_hint_margin: DEFAULT_HINT_MARGIN,
        }
    }
}

/// Per-call options for compaction recall.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionOptions {
    /// Whether this identity may recall memory.
    pub recall: bool,
    /// The compaction recall budget. Values below one millisecond clamp to
    /// one millisecond.
    pub timeout: Duration,
}

/// A tool call paired with the one-line result included in the logged reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoggedToolCall {
    /// The stable API reference stored with the assistant turn.
    pub call: ToolCallRef,
    /// The result text, if the tool returned one.
    pub result: Option<String>,
}

/// The context pack a host can inject, with generic citation hits.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TurnPack {
    /// Rendered memory markdown, without the outer tags.
    pub markdown: String,
    /// Estimated tokens in the markdown.
    pub tokens: usize,
    /// Item ids cited by the rendered pack, in order.
    pub refs: Vec<String>,
    /// The memory engine that answered.
    pub engine: String,
    /// The hits corresponding to `refs`, in citation order.
    #[serde(skip)]
    pub citations: Vec<Hit>,
    /// A stable code when recall was refused or could not be reached.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refusal: Option<&'static str>,
}

impl TurnPack {
    fn from_outcome(
        outcome: tinymemory_api::Result<super::super::recall::ContextPack>,
        allow_refusal: bool,
        engine: String,
    ) -> Option<Self> {
        match outcome {
            Ok(pack) => Self::from_context(pack, allow_refusal),
            Err(error) if allow_refusal => Self::refused(&error, engine),
            Err(_) => None,
        }
    }

    fn from_context(pack: super::super::recall::ContextPack, allow_refusal: bool) -> Option<Self> {
        if pack.is_empty() {
            let error = allow_refusal
                .then(|| {
                    pack.skipped
                        .iter()
                        .find_map(|skipped| match &skipped.reason {
                            super::super::recall::SkipReason::Engine(error)
                                if error.is_account_wide()
                                    || matches!(error, Error::Unavailable(_)) =>
                            {
                                Some((*error).clone())
                            }
                            _ => None,
                        })
                })
                .flatten()?;
            return Self::refused(&error, pack.engine);
        }

        let citations = pack
            .sections
            .iter()
            .flat_map(|section| section.hits.iter())
            .filter(|hit| pack.refs.contains(&hit.id))
            .cloned()
            .collect();
        let refs: Vec<String> = pack.refs.iter().map(ToString::to_string).collect();
        Some(Self {
            markdown: pack.markdown,
            tokens: pack.tokens,
            refs,
            engine: pack.engine,
            citations,
            refusal: None,
        })
    }

    fn refused(error: &Error, engine: String) -> Option<Self> {
        // Unavailable describes this request's reachability, not proof that
        // every request on the account is refused. Keep its truthful
        // turn-local notice without classifying it as account-wide.
        if !error.is_account_wide() && !matches!(error, Error::Unavailable(_)) {
            return None;
        }
        let why = match error {
            Error::Off(_) => "memory is disabled or no engine is configured",
            Error::Unauthorized(_) if error.is_plan_required() => {
                "this memory feature is not included in the account's plan"
            }
            Error::Unauthorized(_) => "the memory engine rejected the sign-in or API key",
            Error::InsufficientCredits(_) => {
                "the account is out of credits; topping up restores it"
            }
            Error::Unavailable(_) => "the memory engine could not be reached",
            _ => return None,
        };
        let markdown = format!(
            "# Memory\n\nMemory could not be read for this turn: {why}. Nothing was recalled, which does not mean nothing is stored. If the user refers to something they told you before, say that memory is unavailable right now and why, instead of saying you do not know it."
        );
        Some(Self {
            tokens: crate::recall::estimate_tokens(&markdown),
            markdown,
            refs: Vec::new(),
            engine,
            citations: Vec::new(),
            refusal: Some(error.code()),
        })
    }

    /// The memory block as it appears in the model prompt.
    #[must_use]
    pub fn injection(&self) -> String {
        format!("{OPEN_TAG}\n{}\n{CLOSE_TAG}", self.markdown.trim())
    }
}

/// Runtime-neutral orchestration for memory work around a model turn.
#[derive(Clone)]
pub struct TurnHooks {
    resolver: Arc<dyn EngineResolver>,
    clock: Arc<dyn Clock>,
    date_hint_model: Option<Arc<dyn DateHintModel>>,
    jobs: Arc<dyn JobsSink>,
    citations: Arc<dyn CitationSink>,
}

impl std::fmt::Debug for TurnHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnHooks")
            .field("date_hint_model", &self.date_hint_model.is_some())
            .finish_non_exhaustive()
    }
}

impl TurnHooks {
    /// Builds hooks from host-owned ports.
    #[must_use]
    pub fn new(
        resolver: Arc<dyn EngineResolver>,
        clock: Arc<dyn Clock>,
        date_hint_model: Option<Arc<dyn DateHintModel>>,
        jobs: Arc<dyn JobsSink>,
        citations: Arc<dyn CitationSink>,
    ) -> Self {
        Self {
            resolver,
            clock,
            date_hint_model,
            jobs,
            citations,
        }
    }

    /// Runs pre-turn logging and recall within one absolute deadline.
    ///
    /// A timeout, resolver failure, blank turn, disabled logging+recall, or a
    /// non-account-wide recall failure degrades to `None`. Dropping this
    /// future drops the in-flight engine and model futures. Account-wide
    /// failures become a refusal notice so an empty pack is never presented
    /// as proof that nothing is stored.
    pub async fn pre_turn(&self, turn: PreTurn, options: PreTurnOptions) -> Option<TurnPack> {
        let timeout = options.timeout;
        self.pre_turn_with_policy(turn, options, DeadlinePolicy::Bounded(timeout))
            .await
    }

    async fn pre_turn_with_policy(
        &self,
        turn: PreTurn,
        options: PreTurnOptions,
        deadline_policy: DeadlinePolicy,
    ) -> Option<TurnPack> {
        if turn.thread_id.trim().is_empty()
            || turn.user_text.trim().is_empty()
            || (!options.logging && !options.recall)
        {
            return None;
        }

        let started_at = self.clock.now();
        let hint_budget_deadline = started_at.saturating_add(options.timeout.max(MIN_TIMEOUT));
        let deadline = deadline_policy.deadline_from(started_at);
        let resolver = self.resolver.clone();
        let clock = self.clock.clone();
        let work_clock = clock.clone();
        let model = self.date_hint_model.clone();
        let operation = async move {
            let mut memory = resolver.resolve().ok()?;
            if deadline.is_some_and(|deadline| work_clock.now() >= deadline) {
                return None;
            }
            if options.logging && !options.recall {
                memory = memory
                    .clone()
                    .with_policy(memory.policy().log_only())
                    .with_core(Vec::new())
                    .ok()?;
            }
            let engine_id = memory.engine().descriptor().id.to_string();

            let pack = if options.logging {
                let hint = if options.recall && options.date_hint {
                    model.map(|model| {
                        let user_text = turn.user_text.clone();
                        let zone = options.zone.clone().unwrap_or_else(|| "UTC".to_string());
                        let instruction = date_hint::INSTRUCTION.to_string();
                        let hint_deadline = hint_budget_deadline
                            .saturating_sub(options.date_hint_margin)
                            .max(clock.now());
                        let timer = clock.sleep_until(hint_deadline);
                        Box::pin(async move {
                            match select(
                                model.complete(instruction, user_text, zone.clone()),
                                timer,
                            )
                            .await
                            {
                                Either::Left((Some(answer), _)) => {
                                    date_hint::parse_range(&answer, Some(&zone))
                                }
                                Either::Left((None, _)) | Either::Right(_) => None,
                            }
                        }) as BoxFuture<'static, Option<TimeHint>>
                    })
                } else {
                    None
                };

                let outcome = if let Some(hint) = hint {
                    memory
                        .pre_turn_dated(turn.clone(), turn.resumed, hint)
                        .await
                        .map(|context| context.pack)
                } else {
                    memory.pre_turn(turn).await.map(|context| context.pack)
                };
                if options.recall {
                    TurnPack::from_outcome(outcome, true, engine_id)
                } else {
                    None
                }
            } else if options.recall {
                let outcome = memory.pre_turn_read(&turn).await;
                TurnPack::from_outcome(outcome, true, engine_id)
            } else {
                None
            };
            if deadline.is_some_and(|deadline| work_clock.now() >= deadline) {
                None
            } else {
                pack
            }
        };

        match deadline {
            Some(deadline) => {
                match select(Box::pin(operation), self.clock.sleep_until(deadline)).await {
                    Either::Left((pack, _)) => pack,
                    Either::Right(_) => None,
                }
            }
            None => operation.await,
        }
    }

    /// Returns an owned task suitable for the host's existing prefetch worker.
    /// The worker's `PrefetchRuntime::spawn` remains the only spawner, and
    /// cancellation of that worker drops this hook's engine/model work. The
    /// hook's pre-turn deadline still applies; use
    /// [`Self::pre_turn_background_work`] to keep late engine results.
    pub fn pre_turn_work(
        self: &Arc<Self>,
        turn: PreTurn,
        options: PreTurnOptions,
    ) -> BoxFuture<'static, Option<TurnPack>> {
        let hooks = self.clone();
        Box::pin(async move { hooks.pre_turn(turn, options).await })
    }

    /// Returns an owned pre-turn task for a host-managed background worker.
    ///
    /// Unlike [`Self::pre_turn`] and [`Self::pre_turn_work`], this work has no
    /// overall hook deadline, so a slow engine result can still be cached for
    /// a later turn. `options.timeout` continues to bound only the optional
    /// date-hint model call. Dropping the future cancels in-flight engine and
    /// model work. The host must call [`Self::deliver_citations`] only if it
    /// chooses to show the returned pack.
    pub fn pre_turn_background_work(
        self: &Arc<Self>,
        turn: PreTurn,
        options: PreTurnOptions,
    ) -> BoxFuture<'static, Option<TurnPack>> {
        let hooks = self.clone();
        Box::pin(async move {
            hooks
                .pre_turn_with_policy(turn, options, DeadlinePolicy::Background)
                .await
        })
    }

    /// Logs a committed assistant turn and sends its returned jobs to the
    /// host sink exactly once. Logging-disabled and failed operations do
    /// nothing; the job batch is not also returned to the caller.
    pub async fn post_turn(&self, turn: PostTurn, logging: bool) {
        if !logging {
            return;
        }
        let Ok(memory) = self.resolver.resolve() else {
            return;
        };
        let Ok(report) = memory.post_turn(turn).await else {
            return;
        };
        self.jobs.enqueue(report.jobs).await;
    }

    /// Recalls context for a compaction under the same cancellation-aware
    /// deadline pattern as pre-turn work. Compaction errors degrade to no pack
    /// and never create a pre-turn refusal notice.
    pub async fn compaction(
        &self,
        request: Compaction,
        options: CompactionOptions,
    ) -> Option<TurnPack> {
        self.compaction_with_policy(
            request,
            options.recall,
            DeadlinePolicy::Bounded(options.timeout),
        )
        .await
    }

    /// Returns an owned compaction recall task for a host-managed background
    /// worker. It has no overall hook deadline, allowing a slow engine result
    /// to reach a cache that can serve a later compaction. Dropping the future
    /// cancels in-flight engine work. A returned pack never delivers citations
    /// until the host calls [`Self::deliver_citations`].
    pub fn compaction_background_work(
        self: &Arc<Self>,
        request: Compaction,
        recall: bool,
    ) -> BoxFuture<'static, Option<TurnPack>> {
        let hooks = self.clone();
        Box::pin(async move {
            hooks
                .compaction_with_policy(request, recall, DeadlinePolicy::Background)
                .await
        })
    }

    async fn compaction_with_policy(
        &self,
        request: Compaction,
        recall: bool,
        deadline_policy: DeadlinePolicy,
    ) -> Option<TurnPack> {
        if !recall || request.dropped.is_empty() || request.thread_id.trim().is_empty() {
            return None;
        }
        let deadline = deadline_policy.deadline_from(self.clock.now());
        let resolver = self.resolver.clone();
        let clock = self.clock.clone();
        let operation = async move {
            let memory = resolver.resolve().ok()?;
            if deadline.is_some_and(|deadline| clock.now() >= deadline) {
                return None;
            }
            let engine_id = memory.engine().descriptor().id.to_string();
            let outcome = memory.recall_for_compaction(request).await;
            let pack = TurnPack::from_outcome(outcome, false, engine_id);
            deadline
                .is_none_or(|deadline| clock.now() < deadline)
                .then_some(pack)
                .flatten()
        };
        match deadline {
            Some(deadline) => {
                match select(Box::pin(operation), self.clock.sleep_until(deadline)).await {
                    Either::Left((pack, _)) => pack,
                    Either::Right(_) => None,
                }
            }
            None => operation.await,
        }
    }

    /// Delivers citations for a pack the host has chosen to show to the turn.
    /// Pack construction and background prefetch never trigger this sink.
    pub fn deliver_citations(&self, pack: &TurnPack) {
        if !pack.citations.is_empty() {
            self.citations.deliver(pack.citations.clone());
        }
    }
}

/// The user-message position after `committed_turns` complete exchanges.
#[must_use]
pub fn user_turn_index(committed_turns: usize) -> u32 {
    u32::try_from(committed_turns.saturating_mul(2)).unwrap_or(u32::MAX - 1)
}

/// Formats the reply and one bounded result line per tool call.
#[must_use]
pub fn logged_reply(text: &str, calls: &[LoggedToolCall]) -> String {
    let lines: Vec<String> = calls
        .iter()
        .filter_map(|call| {
            let result = call
                .result
                .as_deref()?
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            (!result.is_empty()).then(|| {
                let result: String = result.chars().take(MAX_TOOL_LINE_CHARS).collect();
                format!("- {} → {result}", call.call.name)
            })
        })
        .collect();
    if lines.is_empty() {
        text.trim().to_string()
    } else {
        format!("{}\n\nTools:\n{}", text.trim(), lines.join("\n"))
    }
}

#[cfg(test)]
#[path = "turn_pack_tests.rs"]
mod tests;
