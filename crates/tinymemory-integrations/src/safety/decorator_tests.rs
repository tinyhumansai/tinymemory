//! Tests for write scrubbing and complete, semantics-preserving forwarding.

use super::*;

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use crate::safety::timing::timing_outcome;
use async_trait::async_trait;
use tinymemory_api::conformance::ReferenceEngine;
use tinymemory_api::{
    BeliefsRequest, ConsolidateReceipt, ConsolidateRequest, EngineDescriptor, EngineHealth,
    EraseReport, EraseRequest, ExplorePage, ExploreRequest, ExportPage, FetchPage, FetchRequest,
    ForgetReport, ForgetTarget, GetRequest, Hit, ListPage, ListRequest, MemoryEngine, MemoryMeta,
    MetaFilter, Reach, RecallAnswer, RecallRequest, Result, StoreItem, StoreReceipt, WaitFor,
    WriteOptions,
};

fn fake_secret() -> String {
    let mut secret = String::from("sk-ant-");
    secret.push_str("api03-");
    secret.push_str(&"A".repeat(48));
    secret
}

async fn texts(engine: &ReferenceEngine) -> Vec<String> {
    engine
        .list(ListRequest::new(MetaFilter::default(), 50))
        .await
        .expect("reference list")
        .items
        .into_iter()
        .map(|hit| hit.text)
        .collect()
}

#[tokio::test]
async fn scrubbing_decorator_scrubs_each_single_and_bulk_write_path() {
    let reference = Arc::new(ReferenceEngine::new());
    let guarded = ScrubbingEngine::wrap(reference.clone(), Policy::default());
    let secret = fake_secret();
    let item =
        |text: &str| StoreItem::document(format!("{text} key={secret}"), MemoryMeta::default());

    guarded.store(item("one")).await.expect("store");
    guarded
        .store_with(item("two"), WriteOptions::accepted())
        .await
        .expect("store with options");
    guarded
        .store_many(vec![item("three"), item("four")])
        .await
        .expect("store many");
    guarded
        .store_many_with(vec![item("five"), item("six")], WriteOptions::accepted())
        .await
        .expect("store many with options");

    let stored = texts(&reference).await;
    assert_eq!(stored.len(), 6);
    assert!(
        stored.iter().all(|text| !text.contains(&secret)),
        "{stored:?}"
    );
    assert_eq!(guarded.descriptor().id, reference.descriptor().id);
}

#[tokio::test]
async fn scrubbed_documents_keep_page_breaks_for_downstream_chunking() {
    let reference = Arc::new(ReferenceEngine::new());
    let guarded = ScrubbingEngine::wrap(reference.clone(), Policy::default());
    let secret = fake_secret();
    let body = format!("Page one key={secret}\u{c}Page two.\u{c}Page three.");

    guarded
        .store(StoreItem::document(body, MemoryMeta::default()))
        .await
        .expect("document is stored");

    let stored = texts(&reference).await;
    assert_eq!(stored.len(), 1);
    assert!(!stored[0].contains(&secret));
    assert_eq!(stored[0].matches('\u{c}').count(), 2);
    assert!(stored[0].ends_with("\u{c}Page two.\u{c}Page three."));
}

#[tokio::test]
async fn reads_through_the_scrubbing_decorator_match_the_inner_engine() {
    let reference = Arc::new(ReferenceEngine::new());
    let guarded = ScrubbingEngine::wrap(reference.clone(), Policy::default());
    let id = reference
        .store(StoreItem::document("oolong tea", MemoryMeta::default()))
        .await
        .expect("seed item")
        .id;
    let reach = Reach::subtree(tinymemory_api::Namespace::ROOT);

    assert_eq!(guarded.health().await, reference.health().await);
    assert_eq!(
        guarded
            .recall(RecallRequest::new("oolong", 5))
            .await
            .unwrap(),
        reference
            .recall(RecallRequest::new("oolong", 5))
            .await
            .unwrap()
    );
    let fetch = || FetchRequest::new("oolong", tinymemory_api::FetchMode::Keyword, 5);
    assert_eq!(
        guarded.fetch(fetch()).await.unwrap(),
        reference.fetch(fetch()).await.unwrap()
    );
    let get = || GetRequest {
        ids: vec![id.clone()],
        reach: None,
    };
    assert_eq!(
        guarded.get(get()).await.unwrap(),
        reference.get(get()).await.unwrap()
    );
    let list = || ListRequest::new(MetaFilter::default(), 5);
    assert_eq!(
        guarded.list(list()).await.unwrap(),
        reference.list(list()).await.unwrap()
    );
    let explore = || ExploreRequest::new(tinymemory_api::Facet::Kind, 5);
    assert_eq!(
        guarded.explore(explore()).await.ok(),
        reference.explore(explore()).await.ok()
    );
    assert_eq!(
        guarded
            .consolidate(ConsolidateRequest::new(reach.clone()))
            .await
            .ok(),
        reference
            .consolidate(ConsolidateRequest::new(reach.clone()))
            .await
            .ok()
    );
    assert_eq!(
        guarded.beliefs(BeliefsRequest::new(reach, 5)).await.ok(),
        reference
            .beliefs(BeliefsRequest::new(
                Reach::subtree(tinymemory_api::Namespace::ROOT),
                5
            ))
            .await
            .ok()
    );
    assert_eq!(
        guarded
            .forget(ForgetTarget::Ids(vec![id]))
            .await
            .unwrap()
            .forgotten,
        1
    );
}

#[tokio::test]
async fn export_and_erase_keep_the_inner_engine_behavior() {
    let reference = Arc::new(ReferenceEngine::new());
    let guarded = ScrubbingEngine::wrap(reference.clone(), Policy::default());
    let namespace: tinymemory_api::Namespace = "agent:a".parse().unwrap();
    guarded
        .store(StoreItem::document(
            "kept whole",
            MemoryMeta {
                namespace: namespace.clone(),
                ..MemoryMeta::default()
            },
        ))
        .await
        .unwrap();

    let page = guarded
        .export(ListRequest::new(MetaFilter::default(), 10))
        .await
        .expect("export is forwarded, not refused by the wrapper");
    assert_eq!(page.items.len(), 1);
    let report = guarded
        .erase(EraseRequest::new(Reach::exact(namespace)))
        .await
        .expect("erase is forwarded, not refused by the wrapper");
    assert_eq!(report.erased_scopes, 1);
    assert!(texts(&reference).await.is_empty());
}

/// A spy that overrides every current trait method. Falling back to a
/// `MemoryEngine` default in either decorator therefore records the wrong
/// operation here.
#[derive(Default)]
struct ForwardingProbe {
    inner: ReferenceEngine,
    calls: Mutex<Vec<&'static str>>,
    descriptors: AtomicUsize,
    waits: Mutex<Vec<WaitFor>>,
    reaches: Mutex<Vec<Reach>>,
}

impl ForwardingProbe {
    fn record(&self, operation: &'static str) {
        self.calls.lock().expect("probe lock").push(operation);
    }
}

#[async_trait]
impl MemoryEngine for ForwardingProbe {
    fn descriptor(&self) -> &EngineDescriptor {
        self.descriptors.fetch_add(1, Ordering::Relaxed);
        self.inner.descriptor()
    }

    async fn health(&self) -> EngineHealth {
        self.record("health");
        self.inner.health().await
    }

    async fn recall(&self, req: RecallRequest) -> Result<RecallAnswer> {
        self.record("recall");
        self.inner.recall(req).await
    }

    async fn fetch(&self, req: FetchRequest) -> Result<FetchPage> {
        self.record("fetch");
        self.inner.fetch(req).await
    }

    async fn store(&self, item: StoreItem) -> Result<StoreReceipt> {
        self.record("store");
        self.inner.store(item).await
    }

    async fn store_with(&self, item: StoreItem, options: WriteOptions) -> Result<StoreReceipt> {
        self.record("store_with");
        self.inner.store_with(item, options).await
    }

    async fn store_many(&self, items: Vec<StoreItem>) -> Result<Vec<StoreReceipt>> {
        self.record("store_many");
        self.inner.store_many(items).await
    }

    async fn store_many_with(
        &self,
        items: Vec<StoreItem>,
        options: WriteOptions,
    ) -> Result<Vec<StoreReceipt>> {
        self.record("store_many_with");
        self.waits.lock().expect("waits lock").push(options.wait);
        self.inner.store_many_with(items, options).await
    }

    async fn forget(&self, target: ForgetTarget) -> Result<ForgetReport> {
        self.record("forget");
        self.inner.forget(target).await
    }

    async fn forget_within(
        &self,
        ids: Vec<tinymemory_api::ItemId>,
        reach: Reach,
    ) -> Result<ForgetReport> {
        self.record("forget_within");
        self.reaches
            .lock()
            .expect("reaches lock")
            .push(reach.clone());
        self.inner.forget_within(ids, reach).await
    }

    async fn list(&self, req: ListRequest) -> Result<ListPage> {
        self.record("list");
        self.inner.list(req).await
    }

    async fn list_preview(&self, req: ListRequest) -> Result<ListPage> {
        self.record("list_preview");
        let mut page = self.inner.list(req).await?;
        for hit in &mut page.items {
            hit.text = format!("preview: {}", hit.text);
        }
        Ok(page)
    }

    async fn export(&self, req: ListRequest) -> Result<ExportPage> {
        self.record("export");
        self.inner.export(req).await
    }

    async fn erase(&self, req: EraseRequest) -> Result<EraseReport> {
        self.record("erase");
        self.inner.erase(req).await
    }

    async fn explore(&self, req: ExploreRequest) -> Result<ExplorePage> {
        self.record("explore");
        self.inner.explore(req).await
    }

    async fn get(&self, req: GetRequest) -> Result<Vec<Hit>> {
        self.record("get");
        self.inner.get(req).await
    }

    async fn consolidate(&self, req: ConsolidateRequest) -> Result<ConsolidateReceipt> {
        self.record("consolidate");
        self.inner.consolidate(req).await
    }

    async fn beliefs(&self, req: BeliefsRequest) -> Result<Vec<Hit>> {
        self.record("beliefs");
        self.inner.beliefs(req).await
    }
}

#[tokio::test]
async fn both_decorators_forward_every_current_engine_method() {
    let probe = Arc::new(ForwardingProbe::default());
    let engine = TimedEngine::wrap(ScrubbingEngine::wrap(probe.clone(), Policy::default()));
    let descriptors_before_forward = probe.descriptors.load(Ordering::Relaxed);
    let _ = engine.descriptor();
    assert_eq!(
        probe.descriptors.load(Ordering::Relaxed) - descriptors_before_forward,
        1
    );
    let reach = Reach::subtree(tinymemory_api::Namespace::ROOT);
    let list = || ListRequest::new(MetaFilter::default(), 5);
    let _ = engine.health().await;
    let _ = engine.recall(RecallRequest::new("term", 5)).await;
    let _ = engine
        .fetch(FetchRequest::new(
            "term",
            tinymemory_api::FetchMode::Keyword,
            5,
        ))
        .await;
    let item = || StoreItem::document("note", MemoryMeta::default());
    let _ = engine.store(item()).await;
    let _ = engine.store_with(item(), WriteOptions::accepted()).await;
    let _ = engine.store_many(vec![item()]).await;
    let _ = engine
        .store_many_with(vec![item()], WriteOptions::accepted())
        .await;
    let _ = engine
        .store_many_with(vec![item()], WriteOptions::visible())
        .await;
    let _ = engine
        .forget(ForgetTarget::Ids(vec![tinymemory_api::ItemId::new(
            "missing",
        )]))
        .await;
    let expected_reach = Reach::subtree("agent:ann".parse().expect("namespace"));
    let _ = engine
        .forget_within(
            vec![tinymemory_api::ItemId::new("missing")],
            expected_reach.clone(),
        )
        .await;
    let _ = engine.list(list()).await;
    let preview = engine.list_preview(list()).await.expect("preview list");
    assert!(
        preview
            .items
            .iter()
            .all(|hit| hit.text.starts_with("preview: "))
    );
    let _ = engine.export(list()).await;
    let _ = engine.erase(EraseRequest::new(reach.clone())).await;
    let _ = engine
        .explore(ExploreRequest::new(tinymemory_api::Facet::Kind, 5))
        .await;
    let _ = engine
        .get(GetRequest {
            ids: vec![tinymemory_api::ItemId::new("missing")],
            reach: None,
        })
        .await;
    let _ = engine
        .consolidate(ConsolidateRequest::new(reach.clone()))
        .await;
    let _ = engine.beliefs(BeliefsRequest::new(reach, 5)).await;

    let calls = probe.calls.lock().expect("probe lock").clone();
    let expected = [
        "health",
        "recall",
        "fetch",
        "store",
        "store_with",
        "store_many",
        "store_many_with",
        "forget",
        "forget_within",
        "list",
        "list_preview",
        "export",
        "erase",
        "explore",
        "get",
        "consolidate",
        "beliefs",
    ];
    for operation in expected {
        assert_eq!(
            calls.iter().filter(|call| **call == operation).count(),
            if operation == "store_many_with" { 2 } else { 1 },
            "{operation}: {calls:?}"
        );
    }
    assert_eq!(
        *probe.waits.lock().expect("waits lock"),
        vec![WaitFor::Accepted, WaitFor::Visible]
    );
    assert_eq!(
        *probe.reaches.lock().expect("reaches lock"),
        vec![expected_reach]
    );
}

#[test]
fn timing_failure_outcome_is_the_stable_error_code_only() {
    let failure: Result<()> = Err(tinymemory_api::Error::Unauthorized(
        "do-not-log-this-token".to_string(),
    ));

    assert_eq!(timing_outcome(&failure), "UNAUTHORIZED");
    assert_ne!(
        timing_outcome(&failure),
        "unauthorized: do-not-log-this-token"
    );
}
