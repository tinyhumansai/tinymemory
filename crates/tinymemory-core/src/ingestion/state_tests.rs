//! Tests for the surrounding module.

use super::*;
use std::sync::Arc;
use tokio::time::{sleep, Duration};

#[tokio::test]
async fn singleton_serialises_concurrent_acquires() {
    let state = IngestionState::new();
    let counter = Arc::new(parking_lot::Mutex::new(0u32));
    let max_concurrent = Arc::new(parking_lot::Mutex::new(0u32));

    let mut handles = Vec::new();
    for _ in 0..4 {
        let state = state.clone();
        let counter = Arc::clone(&counter);
        let max_concurrent = Arc::clone(&max_concurrent);
        handles.push(tokio::spawn(async move {
            let _g = state.acquire().await;
            let now = {
                let mut c = counter.lock();
                *c += 1;
                *c
            };
            {
                let mut m = max_concurrent.lock();
                if now > *m {
                    *m = now;
                }
            }
            sleep(Duration::from_millis(20)).await;
            *counter.lock() -= 1;
        }));
    }

    for h in handles {
        h.await.unwrap();
    }

    assert_eq!(*max_concurrent.lock(), 1, "ingestion must be singleton");
}

#[test]
fn snapshot_reports_running_and_queue_depth() {
    let state = IngestionState::new();
    state.enqueue();
    state.enqueue();
    let snap = state.snapshot();
    assert_eq!(snap.queue_depth, 2);
    assert!(!snap.running);

    state.dequeue();
    state.mark_running("doc-1", "title", "ns");
    let snap = state.snapshot();
    assert_eq!(snap.queue_depth, 1);
    assert!(snap.running);
    assert_eq!(snap.current_document_id.as_deref(), Some("doc-1"));

    state.mark_completed("doc-1", true, 12345);
    let snap = state.snapshot();
    assert!(!snap.running);
    assert_eq!(snap.last_document_id.as_deref(), Some("doc-1"));
    assert_eq!(snap.last_success, Some(true));
    assert_eq!(snap.last_completed_at, Some(12345));
}

#[test]
fn reset_for_test_clears_queue_depth_and_running_state() {
    let state = IngestionState::new();
    state.enqueue();
    state.enqueue();
    state.mark_running("doc-x", "title", "ns");

    state.reset_for_test();

    let snap = state.snapshot();
    assert_eq!(snap.queue_depth, 0, "queue_depth must be zero after reset");
    assert!(!snap.running, "running must be false after reset");
    assert!(snap.current_document_id.is_none());
}

#[test]
fn reset_for_test_preserves_completion_history() {
    let state = IngestionState::new();
    state.enqueue();
    state.mark_running("doc-y", "title", "ns");
    state.mark_completed("doc-y", true, 99999);
    state.dequeue();

    state.reset_for_test();

    let snap = state.snapshot();
    assert_eq!(snap.queue_depth, 0);
    assert_eq!(
        snap.last_document_id.as_deref(),
        Some("doc-y"),
        "completion history should survive reset"
    );
    assert_eq!(snap.last_success, Some(true));
}
