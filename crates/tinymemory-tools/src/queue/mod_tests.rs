use super::*;

use crate::layout_migration::test_host::RefusingEngine;
use tinymemory_api::conformance::ReferenceEngine;
use tinymemory_api::{ConsolidateRequest, ItemKind, Reach};

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
async fn plan_refusal_does_not_consume_a_jobs_attempts() {
    let queue = queue();
    queue.enqueue(&Namespace::ROOT, vec![build("a")]).await;
    let engine: Arc<dyn MemoryEngine> = Arc::new(RefusingEngine::with(
        tinymemory_api::Error::Unauthorized("[MEMORY_PLAN_REQUIRED] upgrade".into()),
    ));

    for _ in 0..MAX_ATTEMPTS + 2 {
        let runs = queue.run(engine.clone(), Selection::All, 0).await.unwrap();
        assert_eq!(runs[0].outcome, "failed");
    }

    let state = queue.snapshot().await;
    assert_eq!(state.pending.len(), 1, "the job waits for an upgrade");
    assert_eq!(state.pending[0].attempts, 0);
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
