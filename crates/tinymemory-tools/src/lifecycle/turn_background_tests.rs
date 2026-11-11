//! Regression tests for results that finish after a host's turn cutoff.

use super::*;

struct ReleasedEngine {
    inner: Arc<ReferenceEngine>,
    release: Mutex<Option<oneshot::Receiver<()>>>,
    reads_started: AtomicUsize,
}

impl ReleasedEngine {
    async fn wait_for_release(&self) {
        self.reads_started.fetch_add(1, Ordering::SeqCst);
        let release = { self.release.lock().unwrap().take() };
        if let Some(release) = release {
            let _ = release.await;
        }
    }
}

#[async_trait]
impl MemoryEngine for ReleasedEngine {
    fn descriptor(&self) -> &EngineDescriptor {
        self.inner.descriptor()
    }

    async fn health(&self) -> EngineHealth {
        self.inner.health().await
    }

    async fn recall(&self, request: RecallRequest) -> Result<RecallAnswer> {
        self.wait_for_release().await;
        self.inner.recall(request).await
    }

    async fn fetch(&self, request: FetchRequest) -> Result<FetchPage> {
        self.wait_for_release().await;
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

async fn released_fixture(clock: FakeClock) -> (Fixture, Arc<ReleasedEngine>, oneshot::Sender<()>) {
    let inner = Arc::new(ReferenceEngine::new());
    Brain::new(inner.clone(), MemoryLayout::default())
        .ingest(BrainDocument::new(
            BrainSource::Markdown,
            "The office closes on Fridays.",
        ))
        .await
        .unwrap();

    let (release, receiver) = oneshot::channel();
    let engine = Arc::new(ReleasedEngine {
        inner,
        release: Mutex::new(Some(receiver)),
        reads_started: AtomicUsize::new(0),
    });
    let fixture = fixture_with_engine(clock, false, engine.clone(), None);
    (fixture, engine, release)
}

#[tokio::test]
async fn background_pre_turn_keeps_a_late_engine_result_without_delivering_citations() {
    let (fixture, engine, release) = released_fixture(FakeClock::at(Duration::ZERO)).await;
    let mut options = options(true, true);
    options.timeout = Duration::from_millis(1);
    options.date_hint = false;
    let mut work = Box::pin(fixture.hooks.pre_turn_background_work(
        PreTurn::new("thread", 0, "when does the office close?"),
        options,
    ));

    poll_fn(|cx| match work.as_mut().poll(cx) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(_) => panic!("background pre-turn completed before the engine was released"),
    })
    .await;
    assert!(engine.reads_started.load(Ordering::SeqCst) > 0);

    fixture.clock.advance_by(Duration::from_secs(1));
    poll_fn(|cx| match work.as_mut().poll(cx) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(_) => panic!("background pre-turn discarded work at its turn cutoff"),
    })
    .await;
    release.send(()).unwrap();

    let pack = work.await.expect("late pre-turn result");
    assert!(pack.markdown.contains("Fridays"));
    assert!(pack.refusal.is_none());
    assert!(!pack.citations.is_empty());
    assert_eq!(fixture.citations.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn background_compaction_keeps_a_late_engine_result() {
    let (fixture, engine, release) = released_fixture(FakeClock::at(Duration::ZERO)).await;
    let request = Compaction {
        thread_id: "thread".into(),
        dropped: vec![
            Turn::new(tinymemory_api::Role::User, "The office closes on Fridays."),
            Turn::new(tinymemory_api::Role::Assistant, "I will remember."),
        ],
        focus: None,
    };
    let mut work = Box::pin(fixture.hooks.compaction_background_work(request, true));

    poll_fn(|cx| match work.as_mut().poll(cx) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(_) => panic!("background compaction completed before the engine was released"),
    })
    .await;
    assert!(engine.reads_started.load(Ordering::SeqCst) > 0);

    fixture.clock.advance_by(Duration::from_secs(1));
    poll_fn(|cx| match work.as_mut().poll(cx) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(_) => panic!("background compaction discarded work at its cutoff"),
    })
    .await;
    release.send(()).unwrap();

    let pack = work.await.expect("late compaction result");
    assert!(pack.markdown.contains("Fridays"));
    assert!(pack.refusal.is_none());
    assert_eq!(fixture.citations.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn dropping_background_compaction_cancels_in_flight_engine_work() {
    let (fixture, engine_probe) = held_fixture(FakeClock::at(Duration::ZERO));
    let request = Compaction {
        thread_id: "thread".into(),
        dropped: vec![Turn::new(
            tinymemory_api::Role::User,
            "The office closes on Fridays.",
        )],
        focus: None,
    };
    let mut work = Box::pin(fixture.hooks.compaction_background_work(request, true));

    poll_fn(|cx| match work.as_mut().poll(cx) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(_) => panic!("held background compaction completed"),
    })
    .await;
    assert!(engine_probe.live.load(Ordering::SeqCst) > 0);
    assert_eq!(fixture.model.calls.load(Ordering::SeqCst), 0);

    drop(work);
    assert_eq!(engine_probe.live.load(Ordering::SeqCst), 0);
    assert_eq!(
        engine_probe.dropped.load(Ordering::SeqCst),
        engine_probe.started.load(Ordering::SeqCst)
    );
}

#[tokio::test]
async fn dropping_background_pre_turn_cancels_in_flight_engine_and_model_work() {
    let (fixture, engine_probe) = held_fixture(FakeClock::at(Duration::ZERO));
    fixture.model.held.store(true, Ordering::SeqCst);
    let model_probe = fixture.model.probe.clone();
    let mut options = options(true, true);
    options.timeout = Duration::from_secs(10);
    let mut work = Box::pin(
        fixture
            .hooks
            .pre_turn_background_work(PreTurn::new("thread", 0, "yesterday"), options),
    );

    poll_fn(|cx| match work.as_mut().poll(cx) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(_) => panic!("held background pre-turn completed"),
    })
    .await;
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
}

#[tokio::test]
async fn background_pre_turn_still_bounds_its_date_hint_subcall() {
    let (fixture, engine_probe) = held_fixture(FakeClock::at(Duration::ZERO));
    fixture.model.held.store(true, Ordering::SeqCst);
    let model_probe = fixture.model.probe.clone();
    let mut options = options(true, true);
    options.timeout = Duration::from_secs(1);
    let mut work = Box::pin(
        fixture
            .hooks
            .pre_turn_background_work(PreTurn::new("thread", 0, "yesterday"), options),
    );

    poll_fn(|cx| match work.as_mut().poll(cx) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(_) => panic!("held background pre-turn completed before the date-hint budget"),
    })
    .await;
    assert!(model_probe.started.load(Ordering::SeqCst) > 0);
    fixture.clock.advance_by(Duration::from_millis(900));
    poll_fn(|cx| match work.as_mut().poll(cx) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(_) => panic!("background pre-turn completed while its engine read was held"),
    })
    .await;

    assert_eq!(model_probe.live.load(Ordering::SeqCst), 0);
    assert_eq!(
        model_probe.dropped.load(Ordering::SeqCst),
        model_probe.started.load(Ordering::SeqCst)
    );
    assert!(engine_probe.live.load(Ordering::SeqCst) > 0);
    fixture.clock.advance_by(Duration::from_secs(1));
    poll_fn(|cx| match work.as_mut().poll(cx) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(_) => panic!("background pre-turn inherited the date-hint cutoff"),
    })
    .await;
    assert!(engine_probe.live.load(Ordering::SeqCst) > 0);
    drop(work);
    assert_eq!(engine_probe.live.load(Ordering::SeqCst), 0);
}
