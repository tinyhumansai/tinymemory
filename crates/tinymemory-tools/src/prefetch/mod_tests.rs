use super::*;

use tokio::sync::Notify;

struct Tokio;

impl PrefetchRuntime for Tokio {
    fn spawn(&self, task: BoxFuture<'static, ()>) {
        tokio::spawn(task);
    }
    fn sleep(&self, duration: Duration) -> BoxFuture<'static, ()> {
        Box::pin(tokio::time::sleep(duration))
    }
}

type Cache = Prefetcher<&'static str, String>;

fn cache() -> Cache {
    Prefetcher::new(Arc::new(Tokio))
}

async fn settle(cache: &Cache, key: &'static str) {
    tokio::time::timeout(Duration::from_secs(1), async {
        while !cache.is_idle(&key) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("worker completed");
}

fn done(text: &str) -> Work<String> {
    let text = text.to_string();
    Box::pin(async move { Some(text) })
}

#[tokio::test]
async fn held_refresh_returns_immediately_and_later_turn_reads_completed_context() {
    let cache = cache();
    let release = Arc::new(Notify::new());
    let held = release.clone();
    cache.enqueue(
        "k",
        Box::pin(async move {
            held.notified().await;
            Some("later result".to_string())
        }),
    );
    assert!(cache.completed(&"k").is_none(), "cold refresh never waits");
    release.notify_one();
    settle(&cache, "k").await;
    assert_eq!(cache.completed(&"k").unwrap(), "later result");
    assert!(cache.completed(&"other").is_none(), "keys are isolated");
}

#[tokio::test]
async fn expiry_failure_and_filtered_results_are_not_reused() {
    let cache = cache().with_limits(Limits {
        ttl: Duration::from_millis(20),
        ..Limits::default()
    });
    cache.enqueue("k", done("fresh"));
    settle(&cache, "k").await;
    assert!(cache.completed(&"k").is_some());
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert!(cache.completed(&"k").is_none(), "expired");

    cache.enqueue("k", done("fresh"));
    settle(&cache, "k").await;
    cache.enqueue("k", Box::pin(async { None }));
    settle(&cache, "k").await;
    assert!(
        cache.completed(&"k").is_none(),
        "a failed refresh clears it"
    );

    let small = cache.clone();
    drop(small);
    let bounded = Prefetcher::<&'static str, String>::new(Arc::new(Tokio))
        .with_filter(|pack: &String| pack.len() <= 4);
    bounded.enqueue("k", done("too long"));
    settle(&bounded, "k").await;
    assert!(bounded.completed(&"k").is_none(), "oversized is dropped");
    bounded.enqueue("k", done("ok"));
    settle(&bounded, "k").await;
    assert_eq!(bounded.completed(&"k").unwrap(), "ok");
}

#[tokio::test]
async fn a_slow_refresh_times_out() {
    let cache = cache().with_limits(Limits {
        work_timeout: Duration::from_millis(20),
        ..Limits::default()
    });
    cache.enqueue(
        "k",
        Box::pin(async {
            std::future::pending::<()>().await;
            Some("never".to_string())
        }),
    );
    settle(&cache, "k").await;
    assert!(cache.completed(&"k").is_none());
}

#[tokio::test]
async fn invalidation_rejects_late_results_and_keeps_pending_work() {
    let cache = cache();
    let release = Arc::new(Notify::new());
    let started = Arc::new(Notify::new());
    let (held, starting) = (release.clone(), started.clone());
    cache.enqueue(
        "k",
        Box::pin(async move {
            starting.notify_one();
            held.notified().await;
            Some("forgotten".to_string())
        }),
    );
    cache.enqueue("k", done("after"));
    started.notified().await;
    cache.invalidate_where(|key| *key == "k");
    release.notify_one();
    settle(&cache, "k").await;
    assert_eq!(
        cache.completed(&"k").unwrap(),
        "after",
        "the late result was rejected, the queued one ran"
    );

    cache.invalidate_all();
    assert!(cache.completed(&"k").is_none());
    assert!(cache.is_idle(&"k"));
}

#[tokio::test]
async fn pending_work_and_workers_are_bounded() {
    let cache = cache().with_limits(Limits {
        max_pending: 2,
        ..Limits::default()
    });
    let release = Arc::new(Notify::new());
    let held = release.clone();
    cache.enqueue(
        "k",
        Box::pin(async move {
            held.notified().await;
            Some("0".to_string())
        }),
    );
    // The bound counts the first work until its worker pops it, so the
    // second fits and the rest are skipped.
    cache.enqueue("k", done("1"));
    cache.enqueue("k", done("2"));
    cache.enqueue("k", done("3"));
    release.notify_one();
    settle(&cache, "k").await;
    assert_eq!(cache.completed(&"k").unwrap(), "1");

    let pool = WorkerPool::new(1);
    let busy = cache.clone();
    drop(busy);
    let pooled =
        Prefetcher::<&'static str, String>::new(Arc::new(Tokio)).with_workers(pool.clone());
    let release = Arc::new(Notify::new());
    let held = release.clone();
    pooled.enqueue(
        "a",
        Box::pin(async move {
            held.notified().await;
            Some("a".to_string())
        }),
    );
    pooled.enqueue("b", done("b"));
    assert!(pooled.is_idle(&"b") || pooled.completed(&"b").is_none());
    release.notify_one();
    settle(&pooled, "a").await;
    assert_eq!(pool.available(), 1, "the permit is released");
    assert!(pooled.completed(&"b").is_none(), "no worker was free for b");
}

#[test]
fn runtime_shutdown_releases_worker_and_pending_work() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let pool = WorkerPool::new(1);
    let cache = Prefetcher::<&'static str, String>::new(Arc::new(Tokio)).with_workers(pool.clone());
    runtime.block_on(async {
        cache.enqueue(
            "k",
            Box::pin(async {
                std::future::pending::<()>().await;
                None
            }),
        );
        cache.enqueue("k", done("queued"));
        tokio::task::yield_now().await;
    });
    drop(runtime);
    assert!(cache.is_idle(&"k"), "the active flag is released");
    assert_eq!(pool.available(), 1, "the permit is released");
    assert!(cache.completed(&"k").is_none());
}

#[test]
fn concurrent_acquires_never_exceed_the_pool_size() {
    let pool = WorkerPool::new(3);
    let granted = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..16 {
            scope.spawn(|| {
                if let Some(permit) = pool.try_acquire() {
                    granted.lock().unwrap().push(permit);
                }
            });
        }
    });
    assert_eq!(granted.lock().unwrap().len(), 3);
    assert_eq!(pool.available(), 0);
    granted.lock().unwrap().clear();
    assert_eq!(pool.available(), 3);
}
