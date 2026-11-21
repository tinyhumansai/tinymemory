use super::*;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::layout_migration::test_host::RefusingEngine;
use async_trait::async_trait;
use tinymemory_api::conformance::ReferenceEngine;
use tinymemory_api::{ConsolidateRequest, ItemKind, Reach};
use tokio::sync::Semaphore;

fn build(agent: &str) -> BackgroundJob {
    BackgroundJob::BuildBeliefs {
        request: ConsolidateRequest::new(Reach::exact(Namespace::agent(agent)))
            .kinds([ItemKind::Conversation]),
    }
}

fn reference() -> Arc<dyn MemoryEngine> {
    Arc::new(ReferenceEngine::new())
}

fn queue() -> JobQueue<MemoryQueueStore> {
    JobQueue::new(MemoryQueueStore::default())
}

#[test]
fn selection_from_id_uses_all_for_missing_or_blank_ids_and_preserves_nonblank_bytes() {
    assert_eq!(Selection::from_id(None), Selection::All);
    assert_eq!(Selection::from_id(Some(String::new())), Selection::All);
    assert_eq!(Selection::from_id(Some(" \t\n ".into())), Selection::All);
    assert_eq!(
        Selection::from_id(Some(" job-7 ".into())),
        Selection::One(" job-7 ".into())
    );
}

#[tokio::test]
async fn duplicates_merge_and_the_queue_survives_a_reload() {
    let store = Arc::new(MemoryQueueStore::default());
    struct Shared(Arc<MemoryQueueStore>);
    impl QueueStore for Shared {
        fn load(&self) -> QueueState {
            self.0.load()
        }
        fn save(&self, state: &QueueState) -> Result<(), String> {
            self.0.save(state)
        }
    }
    let queue = JobQueue::new(Shared(store.clone()));
    queue
        .enqueue(&Namespace::ROOT, vec![build("a"), build("a"), build("b")])
        .await;
    queue.enqueue(&Namespace::ROOT, vec![build("a")]).await;
    let team: Namespace = "team:acme".parse().unwrap();
    queue.enqueue(&team, vec![build("a")]).await;

    assert_eq!(
        queue.snapshot().await.pending.len(),
        3,
        "one per (root, job)"
    );
    let reloaded = JobQueue::new(Shared(store));
    assert_eq!(reloaded.snapshot().await.pending.len(), 3);
    assert!(reloaded.has_pending());
}

#[tokio::test]
async fn due_jobs_wait_for_the_build_delay() {
    let queue = queue();
    queue.enqueue(&Namespace::ROOT, vec![build("a")]).await;

    let runs = queue.run(reference(), Selection::Due, 3600).await.unwrap();
    assert!(runs.is_empty(), "a fresh job is not due yet");
    assert_eq!(queue.snapshot().await.pending.len(), 1);

    let runs = queue.run(reference(), Selection::Due, 0).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].outcome, "done");
    let state = queue.snapshot().await;
    assert!(state.pending.is_empty());
    assert_eq!(state.history.len(), 1);
}

#[tokio::test]
async fn one_job_runs_by_id_and_an_unknown_id_is_refused() {
    let queue = queue();
    queue
        .enqueue(&Namespace::ROOT, vec![build("a"), build("b")])
        .await;
    let id = queue.snapshot().await.pending[1].id.clone();

    let runs = queue
        .run(reference(), Selection::One(id.clone()), 0)
        .await
        .unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].id, id);
    assert_eq!(queue.snapshot().await.pending.len(), 1);
    assert_eq!(
        queue
            .run(reference(), Selection::One("nope".into()), 0)
            .await,
        Err(QueueError::NoSuchJob("nope".into()))
    );
    assert_eq!(
        queue.snapshot().await.pending.len(),
        1,
        "a refused run keeps the queue"
    );
}

#[tokio::test]
async fn an_account_wide_refusal_never_uses_up_a_jobs_attempts() {
    let queue = queue();
    queue.enqueue(&Namespace::ROOT, vec![build("a")]).await;
    let engine: Arc<dyn MemoryEngine> = Arc::new(RefusingEngine::out_of_credits());

    for _ in 0..MAX_ATTEMPTS + 2 {
        let runs = queue.run(engine.clone(), Selection::All, 0).await.unwrap();
        assert_eq!(runs[0].outcome, "failed");
    }

    let state = queue.snapshot().await;
    assert_eq!(state.pending.len(), 1, "the job waits for credits");
    assert_eq!(state.pending[0].attempts, 0);
    assert!(
        state.pending[0]
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("USER_INSUFFICIENT_CREDITS"))
    );
}

#[tokio::test]
async fn a_job_that_keeps_failing_is_dropped_and_says_so() {
    let queue = queue();
    queue.enqueue(&Namespace::ROOT, vec![build("a")]).await;
    let engine: Arc<dyn MemoryEngine> = Arc::new(RefusingEngine::with(
        tinymemory_api::Error::Engine("index corrupt".into()),
    ));

    let mut last = Vec::new();
    for _ in 0..MAX_ATTEMPTS {
        last = queue.run(engine.clone(), Selection::All, 0).await.unwrap();
    }

    assert!(queue.snapshot().await.pending.is_empty());
    let reason = last[0].reason.as_deref().unwrap_or_default();
    assert!(
        reason.starts_with(&format!("dropped after {MAX_ATTEMPTS} failed attempts")),
        "{reason}"
    );
}

#[tokio::test]
async fn history_keeps_the_newest_runs_only() {
    let queue = queue();
    for n in 0..(MAX_HISTORY + 5) {
        queue
            .enqueue(&Namespace::ROOT, vec![build(&format!("a{n}"))])
            .await;
        queue.run(reference(), Selection::All, 0).await.unwrap();
    }
    assert_eq!(queue.snapshot().await.history.len(), MAX_HISTORY);
}

/// A hosted `*_PLAN_REQUIRED` refusal arrives as `Engine` and refuses every
/// request on the account, so a job must keep its attempts while the account
/// lacks the plan; otherwise every queued belief build is dropped after
/// `MAX_ATTEMPTS` runs.
#[tokio::test]
async fn a_plan_refusal_never_uses_up_a_jobs_attempts() {
    let queue = queue();
    queue.enqueue(&Namespace::ROOT, vec![build("a")]).await;
    let engine: Arc<dyn MemoryEngine> = Arc::new(RefusingEngine::with(
        tinymemory_api::Error::Engine("[MEMORY_PLAN_REQUIRED] not in the plan".into()),
    ));
    for _ in 0..MAX_ATTEMPTS + 2 {
        queue.run(engine.clone(), Selection::All, 0).await.unwrap();
    }
    let state = queue.snapshot().await;
    assert_eq!(state.pending.len(), 1, "the job waits for the plan");
    assert_eq!(state.pending[0].attempts, 0);
}

#[tokio::test]
async fn bounded_runner_processes_all_claimed_jobs() {
    let queue = queue();
    queue
        .enqueue(&Namespace::ROOT, vec![build("a"), build("b")])
        .await;

    let runs = queue
        .run_with_concurrency(reference(), Selection::All, 0, 2)
        .await
        .unwrap();

    assert_eq!(runs.len(), 2);
    assert!(queue.snapshot().await.pending.is_empty());
    assert_eq!(queue.snapshot().await.history.len(), 2);
}

struct GatedEngine {
    inner: ReferenceEngine,
    release: Arc<Semaphore>,
    started: Arc<AtomicUsize>,
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}

impl GatedEngine {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: ReferenceEngine::new(),
            release: Arc::new(Semaphore::new(0)),
            started: Arc::new(AtomicUsize::new(0)),
            active: Arc::new(AtomicUsize::new(0)),
            peak: Arc::new(AtomicUsize::new(0)),
        })
    }
}

#[async_trait]
impl MemoryEngine for GatedEngine {
    fn descriptor(&self) -> &tinymemory_api::EngineDescriptor {
        self.inner.descriptor()
    }

    async fn health(&self) -> tinymemory_api::EngineHealth {
        self.inner.health().await
    }

    async fn recall(
        &self,
        req: tinymemory_api::RecallRequest,
    ) -> tinymemory_api::Result<tinymemory_api::RecallAnswer> {
        self.inner.recall(req).await
    }

    async fn fetch(
        &self,
        req: tinymemory_api::FetchRequest,
    ) -> tinymemory_api::Result<tinymemory_api::FetchPage> {
        self.inner.fetch(req).await
    }

    async fn store(
        &self,
        item: tinymemory_api::StoreItem,
    ) -> tinymemory_api::Result<tinymemory_api::StoreReceipt> {
        self.inner.store(item).await
    }

    async fn forget(
        &self,
        target: tinymemory_api::ForgetTarget,
    ) -> tinymemory_api::Result<tinymemory_api::ForgetReport> {
        self.inner.forget(target).await
    }

    async fn list(
        &self,
        req: tinymemory_api::ListRequest,
    ) -> tinymemory_api::Result<tinymemory_api::ListPage> {
        self.inner.list(req).await
    }

    async fn consolidate(
        &self,
        _req: ConsolidateRequest,
    ) -> tinymemory_api::Result<tinymemory_api::ConsolidateReceipt> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(active, Ordering::SeqCst);
        self.started.fetch_add(1, Ordering::SeqCst);
        let permit = self.release.acquire().await.unwrap();
        permit.forget();
        self.active.fetch_sub(1, Ordering::SeqCst);
        Err(tinymemory_api::Error::Engine("test gate".into()))
    }
}

#[tokio::test]
async fn bounded_runner_limits_actual_engine_overlap() {
    let queue = Arc::new(queue());
    queue
        .enqueue(&Namespace::ROOT, vec![build("a"), build("b"), build("c")])
        .await;
    let engine = GatedEngine::new();
    let run_queue = Arc::clone(&queue);
    let run_engine = Arc::clone(&engine);
    let run = tokio::spawn(async move {
        run_queue
            .run_with_concurrency(run_engine, Selection::All, 0, 2)
            .await
    });

    while engine.started.load(Ordering::SeqCst) < 2 {
        tokio::task::yield_now().await;
    }
    assert_eq!(engine.active.load(Ordering::SeqCst), 2);
    assert_eq!(engine.peak.load(Ordering::SeqCst), 2);
    assert_eq!(engine.started.load(Ordering::SeqCst), 2);

    engine.release.add_permits(2);
    while engine.started.load(Ordering::SeqCst) < 3 {
        tokio::task::yield_now().await;
    }
    engine.release.add_permits(1);
    run.await.unwrap().unwrap();
    assert_eq!(engine.peak.load(Ordering::SeqCst), 2);
}

struct FailingStore {
    state: std::sync::Mutex<QueueState>,
}

impl QueueStore for FailingStore {
    fn load(&self) -> QueueState {
        self.state.lock().unwrap().clone()
    }

    fn save(&self, _state: &QueueState) -> Result<(), String> {
        Err("test save failure".into())
    }
}

#[tokio::test]
async fn claim_save_failure_starts_no_jobs() {
    let entry = queued("root", build("a"));
    let queue = JobQueue::new(FailingStore {
        state: std::sync::Mutex::new(QueueState {
            pending: vec![entry],
            ..QueueState::default()
        }),
    });
    let engine = GatedEngine::new();

    assert!(matches!(
        queue
            .run_with_concurrency(engine.clone(), Selection::All, 0, 1)
            .await,
        Err(QueueError::Store(_))
    ));
    assert_eq!(engine.started.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cancelled_claims_are_recovered() {
    let queue = Arc::new(queue());
    queue.enqueue(&Namespace::ROOT, vec![build("a")]).await;
    let engine = GatedEngine::new();
    let run_queue = Arc::clone(&queue);
    let run_engine = Arc::clone(&engine);
    let run = tokio::spawn(async move {
        run_queue
            .run_with_concurrency(run_engine, Selection::All, 0, 1)
            .await
    });
    while engine.started.load(Ordering::SeqCst) < 1 {
        tokio::task::yield_now().await;
    }
    run.abort();
    let _ = run.await;
    assert_eq!(queue.snapshot().await.in_flight.len(), 1);

    let retry_engine = GatedEngine::new();
    let retry_queue = Arc::clone(&queue);
    let retry_engine_for_run = Arc::clone(&retry_engine);
    let retry = tokio::spawn(async move {
        retry_queue
            .run_with_concurrency(retry_engine_for_run, Selection::All, 0, 1)
            .await
    });
    while retry_engine.started.load(Ordering::SeqCst) < 1 {
        tokio::task::yield_now().await;
    }
    retry_engine.release.add_permits(1);
    retry.await.unwrap().unwrap();
    assert!(queue.snapshot().await.in_flight.is_empty());
}

#[tokio::test]
async fn retry_merge_preserves_attempts_when_requeued_while_running() {
    let queue = Arc::new(queue());
    queue.enqueue(&Namespace::ROOT, vec![build("a")]).await;
    let engine = GatedEngine::new();
    let run_queue = Arc::clone(&queue);
    let run_engine = Arc::clone(&engine);
    let run = tokio::spawn(async move {
        run_queue
            .run_with_concurrency(run_engine, Selection::All, 0, 1)
            .await
    });
    while engine.started.load(Ordering::SeqCst) < 1 {
        tokio::task::yield_now().await;
    }
    queue.enqueue(&Namespace::ROOT, vec![build("a")]).await;
    engine.release.add_permits(1);
    run.await.unwrap().unwrap();

    let state = queue.snapshot().await;
    assert_eq!(state.pending.len(), 1);
    assert_eq!(state.pending[0].attempts, 1);
}
