//! Host-neutral planning, state and execution for conversation backfill.

use std::collections::{BTreeMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tinymemory_api::{
    Error, ItemKind, ListRequest, MemoryEngine, MetaFilter, Result, WriteOptions,
};

use super::{
    BACKFILL_TAG, PastTurn, ThreadMessage, group_items, pending_range, turn_groups, turns_of,
};
use crate::layout::MemoryLayout;

const LIST_PAGE_LIMIT: usize = 100;

/// The persisted progress phase for a conversation backfill.
///
/// The snake-case wire values match the host's existing import-phase field,
/// keeping previously written state files readable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackfillPhase {
    /// No backfill has run.
    #[default]
    Idle,
    /// A backfill is running.
    Running,
    /// All planned turns have been stored.
    Done,
    /// A store failed or a running state was interrupted.
    Error,
}

/// Progress persisted by a host's [`CounterStore`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackfillState {
    /// Idle, running, done, or stopped on an error.
    pub phase: BackfillPhase,
    /// Threads with turns to store when the run started.
    pub threads_total: u64,
    /// Of those, threads finished.
    pub threads_done: u64,
    /// Turns stored by the run.
    pub turns_stored: u64,
    /// Conversation items stored by the run.
    pub items_stored: u64,
    /// Why it stopped, when it failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// When the last run finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
}

/// The host's persisted file: progress plus completed turns per thread.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackfillFile {
    /// Progress shown to the host.
    #[serde(default)]
    pub state: BackfillState,
    /// Turns stored so far, per thread.
    #[serde(default)]
    pub stored: BTreeMap<String, u32>,
}

/// A status response, including work still pending.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BackfillView {
    /// The run's progress.
    pub state: BackfillState,
    /// Threads that still have earlier turns to store.
    pub pending_threads: u64,
    /// Turns still to store across them.
    pub pending_turns: u64,
}

/// The host-side source of conversation threads and messages.
#[async_trait]
pub trait ThreadSource: Send + Sync {
    /// Lists thread ids. A failure here prevents a complete plan.
    ///
    /// # Errors
    ///
    /// Returns a description when the thread store cannot list its threads.
    async fn list_threads(&self) -> std::result::Result<Vec<String>, String>;

    /// Reads one thread's messages in chronological order.
    ///
    /// # Errors
    ///
    /// Returns a description when the thread store cannot read this thread.
    async fn messages(&self, thread_id: &str) -> std::result::Result<Vec<ThreadMessage>, String>;
}

/// Host-owned persistence for the backfill state file.
///
/// Implementations should save atomically. A failed save is logged and the
/// run continues, matching the host's existing best-effort counter writes.
pub trait CounterStore: Send + Sync {
    /// The persisted state, or its default when none can be read.
    fn load(&self) -> BackfillFile;

    /// Persists the state whole.
    ///
    /// # Errors
    ///
    /// Returns a description when the host cannot save the state.
    fn save(&self, file: &BackfillFile) -> std::result::Result<(), String>;
}

#[derive(Debug, Clone)]
struct ThreadPlan {
    thread_id: String,
    turns: Vec<PastTurn>,
    range: Range<usize>,
}

struct SourceThread {
    thread_id: String,
    turns: Vec<PastTurn>,
}

/// A planned backfill run, returned by [`Backfiller::begin`].
#[must_use = "a planned backfill does nothing until it is run"]
#[derive(Debug)]
pub struct BackfillRun {
    file: BackfillFile,
    plans: Vec<ThreadPlan>,
}

/// Plans and stores earlier conversations without owning the host's thread
/// store, persistence format, consent flow, or task executor. Its associated
/// [`Backfiller::status_without_engine`] method reports saved-counter status
/// without constructing a driver or binding an engine.
pub struct Backfiller {
    engine: Arc<dyn MemoryEngine>,
    layout: MemoryLayout,
    agent_id: String,
    source: Arc<dyn ThreadSource>,
    store: Arc<dyn CounterStore>,
}

impl Backfiller {
    /// A driver for `agent_id`'s conversations at `layout`.
    #[must_use]
    pub fn new(
        engine: Arc<dyn MemoryEngine>,
        layout: MemoryLayout,
        agent_id: impl Into<String>,
        source: Arc<dyn ThreadSource>,
        store: Arc<dyn CounterStore>,
    ) -> Self {
        Self {
            engine,
            layout,
            agent_id: agent_id.into(),
            source,
            store,
        }
    }

    /// Progress and work still pending. A stale `Running` state is reported
    /// as interrupted when `running` says no task owns its run slot.
    ///
    /// # Errors
    ///
    /// Returns an engine error if listing threads or stored items fails.
    pub async fn status(&self, running: bool) -> Result<BackfillView> {
        let file = self.store.load();
        let state = status_state(&file.state, running);
        let plans = self.plan(&file).await?;
        Ok(status_view(state, &plans))
    }

    /// Progress and pending work from saved counters and the host thread
    /// source, without requiring or querying a memory engine.
    ///
    /// Use this when a host reports backfill status while memory is off or
    /// no engine is currently bound. Pending ranges use saved per-thread
    /// counters and assume there is no live-logging cutoff to query. This
    /// method never writes the state file; stale `Running` is reflected as
    /// interrupted in the returned view only.
    ///
    /// # Errors
    ///
    /// Returns an engine error if listing threads fails. Individual unreadable
    /// threads are skipped, as in engine-backed status.
    pub async fn status_without_engine(
        source: &dyn ThreadSource,
        store: &dyn CounterStore,
        running: bool,
    ) -> Result<BackfillView> {
        let file = store.load();
        let state = status_state(&file.state, running);
        let listed = list_thread_ids(source).await?;
        let mut plans = Vec::new();
        for thread_id in listed {
            let stored = file.stored.get(&thread_id).copied().unwrap_or(0);
            let Some(thread) = source_thread(source, thread_id).await else {
                continue;
            };
            if let Some(plan) = plan_thread(thread, None, stored) {
                plans.push(plan);
            }
        }
        Ok(status_view(state, &plans))
    }

    /// Plans a run and persists its `Running` state. Consent and executor
    /// ownership stay with the host.
    ///
    /// # Errors
    ///
    /// Returns an engine error if listing threads or stored items fails.
    pub async fn begin(&self) -> Result<BackfillRun> {
        let mut file = self.store.load();
        let plans = self.plan(&file).await?;
        file.state = BackfillState {
            phase: BackfillPhase::Running,
            threads_total: plans.len() as u64,
            ..BackfillState::default()
        };
        self.save(&file);
        Ok(BackfillRun { file, plans })
    }

    /// Stores the plan, with accepted writes for intermediate batches and a
    /// visible write for the last nonempty batch.
    pub async fn run(&self, run: BackfillRun) -> BackfillState {
        let mut file = run.file;
        let last = self.last_batch(&run.plans);
        for (thread_index, plan) in run.plans.into_iter().enumerate() {
            for (group_index, group) in turn_groups(plan.range.clone()).into_iter().enumerate() {
                let items = group_items(
                    &self.layout,
                    &self.agent_id,
                    &plan.thread_id,
                    &plan.turns,
                    &group,
                );
                let item_count = items.len() as u64;
                if item_count > 0 {
                    let options = if last == Some((thread_index, group_index)) {
                        WriteOptions::visible()
                    } else {
                        WriteOptions::accepted()
                    };
                    if let Err(error) = self.engine.store_many_with(items, options).await {
                        file.state.phase = BackfillPhase::Error;
                        file.state.error = Some(error.to_string());
                        file.state.finished_at = Some(Utc::now());
                        self.save(&file);
                        return file.state;
                    }
                }
                let end = group.last().map_or(0, |last| last + 1);
                file.stored.insert(
                    plan.thread_id.clone(),
                    u32::try_from(end).unwrap_or(u32::MAX),
                );
                file.state.turns_stored += group.len() as u64;
                file.state.items_stored += item_count;
                self.save(&file);
            }
            file.state.threads_done += 1;
            self.save(&file);
        }
        file.state.phase = BackfillPhase::Done;
        file.state.finished_at = Some(Utc::now());
        self.save(&file);
        file.state
    }

    async fn plan(&self, file: &BackfillFile) -> Result<Vec<ThreadPlan>> {
        // Fail before planning or persisting if this layout cannot represent
        // the selected agent. `conversations_filter` deliberately falls back
        // to a broad root filter when its optional agent cannot be resolved.
        self.layout.conversations(&self.agent_id)?;
        let listed = list_thread_ids(self.source.as_ref()).await?;
        let mut plans = Vec::new();
        for thread_id in listed {
            let saved = file.stored.get(&thread_id).copied().unwrap_or(0);
            let Some(thread) = source_thread(self.source.as_ref(), thread_id).await else {
                continue;
            };
            let mut filter = self.layout.conversations_filter(Some(&self.agent_id));
            filter.thread_id = Some(thread.thread_id.clone());
            let (first_live, stored) = match self.list_all(filter.clone()).await {
                Ok(hits) => (
                    Self::first_live_turn_from_hits(
                        &hits,
                        &filter,
                        &self.agent_id,
                        &thread.thread_id,
                    ),
                    Self::stored_turns_from_hits(
                        &hits,
                        &filter,
                        &self.agent_id,
                        &thread.thread_id,
                        &thread.turns,
                    ),
                ),
                Err(Error::Unsupported(_)) => (None, saved),
                Err(error) => {
                    log::debug!("[memory:backfill] live turn lookup failed: {error}");
                    return Err(error);
                }
            };
            if let Some(plan) = plan_thread(thread, first_live, stored) {
                plans.push(plan);
            }
        }
        Ok(plans)
    }

    async fn list_all(&self, filter: MetaFilter) -> Result<Vec<tinymemory_api::Hit>> {
        let mut cursor = None;
        let mut seen_cursors = HashSet::new();
        let mut hits = Vec::new();
        loop {
            let mut request = ListRequest::new(filter.clone(), LIST_PAGE_LIMIT);
            request.cursor = cursor;
            let page = self.engine.list(request).await?;
            hits.extend(page.items);
            let Some(next) = page.next_cursor else {
                return Ok(hits);
            };
            if !seen_cursors.insert(next.clone()) {
                return Err(Error::Engine(
                    "backfill listing repeated a pagination cursor".to_string(),
                ));
            }
            cursor = Some(next);
        }
    }
    fn first_live_turn_from_hits(
        hits: &[tinymemory_api::Hit],
        filter: &MetaFilter,
        agent_id: &str,
        thread_id: &str,
    ) -> Option<usize> {
        hits.iter()
            .filter(|hit| {
                filter.matches(hit.kind, &hit.meta)
                    && hit.meta.agent_id.as_deref() == Some(agent_id)
                    && hit.meta.thread_id.as_deref() == Some(thread_id)
                    && hit.kind == ItemKind::Conversation
                    && !hit.meta.tags.iter().any(|tag| tag == BACKFILL_TAG)
            })
            .filter_map(|hit| hit.meta.turns.map(|turns| turns.first as usize / 2))
            .min()
    }

    fn stored_turns_from_hits(
        hits: &[tinymemory_api::Hit],
        filter: &MetaFilter,
        agent_id: &str,
        thread_id: &str,
        turns: &[PastTurn],
    ) -> u32 {
        let mut stored_items = HashSet::new();
        for hit in hits {
            if !filter.matches(hit.kind, &hit.meta)
                || hit.meta.agent_id.as_deref() != Some(agent_id)
                || hit.meta.thread_id.as_deref() != Some(thread_id)
                || !hit.meta.tags.iter().any(|tag| tag == BACKFILL_TAG)
            {
                continue;
            }
            let Some(turn_range) = hit.meta.turns else {
                continue;
            };
            if turn_range.first != turn_range.last {
                continue;
            }
            let turn_index = (turn_range.first / 2) as usize;
            let Some(turn) = turns.get(turn_index) else {
                continue;
            };
            let expected = if turn_range.first % 2 == 0 {
                !turn.user.trim().is_empty()
            } else {
                !turn.assistant.trim().is_empty()
            };
            if expected {
                stored_items.insert(turn_range.first);
            }
        }

        let mut complete = 0usize;
        for (index, turn) in turns.iter().enumerate() {
            let user_expected = !turn.user.trim().is_empty();
            let assistant_expected = !turn.assistant.trim().is_empty();
            let user_stored = !user_expected || stored_items.contains(&turn_index(index, false));
            let assistant_stored =
                !assistant_expected || stored_items.contains(&turn_index(index, true));
            if !user_stored || !assistant_stored {
                break;
            }
            complete = index + 1;
        }
        u32::try_from(complete).unwrap_or(u32::MAX)
    }

    fn last_batch(&self, plans: &[ThreadPlan]) -> Option<(usize, usize)> {
        plans.iter().enumerate().rev().find_map(|(thread, plan)| {
            turn_groups(plan.range.clone())
                .iter()
                .enumerate()
                .rev()
                .find(|(_, group)| {
                    !group_items(
                        &self.layout,
                        &self.agent_id,
                        &plan.thread_id,
                        &plan.turns,
                        group,
                    )
                    .is_empty()
                })
                .map(|(index, _)| (thread, index))
        })
    }

    fn save(&self, file: &BackfillFile) {
        if let Err(error) = self.store.save(file) {
            log::warn!("[memory:backfill] writing state failed: {error}");
        }
    }
}

async fn list_thread_ids(source: &dyn ThreadSource) -> Result<Vec<String>> {
    source
        .list_threads()
        .await
        .map_err(|error| Error::Engine(format!("reading chat threads failed: {error}")))
}

async fn source_thread(source: &dyn ThreadSource, thread_id: String) -> Option<SourceThread> {
    let messages = match source.messages(&thread_id).await {
        Ok(messages) => messages,
        Err(error) => {
            log::warn!("[memory:backfill] a thread was unreadable; skipped: {error}");
            return None;
        }
    };
    Some(SourceThread {
        thread_id,
        turns: turns_of(&messages),
    })
}

fn plan_thread(thread: SourceThread, first_live: Option<usize>, stored: u32) -> Option<ThreadPlan> {
    let range = pending_range(thread.turns.len(), first_live, stored);
    if range.is_empty() {
        return None;
    }
    Some(ThreadPlan {
        thread_id: thread.thread_id,
        turns: thread.turns,
        range,
    })
}

fn status_state(saved: &BackfillState, running: bool) -> BackfillState {
    let mut state = saved.clone();
    if state.phase == BackfillPhase::Running && !running {
        state.phase = BackfillPhase::Error;
        state.error = Some("the sync was interrupted; start it again to resume".to_string());
    }
    state
}

fn status_view(state: BackfillState, plans: &[ThreadPlan]) -> BackfillView {
    BackfillView {
        state,
        pending_threads: plans.len() as u64,
        pending_turns: plans.iter().map(|plan| plan.range.len() as u64).sum(),
    }
}

impl std::fmt::Debug for Backfiller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Backfiller")
            .field("agent_id", &self.agent_id)
            .field("layout", &self.layout)
            .finish_non_exhaustive()
    }
}

fn turn_index(index: usize, assistant: bool) -> u32 {
    let base = u32::try_from(index.saturating_mul(2)).unwrap_or(u32::MAX - 1);
    if assistant {
        base.saturating_add(1)
    } else {
        base
    }
}
