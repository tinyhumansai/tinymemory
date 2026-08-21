//! Tests for the surrounding module.

//! Channel-bound tests. These build an [`IngestionQueue`] from a raw
//! `mpsc::channel` without spawning a worker — that lets the suite drive
//! the at-capacity and channel-closed branches deterministically without
//! standing up a real `UnifiedMemory` or contending with a draining task.
use super::*;

use serde_json::json;

impl IngestionQueue {
    fn from_parts(tx: mpsc::Sender<IngestionJob>, state: IngestionState, capacity: usize) -> Self {
        Self {
            tx,
            state,
            capacity,
        }
    }
}

fn fixture_job(title: &str) -> IngestionJob {
    IngestionJob {
        document_id: format!("doc-{title}"),
        document: NamespaceDocumentInput {
            namespace: "skill-test".to_string(),
            key: title.to_string(),
            title: title.to_string(),
            content: "body".to_string(),
            source_type: "doc".to_string(),
            priority: "medium".to_string(),
            tags: Vec::new(),
            metadata: json!({}),
            category: "core".to_string(),
            session_id: None,
            document_id: None,
            taint: crate::MemoryTaint::Internal,
        },
        config: MemoryIngestionConfig::default(),
    }
}

#[tokio::test]
async fn submit_succeeds_until_capacity_then_drops() {
    let state = IngestionState::new();
    let (tx, _rx) = mpsc::channel::<IngestionJob>(2);
    let queue = IngestionQueue::from_parts(tx, state.clone(), 2);

    assert!(queue.submit(fixture_job("a")), "first submit must enqueue");
    assert!(queue.submit(fixture_job("b")), "second submit must enqueue");

    // Channel is now full. tokio's bounded mpsc reserves one slot per
    // permit, so capacity=2 means at most two pending; the third must be
    // rejected with `false`.
    assert!(
        !queue.submit(fixture_job("c")),
        "submit at capacity must return false (drop)"
    );

    // queue_depth must reflect only the accepted jobs — the drop path
    // is required to decrement so the status RPC does not drift upward.
    assert_eq!(
        state.snapshot().queue_depth,
        2,
        "queue_depth must roll back on overflow drop"
    );
}

#[tokio::test]
async fn submit_recovers_after_drain() {
    let state = IngestionState::new();
    let (tx, mut rx) = mpsc::channel::<IngestionJob>(1);
    let queue = IngestionQueue::from_parts(tx, state.clone(), 1);

    assert!(queue.submit(fixture_job("first")));
    assert!(
        !queue.submit(fixture_job("over")),
        "second submit at cap=1 must drop"
    );

    // Drain the receiver to free a slot.
    let pulled = rx.try_recv().expect("first job must be readable");
    assert_eq!(pulled.document.title, "first");
    // Mirror the worker's accounting (queue depth -> dequeue) so the
    // post-drain snapshot does not look like a leftover queued job.
    state.dequeue();

    assert!(
        queue.submit(fixture_job("after-drain")),
        "submit after drain must enqueue"
    );
    assert_eq!(state.snapshot().queue_depth, 1);
}

#[tokio::test]
async fn submit_after_worker_gone_returns_false() {
    let state = IngestionState::new();
    let (tx, rx) = mpsc::channel::<IngestionJob>(4);
    drop(rx); // simulate worker task exiting and dropping its receiver
    let queue = IngestionQueue::from_parts(tx, state.clone(), 4);

    assert!(
        !queue.submit(fixture_job("orphan")),
        "submit must return false once the receiver is dropped"
    );
    assert_eq!(
        state.snapshot().queue_depth,
        0,
        "channel-closed drop path must roll the depth counter back"
    );
}

#[test]
fn default_queue_capacity_is_bounded_and_reasonable() {
    // Guardrail so future changes don't accidentally regress to an
    // arbitrarily large default (or `usize::MAX`) without thinking about
    // the producer-side memory bound.
    const _: () = assert!(DEFAULT_QUEUE_CAPACITY > 0);
    const _: () = assert!(
        DEFAULT_QUEUE_CAPACITY <= 8 * 1024,
        "default capacity is the memory ceiling under sustained overflow — keep it tight"
    );
}

/// Zero capacity would otherwise panic from inside
/// `tokio::sync::mpsc::channel` with a cryptic Tokio-internal message
/// (`mpsc bounded channel requires buffer > 0`) — the explicit guard in
/// [`start_worker_with_capacity`] turns that into a clear, grep-friendly
/// assertion at the call site so misuse fails fast with an actionable
/// message instead of looking like a Tokio bug.
#[tokio::test]
#[should_panic(expected = "ingestion queue capacity must be greater than zero")]
async fn start_worker_rejects_zero_capacity() {
    use tempfile::TempDir;
    use tinymemory_api::host::NoopEmbedding;
    let tmp = TempDir::new().unwrap();
    let memory = UnifiedMemory::new(tmp.path(), Arc::new(NoopEmbedding), None).unwrap();
    // Panic must surface from our own assert, not from the Tokio
    // channel constructor on the line after — that's the contract this
    // test pins.
    let _ = start_worker_with_capacity(Arc::new(memory), IngestionState::new(), 0);
}
