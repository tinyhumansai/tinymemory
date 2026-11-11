//! Running jobs: builds against engines that do and do not consolidate, and
//! deferred brain ingestion.

use async_trait::async_trait;
use tinymemory_api::conformance::{CONSOLIDATED_TAG, ReferenceEngine};
use tinymemory_api::{
    EngineDescriptor, EngineHealth, FetchPage, FetchRequest, ForgetReport, ForgetTarget, ItemKind,
    ListPage, ListRequest, MemoryMeta, MetaFilter, Namespace, Reach, RecallAnswer, RecallRequest,
    StoreItem,
};

use super::*;
use crate::layout::BrainSource;
use crate::run_slots::StartGate;

struct MutexStartGate(std::sync::Mutex<()>);

impl StartGate for MutexStartGate {
    fn with_gate(&self, reserve: &mut dyn FnMut() -> bool) -> bool {
        let _guard = self.0.lock().unwrap();
        reserve()
    }
}

#[test]
fn run_slots_exclude_duplicate_keys_and_release_on_guard_drop() {
    let slots = crate::run_slots::RunSlots::new();
    let first = slots.try_acquire("workspace-a".to_string());
    assert!(first.is_some());
    assert!(slots.is_running(&"workspace-a".to_string()));
    assert!(slots.try_acquire("workspace-b".to_string()).is_some());
    assert!(
        slots.try_acquire("workspace-a".to_string()).is_none(),
        "the same key cannot be reserved twice"
    );

    drop(first);
    assert!(
        !slots.is_running(&"workspace-a".to_string()),
        "dropping the guard releases the reservation"
    );
    assert!(slots.try_acquire("workspace-a".to_string()).is_some());
}

#[test]
fn start_gate_holds_the_shared_lock_while_peer_check_and_reservation_run() {
    let gate = MutexStartGate(std::sync::Mutex::new(()));
    let mut reserved = false;
    assert!(gate.with_gate(&mut || {
        assert!(
            gate.0.try_lock().is_err(),
            "the callback runs while the shared import/migration lock is held"
        );
        reserved = true;
        true
    }));
    assert!(reserved);
}

fn runner(engine: Arc<dyn MemoryEngine>) -> BackgroundRunner {
    BackgroundRunner::new(engine, MemoryLayout::default())
}

#[tokio::test]
async fn a_build_on_a_consolidating_engine_reports_its_receipt() {
    let engine = Arc::new(ReferenceEngine::new());
    engine
        .store(StoreItem::document(
            "Refunds take five days.",
            MemoryMeta {
                namespace: Namespace::source("pdf"),
                ..MemoryMeta::default()
            },
        ))
        .await
        .unwrap();
    let job = BackgroundJob::BuildBeliefs {
        request: ConsolidateRequest::new(Reach::exact(Namespace::source("pdf"))),
    };
    let report = runner(engine.clone()).run(job).await.unwrap();
    assert_eq!(report.job, "build_beliefs");
    assert_eq!(report.outcome, JobOutcome::Done);
    let receipt = report.consolidation.expect("a build reports its receipt");
    assert_eq!(receipt.status, ConsolidateStatus::Completed);
    assert_eq!(receipt.built, Some(1), "one belief from one document");
    let learnings = engine
        .list(ListRequest::new(
            MetaFilter::kinds([ItemKind::Learning]),
            10,
        ))
        .await
        .unwrap();
    assert_eq!(learnings.items.len(), 1);
    assert_eq!(learnings.items[0].meta.tags, [CONSOLIDATED_TAG]);
}

/// The reference engine without consolidation: the trait's default refusal.
struct Plain(ReferenceEngine);

#[async_trait]
impl MemoryEngine for Plain {
    fn descriptor(&self) -> &EngineDescriptor {
        self.0.descriptor()
    }
    async fn health(&self) -> EngineHealth {
        EngineHealth::Ok
    }
    async fn recall(&self, req: RecallRequest) -> Result<RecallAnswer> {
        self.0.recall(req).await
    }
    async fn fetch(&self, req: FetchRequest) -> Result<FetchPage> {
        self.0.fetch(req).await
    }
    async fn store(&self, item: StoreItem) -> Result<StoreReceipt> {
        self.0.store(item).await
    }
    async fn forget(&self, target: ForgetTarget) -> Result<ForgetReport> {
        self.0.forget(target).await
    }
    async fn list(&self, req: ListRequest) -> Result<ListPage> {
        self.0.list(req).await
    }
}

#[tokio::test]
async fn a_build_on_an_engine_that_cannot_is_skipped_not_failed() {
    let report = runner(Arc::new(Plain(ReferenceEngine::new())))
        .run(BackgroundJob::BuildBeliefs {
            request: ConsolidateRequest::new(Reach::default()),
        })
        .await
        .unwrap();
    assert!(
        matches!(report.outcome, JobOutcome::Skipped { .. }),
        "{report:?}"
    );
    assert!(report.consolidation.is_none());
}

#[tokio::test]
async fn an_invalid_build_is_an_error() {
    let error = runner(Arc::new(ReferenceEngine::new()))
        .run(BackgroundJob::BuildBeliefs {
            request: ConsolidateRequest::new(Reach::default())
                .kinds([ItemKind::Document, ItemKind::Document]),
        })
        .await
        .unwrap_err();
    assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");
}

#[tokio::test]
async fn a_deferred_ingest_stores_and_hands_back_its_builds() {
    let engine = Arc::new(ReferenceEngine::new());
    let job = BackgroundJob::IngestBrain {
        documents: vec![
            BrainDocument::new(BrainSource::Pdf, "one"),
            BrainDocument::new(BrainSource::Pdf, "two"),
        ],
    };
    let json = serde_json::to_value(&job).unwrap();
    assert_eq!(json["job"], "ingest_brain");
    let job: BackgroundJob = serde_json::from_value(json).unwrap();
    let report = runner(engine.clone()).run(job).await.unwrap();
    assert_eq!(report.outcome, JobOutcome::Done);
    assert_eq!(report.stored.len(), 2);
    assert_eq!(report.follow_ups.len(), 1);
    assert_eq!(engine.len(), 2);
}
