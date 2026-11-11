//! Regression tests for per-partition drain serialization.

use super::*;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};

use crate::list_store::MemoryListStore;

fn poll_once<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(futures::task::noop_waker_ref()))
}

#[tokio::test]
async fn partition_locks_serialize_one_file_without_blocking_another() {
    let locks = PartitionLocks::new();
    let first = locks.for_partition("owner-a/root");
    let same = locks.for_partition("owner-a/root");
    let other = locks.for_partition("owner-a/agent");
    assert!(Arc::ptr_eq(&first, &same));
    assert!(!Arc::ptr_eq(&first, &other));

    let held = first.lock_arc().await;
    let independent = other.try_lock_arc();
    assert!(
        independent.is_some(),
        "a different file has an independent drain lock"
    );
    assert!(
        same.try_lock_arc().is_none(),
        "one file has only one drain at a time"
    );
    drop(independent);
    drop(held);
}

#[tokio::test]
async fn cancelling_a_waiting_partition_drain_releases_its_lock_request() {
    let locks = PartitionLocks::new();
    let partition = locks.for_partition("owner-a/root");
    let held = partition.lock_arc().await;
    let waiting_partition = partition.clone();
    let mut waiting = Box::pin(waiting_partition.lock_arc());
    assert!(poll_once(waiting.as_mut()).is_pending());
    drop(waiting);
    drop(held);

    assert!(partition.try_lock_arc().is_some());
}

#[tokio::test]
async fn an_owned_drain_guard_keeps_its_partition_lock_shared() {
    let locks = PartitionLocks::new();
    let partition = locks.for_partition("owner-a/root");
    let guard = partition.lock_arc().await;
    drop(partition);

    let same_partition = locks.for_partition("owner-a/root");
    assert!(
        same_partition.try_lock_arc().is_none(),
        "the owned guard retains the original shared lock"
    );

    drop(guard);
    assert!(same_partition.try_lock_arc().is_some());
}

#[test]
fn partition_registry_evicts_locks_after_the_last_handle_drops() {
    let locks = PartitionLocks::new();
    let first = locks.for_partition("owner-a/root");
    drop(first);

    let replacement = locks.for_partition("owner-a/agent");

    assert_eq!(locks.locks.lock().unwrap().len(), 1);
    assert!(replacement.try_lock_arc().is_some());
}

#[test]
fn partition_registry_shares_and_releases_queue_instances() {
    use std::path::PathBuf;

    let partitions = PendingPartitions::<MemoryListStore<Entry>, PathBuf>::new(Limits::default());
    let factories = AtomicUsize::new(0);
    let key = PathBuf::from("memory/owner-a/root.json");
    let first = partitions.get_or_create(key.clone(), || {
        factories.fetch_add(1, Ordering::SeqCst);
        MemoryListStore::<Entry>::default()
    });
    let shared = partitions.get_or_create(key.clone(), || {
        factories.fetch_add(1, Ordering::SeqCst);
        MemoryListStore::<Entry>::default()
    });
    assert!(Arc::ptr_eq(&first, &shared));
    assert_eq!(factories.load(Ordering::SeqCst), 1);

    drop(first);
    drop(shared);
    let replacement = partitions.get_or_create(key, || {
        factories.fetch_add(1, Ordering::SeqCst);
        MemoryListStore::<Entry>::default()
    });

    assert_eq!(factories.load(Ordering::SeqCst), 2);
    assert_eq!(partitions.queues.lock().unwrap().len(), 1);
    drop(replacement);
}

#[test]
fn partition_registries_accept_lossless_filesystem_path_keys() {
    use std::path::PathBuf;

    let locks = PartitionLocks::<PathBuf>::new();
    let queue_key = PathBuf::from("memory/owner-a/root.json");
    let first = locks.for_partition(queue_key.clone());
    let second = locks.for_partition(queue_key);

    assert!(Arc::ptr_eq(&first, &second));
}
