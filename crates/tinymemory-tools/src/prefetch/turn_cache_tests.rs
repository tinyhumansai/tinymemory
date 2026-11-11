//! Contracts for TinyMemory's completed-turn cache and weak cache registry.

use super::*;

use std::time::Duration;

use crate::lifecycle::turn::TurnPack;
use futures::future::BoxFuture;
use tinymemory_api::{Hit, ItemId, ItemKind, MemoryMeta};

const MAX_PACK_BYTES: usize = 64 * 1024;

struct Tokio;

impl PrefetchRuntime for Tokio {
    fn spawn(&self, task: BoxFuture<'static, ()>) {
        tokio::spawn(task);
    }

    fn sleep(&self, duration: Duration) -> BoxFuture<'static, ()> {
        Box::pin(tokio::time::sleep(duration))
    }
}

fn pack(markdown: &str) -> TurnPack {
    TurnPack {
        markdown: markdown.into(),
        tokens: 3,
        refs: vec!["item-1".into()],
        engine: "test-engine".into(),
        citations: Vec::new(),
        refusal: None,
    }
}

fn hit(text: &str) -> Hit {
    Hit {
        id: ItemId::new("item-1"),
        kind: ItemKind::Document,
        text: text.into(),
        meta: MemoryMeta::default(),
        score: 1.0,
        confidence: None,
    }
}

fn cache<K>() -> TurnCache<K>
where
    K: Clone + Hash + Eq + Send + Sync + 'static,
{
    TurnCache::new(Arc::new(Tokio))
}

fn encoded_size(pack: &TurnPack) -> usize {
    serde_json::to_vec(pack)
        .unwrap()
        .len()
        .saturating_add(serde_json::to_vec(&pack.citations).unwrap().len())
}

async fn wait_for_workers(pool: &WorkerPool, available: usize) {
    tokio::time::timeout(Duration::from_secs(1), async {
        while pool.available() != available {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("turn-cache workers completed");
}

#[tokio::test]
async fn completed_turns_add_the_notice_once_and_preserve_delivery_fields() {
    let pool = WorkerPool::new(1);
    let cache = cache::<&'static str>().with_workers(pool.clone());
    let citations = vec![hit("a cited fact")];
    let mut source = pack("The user's preferred color is teal.");
    source.citations = citations.clone();
    source.refusal = Some("UNAVAILABLE");
    let work_value = source.clone();
    cache.enqueue("thread", Box::pin(async move { Some(work_value) }));
    wait_for_workers(&pool, 1).await;

    let first = cache.completed(&"thread").expect("completed pack");
    let second = cache.completed(&"thread").expect("same completed pack");
    let notice = "This is cached memory from a completed background lookup for an earlier turn. It may be incomplete or unrelated to the current question; use explicit memory recall or fetch when you need a current answer.\n\n";
    assert_eq!(first.markdown, format!("{notice}{}", source.markdown));
    assert_eq!(
        first.tokens,
        source.tokens + crate::recall::estimate_tokens(notice)
    );
    assert_eq!(first.refs, source.refs);
    assert_eq!(first.engine, source.engine);
    assert_eq!(first.citations, citations);
    assert_eq!(first.refusal, source.refusal);
    assert_eq!(
        second.markdown, first.markdown,
        "the notice is not prepended twice"
    );
    assert_eq!(
        second.tokens, first.tokens,
        "notice tokens are counted once"
    );
}

#[tokio::test]
async fn turn_cache_admits_a_boundary_pack_and_rejects_an_oversized_citation_pack() {
    let pool = WorkerPool::new(1);
    let cache = cache::<&'static str>().with_workers(pool.clone());
    assert!(
        cache.completed(&"thread").is_none(),
        "a cold cache is empty"
    );

    let mut citation_heavy = pack("small rendered text");
    citation_heavy.citations = vec![hit(&"c".repeat(MAX_PACK_BYTES))];
    cache.enqueue("thread", Box::pin(async move { Some(citation_heavy) }));
    wait_for_workers(&pool, 1).await;
    assert!(
        cache.completed(&"thread").is_none(),
        "an oversized citation body is not admitted to the cache"
    );

    let empty_size = encoded_size(&pack(""));
    let boundary = pack(&"x".repeat(MAX_PACK_BYTES - empty_size));
    assert_eq!(encoded_size(&boundary), MAX_PACK_BYTES);
    cache.enqueue("thread", Box::pin(async move { Some(boundary) }));
    wait_for_workers(&pool, 1).await;
    assert!(
        cache.completed(&"thread").is_some(),
        "a pack at the byte limit is admitted"
    );
}

#[test]
fn pack_admission_accepts_exactly_sixty_four_kib_and_counts_skipped_citations() {
    let empty = pack("");
    let empty_size = encoded_size(&empty);
    let mut exact = pack(&"x".repeat(MAX_PACK_BYTES - empty_size));
    assert_eq!(encoded_size(&exact), MAX_PACK_BYTES);
    assert!(pack_fits(&exact), "the exact byte limit is accepted");

    exact.markdown.push('x');
    assert_eq!(encoded_size(&exact), MAX_PACK_BYTES + 1);
    assert!(!pack_fits(&exact), "a pack over the byte limit is rejected");

    let mut citation_heavy = pack("small rendered text");
    citation_heavy.citations = vec![hit(&"c".repeat(MAX_PACK_BYTES))];
    assert!(serde_json::to_vec(&citation_heavy).unwrap().len() < MAX_PACK_BYTES);
    assert!(serde_json::to_vec(&citation_heavy.citations).unwrap().len() > MAX_PACK_BYTES);
    assert!(
        !pack_fits(&citation_heavy),
        "citation bodies count even though TurnPack skips them in its own serialization"
    );
}

#[tokio::test]
async fn registry_invalidates_matching_keys_across_live_caches() {
    let pool = WorkerPool::new(4);
    let first = Arc::new(cache::<&'static str>().with_workers(pool.clone()));
    let second = Arc::new(cache::<&'static str>().with_workers(pool.clone()));
    let registry = TurnCacheRegistry::default();
    registry.register(&first);
    registry.register(&second);

    first.enqueue("drop", Box::pin(async { Some(pack("first drop")) }));
    first.enqueue("keep", Box::pin(async { Some(pack("first keep")) }));
    second.enqueue("drop", Box::pin(async { Some(pack("second drop")) }));
    second.enqueue("keep", Box::pin(async { Some(pack("second keep")) }));
    wait_for_workers(&pool, 4).await;
    assert!(first.completed(&"drop").is_some());
    assert!(first.completed(&"keep").is_some());
    assert!(second.completed(&"drop").is_some());
    assert!(second.completed(&"keep").is_some());

    registry.invalidate_where(|key| *key == "drop");
    assert!(first.completed(&"drop").is_none());
    assert!(first.completed(&"keep").is_some());
    assert!(second.completed(&"drop").is_none());
    assert!(second.completed(&"keep").is_some());

    registry.invalidate_all();
    assert!(first.completed(&"keep").is_none());
    assert!(second.completed(&"keep").is_none());
}

#[tokio::test]
async fn registry_invalidation_rejects_a_late_result() {
    let pool = WorkerPool::new(1);
    let cache = Arc::new(cache::<&'static str>().with_workers(pool.clone()));
    let registry = TurnCacheRegistry::default();
    registry.register(&cache);
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let (starting, held) = (started.clone(), release.clone());
    cache.enqueue(
        "thread",
        Box::pin(async move {
            starting.notify_one();
            held.notified().await;
            Some(pack("pre-invalidation result"))
        }),
    );
    started.notified().await;

    registry.invalidate_where(|key| *key == "thread");
    release.notify_one();
    wait_for_workers(&pool, 1).await;
    assert!(cache.completed(&"thread").is_none());
}

#[test]
fn registry_keeps_only_weak_cache_references_and_prunes_dropped_entries() {
    let registry = TurnCacheRegistry::default();
    let dropped = Arc::new(cache::<&'static str>());
    let weak = Arc::downgrade(&dropped);
    registry.register(&dropped);
    registry.register(&dropped);
    assert_eq!(
        registry.caches.lock().unwrap().len(),
        1,
        "registration deduplicates"
    );

    drop(dropped);
    assert!(
        weak.upgrade().is_none(),
        "the registry does not keep a cache alive"
    );

    let live = Arc::new(cache::<&'static str>());
    registry.register(&live);
    assert_eq!(
        registry.caches.lock().unwrap().len(),
        1,
        "registration prunes dead caches"
    );
}

#[tokio::test]
async fn registry_does_not_hold_its_lock_while_matching_cache_keys() {
    let pool = WorkerPool::new(2);
    let first = Arc::new(cache::<&'static str>().with_workers(pool.clone()));
    let late = Arc::new(cache::<&'static str>().with_workers(pool.clone()));
    let registry = TurnCacheRegistry::default();
    registry.register(&first);
    first.enqueue("invalidate", Box::pin(async { Some(pack("old")) }));
    late.enqueue("invalidate", Box::pin(async { Some(pack("new")) }));
    wait_for_workers(&pool, 2).await;

    registry.invalidate_where(|key| {
        assert!(
            registry.caches.try_lock().is_ok(),
            "the registry lock is released before evaluating a predicate"
        );
        registry.register(&late);
        *key == "invalidate"
    });
    assert!(first.completed(&"invalidate").is_none());
    assert!(late.completed(&"invalidate").is_some());
}

#[tokio::test]
async fn turn_cache_is_idle_tracks_active_pending_and_drained_work() {
    let pool = WorkerPool::new(1);
    let cache = cache::<&'static str>().with_workers(pool.clone());
    assert!(cache.is_idle(&"thread"), "an unknown key is idle");

    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let (starting, held) = (started.clone(), release.clone());
    cache.enqueue(
        "thread",
        Box::pin(async move {
            starting.notify_one();
            held.notified().await;
            Some(pack("active refresh"))
        }),
    );
    started.notified().await;
    assert!(!cache.is_idle(&"thread"), "an active refresh is not idle");

    let pending_runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let ran = pending_runs.clone();
    cache.enqueue(
        "thread",
        Box::pin(async move {
            ran.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Some(pack("pending refresh"))
        }),
    );
    assert!(!cache.is_idle(&"thread"), "queued work is not idle");

    release.notify_one();
    wait_for_workers(&pool, 1).await;
    assert_eq!(pending_runs.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(
        cache.is_idle(&"thread"),
        "the key is idle after its queue drains"
    );
}

#[tokio::test]
async fn turn_cache_is_idle_does_not_imply_invalidated_work_finished() {
    let pool = WorkerPool::new(1);
    let cache = Arc::new(cache::<&'static str>().with_workers(pool.clone()));
    let registry = TurnCacheRegistry::default();
    registry.register(&cache);
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let (starting, held) = (started.clone(), release.clone());
    cache.enqueue(
        "thread",
        Box::pin(async move {
            starting.notify_one();
            held.notified().await;
            Some(pack("invalidated in-flight result"))
        }),
    );
    started.notified().await;
    assert!(!cache.is_idle(&"thread"));

    registry.invalidate_all();
    assert!(
        cache.is_idle(&"thread"),
        "a cleared entry is missing and idle"
    );
    assert_eq!(
        pool.available(),
        0,
        "the invalidated worker is still active"
    );

    release.notify_one();
    wait_for_workers(&pool, 1).await;
    assert!(cache.is_idle(&"thread"));
    assert!(cache.completed(&"thread").is_none());
}

#[test]
fn turn_cache_is_idle_after_runtime_shutdown_and_allows_a_retry() {
    let pool = WorkerPool::new(1);
    let cache = Arc::new(cache::<&'static str>().with_workers(pool.clone()));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        cache.enqueue("thread", Box::pin(std::future::pending()));
        cache.enqueue(
            "thread",
            Box::pin(async { Some(pack("queued before shutdown")) }),
        );
        tokio::task::yield_now().await;
        assert!(!cache.is_idle(&"thread"));
    });
    drop(runtime);
    assert!(cache.is_idle(&"thread"));
    assert_eq!(pool.available(), 1, "shutdown releases the worker permit");

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        cache.enqueue(
            "thread",
            Box::pin(async { Some(pack("retry after shutdown")) }),
        );
        wait_for_workers(&pool, 1).await;
        assert!(cache.is_idle(&"thread"));
        assert!(
            cache
                .completed(&"thread")
                .unwrap()
                .markdown
                .contains("retry after shutdown")
        );
    });
}
