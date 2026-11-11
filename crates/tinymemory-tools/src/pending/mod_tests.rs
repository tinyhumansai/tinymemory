use super::*;

use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use tinymemory_api::{LearningKind, MemoryMeta, Namespace, Reach, StoreItem};

use crate::list_store::MemoryListStore;

type Queue = PendingWrites<MemoryListStore<Entry>>;

fn learn(text: &str) -> WriteOperation {
    WriteOperation::Learn {
        item: StoreItem::learning(text, LearningKind::Fact, 0.5, MemoryMeta::default()),
    }
}

fn forget(ids: &[&str]) -> WriteOperation {
    WriteOperation::Forget {
        ids: ids.iter().map(|id| (*id).to_string()).collect(),
        reach: Reach::default(),
    }
}

fn binding() -> Binding {
    Binding("owner".into())
}

fn queue(max_entries: usize) -> Queue {
    PendingWrites::new(
        MemoryListStore::default(),
        Limits {
            max_entries,
            ..Limits::default()
        },
    )
}

fn add(queue: &Queue, gate: &WriteGate, op: WriteOperation) -> Result<Ack, PendingError> {
    queue.enqueue(op, Namespace::ROOT, binding(), gate)
}

/// Records what ran; fails the operations `fail_on` names.
struct Recorder {
    ran: StdMutex<Vec<WriteOperation>>,
    fail: bool,
}

#[async_trait::async_trait]
impl WriteExecutor for Recorder {
    async fn run(&self, operation: &WriteOperation) -> Result<(), String> {
        self.ran.lock().unwrap().push(operation.clone());
        if self.fail {
            Err("ENGINE_DOWN".into())
        } else {
            Ok(())
        }
    }
}

fn recorder(fail: bool) -> Recorder {
    Recorder {
        ran: StdMutex::new(Vec::new()),
        fail,
    }
}

async fn drain(queue: &Queue, gate: &WriteGate, executor: &Recorder) -> usize {
    queue
        .drain(executor, gate, gate.generation(), |_| true, |_| {})
        .await
}

#[tokio::test]
async fn writes_drain_oldest_first_and_settle() {
    let (queue, gate) = (queue(8), WriteGate::new("p"));
    add(&queue, &gate, learn("a")).unwrap();
    add(&queue, &gate, forget(&["x"])).unwrap();
    add(&queue, &gate, learn("b")).unwrap();
    let executor = recorder(false);

    assert_eq!(drain(&queue, &gate, &executor).await, 3);

    let ran = executor.ran.lock().unwrap().clone();
    assert_eq!(ran, vec![learn("a"), forget(&["x"]), learn("b")]);
    assert!(queue.is_empty());
}

#[test]
fn the_queue_refuses_past_its_entry_bound() {
    let (queue, gate) = (queue(2), WriteGate::new("p"));
    add(&queue, &gate, learn("a")).unwrap();
    add(&queue, &gate, learn("b")).unwrap();
    assert_eq!(add(&queue, &gate, learn("c")), Err(PendingError::Full));
    assert_eq!(queue.entries().unwrap().len(), 2);
}

#[test]
fn the_queue_refuses_past_its_byte_bound() {
    let queue = PendingWrites::new(
        MemoryListStore::default(),
        Limits {
            max_entries: 8,
            max_bytes: 400,
        },
    );
    let gate = WriteGate::new("p");
    let huge = "x".repeat(1000);
    assert_eq!(
        queue.enqueue(learn(&huge), Namespace::ROOT, binding(), &gate),
        Err(PendingError::Unavailable)
    );
    assert!(queue.is_empty());
}

#[test]
fn adjacent_retries_coalesce_but_not_across_a_forget() {
    let (queue, gate) = (queue(8), WriteGate::new("p"));
    add(&queue, &gate, learn("a")).unwrap();
    add(&queue, &gate, learn("a")).unwrap();
    assert_eq!(queue.entries().unwrap().len(), 1);
    add(&queue, &gate, forget(&["a"])).unwrap();
    add(&queue, &gate, learn("a")).unwrap();
    assert_eq!(queue.entries().unwrap().len(), 3);
}

#[test]
fn a_fence_stops_coalescing_and_retires_earlier_writes() {
    let (queue, gate) = (queue(8), WriteGate::new("p"));
    add(&queue, &gate, learn("old")).unwrap();
    let fence = gate.begin_fence();
    assert!(gate.is_fenced());
    // Same write while fenced is queued again, under the new epoch.
    add(&queue, &gate, learn("old")).unwrap();
    assert_eq!(queue.entries().unwrap().len(), 2);
    assert_eq!(queue.clear_stale("p", fence.generation()).unwrap(), 1);
    let left = queue.entries().unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(
        left[0].epoch.as_ref().unwrap().generation,
        fence.generation()
    );
    drop(fence);
    assert!(!gate.is_fenced());
}

#[test]
fn writes_from_another_process_or_without_an_epoch_are_cleared() {
    let (queue, gate) = (queue(8), WriteGate::new("old-process"));
    add(&queue, &gate, learn("a")).unwrap();
    assert_eq!(queue.clear_stale("new-process", 0).unwrap(), 1);
    assert!(queue.is_empty());
}

#[tokio::test]
async fn a_failed_write_is_kept_with_its_status_and_blocks_later_ones() {
    let (queue, gate) = (queue(8), WriteGate::new("p"));
    add(&queue, &gate, learn("a")).unwrap();
    add(&queue, &gate, learn("b")).unwrap();
    let executor = recorder(true);

    assert_eq!(drain(&queue, &gate, &executor).await, 0);

    assert_eq!(executor.ran.lock().unwrap().len(), 1);
    let kept = queue.entries().unwrap();
    assert_eq!(kept.len(), 2);
    assert_eq!(kept[0].status.as_deref(), Some("ENGINE_DOWN"));
    assert_eq!(kept[1].status, None);
}

#[tokio::test]
async fn a_drain_stops_when_the_owner_changed_or_a_fence_began() {
    let (queue, gate) = (queue(8), WriteGate::new("p"));
    add(&queue, &gate, learn("a")).unwrap();
    let executor = recorder(false);
    let settled = queue
        .drain(&executor, &gate, gate.generation(), |_| false, |_| {})
        .await;
    assert_eq!(settled, 0);

    let stale = gate.generation();
    let _fence = gate.begin_fence();
    let settled = queue.drain(&executor, &gate, stale, |_| true, |_| {}).await;
    assert_eq!(settled, 0);
    assert!(executor.ran.lock().unwrap().is_empty());
    assert_eq!(queue.entries().unwrap().len(), 1);
}

#[tokio::test]
async fn settled_writes_are_reported() {
    let (queue, gate) = (queue(8), WriteGate::new("p"));
    add(&queue, &gate, learn("a")).unwrap();
    let seen = Arc::new(StdMutex::new(0));
    let counter = seen.clone();
    queue
        .drain(
            &recorder(false),
            &gate,
            gate.generation(),
            |_| true,
            move |_| {
                *counter.lock().unwrap() += 1;
            },
        )
        .await;
    assert_eq!(*seen.lock().unwrap(), 1);
}

#[test]
fn an_unreadable_store_is_unavailable_not_overwritten() {
    struct Broken;
    impl ListStore<Entry> for Broken {
        fn load(&self) -> Result<Vec<Entry>, StoreError> {
            Err(StoreError("io".into()))
        }
        fn save(&self, _: &[Entry]) -> Result<(), StoreError> {
            panic!("must not overwrite an unreadable queue");
        }
    }
    let queue = PendingWrites::new(Broken, Limits::default());
    let gate = WriteGate::new("p");
    assert_eq!(
        queue.enqueue(learn("a"), Namespace::ROOT, binding(), &gate),
        Err(PendingError::Unavailable)
    );
}

#[test]
fn tickets_are_unique() {
    assert_ne!(new_ticket(), new_ticket());
}
