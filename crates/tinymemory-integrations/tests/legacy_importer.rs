//! The resumable `Importer`: consent, checkpoints, retries, refused items,
//! pausing and the failure classes, over a reference engine that can be told
//! to misbehave.

#![allow(clippy::expect_used, clippy::unwrap_used, dead_code)]

mod support;

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use support::{MEMORY_DDL, doc, workspace};
use tinymemory_api::conformance::ReferenceEngine;
use tinymemory_api::{
    EngineDescriptor, EngineHealth, Error, FetchPage, FetchRequest, ForgetReport, ForgetTarget,
    ListPage, ListRequest, MemoryEngine, RecallAnswer, RecallRequest, StoreItem, StoreReceipt,
};
use tinymemory_integrations::import::{
    CheckpointStore, FailedItem, ImportError, ImportFile, ImportPhase, Importer,
    MemoryCheckpointStore, Pacer, ResumePlan, RetryPolicy, record_resume_failure,
};

const COUNT: usize = 60;

/// How the engine answers a store.
#[derive(Clone)]
enum Mode {
    Healthy,
    Unavailable,
    Credits,
    /// Refuses these legacy ids as invalid, taking the rest.
    Refuse(HashSet<String>),
    /// Refuses every bulk store as invalid, so items go one at a time, and
    /// is unavailable after this many single stores.
    DownAfter(usize),
}

struct Scripted {
    inner: ReferenceEngine,
    mode: Mutex<Mode>,
    calls: AtomicUsize,
    singles: AtomicUsize,
}

impl Scripted {
    fn new(mode: Mode) -> Arc<Self> {
        Arc::new(Self {
            inner: ReferenceEngine::new(),
            mode: Mutex::new(mode),
            calls: AtomicUsize::new(0),
            singles: AtomicUsize::new(0),
        })
    }

    fn set(&self, mode: Mode) {
        *self.mode.lock().unwrap() = mode;
    }

    fn check(&self, items: &[StoreItem]) -> tinymemory_api::Result<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.mode.lock().unwrap().clone() {
            Mode::Healthy => Ok(()),
            Mode::Unavailable => Err(Error::Unavailable("engine down".into())),
            Mode::Credits => Err(Error::InsufficientCredits(
                "[USER_INSUFFICIENT_CREDITS] top up".into(),
            )),
            Mode::Refuse(ids) => {
                let bad = items
                    .iter()
                    .any(|item| ids.contains(item.meta().source.id.as_deref().unwrap_or("")));
                if bad {
                    Err(Error::InvalidRequest("too large".into()))
                } else {
                    Ok(())
                }
            }
            Mode::DownAfter(_) if items.len() > 1 => {
                Err(Error::InvalidRequest("batch too large".into()))
            }
            Mode::DownAfter(limit) => {
                if self.singles.fetch_add(1, Ordering::SeqCst) < limit {
                    Ok(())
                } else {
                    Err(Error::Unavailable("engine down".into()))
                }
            }
        }
    }
}

#[async_trait]
impl MemoryEngine for Scripted {
    fn descriptor(&self) -> &EngineDescriptor {
        self.inner.descriptor()
    }
    async fn health(&self) -> EngineHealth {
        self.inner.health().await
    }
    async fn recall(&self, req: RecallRequest) -> tinymemory_api::Result<RecallAnswer> {
        self.inner.recall(req).await
    }
    async fn fetch(&self, req: FetchRequest) -> tinymemory_api::Result<FetchPage> {
        self.inner.fetch(req).await
    }
    async fn store(&self, item: StoreItem) -> tinymemory_api::Result<StoreReceipt> {
        self.check(std::slice::from_ref(&item))?;
        self.inner.store(item).await
    }
    async fn store_many(&self, items: Vec<StoreItem>) -> tinymemory_api::Result<Vec<StoreReceipt>> {
        self.check(&items)?;
        self.inner.store_many(items).await
    }
    async fn forget(&self, target: ForgetTarget) -> tinymemory_api::Result<ForgetReport> {
        self.inner.forget(target).await
    }
    async fn list(&self, req: ListRequest) -> tinymemory_api::Result<ListPage> {
        self.inner.list(req).await
    }
}

/// Records every wait instead of sleeping.
#[derive(Default)]
struct CountingPacer {
    sleeps: Mutex<Vec<Duration>>,
}

#[async_trait]
impl Pacer for CountingPacer {
    async fn sleep(&self, delay: Duration) {
        self.sleeps.lock().unwrap().push(delay);
    }
}

fn legacy() -> tempfile::TempDir {
    let (dir, conn) = workspace(MEMORY_DDL);
    for index in 0..COUNT {
        doc(
            &conn,
            &format!("d{index:03}"),
            "document_notes",
            None,
            &format!("Note {index}"),
            &format!("Body of note {index}."),
            "[]",
            "{}",
            1_700_000_000.0 + index as f64,
        );
    }
    dir
}

struct Rig {
    importer: Importer,
    store: Arc<MemoryCheckpointStore>,
    pacer: Arc<CountingPacer>,
}

fn rig(dir: &std::path::Path, engine: Arc<Scripted>, consent: bool) -> Rig {
    let store = Arc::new(MemoryCheckpointStore::default());
    let pacer = Arc::new(CountingPacer::default());
    let importer = Importer::new(
        dir,
        engine,
        store.clone(),
        pacer.clone(),
        Arc::new(move || consent),
    )
    .with_retry_policy(RetryPolicy {
        delays: vec![Duration::from_millis(1), Duration::from_millis(2)],
    });
    Rig {
        importer,
        store,
        pacer,
    }
}

async fn import(rig: &Rig) -> ImportFile {
    let begun = rig.importer.begin().unwrap();
    rig.importer.run(begun).await;
    rig.store.load()
}

#[test]
fn resume_plan_covers_phase_billing_pause_and_retry_state() {
    let phases = [
        ImportPhase::Idle,
        ImportPhase::Running,
        ImportPhase::Done,
        ImportPhase::Error,
    ];
    for phase in phases {
        for paused_for_credits in [false, true] {
            for billing_ok in [false, true] {
                for paused in [false, true] {
                    for retrying in [false, true] {
                        let file = ImportFile {
                            state: tinymemory_integrations::import::ImportState {
                                phase,
                                ..Default::default()
                            },
                            paused_for_credits,
                            retrying,
                            ..Default::default()
                        };
                        let expected = if paused {
                            ResumePlan::DoNotResume
                        } else {
                            match phase {
                                ImportPhase::Running if retrying => ResumePlan::ResumeRetry,
                                ImportPhase::Running => ResumePlan::ResumeImport,
                                ImportPhase::Error if paused_for_credits && billing_ok => {
                                    ResumePlan::ResumeImport
                                }
                                _ => ResumePlan::DoNotResume,
                            }
                        };
                        assert_eq!(
                            Importer::resume_plan(&file, billing_ok, paused),
                            expected,
                            "phase={phase:?}, credits={paused_for_credits}, billing={billing_ok}, paused={paused}, retrying={retrying}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn store_only_resume_failure_preserves_checkpoint_and_failed_items() {
    let mut file = ImportFile::default();
    file.state.phase = ImportPhase::Running;
    file.retrying = true;
    file.failed.push(FailedItem {
        id: "memory_docs:retry-me".to_string(),
        reason: "invalid item".to_string(),
    });
    file.checkpoint.documents = Some("last-saved-document".to_string());
    let expected_checkpoint = file.checkpoint.clone();
    let expected_failed = file.failed.clone();
    let store = MemoryCheckpointStore::new(file);

    record_resume_failure(&store, "cannot reach engine");

    let saved = store.load();
    assert_eq!(saved.state.phase, ImportPhase::Done);
    assert_eq!(
        saved.state.error.as_deref(),
        Some("the retry could not resume: cannot reach engine; press Retry again")
    );
    assert!(!saved.retrying);
    assert_eq!(saved.checkpoint, expected_checkpoint);
    assert_eq!(saved.failed, expected_failed);
}

#[test]
fn import_resume_failure_clears_credits_pause_and_preserves_progress() {
    let mut file = ImportFile::default();
    file.state.phase = ImportPhase::Error;
    file.paused_for_credits = true;
    file.checkpoint.documents = Some("saved-document".to_string());
    file.failed.push(FailedItem {
        id: "memory_docs:bad-item".to_string(),
        reason: "too large".to_string(),
    });
    let expected_checkpoint = file.checkpoint.clone();
    let expected_failed = file.failed.clone();
    let store = MemoryCheckpointStore::new(file);

    record_resume_failure(&store, "account is signed out");

    let saved = store.load();
    assert_eq!(saved.state.phase, ImportPhase::Error);
    assert_eq!(
        saved.state.error.as_deref(),
        Some("the import could not resume: account is signed out")
    );
    assert!(!saved.paused_for_credits);
    assert_eq!(saved.checkpoint, expected_checkpoint);
    assert_eq!(saved.failed, expected_failed);
}

struct RejectingCheckpointStore {
    file: Mutex<ImportFile>,
    attempts: AtomicUsize,
}

impl CheckpointStore for RejectingCheckpointStore {
    fn load(&self) -> ImportFile {
        self.file.lock().unwrap().clone()
    }

    fn save(&self, _file: &ImportFile) -> Result<(), String> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        Err("checkpoint is read-only".to_string())
    }
}

#[test]
fn store_only_resume_failure_logs_a_rejected_save_without_mutating_loaded_state() {
    let mut file = ImportFile::default();
    file.state.phase = ImportPhase::Running;
    file.checkpoint.documents = Some("last-known-checkpoint".to_string());
    let store = RejectingCheckpointStore {
        file: Mutex::new(file.clone()),
        attempts: AtomicUsize::new(0),
    };

    record_resume_failure(&store, "engine unavailable");

    assert_eq!(store.attempts.load(Ordering::SeqCst), 1);
    assert_eq!(store.load(), file, "a failed save cannot appear persisted");
}

#[test]
fn an_import_without_consent_does_not_begin() {
    let dir = legacy();
    let rig = rig(dir.path(), Scripted::new(Mode::Healthy), false);
    assert_eq!(
        rig.importer.begin().unwrap_err(),
        ImportError::ConsentRequired
    );
    assert_eq!(rig.store.load().state.phase, ImportPhase::Idle);
}

#[test]
fn a_workspace_with_no_legacy_store_does_not_begin() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(dir.path(), Scripted::new(Mode::Healthy), true);
    assert_eq!(
        rig.importer.begin().unwrap_err(),
        ImportError::NoLegacyStore
    );
}

#[tokio::test]
async fn a_full_import_stores_everything_and_ends_done() {
    let dir = legacy();
    let engine = Scripted::new(Mode::Healthy);
    let rig = rig(dir.path(), engine.clone(), true);
    let begun = rig.importer.begin().unwrap();
    assert_eq!(begun.state.phase, ImportPhase::Running);
    assert_eq!(begun.state.total, COUNT as u64);
    rig.importer.run(begun).await;
    let file = rig.store.load();
    assert_eq!(file.state.phase, ImportPhase::Done);
    assert_eq!(file.state.imported, COUNT as u64);
    assert_eq!(file.checkpoint.documents.as_deref(), Some("d059"));
    assert!(file.failed.is_empty());
    assert!(!file.listed_unconfirmed);
    assert_eq!(engine.inner.len(), COUNT);
}

#[tokio::test]
async fn a_refused_item_is_skipped_listed_and_retried_once_the_engine_takes_it() {
    let dir = legacy();
    let engine = Scripted::new(Mode::Refuse(HashSet::from(
        ["memory_docs:d030".to_string()],
    )));
    let rig = rig(dir.path(), engine.clone(), true);
    let file = import(&rig).await;
    assert_eq!(file.state.phase, ImportPhase::Done);
    assert_eq!(file.state.imported, COUNT as u64 - 1);
    assert_eq!(file.state.failed, 1);
    assert_eq!(file.failed.len(), 1);
    assert_eq!(file.failed[0].id, "memory_docs:d030");
    assert!(file.failed[0].reason.contains("too large"));

    // Still refused: stays listed.
    let begun = rig.importer.begin_retry().unwrap();
    assert!(rig.store.load().retrying);
    rig.importer.run_retry(begun).await;
    let file = rig.store.load();
    assert_eq!(file.failed.len(), 1);
    assert_eq!(file.state.phase, ImportPhase::Done);
    assert!(!file.retrying);

    // Taken now: leaves the list.
    engine.set(Mode::Healthy);
    let begun = rig.importer.begin_retry().unwrap();
    rig.importer.run_retry(begun).await;
    let file = rig.store.load();
    assert!(file.failed.is_empty());
    assert_eq!(file.state.failed, 0);
    assert_eq!(file.state.imported, COUNT as u64);
    assert_eq!(engine.inner.len(), COUNT);
    assert_eq!(
        rig.importer.begin_retry().unwrap_err(),
        ImportError::NothingToRetry
    );
}

#[tokio::test]
async fn an_unavailable_engine_is_retried_with_backoff_then_left_running_to_resume() {
    let dir = legacy();
    let engine = Scripted::new(Mode::Unavailable);
    let rig = rig(dir.path(), engine.clone(), true);
    let file = import(&rig).await;
    assert_eq!(file.state.phase, ImportPhase::Running);
    assert!(file.state.error.as_deref().unwrap().contains("unavailable"));
    assert!(!file.paused_for_credits);
    assert!(file.checkpoint.is_start());
    assert_eq!(
        *rig.pacer.sleeps.lock().unwrap(),
        vec![Duration::from_millis(1), Duration::from_millis(2)]
    );

    // The engine is back: the next run resumes from the start and finishes.
    engine.set(Mode::Healthy);
    let file = import(&rig).await;
    assert_eq!(file.state.phase, ImportPhase::Done);
    assert_eq!(file.state.imported, COUNT as u64);
    assert!(file.state.error.is_none());
}

#[tokio::test]
async fn exhausted_credits_stop_the_import_and_mark_it_paused_for_credits() {
    let dir = legacy();
    let rig = rig(dir.path(), Scripted::new(Mode::Credits), true);
    let file = import(&rig).await;
    assert_eq!(file.state.phase, ImportPhase::Error);
    assert!(file.paused_for_credits);
    assert!(
        file.state
            .error
            .as_deref()
            .unwrap()
            .contains("not enough credits")
    );
    assert!(file.checkpoint.is_start());
    // A credits failure is not transient: no backoff.
    assert!(rig.pacer.sleeps.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_pause_leaves_the_import_running_and_a_later_run_finishes_it() {
    let dir = legacy();
    let engine = Scripted::new(Mode::Healthy);
    let paused = Arc::new(AtomicUsize::new(0));
    let flag = paused.clone();
    let store = Arc::new(MemoryCheckpointStore::default());
    let importer = Importer::new(
        dir.path(),
        engine.clone(),
        store.clone(),
        Arc::new(CountingPacer::default()),
        Arc::new(|| true),
    )
    // Allows the first batch, then pauses.
    .with_pause(Arc::new(move || flag.fetch_add(1, Ordering::SeqCst) >= 1));
    let begun = importer.begin().unwrap();
    importer.run(begun).await;
    let file = store.load();
    assert_eq!(file.state.phase, ImportPhase::Running);
    assert_eq!(file.state.imported, 25);
    assert_eq!(file.checkpoint.documents.as_deref(), Some("d024"));

    let resumed = Importer::new(
        dir.path(),
        engine.clone(),
        store.clone(),
        Arc::new(CountingPacer::default()),
        Arc::new(|| true),
    );
    let begun = resumed.begin().unwrap();
    // A resume keeps the total it already knew.
    assert_eq!(begun.state.total, COUNT as u64);
    resumed.run(begun).await;
    let file = store.load();
    assert_eq!(file.state.phase, ImportPhase::Done);
    assert_eq!(file.state.imported, COUNT as u64);
    assert_eq!(engine.inner.len(), COUNT);
}

#[tokio::test]
async fn a_finished_import_starts_over_when_begun_again() {
    let dir = legacy();
    let rig = rig(dir.path(), Scripted::new(Mode::Healthy), true);
    import(&rig).await;
    let begun = rig.importer.begin().unwrap();
    assert_eq!(begun.state.imported, 0);
    assert_eq!(begun.state.phase, ImportPhase::Running);
}

#[test]
fn a_resume_failure_stops_an_import_and_keeps_a_retry_available() {
    let dir = legacy();
    let rig = rig(dir.path(), Scripted::new(Mode::Healthy), true);
    let mut file = ImportFile::default();
    file.state.phase = ImportPhase::Running;
    file.state.imported = 7;
    rig.store.save(&file).unwrap();
    rig.importer.record_resume_failure("memory is off");
    let saved = rig.store.load();
    assert_eq!(saved.state.phase, ImportPhase::Error);
    assert_eq!(saved.state.imported, 7);
    assert_eq!(
        saved.state.error.as_deref(),
        Some("the import could not resume: memory is off")
    );

    file.retrying = true;
    rig.store.save(&file).unwrap();
    rig.importer.record_resume_failure("memory is off");
    let saved = rig.store.load();
    assert_eq!(saved.state.phase, ImportPhase::Done);
    assert!(!saved.retrying);
    assert!(
        saved
            .state
            .error
            .as_deref()
            .unwrap()
            .contains("press Retry again")
    );
}

#[test]
fn the_persisted_file_keeps_its_wire_shape() {
    let mut file = ImportFile::default();
    file.state.phase = ImportPhase::Running;
    file.state.total = 3;
    let json = serde_json::to_value(&file).unwrap();
    assert_eq!(
        json,
        serde_json::json!({
            "state": {"phase": "running", "imported": 0, "total": 3, "failed": 0},
            "checkpoint": {}
        })
    );
    let back: ImportFile = serde_json::from_value(json).unwrap();
    assert_eq!(back, file);
    // A state file written before the optional fields existed still loads.
    let old: ImportFile =
        serde_json::from_str(r#"{"state":{"phase":"done","imported":2,"total":2}}"#).unwrap();
    assert_eq!(old.state.phase, ImportPhase::Done);
}

#[tokio::test]
async fn a_failure_in_the_one_by_one_fallback_checkpoints_the_last_stored_item() {
    let dir = legacy();
    let engine = Scripted::new(Mode::DownAfter(5));
    let rig = rig(dir.path(), engine, true);
    let file = import(&rig).await;
    assert_eq!(file.state.imported, 5);
    assert_eq!(
        file.checkpoint.documents.as_deref(),
        Some("d004"),
        "the checkpoint must not pass an item that was never stored"
    );
}
