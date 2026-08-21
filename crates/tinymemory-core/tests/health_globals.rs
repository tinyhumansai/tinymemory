//! Isolated tests for process-global degradation flags and announcement latch.
//!
//! These assertions intentionally live in their own integration-test process.
//! The core unit-test binary runs factory and seal tests in parallel, and those
//! production paths legitimately clear the same process-global health state.

use std::sync::{Arc, Mutex as StdMutex, MutexGuard};

use parking_lot::Mutex;
use tinymemory_core::events::{self, MemoryEvent, MemoryEventSink};
use tinymemory_core::tree::health::{
    clear_semantic_recall_degraded, clear_storage_degraded, clear_structure_degraded,
    current_degraded_state, mark_local_model_unavailable_if_applicable,
    mark_semantic_recall_degraded, mark_storage_degraded, mark_structure_degraded, FailureClass,
    FailureCode, PipelineFailure,
};

static HEALTH_LOCK: StdMutex<()> = StdMutex::new(());

fn health_guard() -> MutexGuard<'static, ()> {
    let guard = HEALTH_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    clear_semantic_recall_degraded();
    clear_structure_degraded();
    clear_storage_degraded();
    guard
}

#[derive(Debug, Default)]
struct RecordingSink {
    events: Mutex<Vec<MemoryEvent>>,
}

impl RecordingSink {
    fn drain(&self) -> Vec<MemoryEvent> {
        std::mem::take(&mut *self.events.lock())
    }
}

impl MemoryEventSink for RecordingSink {
    fn publish(&self, event: MemoryEvent) {
        self.events.lock().push(event);
    }
}

struct SinkRestore {
    previous: Option<Arc<dyn MemoryEventSink>>,
    installed: Arc<dyn MemoryEventSink>,
}

impl Drop for SinkRestore {
    fn drop(&mut self) {
        let owns_slot = events::event_sink()
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &self.installed));
        if !owns_slot {
            return;
        }
        match self.previous.take() {
            Some(previous) => events::set_event_sink(previous),
            None => events::clear_event_sink(),
        }
    }
}

fn install_sink() -> (Arc<RecordingSink>, SinkRestore) {
    let previous = events::event_sink();
    let sink = Arc::new(RecordingSink::default());
    let installed = Arc::clone(&sink) as Arc<dyn MemoryEventSink>;
    events::set_event_sink(Arc::clone(&installed));
    (
        sink,
        SinkRestore {
            previous,
            installed,
        },
    )
}

#[test]
fn local_model_unavailable_marks_recall_degraded_with_its_cause() {
    let _guard = health_guard();
    mark_local_model_unavailable_if_applicable(&PipelineFailure::new(
        FailureCode::LocalModelUnavailable,
    ));

    let state = current_degraded_state();
    assert!(state.semantic_recall);
    assert_eq!(
        state.cause.as_ref().map(|cause| cause.code),
        Some(FailureCode::LocalModelUnavailable)
    );
    assert_eq!(
        state
            .cause
            .as_ref()
            .map(|cause| cause.remediation_key.as_str()),
        Some("memory.health.remediation.local_model_unavailable")
    );
}

#[test]
fn local_model_unavailable_broadcasts_once_per_transition() {
    let _guard = health_guard();
    let (sink, _restore) = install_sink();
    let failure = PipelineFailure::new(FailureCode::LocalModelUnavailable);

    mark_local_model_unavailable_if_applicable(&failure);
    assert_eq!(sink.drain().len(), 1);
    mark_local_model_unavailable_if_applicable(&failure);
    mark_local_model_unavailable_if_applicable(&failure);
    assert!(sink.drain().is_empty());

    clear_semantic_recall_degraded();
    mark_local_model_unavailable_if_applicable(&failure);
    assert_eq!(sink.drain().len(), 1);
}

#[test]
fn concurrent_failures_announce_exactly_once() {
    let _guard = health_guard();
    let (sink, _restore) = install_sink();

    const THREADS: usize = 8;
    std::thread::scope(|scope| {
        for _ in 0..THREADS {
            scope.spawn(|| {
                mark_local_model_unavailable_if_applicable(&PipelineFailure::new(
                    FailureCode::LocalModelUnavailable,
                ));
            });
        }
    });

    assert_eq!(
        sink.drain().len(),
        1,
        "{THREADS} concurrent failures must yield exactly one announcement"
    );
}

#[test]
fn announcement_reaches_a_client_that_connects_mid_outage() {
    let _guard = health_guard();
    let failure = PipelineFailure::new(FailureCode::LocalModelUnavailable);
    mark_local_model_unavailable_if_applicable(&failure);

    let (sink, _restore) = install_sink();
    assert!(sink.drain().is_empty());
    clear_semantic_recall_degraded();
    mark_local_model_unavailable_if_applicable(&failure);

    let events = sink.drain();
    assert_eq!(events.len(), 1);
    assert!(matches!(
        events[0],
        MemoryEvent::LocalModelUnavailable { .. }
    ));
}

#[test]
fn local_model_unavailable_broadcasts_over_a_different_active_cause() {
    let _guard = health_guard();
    let (sink, _restore) = install_sink();
    mark_semantic_recall_degraded(FailureCode::EmbeddingsUnconfigured);
    mark_local_model_unavailable_if_applicable(&PipelineFailure::new(
        FailureCode::LocalModelUnavailable,
    ));
    assert_eq!(sink.drain().len(), 1);
}

#[test]
fn other_failure_codes_do_not_mark_recall_degraded() {
    let _guard = health_guard();
    for code in [
        FailureCode::Transient,
        FailureCode::BudgetExhausted,
        FailureCode::AuthMissing,
    ] {
        mark_local_model_unavailable_if_applicable(&PipelineFailure::new(code));
        assert!(
            !current_degraded_state().semantic_recall,
            "{} must not flip the recall flag",
            code.as_str()
        );
    }
}

#[test]
fn degraded_cause_is_per_flag_not_shared() {
    let _guard = health_guard();
    mark_semantic_recall_degraded(FailureCode::EmbeddingsUnconfigured);
    mark_structure_degraded(FailureCode::ExtractionTimeout);
    assert_eq!(
        current_degraded_state()
            .cause
            .as_ref()
            .map(|cause| cause.code),
        Some(FailureCode::ExtractionTimeout)
    );

    clear_structure_degraded();
    let state = current_degraded_state();
    assert!(state.semantic_recall && !state.structure);
    assert_eq!(
        state.cause.as_ref().map(|cause| cause.code),
        Some(FailureCode::EmbeddingsUnconfigured)
    );
}

#[test]
fn storage_degradation_outranks_structure_and_recall() {
    let _guard = health_guard();
    mark_semantic_recall_degraded(FailureCode::EmbeddingsUnconfigured);
    mark_structure_degraded(FailureCode::ExtractionTimeout);
    mark_storage_degraded(FailureCode::StorageUnavailable);

    let state = current_degraded_state();
    assert!(state.storage && state.structure && state.semantic_recall);
    assert_eq!(
        state.cause.as_ref().map(|cause| cause.code),
        Some(FailureCode::StorageUnavailable)
    );

    clear_storage_degraded();
    assert_eq!(
        current_degraded_state()
            .cause
            .as_ref()
            .map(|cause| cause.code),
        Some(FailureCode::ExtractionTimeout)
    );
}

#[test]
fn storage_unavailable_is_unrecoverable_with_a_remediation_key() {
    let failure = PipelineFailure::new(FailureCode::StorageUnavailable);
    assert_eq!(failure.class, FailureClass::Unrecoverable);
    assert!(failure.is_unrecoverable());
    assert_eq!(
        failure.remediation_key,
        "memory.health.remediation.storage_unavailable"
    );
}
