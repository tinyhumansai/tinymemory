//! Public binder timing behavior: one content-free line per async operation.

use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex, Once,
    atomic::{AtomicBool, Ordering},
};

use async_trait::async_trait;
use tinymemory_api::conformance::ReferenceEngine;
use tinymemory_api::{
    BeliefsRequest, ConsolidateReceipt, ConsolidateRequest, EngineDescriptor, EngineHealth,
    EraseReport, EraseRequest, ExplorePage, ExploreRequest, ExportPage, FetchPage, FetchRequest,
    ForgetReport, ForgetTarget, GetRequest, Hit, ItemId, ListPage, ListRequest, MemoryEngine,
    MemoryMeta, MetaFilter, Reach, RecallAnswer, RecallRequest, Result, StoreItem, StoreReceipt,
    WriteOptions,
};
use tinymemory_integrations::{EngineBinder, safety::Policy};

static RECORDS: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn records() -> std::sync::MutexGuard<'static, Vec<String>> {
    RECORDS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct Capture;

impl log::Log for Capture {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        records().push(record.args().to_string());
    }

    fn flush(&self) {}
}

static CAPTURE: Capture = Capture;

fn install_capture() {
    static INSTALL: Once = Once::new();
    static INSTALLED: AtomicBool = AtomicBool::new(false);
    INSTALL.call_once(|| {
        let installed = log::set_logger(&CAPTURE).is_ok();
        INSTALLED.store(installed, Ordering::SeqCst);
        if installed {
            log::set_max_level(log::LevelFilter::Trace);
        }
    });
    assert!(
        INSTALLED.load(Ordering::SeqCst),
        "integration test logger installs"
    );
}

struct TimingProbe {
    descriptor: ReferenceEngine,
    health: Mutex<VecDeque<EngineHealth>>,
}

impl TimingProbe {
    fn unauthorized<T>() -> Result<T> {
        Err(tinymemory_api::Error::Unauthorized(
            "private backend detail".to_string(),
        ))
    }

    fn receipt() -> StoreReceipt {
        StoreReceipt {
            id: ItemId::new("stored"),
            replayed: false,
        }
    }
}

#[async_trait]
impl MemoryEngine for TimingProbe {
    fn descriptor(&self) -> &EngineDescriptor {
        self.descriptor.descriptor()
    }

    async fn health(&self) -> EngineHealth {
        self.health
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .unwrap_or_else(|| EngineHealth::Down("missing queued health result".to_string()))
    }

    async fn recall(&self, _: RecallRequest) -> Result<RecallAnswer> {
        Self::unauthorized()
    }

    async fn fetch(&self, _: FetchRequest) -> Result<FetchPage> {
        Self::unauthorized()
    }

    async fn store(&self, _: StoreItem) -> Result<StoreReceipt> {
        Ok(Self::receipt())
    }

    async fn store_with(&self, _: StoreItem, _: WriteOptions) -> Result<StoreReceipt> {
        Ok(Self::receipt())
    }

    async fn store_many(&self, items: Vec<StoreItem>) -> Result<Vec<StoreReceipt>> {
        Ok((0..items.len()).map(|_| Self::receipt()).collect())
    }

    async fn store_many_with(
        &self,
        items: Vec<StoreItem>,
        _: WriteOptions,
    ) -> Result<Vec<StoreReceipt>> {
        Ok((0..items.len()).map(|_| Self::receipt()).collect())
    }

    async fn forget(&self, _: ForgetTarget) -> Result<ForgetReport> {
        Self::unauthorized()
    }

    async fn forget_within(&self, _: Vec<ItemId>, _: Reach) -> Result<ForgetReport> {
        Self::unauthorized()
    }

    async fn list(&self, _: ListRequest) -> Result<ListPage> {
        Self::unauthorized()
    }

    async fn list_preview(&self, _: ListRequest) -> Result<ListPage> {
        Self::unauthorized()
    }

    async fn export(&self, _: ListRequest) -> Result<ExportPage> {
        Self::unauthorized()
    }

    async fn erase(&self, _: EraseRequest) -> Result<EraseReport> {
        Self::unauthorized()
    }

    async fn explore(&self, _: ExploreRequest) -> Result<ExplorePage> {
        Self::unauthorized()
    }

    async fn get(&self, _: GetRequest) -> Result<Vec<Hit>> {
        Self::unauthorized()
    }

    async fn consolidate(&self, _: ConsolidateRequest) -> Result<ConsolidateReceipt> {
        Self::unauthorized()
    }

    async fn beliefs(&self, _: BeliefsRequest) -> Result<Vec<Hit>> {
        Self::unauthorized()
    }
}

#[tokio::test]
async fn external_binding_times_every_async_method_without_logging_payloads() {
    install_capture();
    records().clear();

    let binder = EngineBinder::default();
    let probe = TimingProbe {
        descriptor: ReferenceEngine::new(),
        health: Mutex::new(VecDeque::from([
            EngineHealth::Ok,
            EngineHealth::Degraded("private health reason".to_string()),
            EngineHealth::Down("private health reason".to_string()),
        ])),
    };
    let bound = binder.bind_external(Arc::new(probe), "profile://transient", Policy::default());

    let _ = bound.engine.health().await;
    let _ = bound.engine.health().await;
    let _ = bound.engine.health().await;
    let _ = bound
        .engine
        .recall(RecallRequest::new("private user prompt", 5))
        .await;
    let _ = bound
        .engine
        .fetch(FetchRequest::new(
            "private user prompt",
            tinymemory_api::FetchMode::Keyword,
            5,
        ))
        .await;
    let item = || StoreItem::document("private user text", MemoryMeta::default());
    let _ = bound.engine.store(item()).await;
    let _ = bound
        .engine
        .store_with(item(), WriteOptions::accepted())
        .await;
    let _ = bound
        .engine
        .store_with(item(), WriteOptions::visible())
        .await;
    let _ = bound.engine.store_many(vec![item()]).await;
    let _ = bound
        .engine
        .store_many_with(vec![item()], WriteOptions::accepted())
        .await;
    let _ = bound
        .engine
        .store_many_with(vec![item()], WriteOptions::visible())
        .await;
    let _ = bound
        .engine
        .forget(ForgetTarget::Ids(vec![ItemId::new("private-item-id")]))
        .await;
    let _ = bound
        .engine
        .forget_within(
            vec![ItemId::new("private-item-id")],
            Reach::subtree(tinymemory_api::Namespace::ROOT),
        )
        .await;
    let list = || ListRequest::new(MetaFilter::default(), 5);
    let _ = bound.engine.list(list()).await;
    let _ = bound.engine.list_preview(list()).await;
    let _ = bound.engine.export(list()).await;
    let _ = bound
        .engine
        .erase(EraseRequest::new(Reach::subtree(
            tinymemory_api::Namespace::ROOT,
        )))
        .await;
    let _ = bound
        .engine
        .explore(ExploreRequest::new(tinymemory_api::Facet::Kind, 5))
        .await;
    let _ = bound
        .engine
        .get(GetRequest {
            ids: vec![ItemId::new("private-item-id")],
            reach: None,
        })
        .await;
    let _ = bound
        .engine
        .consolidate(ConsolidateRequest::new(Reach::subtree(
            tinymemory_api::Namespace::ROOT,
        )))
        .await;
    let _ = bound
        .engine
        .beliefs(BeliefsRequest::new(
            Reach::subtree(tinymemory_api::Namespace::ROOT),
            5,
        ))
        .await;

    let lines: Vec<_> = records()
        .iter()
        .filter(|line| line.contains("[memory:timing]"))
        .cloned()
        .collect();
    let expected_counts = [
        ("health", 3),
        ("recall", 1),
        ("fetch", 1),
        ("store", 2),
        ("store_accepted", 1),
        ("store_many", 2),
        ("store_many_accepted", 1),
        ("forget", 1),
        ("forget_within", 1),
        ("list", 1),
        ("list_preview", 1),
        ("export", 1),
        ("erase", 1),
        ("explore", 1),
        ("get", 1),
        ("consolidate", 1),
        ("beliefs", 1),
    ];
    assert_eq!(lines.len(), 21, "{lines:?}");
    for (operation, count) in expected_counts {
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.contains(&format!("op={operation} ")))
                .count(),
            count,
            "wrong number of timing records for {operation}: {lines:?}"
        );
    }
    for line in &lines {
        assert!(line.contains("engine=reference"), "{line}");
        assert!(line.contains("elapsed_ms="), "{line}");
        if !line.contains("op=health ") {
            assert!(line.contains("count="), "{line}");
            let successful_write = [
                "op=store ",
                "op=store_accepted ",
                "op=store_many ",
                "op=store_many_accepted ",
            ]
            .iter()
            .any(|operation| line.contains(operation));
            if successful_write {
                assert!(line.contains("outcome=ok"), "{line}");
                assert!(line.contains("count=1"), "{line}");
            } else {
                assert!(line.contains("outcome=UNAUTHORIZED"), "{line}");
                assert!(line.contains("count=0"), "{line}");
            }
        }
    }
    let health_lines: Vec<_> = lines
        .iter()
        .filter(|line| line.contains("op=health "))
        .collect();
    assert!(
        health_lines[0].contains("outcome=ok"),
        "{}",
        health_lines[0]
    );
    assert!(
        health_lines[1].contains("outcome=degraded"),
        "{}",
        health_lines[1]
    );
    assert!(
        health_lines[2].contains("outcome=down"),
        "{}",
        health_lines[2]
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("op=store_accepted ") && line.contains("count=1")),
        "accepted write includes its safe result count: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("op=store_many_accepted ") && line.contains("count=1")),
        "accepted bulk write includes its safe result count: {lines:?}"
    );
    for private_value in [
        "private user prompt",
        "private user text",
        "private backend detail",
        "private health reason",
        "private-item-id",
    ] {
        assert!(
            lines.iter().all(|line| !line.contains(private_value)),
            "timing diagnostics leaked {private_value}: {lines:?}"
        );
    }
}
