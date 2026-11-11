//! Deterministic port tests for turn orchestration and cancellation.

use std::collections::VecDeque;
use std::future::{Future, pending};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Duration;

use crate::background::BackgroundJob;
use crate::brain::{Brain, BrainDocument};
use crate::layout::{BrainSource, CoreScope, MemoryLayout};
use crate::prefetch::{Limits, PrefetchRuntime, Prefetcher};
use async_trait::async_trait;
use futures::channel::oneshot;
use futures::future::{BoxFuture, poll_fn};
use tinymemory_api::conformance::ReferenceEngine;
use tinymemory_api::{
    EngineDescriptor, EngineHealth, Error, FetchPage, FetchRequest, ForgetReport, ForgetTarget,
    Hit, ItemId, ItemKind, LearningKind, ListPage, ListRequest, MemoryEngine, MemoryMeta,
    MetaFilter, Namespace, RecallAnswer, RecallRequest, Result, StoreItem, StoreReceipt, Turn,
};

use super::date_hint::DateHintModel;
use super::turn::{
    CitationSink, Clock, CompactionOptions, EngineResolver, JobsSink, PreTurnOptions, TurnHooks,
    TurnPack,
};
use super::{AgentMemory, Compaction, PostTurn, PreTurn, RecallPolicy};

#[derive(Clone)]
struct FakeClock(Arc<ClockState>);

struct ClockState {
    now: Mutex<Duration>,
    deadlines: Mutex<Vec<Duration>>,
    sleepers: Mutex<Vec<(Duration, oneshot::Sender<()>)>>,
}

impl FakeClock {
    fn at(now: Duration) -> Self {
        Self(Arc::new(ClockState {
            now: Mutex::new(now),
            deadlines: Mutex::default(),
            sleepers: Mutex::default(),
        }))
    }

    fn advance_by(&self, duration: Duration) {
        let now = {
            let mut now = self.0.now.lock().unwrap();
            *now = now.saturating_add(duration);
            *now
        };
        let due = {
            let mut sleepers = self.0.sleepers.lock().unwrap();
            let mut due = Vec::new();
            let mut pending = Vec::new();
            for (deadline, sender) in sleepers.drain(..) {
                if deadline <= now {
                    due.push(sender);
                } else {
                    pending.push((deadline, sender));
                }
            }
            *sleepers = pending;
            due
        };
        for sender in due {
            let _ = sender.send(());
        }
    }

    fn deadlines(&self) -> Vec<Duration> {
        self.0.deadlines.lock().unwrap().clone()
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Duration {
        *self.0.now.lock().unwrap()
    }

    fn sleep_until(&self, deadline: Duration) -> BoxFuture<'static, ()> {
        self.0.deadlines.lock().unwrap().push(deadline);
        if deadline <= self.now() {
            Box::pin(async {})
        } else {
            let (sender, receiver) = oneshot::channel();
            self.0.sleepers.lock().unwrap().push((deadline, sender));
            Box::pin(async move {
                let _ = receiver.await;
            })
        }
    }
}

#[derive(Default)]
struct WorkProbe {
    started: AtomicUsize,
    live: AtomicUsize,
    dropped: AtomicUsize,
    completed: AtomicUsize,
}

impl WorkProbe {
    fn enter(self: &Arc<Self>) -> ActiveWork {
        self.started.fetch_add(1, Ordering::SeqCst);
        self.live.fetch_add(1, Ordering::SeqCst);
        ActiveWork {
            probe: self.clone(),
        }
    }
}

struct ActiveWork {
    probe: Arc<WorkProbe>,
}

impl ActiveWork {
    fn complete(&mut self) {
        self.probe.completed.fetch_add(1, Ordering::SeqCst);
    }
}

impl Drop for ActiveWork {
    fn drop(&mut self) {
        self.probe.live.fetch_sub(1, Ordering::SeqCst);
        self.probe.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

struct StaticResolver {
    calls: AtomicUsize,
    memory: AgentMemory,
    fail: bool,
    clock: FakeClock,
    advance: Duration,
}

struct DeferredResolver {
    memory: AgentMemory,
    release: Mutex<Option<oneshot::Receiver<()>>>,
    calls: AtomicUsize,
}

#[async_trait]
impl EngineResolver for DeferredResolver {
    async fn resolve(&self) -> Result<AgentMemory> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let release = self.release.lock().unwrap().take();
        if let Some(release) = release {
            let _ = release.await;
        }
        Ok(self.memory.clone())
    }
}

#[async_trait]
impl EngineResolver for StaticResolver {
    async fn resolve(&self) -> Result<AgentMemory> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.clock.advance_by(self.advance);
        if self.fail {
            Err(Error::Unavailable("resolver offline".into()))
        } else {
            Ok(self.memory.clone())
        }
    }
}

#[derive(Default)]
struct SpyModel {
    calls: AtomicUsize,
    input: Mutex<Vec<(String, String, String)>>,
    answer: Mutex<Option<String>>,
    held: AtomicBool,
    probe: Arc<WorkProbe>,
}

impl DateHintModel for SpyModel {
    fn complete(
        &self,
        instruction: String,
        user_text: String,
        zone: String,
    ) -> BoxFuture<'static, Option<String>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.input
            .lock()
            .unwrap()
            .push((instruction, user_text, zone));
        let answer = self.answer.lock().unwrap().clone();
        let probe = self.probe.clone();
        let held = self.held.load(Ordering::SeqCst);
        Box::pin(async move {
            let mut guard = probe.enter();
            if held {
                pending::<()>().await;
            }
            guard.complete();
            answer
        })
    }
}

#[derive(Default)]
struct RecordingJobs {
    calls: AtomicUsize,
    jobs: Mutex<Vec<BackgroundJob>>,
}

#[async_trait]
impl JobsSink for RecordingJobs {
    async fn enqueue(&self, jobs: Vec<BackgroundJob>) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.jobs.lock().unwrap().extend(jobs);
    }
}

#[derive(Default)]
struct RecordingCitations {
    calls: AtomicUsize,
    hits: Mutex<Vec<Hit>>,
}

impl CitationSink for RecordingCitations {
    fn deliver(&self, citations: Vec<Hit>) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.hits.lock().unwrap().extend(citations);
    }
}

fn memory(engine: Arc<dyn MemoryEngine>) -> AgentMemory {
    AgentMemory::new(engine, MemoryLayout::default(), "agent")
        .unwrap()
        .with_policy(RecallPolicy::default())
}

struct Fixture {
    hooks: Arc<TurnHooks>,
    resolver: Arc<StaticResolver>,
    clock: FakeClock,
    model: Arc<SpyModel>,
    jobs: Arc<RecordingJobs>,
    citations: Arc<RecordingCitations>,
    engine: Option<Arc<ReferenceEngine>>,
}

fn fixture(clock: FakeClock, fail_resolve: bool) -> Fixture {
    let engine = Arc::new(ReferenceEngine::new());
    fixture_with_engine(clock, fail_resolve, engine.clone(), Some(engine))
}

fn fixture_with_resolve_advance(
    clock: FakeClock,
    fail_resolve: bool,
    advance: Duration,
) -> Fixture {
    let engine = Arc::new(ReferenceEngine::new());
    fixture_with_engine_and_advance(clock, fail_resolve, engine.clone(), Some(engine), advance)
}

fn fixture_with_engine(
    clock: FakeClock,
    fail_resolve: bool,
    engine: Arc<dyn MemoryEngine>,
    reference: Option<Arc<ReferenceEngine>>,
) -> Fixture {
    fixture_with_engine_and_advance(clock, fail_resolve, engine, reference, Duration::ZERO)
}

fn fixture_with_engine_and_advance(
    clock: FakeClock,
    fail_resolve: bool,
    engine: Arc<dyn MemoryEngine>,
    reference: Option<Arc<ReferenceEngine>>,
    advance: Duration,
) -> Fixture {
    fixture_with_memory_and_advance(clock, fail_resolve, memory(engine), reference, advance)
}

fn fixture_with_memory_and_advance(
    clock: FakeClock,
    fail_resolve: bool,
    memory: AgentMemory,
    reference: Option<Arc<ReferenceEngine>>,
    advance: Duration,
) -> Fixture {
    let resolver = Arc::new(StaticResolver {
        calls: AtomicUsize::new(0),
        memory,
        fail: fail_resolve,
        clock: clock.clone(),
        advance,
    });
    let model = Arc::new(SpyModel::default());
    let jobs = Arc::new(RecordingJobs::default());
    let citations = Arc::new(RecordingCitations::default());
    let hooks = Arc::new(TurnHooks::new(
        resolver.clone(),
        Arc::new(clock.clone()),
        Some(model.clone()),
        jobs.clone(),
        citations.clone(),
    ));
    Fixture {
        hooks,
        resolver,
        clock,
        model,
        jobs,
        citations,
        engine: reference,
    }
}

#[tokio::test]
async fn pre_turn_awaits_async_engine_resolution() {
    let engine = Arc::new(ReferenceEngine::new());
    let (release, wait) = oneshot::channel();
    let resolver = Arc::new(DeferredResolver {
        memory: memory(engine),
        release: Mutex::new(Some(wait)),
        calls: AtomicUsize::new(0),
    });
    let clock = FakeClock::at(Duration::ZERO);
    let citations = Arc::new(RecordingCitations::default());
    let hooks = TurnHooks::new(
        resolver.clone(),
        Arc::new(clock.clone()),
        None,
        Arc::new(RecordingJobs::default()),
        citations.clone(),
    );
    let mut work = Box::pin(hooks.pre_turn(
        PreTurn::new("thread", 0, "remember this"),
        options(true, true),
    ));

    poll_fn(|cx| match work.as_mut().poll(cx) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(_) => panic!("pre-turn completed before async engine resolution"),
    })
    .await;
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    release.send(()).unwrap();

    let _ = work.await;
    assert_eq!(citations.calls.load(Ordering::SeqCst), 0);
}

fn options(logging: bool, recall: bool) -> PreTurnOptions {
    PreTurnOptions {
        logging,
        recall,
        timeout: Duration::from_secs(1),
        date_hint: true,
        zone: Some("Asia/Kolkata".into()),
        date_hint_margin: Duration::from_millis(100),
    }
}

async fn stored_conversations(engine: &ReferenceEngine) -> usize {
    engine
        .list(ListRequest::new(
            MetaFilter::kinds([ItemKind::Conversation]),
            100,
        ))
        .await
        .unwrap()
        .items
        .len()
}

#[tokio::test]
async fn pre_turn_obeys_all_logging_and_recall_combinations() {
    for (logging, recall, resolves, logs, model_calls) in [
        (false, false, 0, 0, 0),
        (false, true, 1, 0, 0),
        (true, false, 1, 1, 0),
        (true, true, 1, 1, 1),
    ] {
        let fixture = fixture(FakeClock::at(Duration::from_secs(20)), false);
        let _ = fixture
            .hooks
            .pre_turn(
                PreTurn::new("thread", 0, "what happened?"),
                options(logging, recall),
            )
            .await;
        assert_eq!(fixture.resolver.calls.load(Ordering::SeqCst), resolves);
        assert_eq!(
            stored_conversations(fixture.engine.as_ref().unwrap()).await,
            logs
        );
        assert_eq!(fixture.model.calls.load(Ordering::SeqCst), model_calls);
    }
}

#[tokio::test]
async fn recall_off_does_not_read_positive_core_scopes() {
    let root: Namespace = "ws:acme/team:hive".parse().unwrap();
    let company: Namespace = "ws:acme".parse().unwrap();
    let engine = Arc::new(ReadSpyEngine {
        inner: ReferenceEngine::new(),
        reads: AtomicUsize::new(0),
    });
    engine
        .inner
        .store(StoreItem::learning(
            "Company policy: renewals happen in April.",
            LearningKind::Fact,
            0.9,
            MemoryMeta {
                namespace: company.clone(),
                ..MemoryMeta::default()
            },
        ))
        .await
        .unwrap();
    let memory = AgentMemory::new(engine.clone(), MemoryLayout::new(root).unwrap(), "agent")
        .unwrap()
        .with_core(vec![CoreScope::new(company, "Company")])
        .unwrap();
    let fixture = fixture_with_memory_and_advance(
        FakeClock::at(Duration::ZERO),
        false,
        memory,
        None,
        Duration::ZERO,
    );

    assert!(
        fixture
            .hooks
            .pre_turn(
                PreTurn::new("thread", 0, "when are renewals?"),
                options(true, false),
            )
            .await
            .is_none()
    );
    assert_eq!(engine.reads.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn pre_turn_resumed_flag_and_legacy_overrides_have_defined_precedence() {
    let engine = Arc::new(ReferenceEngine::new());
    let memory = memory(engine.clone());
    memory
        .pre_turn(PreTurn::new("thread", 0, "The old project name was Cedar."))
        .await
        .unwrap();
    memory
        .post_turn(PostTurn::new("thread", 1, "I will remember Cedar."))
        .await
        .unwrap();

    let resumed = PreTurn {
        resumed: true,
        in_prompt_from: 2,
        ..PreTurn::new("thread", 2, "what was the old project name?")
    };
    let ordinary = memory.pre_turn(resumed.clone()).await.unwrap().pack;
    assert!(ordinary.markdown.contains("Earlier in this thread"));
    assert!(ordinary.markdown.contains("Cedar"));

    let mut legacy_plain = resumed.clone();
    legacy_plain.resumed = false;
    let forced_resumed = memory
        .pre_turn_resumed(legacy_plain.clone())
        .await
        .unwrap()
        .pack;
    assert!(forced_resumed.markdown.contains("Earlier in this thread"));

    // The explicit bool on the dated legacy method wins over the new field.
    let forced_plain = memory
        .pre_turn_dated(resumed, false, async { None })
        .await
        .unwrap()
        .pack;
    assert!(!forced_plain.markdown.contains("Earlier in this thread"));

    let forced_dated_resumed = memory
        .pre_turn_dated(legacy_plain, true, async { None })
        .await
        .unwrap()
        .pack;
    assert!(
        forced_dated_resumed
            .markdown
            .contains("Earlier in this thread")
    );
}

#[tokio::test]
async fn channel_turn_metadata_is_stamped_on_both_roles_and_retries_keep_identity() {
    let engine = Arc::new(ReferenceEngine::new());
    let memory = memory(engine.clone());
    let mut user = PreTurn::new("channel-thread", 0, "hello from chat");
    user.channel = Some("telegram".into());
    let first_user = memory.pre_turn(user.clone()).await.unwrap().logged.unwrap();
    let retry_user = memory.pre_turn(user).await.unwrap().logged.unwrap();
    assert_eq!(first_user.id, retry_user.id);
    assert!(retry_user.replayed);

    let mut reply = PostTurn::new("channel-thread", 1, "hello back");
    reply.channel = Some("telegram".into());
    let first_reply = memory.post_turn(reply.clone()).await.unwrap().receipt;
    let retry_reply = memory.post_turn(reply).await.unwrap().receipt;
    assert_eq!(first_reply.id, retry_reply.id);
    assert!(retry_reply.replayed);

    let rows = engine
        .list(ListRequest::new(
            MetaFilter::kinds([ItemKind::Conversation]),
            10,
        ))
        .await
        .unwrap()
        .items;
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter()
            .all(|row| row.meta.channel.as_deref() == Some("telegram"))
    );

    let untagged_ids = channel_turn_ids(None).await;
    let tagged_ids = channel_turn_ids(Some("telegram")).await;
    assert_eq!(untagged_ids, tagged_ids);
}

async fn channel_turn_ids(channel: Option<&str>) -> (ItemId, ItemId) {
    let engine = Arc::new(ReferenceEngine::new());
    let memory = memory(engine);
    let mut user = PreTurn::new("stable-thread", 4, "same user turn");
    user.channel = channel.map(str::to_string);
    let user_id = memory.pre_turn(user).await.unwrap().logged.unwrap().id;

    let mut reply = PostTurn::new("stable-thread", 5, "same assistant turn");
    reply.channel = channel.map(str::to_string);
    let reply_id = memory.post_turn(reply).await.unwrap().receipt.id;
    (user_id, reply_id)
}

#[tokio::test]
async fn resumed_pack_respects_budget_and_deduplicates_thread_hits_without_logging() {
    let engine = Arc::new(ReferenceEngine::new());
    let memory = memory(engine.clone()).with_policy(RecallPolicy {
        budget_tokens: 180,
        ..RecallPolicy::default()
    });
    let fixture = fixture_with_memory_and_advance(
        FakeClock::at(Duration::ZERO),
        false,
        memory.clone(),
        Some(engine),
        Duration::ZERO,
    );
    for index in 0..4 {
        memory
            .pre_turn(PreTurn::new(
                "thread",
                index * 2,
                format!("The project milestone {index} is recorded."),
            ))
            .await
            .unwrap();
        memory
            .post_turn(PostTurn::new(
                "thread",
                index * 2 + 1,
                format!("I recorded milestone {index}."),
            ))
            .await
            .unwrap();
    }
    let before = stored_conversations(fixture.engine.as_ref().unwrap()).await;
    let mut request = PreTurn::new("thread", 8, "show the earlier project milestones");
    request.in_prompt_from = 4;
    request.resumed = true;
    let recalled = fixture
        .hooks
        .pre_turn(request, options(false, true))
        .await
        .expect("resumed nonlogging lookup returns earlier thread context");
    assert!(recalled.markdown.contains("Earlier in this thread"));
    assert!(recalled.markdown.contains("project milestone 0"));
    assert!(recalled.markdown.contains("project milestone 1"));
    assert!(
        !recalled.markdown.contains("project milestone 2"),
        "turns already in the prompt are excluded"
    );
    assert!(!recalled.markdown.contains("project milestone 3"));
    assert!(recalled.tokens <= 180);
    let bullets: Vec<_> = recalled
        .markdown
        .lines()
        .filter(|line| line.starts_with("- "))
        .collect();
    let unique: std::collections::HashSet<_> = bullets.iter().copied().collect();
    assert_eq!(bullets.len(), unique.len());
    assert_eq!(
        stored_conversations(fixture.engine.as_ref().unwrap()).await,
        before
    );
    assert_eq!(fixture.model.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn resolver_failure_degrades_to_no_pack() {
    let fixture = fixture(FakeClock::at(Duration::ZERO), true);
    let pack = fixture
        .hooks
        .pre_turn(PreTurn::new("thread", 0, "hello"), options(true, true))
        .await;
    assert!(pack.is_none());
    assert_eq!(fixture.resolver.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn blank_turn_inputs_are_skipped_before_engine_resolution() {
    for (thread_id, user_text) in [(" ", "hello"), ("thread", " \n\t ")] {
        let fixture = fixture(FakeClock::at(Duration::ZERO), false);
        assert!(
            fixture
                .hooks
                .pre_turn(PreTurn::new(thread_id, 0, user_text), options(true, true))
                .await
                .is_none()
        );
        assert_eq!(fixture.resolver.calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn account_wide_pre_turn_refusal_is_a_notice_not_an_empty_pack() {
    let engine: Arc<dyn MemoryEngine> = Arc::new(HeldEngine {
        inner: ReferenceEngine::new(),
        held_reads: false,
        read_error: Some(Error::InsufficientCredits("sensitive-payload".into())),
        probe: Arc::new(WorkProbe::default()),
    });
    let fixture = fixture_with_engine(FakeClock::at(Duration::ZERO), false, engine, None);
    let pack = fixture
        .hooks
        .pre_turn(
            PreTurn::new("thread", 0, "did you save it?"),
            options(true, true),
        )
        .await
        .expect("account-wide failures must remain distinguishable");
    assert_eq!(pack.refusal, Some("INSUFFICIENT_CREDITS"));
    assert!(pack.markdown.contains("does not mean nothing is stored"));
    assert!(pack.citations.is_empty());
    assert!(!pack.markdown.contains("sensitive-payload"));
    assert_eq!(
        pack.injection(),
        format!(
            "{}\n{}\n{}",
            super::turn::OPEN_TAG,
            pack.markdown.trim(),
            super::turn::CLOSE_TAG
        )
    );
}

#[tokio::test]
async fn every_typed_account_wide_skip_reason_maps_to_its_stable_refusal_code() {
    for (error, code) in [
        (Error::Off("private detail".into()), "MEMORY_OFF"),
        (Error::Unauthorized("private detail".into()), "UNAUTHORIZED"),
        (
            Error::InsufficientCredits("private detail".into()),
            "INSUFFICIENT_CREDITS",
        ),
    ] {
        let engine: Arc<dyn MemoryEngine> = Arc::new(HeldEngine {
            inner: ReferenceEngine::new(),
            held_reads: false,
            read_error: Some(error),
            probe: Arc::new(WorkProbe::default()),
        });
        let fixture = fixture_with_engine(FakeClock::at(Duration::ZERO), false, engine, None);
        let pack = fixture
            .hooks
            .pre_turn(PreTurn::new("thread", 0, "hello"), options(true, true))
            .await
            .expect("account-wide skipped reads should be distinguished");
        assert_eq!(pack.refusal, Some(code));
        assert!(!pack.markdown.contains("private detail"));
    }
}

#[tokio::test]
async fn unreachable_recall_gets_a_request_scoped_notice() {
    let engine: Arc<dyn MemoryEngine> = Arc::new(HeldEngine {
        inner: ReferenceEngine::new(),
        held_reads: false,
        read_error: Some(Error::Unavailable("sensitive-payload".into())),
        probe: Arc::new(WorkProbe::default()),
    });
    let fixture = fixture_with_engine(FakeClock::at(Duration::ZERO), false, engine, None);
    let pack = fixture
        .hooks
        .pre_turn(PreTurn::new("thread", 0, "hello"), options(true, true))
        .await
        .expect("a request-level reachability failure needs a notice");
    assert!(!Error::Unavailable("sensitive-payload".into()).is_account_wide());
    assert_eq!(pack.refusal, Some("UNAVAILABLE"));
    assert!(pack.markdown.contains("could not be reached"));
    assert!(!pack.markdown.contains("sensitive-payload"));
}

#[tokio::test]
async fn non_account_wide_engine_fault_degrades_without_a_refusal_notice() {
    for error in [
        Error::Engine("index corrupt".into()),
        Error::Unsupported("feature unavailable".into()),
        Error::Conflict("item changed".into()),
    ] {
        let engine: Arc<dyn MemoryEngine> = Arc::new(HeldEngine {
            inner: ReferenceEngine::new(),
            held_reads: false,
            read_error: Some(error),
            probe: Arc::new(WorkProbe::default()),
        });
        let fixture = fixture_with_engine(FakeClock::at(Duration::ZERO), false, engine, None);
        assert!(
            fixture
                .hooks
                .pre_turn(PreTurn::new("thread", 0, "hello"), options(true, true))
                .await
                .is_none()
        );
    }
}

#[tokio::test]
async fn date_hint_uses_the_turn_deadline_and_the_margin_from_that_deadline() {
    let fixture = fixture_with_resolve_advance(
        FakeClock::at(Duration::from_secs(5)),
        false,
        Duration::from_millis(200),
    );
    let mut options = options(true, true);
    options.timeout = Duration::from_secs(1);
    *fixture.model.answer.lock().unwrap() =
        Some("{\"from\":\"2026-10-17\",\"to\":\"2026-10-18\"}".into());
    fixture
        .hooks
        .pre_turn(PreTurn::new("thread", 0, "yesterday"), options)
        .await;
    let deadlines = fixture.clock.deadlines();
    assert!(deadlines.contains(&Duration::from_secs(6)));
    assert!(deadlines.contains(&Duration::from_millis(5_900)));
    assert!(
        deadlines
            .iter()
            .all(|deadline| *deadline <= Duration::from_secs(6))
    );
    let calls = fixture.model.input.lock().unwrap();
    assert!(calls[0].0.contains("YYYY-MM-DD"));
    assert_eq!(calls[0].1, "yesterday");
    assert_eq!(calls[0].2, "Asia/Kolkata");
}

#[tokio::test]
async fn zero_timeout_is_clamped_and_missing_zone_uses_utc() {
    let fixture = fixture(FakeClock::at(Duration::from_secs(2)), false);
    let mut options = options(true, true);
    options.timeout = Duration::ZERO;
    options.zone = None;
    options.date_hint_margin = Duration::ZERO;
    *fixture.model.answer.lock().unwrap() = Some("nonsense".into());
    let _ = fixture
        .hooks
        .pre_turn(PreTurn::new("thread", 0, "today"), options)
        .await;
    let deadlines = fixture.clock.deadlines();
    assert!(deadlines.contains(&Duration::from_millis(2_001)));
    let calls = fixture.model.input.lock().unwrap();
    assert_eq!(calls[0].2, "UTC");
}

#[tokio::test]
async fn hint_margin_never_places_its_deadline_before_now() {
    let fixture = fixture(FakeClock::at(Duration::from_secs(5)), false);
    let mut options = options(true, true);
    options.timeout = Duration::from_millis(50);
    options.date_hint_margin = Duration::from_secs(1);
    let _ = fixture
        .hooks
        .pre_turn(PreTurn::new("thread", 0, "tomorrow"), options)
        .await;
    assert!(fixture.clock.deadlines().contains(&Duration::from_secs(5)));
}

#[tokio::test]
async fn a_late_date_hint_cannot_cost_a_completed_memory_pack() {
    let fixture = fixture(FakeClock::at(Duration::ZERO), false);
    Brain::new(
        fixture.engine.as_ref().unwrap().clone(),
        MemoryLayout::default(),
    )
    .ingest(BrainDocument::new(
        BrainSource::Markdown,
        "The release date is October 31.",
    ))
    .await
    .unwrap();
    fixture.model.held.store(true, Ordering::SeqCst);
    let model_probe = fixture.model.probe.clone();
    let mut options = options(true, true);
    options.timeout = Duration::from_secs(1);
    options.date_hint_margin = Duration::from_millis(100);
    let mut work = Box::pin(
        fixture
            .hooks
            .pre_turn(PreTurn::new("thread", 0, "when is the release?"), options),
    );
    poll_fn(|cx| match work.as_mut().poll(cx) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(_) => panic!("held hint unexpectedly completed before its deadline"),
    })
    .await;
    assert!(model_probe.started.load(Ordering::SeqCst) > 0);
    fixture.clock.advance_by(Duration::from_millis(900));
    let pack = work
        .await
        .expect("the late hint must not block completed recall");
    assert!(pack.markdown.contains("October 31"));
    assert_eq!(model_probe.live.load(Ordering::SeqCst), 0);
    assert_eq!(model_probe.completed.load(Ordering::SeqCst), 0);
    assert_eq!(
        model_probe.dropped.load(Ordering::SeqCst),
        model_probe.started.load(Ordering::SeqCst)
    );
}

struct HeldEngine {
    inner: ReferenceEngine,
    held_reads: bool,
    read_error: Option<Error>,
    probe: Arc<WorkProbe>,
}

struct ReadSpyEngine {
    inner: ReferenceEngine,
    reads: AtomicUsize,
}

#[async_trait]
impl MemoryEngine for ReadSpyEngine {
    fn descriptor(&self) -> &EngineDescriptor {
        self.inner.descriptor()
    }

    async fn health(&self) -> EngineHealth {
        self.inner.health().await
    }

    async fn recall(&self, request: RecallRequest) -> Result<RecallAnswer> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.recall(request).await
    }

    async fn fetch(&self, request: FetchRequest) -> Result<FetchPage> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.fetch(request).await
    }

    async fn store(&self, item: StoreItem) -> Result<StoreReceipt> {
        self.inner.store(item).await
    }

    async fn forget(&self, target: ForgetTarget) -> Result<ForgetReport> {
        self.inner.forget(target).await
    }

    async fn list(&self, request: ListRequest) -> Result<ListPage> {
        self.inner.list(request).await
    }
}

#[async_trait]
impl MemoryEngine for HeldEngine {
    fn descriptor(&self) -> &EngineDescriptor {
        self.inner.descriptor()
    }

    async fn health(&self) -> EngineHealth {
        self.inner.health().await
    }

    async fn recall(&self, request: RecallRequest) -> Result<RecallAnswer> {
        if let Some(error) = &self.read_error {
            return Err(error.clone());
        }
        if self.held_reads {
            let mut guard = self.probe.enter();
            pending::<()>().await;
            guard.complete();
        }
        self.inner.recall(request).await
    }

    async fn fetch(&self, request: FetchRequest) -> Result<FetchPage> {
        if let Some(error) = &self.read_error {
            return Err(error.clone());
        }
        if self.held_reads {
            let mut guard = self.probe.enter();
            pending::<()>().await;
            guard.complete();
        }
        self.inner.fetch(request).await
    }

    async fn store(&self, item: StoreItem) -> Result<StoreReceipt> {
        self.inner.store(item).await
    }

    async fn forget(&self, target: ForgetTarget) -> Result<ForgetReport> {
        self.inner.forget(target).await
    }

    async fn list(&self, request: ListRequest) -> Result<ListPage> {
        self.inner.list(request).await
    }
}

fn held_fixture(clock: FakeClock) -> (Fixture, Arc<WorkProbe>) {
    let probe = Arc::new(WorkProbe::default());
    let engine: Arc<dyn MemoryEngine> = Arc::new(HeldEngine {
        inner: ReferenceEngine::new(),
        held_reads: true,
        read_error: None,
        probe: probe.clone(),
    });
    (fixture_with_engine(clock, false, engine, None), probe)
}

#[tokio::test]
async fn timeout_cancels_in_flight_engine_and_model_work() {
    let (fixture, engine_probe) = held_fixture(FakeClock::at(Duration::from_secs(10)));
    fixture.model.held.store(true, Ordering::SeqCst);
    let model_probe = fixture.model.probe.clone();
    let mut options = options(true, true);
    options.timeout = Duration::from_secs(1);
    let mut work = Box::pin(
        fixture
            .hooks
            .pre_turn(PreTurn::new("thread", 0, "yesterday"), options),
    );
    poll_fn(|cx| match work.as_mut().poll(cx) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(_) => panic!("held pre-turn unexpectedly completed before deadline"),
    })
    .await;
    assert!(engine_probe.started.load(Ordering::SeqCst) > 0);
    assert!(model_probe.started.load(Ordering::SeqCst) > 0);
    fixture.clock.advance_by(Duration::from_secs(1));
    assert!(work.await.is_none());
    assert_eq!(engine_probe.live.load(Ordering::SeqCst), 0);
    assert_eq!(model_probe.live.load(Ordering::SeqCst), 0);
    assert_eq!(
        engine_probe.dropped.load(Ordering::SeqCst),
        engine_probe.started.load(Ordering::SeqCst)
    );
    assert_eq!(
        model_probe.dropped.load(Ordering::SeqCst),
        model_probe.started.load(Ordering::SeqCst)
    );
    assert_eq!(engine_probe.completed.load(Ordering::SeqCst), 0);
    assert_eq!(model_probe.completed.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn dropping_owned_turn_future_cancels_in_flight_engine_and_model_work() {
    let (fixture, engine_probe) = held_fixture(FakeClock::at(Duration::ZERO));
    fixture.model.held.store(true, Ordering::SeqCst);
    let model_probe = fixture.model.probe.clone();
    let mut work = Box::pin(
        fixture
            .hooks
            .pre_turn(PreTurn::new("thread", 0, "yesterday"), options(true, true)),
    );
    poll_fn(|cx| match work.as_mut().poll(cx) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(_) => panic!("held pre-turn unexpectedly completed"),
    })
    .await;
    assert!(engine_probe.started.load(Ordering::SeqCst) > 0);
    assert!(model_probe.started.load(Ordering::SeqCst) > 0);
    assert!(engine_probe.live.load(Ordering::SeqCst) > 0);
    assert!(model_probe.live.load(Ordering::SeqCst) > 0);
    drop(work);
    assert_eq!(engine_probe.live.load(Ordering::SeqCst), 0);
    assert_eq!(model_probe.live.load(Ordering::SeqCst), 0);
    assert_eq!(
        engine_probe.dropped.load(Ordering::SeqCst),
        engine_probe.started.load(Ordering::SeqCst)
    );
    assert_eq!(
        model_probe.dropped.load(Ordering::SeqCst),
        model_probe.started.load(Ordering::SeqCst)
    );
    assert_eq!(engine_probe.completed.load(Ordering::SeqCst), 0);
    assert_eq!(model_probe.completed.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn post_turn_enqueues_returned_jobs_once_and_skips_engine_when_logging_is_off() {
    let enabled = fixture(FakeClock::at(Duration::ZERO), false);
    enabled
        .hooks
        .post_turn(PostTurn::new("thread", 9, "done"), true)
        .await;
    assert_eq!(enabled.resolver.calls.load(Ordering::SeqCst), 1);
    assert_eq!(enabled.jobs.calls.load(Ordering::SeqCst), 1);
    assert_eq!(enabled.jobs.jobs.lock().unwrap().len(), 1);

    let disabled = fixture(FakeClock::at(Duration::ZERO), false);
    disabled
        .hooks
        .post_turn(PostTurn::new("thread", 9, "done"), false)
        .await;
    assert_eq!(disabled.resolver.calls.load(Ordering::SeqCst), 0);
    assert_eq!(disabled.jobs.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn compaction_gates_recall_and_degrades_on_timeout_or_engine_error() {
    let gated = fixture(FakeClock::at(Duration::ZERO), false);
    let nonempty = Compaction {
        thread_id: "thread".into(),
        dropped: vec![Turn::new(tinymemory_api::Role::User, "old")],
        focus: None,
    };
    assert!(
        gated
            .hooks
            .compaction(
                nonempty,
                CompactionOptions {
                    recall: false,
                    timeout: Duration::from_secs(1),
                },
            )
            .await
            .is_none()
    );
    assert_eq!(gated.resolver.calls.load(Ordering::SeqCst), 0);

    let empty = fixture(FakeClock::at(Duration::ZERO), false);
    assert!(
        empty
            .hooks
            .compaction(
                Compaction {
                    thread_id: "thread".into(),
                    dropped: vec![],
                    focus: None,
                },
                CompactionOptions {
                    recall: true,
                    timeout: Duration::from_secs(1),
                },
            )
            .await
            .is_none()
    );
    assert_eq!(empty.resolver.calls.load(Ordering::SeqCst), 0);

    let blank = fixture(FakeClock::at(Duration::ZERO), false);
    assert!(
        blank
            .hooks
            .compaction(
                Compaction {
                    thread_id: "  ".into(),
                    dropped: vec![Turn::new(tinymemory_api::Role::User, "old")],
                    focus: None,
                },
                CompactionOptions {
                    recall: true,
                    timeout: Duration::from_secs(1),
                },
            )
            .await
            .is_none()
    );
    assert_eq!(blank.resolver.calls.load(Ordering::SeqCst), 0);

    let (timed, probe) = held_fixture(FakeClock::at(Duration::ZERO));
    let request = Compaction {
        thread_id: "thread".into(),
        dropped: vec![
            Turn::new(tinymemory_api::Role::User, "old"),
            Turn::new(tinymemory_api::Role::Assistant, "reply"),
        ],
        focus: None,
    };
    let mut work = Box::pin(timed.hooks.compaction(
        request,
        CompactionOptions {
            recall: true,
            timeout: Duration::from_secs(1),
        },
    ));
    poll_fn(|cx| match work.as_mut().poll(cx) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(_) => panic!("held compaction unexpectedly completed before deadline"),
    })
    .await;
    assert!(probe.started.load(Ordering::SeqCst) > 0);
    timed.clock.advance_by(Duration::from_secs(1));
    assert!(work.await.is_none());
    assert_eq!(probe.live.load(Ordering::SeqCst), 0);
    assert_eq!(
        probe.dropped.load(Ordering::SeqCst),
        probe.started.load(Ordering::SeqCst)
    );
    assert_eq!(probe.completed.load(Ordering::SeqCst), 0);

    let failing_engine: Arc<dyn MemoryEngine> = Arc::new(HeldEngine {
        inner: ReferenceEngine::new(),
        held_reads: false,
        read_error: Some(Error::InsufficientCredits(
            "compaction should suppress this".into(),
        )),
        probe: Arc::new(WorkProbe::default()),
    });
    let failing = fixture_with_engine(FakeClock::at(Duration::ZERO), false, failing_engine, None);
    // Compaction errors remain a no-pack degradation; they do not turn into
    // pre-turn refusal notices.
    assert!(
        failing
            .hooks
            .compaction(
                Compaction {
                    thread_id: "thread".into(),
                    dropped: vec![Turn::new(tinymemory_api::Role::User, "old")],
                    focus: None,
                },
                CompactionOptions {
                    recall: true,
                    timeout: Duration::from_secs(1),
                },
            )
            .await
            .is_none()
    );
}

#[tokio::test]
async fn successful_compaction_returns_a_generic_pack() {
    let fixture = fixture(FakeClock::at(Duration::ZERO), false);
    fixture
        .resolver
        .memory
        .pre_turn(PreTurn::new("thread", 0, "The renewal date is April 12."))
        .await
        .unwrap();
    fixture
        .resolver
        .memory
        .post_turn(PostTurn::new("thread", 1, "I will keep that date in mind."))
        .await
        .unwrap();
    let pack = fixture
        .hooks
        .compaction(
            Compaction {
                thread_id: "thread".into(),
                dropped: vec![
                    Turn::new(tinymemory_api::Role::User, "The renewal date is April 12."),
                    Turn::new(
                        tinymemory_api::Role::Assistant,
                        "I will keep that date in mind.",
                    ),
                ],
                focus: None,
            },
            CompactionOptions {
                recall: true,
                timeout: Duration::from_secs(1),
            },
        )
        .await
        .expect("compaction recall");
    assert!(!pack.markdown.trim().is_empty());
    assert_eq!(fixture.resolver.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn citations_are_built_from_pack_refs_and_delivered_only_explicitly() {
    let fixture = fixture(FakeClock::at(Duration::ZERO), false);
    let engine = fixture.engine.as_ref().unwrap();
    Brain::new(engine.clone(), MemoryLayout::default())
        .ingest(BrainDocument::new(
            BrainSource::Markdown,
            "The office closes on Fridays.",
        ))
        .await
        .unwrap();
    let pack = fixture
        .hooks
        .pre_turn(
            PreTurn::new("thread", 0, "when does the office close?"),
            options(true, true),
        )
        .await
        .expect("nonempty recall pack");
    assert!(!pack.citations.is_empty());
    assert!(
        pack.citations
            .iter()
            .all(|hit| pack.refs.contains(&hit.id.to_string()))
    );
    assert!(
        serde_json::to_value(&pack)
            .unwrap()
            .get("citations")
            .is_none()
    );
    assert_eq!(fixture.citations.calls.load(Ordering::SeqCst), 0);

    fixture.hooks.deliver_citations(&pack);
    assert_eq!(fixture.citations.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        fixture.citations.hits.lock().unwrap().len(),
        pack.citations.len()
    );
}

#[derive(Default)]
struct ManualPrefetch {
    tasks: Mutex<VecDeque<BoxFuture<'static, ()>>>,
}

impl PrefetchRuntime for ManualPrefetch {
    fn spawn(&self, task: BoxFuture<'static, ()>) {
        self.tasks.lock().unwrap().push_back(task);
    }

    fn sleep(&self, _duration: Duration) -> BoxFuture<'static, ()> {
        Box::pin(pending())
    }
}

#[tokio::test]
async fn owned_hook_work_runs_in_existing_prefetcher_without_delivering_citations() {
    let fixture = fixture(FakeClock::at(Duration::ZERO), false);
    Brain::new(
        fixture.engine.as_ref().unwrap().clone(),
        MemoryLayout::default(),
    )
    .ingest(BrainDocument::new(
        BrainSource::Markdown,
        "The project started in 2024.",
    ))
    .await
    .unwrap();
    let runtime = Arc::new(ManualPrefetch::default());
    let cache: Prefetcher<&'static str, TurnPack> =
        Prefetcher::new(runtime.clone()).with_limits(Limits {
            work_timeout: Duration::from_secs(1),
            ..Limits::default()
        });
    cache.enqueue(
        "thread",
        fixture.hooks.pre_turn_work(
            PreTurn::new("thread", 0, "what is stored?"),
            options(true, true),
        ),
    );
    let worker = runtime.tasks.lock().unwrap().pop_front().unwrap();
    worker.await;
    assert!(cache.is_idle(&"thread"));
    assert!(cache.completed(&"thread").is_some());
    assert_eq!(fixture.citations.calls.load(Ordering::SeqCst), 0);
}

#[cfg(test)]
#[path = "turn_background_tests.rs"]
mod background_tests;
