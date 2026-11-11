//! Layout migration scans, status, service, and filesystem-claim behavior.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use tinymemory_api::MemoryEngine;

use super::super::copy::Engines;
use super::super::job::{LayoutHost, LegacyClaim};
use super::super::job::{Trigger, run};
use super::super::map::Placement;
use super::super::state::{MigrationState, MigrationStateStore};
use super::super::test_host::{FakeHost, count, fact};
use super::*;

async fn move_all(host: &FakeHost) {
    run(host, Trigger::Auto, || async { false }).await.unwrap();
}

#[derive(Clone)]
struct MutexStartGate(Arc<Mutex<()>>);

impl crate::run_slots::StartGate for MutexStartGate {
    fn with_gate(&self, reserve: &mut dyn FnMut() -> bool) -> bool {
        let _gate = self.0.lock().unwrap();
        reserve()
    }
}

#[derive(Debug, PartialEq, Eq)]
enum GateEvent {
    GateCalled,
    ImportChecked(bool),
    MigrationChecked(bool),
    MigrationStarted(bool),
}

#[derive(Clone)]
struct SignalStartGate {
    lock: Arc<Mutex<()>>,
    events: std::sync::mpsc::Sender<GateEvent>,
}

impl crate::run_slots::StartGate for SignalStartGate {
    fn with_gate(&self, reserve: &mut dyn FnMut() -> bool) -> bool {
        self.events.send(GateEvent::GateCalled).unwrap();
        let _gate = self.lock.lock().unwrap();
        reserve()
    }
}

#[derive(Clone, Default)]
struct CapturingSpawner(Arc<Mutex<Option<MigrationTask>>>);

impl MigrationSpawner for CapturingSpawner {
    fn spawn(&self, task: MigrationTask) -> std::result::Result<(), String> {
        *self.0.lock().unwrap() = Some(task);
        Ok(())
    }
}

impl CapturingSpawner {
    fn take(&self) -> MigrationTask {
        self.0.lock().unwrap().take().expect("a task was accepted")
    }

    fn is_empty(&self) -> bool {
        self.0.lock().unwrap().is_none()
    }
}

#[derive(Default)]
struct RejectingSpawner;

impl MigrationSpawner for RejectingSpawner {
    fn spawn(&self, task: MigrationTask) -> std::result::Result<(), String> {
        drop(task);
        Err("executor refused migration".to_string())
    }
}

struct CountingStore {
    state: Mutex<MigrationState>,
    saves: std::sync::atomic::AtomicUsize,
}

impl Default for CountingStore {
    fn default() -> Self {
        Self {
            state: Mutex::new(MigrationState::default()),
            saves: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

impl MigrationStateStore for CountingStore {
    fn load(&self) -> crate::layout_migration::Result<MigrationState> {
        Ok(self.state.lock().unwrap().clone())
    }

    fn save(&self, state: &MigrationState) -> crate::layout_migration::Result<()> {
        self.saves.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        *self.state.lock().unwrap() = state.clone();
        Ok(())
    }
}

struct CountingHost {
    inner: FakeHost,
    store: CountingStore,
    fail_engines: bool,
    blocked_legacy: Option<Arc<BlockingListEngine>>,
}

impl CountingHost {
    async fn new(learnings: usize, fail_engines: bool) -> Self {
        Self {
            inner: FakeHost::with(learnings).await,
            store: CountingStore::default(),
            fail_engines,
            blocked_legacy: None,
        }
    }

    async fn with_blocked_legacy() -> (Self, tokio::sync::oneshot::Receiver<()>) {
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let host = Self {
            inner: FakeHost::with(0).await,
            store: CountingStore::default(),
            fail_engines: false,
            blocked_legacy: Some(Arc::new(BlockingListEngine {
                inner: tinymemory_api::conformance::ReferenceEngine::new(),
                entered: Mutex::new(Some(entered_tx)),
            })),
        };
        (host, entered_rx)
    }
}

struct BlockingListEngine {
    inner: tinymemory_api::conformance::ReferenceEngine,
    entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

#[async_trait::async_trait]
impl MemoryEngine for BlockingListEngine {
    fn descriptor(&self) -> &tinymemory_api::EngineDescriptor {
        self.inner.descriptor()
    }

    async fn health(&self) -> tinymemory_api::EngineHealth {
        self.inner.health().await
    }

    async fn recall(
        &self,
        request: tinymemory_api::RecallRequest,
    ) -> tinymemory_api::Result<tinymemory_api::RecallAnswer> {
        self.inner.recall(request).await
    }

    async fn fetch(
        &self,
        request: tinymemory_api::FetchRequest,
    ) -> tinymemory_api::Result<tinymemory_api::FetchPage> {
        self.inner.fetch(request).await
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
        _request: tinymemory_api::ListRequest,
    ) -> tinymemory_api::Result<tinymemory_api::ListPage> {
        if let Some(entered) = self.entered.lock().unwrap().take() {
            let _ = entered.send(());
        }
        std::future::pending().await
    }

    async fn export(
        &self,
        _request: tinymemory_api::ListRequest,
    ) -> tinymemory_api::Result<tinymemory_api::ExportPage> {
        if let Some(entered) = self.entered.lock().unwrap().take() {
            let _ = entered.send(());
        }
        std::future::pending().await
    }
}

#[async_trait::async_trait]
impl LayoutHost for CountingHost {
    fn store(&self) -> &dyn MigrationStateStore {
        &self.store
    }

    async fn engines(&self) -> crate::layout_migration::Result<Engines> {
        if self.fail_engines {
            Err(crate::layout_migration::Error::Off(
                "engine unavailable".to_string(),
            ))
        } else if let Some(legacy) = &self.blocked_legacy {
            Ok(Engines {
                legacy: legacy.clone(),
                tree: self.inner.tree.clone(),
            })
        } else {
            self.inner.engines().await
        }
    }

    fn placement(&self) -> crate::layout_migration::Result<Placement> {
        self.inner.placement()
    }

    fn is_switched(&self) -> bool {
        self.inner.is_switched()
    }

    async fn switch(&self) -> crate::layout_migration::Result<()> {
        self.inner.switch().await
    }

    async fn free_now(&self) -> bool {
        self.inner.free_now().await
    }

    async fn legacy_claim(&self) -> crate::layout_migration::Result<Option<Box<dyn LegacyClaim>>> {
        self.inner.legacy_claim().await
    }

    fn listed_unconfirmed(&self) -> bool {
        self.inner.listed_unconfirmed()
    }

    async fn wait_for_listing(&self) {
        self.inner.wait_for_listing().await
    }
}

fn migration_service<G, S>(slots: RunSlots<PathBuf>, gate: G, spawner: S) -> MigrationService<G, S>
where
    G: StartGate,
    S: MigrationSpawner,
{
    MigrationService::new(slots, gate, spawner)
}

#[tokio::test]
async fn scan_finds_legacy_memory_and_nothing_after_the_move() {
    let host = FakeHost::with(3).await;
    assert_eq!(
        scan(&host).await.unwrap(),
        ScanView {
            needed: true,
            shared: false
        }
    );
    move_all(&host).await;
    assert!(!scan(&host).await.unwrap().needed);
    assert_eq!(status(&host, false).unwrap().state.phase, Phase::Cleaned);
}

#[tokio::test]
async fn nothing_to_move_shows_nothing() {
    let host = FakeHost::with(0).await;
    assert!(!scan(&host).await.unwrap().needed);
}

#[tokio::test]
async fn a_stopped_run_reads_as_interrupted_until_one_is_going() {
    let host = FakeHost::with(120).await;
    let mut stopped = host.store.load().unwrap();
    stopped.phase = Phase::Copying;
    host.store.save(&stopped).unwrap();
    assert!(status(&host, false).unwrap().interrupted);
    let going = status(&host, true).unwrap();
    assert!(going.running && !going.interrupted);

    assert!(!is_finished(&host).unwrap());
    move_all(&host).await;
    assert_eq!(status(&host, false).unwrap().state.phase, Phase::Cleaned);
    assert!(is_finished(&host).unwrap(), "nothing left to start");
}

#[tokio::test]
async fn a_cleaned_migration_with_an_unconfirmed_import_is_not_finished() {
    let host = FakeHost::with(2).await;
    host.unconfirmed.store(true, Ordering::SeqCst);
    move_all(&host).await;
    assert!(host.store.load().unwrap().rechecked);
    assert!(is_finished(&host).unwrap());

    let mut state = host.store.load().unwrap();
    state.rechecked = false;
    host.store.save(&state).unwrap();
    assert!(
        !is_finished(&host).unwrap(),
        "the import still has to be re-checked"
    );
}

#[tokio::test]
async fn retry_puts_a_lost_copy_back_in_line() {
    let host = FakeHost::with(4).await;
    move_all(&host).await;
    assert_eq!(status(&host, false).unwrap().state.phase, Phase::Cleaned);

    // A legacy item the move did not reach (written after cleanup).
    host.legacy.store(fact("late")).await.unwrap();
    let state = retry(&host, false).unwrap();
    assert!(!state.caught_up && !state.cleaning);
    run(&host, Trigger::Manual { takeover: false }, || async {
        false
    })
    .await
    .unwrap();
    assert_eq!(count(host.tree.as_ref()).await, 5);
    assert_eq!(count(host.legacy.as_ref()).await, 0);
}

#[tokio::test]
async fn retry_refuses_while_a_run_is_going() {
    let host = FakeHost::with(1).await;
    assert!(matches!(retry(&host, true), Err(Error::InvalidRequest(_))));
}

#[tokio::test]
async fn a_failed_run_is_recorded_as_paused_with_its_reason() {
    let host = FakeHost::with(1).await;
    record_failure(&host, &Error::Off("sign in".into()));
    let state = host.store.load().unwrap();
    assert_eq!(state.phase, Phase::Paused);
    assert_eq!(state.error.as_deref(), Some("memory is off: sign in"));
}

#[cfg(feature = "fs-claim")]
#[test]
fn filesystem_claim_normalizes_endpoints_and_preserves_first_winner() {
    use crate::layout_migration::{FsLegacyClaim, LegacyClaim};

    let tmp = tempfile::tempdir().unwrap();
    let first = FsLegacyClaim::new(tmp.path(), " HTTP://127.0.0.1:3141/ ", "user:first");
    let same_endpoint = FsLegacyClaim::new(tmp.path(), "http://127.0.0.1:3141", "user:second");
    let different_endpoint = FsLegacyClaim::new(tmp.path(), "http://127.0.0.1:3142", "user:second");
    assert_eq!(first.marker_path(), same_endpoint.marker_path());
    assert_ne!(first.marker_path(), different_endpoint.marker_path());
    assert!(!first.held_by_other().unwrap());
    assert!(first.take().unwrap());
    assert!(first.take().unwrap(), "the owner can safely take it again");
    assert!(same_endpoint.held_by_other().unwrap());
    assert!(!same_endpoint.take().unwrap());
    assert!(!first.held_by_other().unwrap());
    assert!(first.marker_path().is_file());
}

#[cfg(feature = "fs-claim")]
#[test]
fn filesystem_claim_race_has_one_winner_and_torn_markers_are_errors() {
    use crate::layout_migration::{FsLegacyClaim, LegacyClaim};
    use std::sync::Arc;

    let tmp = tempfile::tempdir().unwrap();
    let first = Arc::new(FsLegacyClaim::new(
        tmp.path(),
        "http://127.0.0.1:3141",
        "user:a",
    ));
    let second = Arc::new(FsLegacyClaim::new(
        tmp.path(),
        "http://127.0.0.1:3141",
        "user:b",
    ));
    let a = Arc::clone(&first);
    let b = Arc::clone(&second);
    let first_result = std::thread::spawn(move || a.take().unwrap());
    let second_result = std::thread::spawn(move || b.take().unwrap());
    let took_first = first_result.join().unwrap();
    let took_second = second_result.join().unwrap();
    assert_ne!(took_first, took_second, "hard-link creation has one winner");

    let winner = if took_first { first } else { second };
    let torn = FsLegacyClaim::new(tmp.path(), "http://127.0.0.1:3142", "user:torn");
    std::fs::create_dir_all(torn.marker_path().parent().unwrap()).unwrap();
    std::fs::write(torn.marker_path(), b"{ torn").unwrap();
    assert!(torn.held_by_other().is_err());
    assert!(torn.take().is_err());
    assert!(!winner.held_by_other().unwrap());
}

#[tokio::test]
async fn separate_services_share_slots_and_dropped_tasks_release_them() {
    let slots = RunSlots::new();
    let gate = MutexStartGate(Arc::new(Mutex::new(())));
    let first_spawner = CapturingSpawner::default();
    let first = migration_service(slots.clone(), gate.clone(), first_spawner.clone());
    let second_spawner = CapturingSpawner::default();
    let second = migration_service(slots.clone(), gate, second_spawner.clone());
    let host = Arc::new(CountingHost::new(1, false).await);
    let key = PathBuf::from("workspace-a");

    assert!(first.start(key.clone(), host.clone(), || false, Trigger::Auto, || false,));
    assert!(!second.start(key.clone(), host, || false, Trigger::Auto, || false,));
    assert!(slots.is_running(&key));
    assert!(second_spawner.is_empty());

    drop(first_spawner.take());
    assert!(
        !slots.is_running(&key),
        "dropping a queued task releases its slot"
    );
}

#[tokio::test]
async fn rejected_spawn_records_one_failure_and_releases_its_slot() {
    let slots = RunSlots::new();
    let service = migration_service(
        slots.clone(),
        MutexStartGate(Arc::new(Mutex::new(()))),
        RejectingSpawner,
    );
    let host = Arc::new(CountingHost::new(1, false).await);
    let key = PathBuf::from("workspace-rejected");

    assert!(!service.start(key.clone(), host.clone(), || false, Trigger::Auto, || false,));

    let state = host.store.load().unwrap();
    assert_eq!(host.store.saves.load(Ordering::SeqCst), 1);
    assert_eq!(state.phase, Phase::Paused);
    assert!(state.error.unwrap().contains("executor refused migration"));
    assert!(!slots.is_running(&key));
}

#[tokio::test]
async fn run_failure_records_once_and_cancelling_an_accepted_task_releases_its_slot() {
    let slots = RunSlots::new();
    let spawner = CapturingSpawner::default();
    let service = migration_service(
        slots.clone(),
        MutexStartGate(Arc::new(Mutex::new(()))),
        spawner.clone(),
    );
    let host = Arc::new(CountingHost::new(1, true).await);
    let key = PathBuf::from("workspace-failed");
    assert!(service.start(key.clone(), host.clone(), || false, Trigger::Auto, || false,));

    spawner.take().await;

    let state = host.store.load().unwrap();
    assert_eq!(host.store.saves.load(Ordering::SeqCst), 1);
    assert_eq!(state.phase, Phase::Paused);
    assert_eq!(
        state.error.as_deref(),
        Some("memory is off: engine unavailable")
    );
    assert!(!slots.is_running(&key));

    assert!(service.start(key.clone(), host, || false, Trigger::Auto, || false,));
    drop(spawner.take());
    assert!(!slots.is_running(&key));
}

#[tokio::test]
async fn cancelling_a_pending_migration_releases_the_slot_without_recording_failure() {
    let slots = RunSlots::new();
    let spawner = CapturingSpawner::default();
    let service = migration_service(
        slots.clone(),
        MutexStartGate(Arc::new(Mutex::new(()))),
        spawner.clone(),
    );
    let (host, entered) = CountingHost::with_blocked_legacy().await;
    let host = Arc::new(host);
    let key = PathBuf::from("workspace-cancelled");
    let persisted = MigrationState {
        phase: Phase::Copying,
        ..MigrationState::default()
    };
    host.store.save(&persisted).unwrap();

    assert!(service.start(key.clone(), host.clone(), || false, Trigger::Auto, || false,));
    let handle = tokio::spawn(spawner.take());
    entered.await.unwrap();
    let saves_before_cancel = host.store.saves.load(Ordering::SeqCst);
    let state_before_cancel = host.store.load().unwrap();
    assert_eq!(state_before_cancel.phase, Phase::Copying);
    handle.abort();
    assert!(handle.await.unwrap_err().is_cancelled());

    assert!(!slots.is_running(&key));
    assert_eq!(
        host.store.saves.load(Ordering::SeqCst),
        saves_before_cancel,
        "cancellation releases the slot without recording a new failure"
    );
    let state = host.store.load().unwrap();
    assert_eq!(state, state_before_cancel);
    assert!(status(host.as_ref(), false).unwrap().interrupted);
}

#[tokio::test]
async fn tick_checks_finished_then_pause_then_running_before_starting() {
    let slots = RunSlots::new();
    let spawner = CapturingSpawner::default();
    let service = migration_service(
        slots.clone(),
        MutexStartGate(Arc::new(Mutex::new(()))),
        spawner.clone(),
    );
    let finished_host = Arc::new(CountingHost::new(0, false).await);
    let finished = MigrationState {
        phase: Phase::Cleaned,
        ..MigrationState::default()
    };
    finished_host.store.save(&finished).unwrap();
    let paused_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let paused_count = Arc::clone(&paused_calls);
    assert!(!service.tick(
        PathBuf::from("workspace-finished"),
        finished_host,
        || panic!("finished migrations skip peer and pause checks"),
        move || {
            paused_count.fetch_add(1, Ordering::SeqCst);
            false
        },
    ));
    assert_eq!(paused_calls.load(Ordering::SeqCst), 0);
    assert!(spawner.is_empty());

    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let calls_for_pause = Arc::clone(&calls);
    assert!(!service.tick(
        PathBuf::from("workspace-paused"),
        Arc::new(CountingHost::new(1, false).await),
        || panic!("paused migrations do not check peer state"),
        move || {
            calls_for_pause.fetch_add(1, Ordering::SeqCst);
            true
        },
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(spawner.is_empty(), "a paused tick starts no run");

    let host = Arc::new(CountingHost::new(1, false).await);
    let key = PathBuf::from("workspace-running");
    assert!(service.start(key.clone(), host.clone(), || false, Trigger::Auto, || false,));
    let running_task = spawner.take();
    assert!(!service.tick(
        key.clone(),
        host,
        || panic!("already-running migrations do not check peer state"),
        || false,
    ));
    assert!(
        spawner.is_empty(),
        "an unpaused running tick starts no second run"
    );
    assert!(slots.is_running(&key));
    drop(running_task);

    assert!(service.tick(
        PathBuf::from("workspace-tick-start"),
        Arc::new(CountingHost::new(1, false).await),
        || false,
        || false,
    ));
    drop(spawner.take());
}

#[tokio::test]
async fn shared_start_gate_serializes_import_and_migration_reservations() {
    use crate::run_slots::StartGate;
    use std::sync::mpsc;

    let (events_tx, events_rx) = mpsc::channel();
    let gate = SignalStartGate {
        lock: Arc::new(Mutex::new(())),
        events: events_tx.clone(),
    };
    let import_running = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (release_tx, release_rx) = mpsc::channel();
    let import_gate = gate.clone();
    let import_flag = Arc::clone(&import_running);
    let import = std::thread::spawn(move || {
        let mut reserve = || {
            let peer_running = false;
            import_gate
                .events
                .send(GateEvent::ImportChecked(peer_running))
                .unwrap();
            release_rx.recv().unwrap();
            import_flag.store(true, Ordering::SeqCst);
            true
        };
        import_gate.with_gate(&mut reserve)
    });
    assert_eq!(events_rx.recv().unwrap(), GateEvent::GateCalled);
    assert_eq!(events_rx.recv().unwrap(), GateEvent::ImportChecked(false));

    let slots = RunSlots::new();
    let spawner = CapturingSpawner::default();
    let service = migration_service(slots, gate, spawner.clone());
    let migration_import_flag = Arc::clone(&import_running);
    let migration_events = events_tx;
    let host: Arc<dyn LayoutHost> = Arc::new(CountingHost::new(1, false).await);
    let migration = std::thread::spawn(move || {
        let started = service.start(
            PathBuf::from("workspace-interlock"),
            host,
            || {
                let peer_running = migration_import_flag.load(Ordering::SeqCst);
                migration_events
                    .send(GateEvent::MigrationChecked(peer_running))
                    .unwrap();
                peer_running
            },
            Trigger::Auto,
            || false,
        );
        let _ = migration_events.send(GateEvent::MigrationStarted(started));
        started
    });
    // A correct service enters the same gate before checking import state.
    // The event is sent immediately before it blocks on the import's lock.
    let second_gate_call = events_rx.recv().unwrap();
    release_tx.send(()).unwrap();
    let remaining: Vec<_> = std::iter::from_fn(|| events_rx.recv().ok())
        .take(2)
        .collect();
    import.join().unwrap();
    let migration_started = migration.join().unwrap();

    if !spawner.is_empty() {
        drop(spawner.take());
    }

    assert_eq!(second_gate_call, GateEvent::GateCalled);
    assert_eq!(
        remaining,
        [
            GateEvent::MigrationChecked(true),
            GateEvent::MigrationStarted(false)
        ]
    );
    assert!(!migration_started);
    assert!(spawner.is_empty());
    assert!(import_running.load(Ordering::SeqCst));
}
