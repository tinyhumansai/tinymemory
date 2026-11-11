//! Brain ingestion, search, forgetting and the belief builds it hands back.

use async_trait::async_trait;
use std::sync::Mutex;
use tinymemory_api::conformance::ReferenceEngine;
use tinymemory_api::{
    ConsolidateRequest, Consolidation, EngineDescriptor, EngineHealth, Error, ExplorePage,
    ExploreRequest, Facet, FacetBucket, FetchPage, FetchRequest, ForgetReport, ForgetTarget,
    ListPage, ListRequest, MemoryMeta, Namespace, RecallAnswer, RecallRequest, SourceKind,
    SourceRef, StoreItem, StoreReceipt,
};

use super::*;

fn brain() -> (Arc<ReferenceEngine>, Brain) {
    let engine = Arc::new(ReferenceEngine::new());
    (engine.clone(), Brain::new(engine, MemoryLayout::default()))
}

async fn store_document_at(engine: &ReferenceEngine, namespace: &str, text: &str) {
    engine
        .store(StoreItem::document(
            text,
            MemoryMeta {
                namespace: namespace.parse().unwrap(),
                ..MemoryMeta::default()
            },
        ))
        .await
        .unwrap();
}

#[tokio::test]
async fn sources_fold_collections_sort_ties_and_count_unfiled_below_the_root() {
    let engine = Arc::new(ReferenceEngine::new());
    let layout = MemoryLayout::new("team:acme".parse().unwrap()).unwrap();
    let brain = Brain::new(engine.clone(), layout);
    for (namespace, text) in [
        ("team:acme/source:github", "github root"),
        (
            "team:acme/source:github/project:acme-api",
            "github collection",
        ),
        ("team:acme/source:notion", "notion"),
        ("team:acme/source:pdf", "pdf"),
        ("team:acme", "root item"),
        ("team:acme/agent:writer", "agent item"),
        ("team:acme/ws:main/source:wrong-depth", "wrong depth"),
        ("team:else/source:outside", "outside layout"),
    ] {
        store_document_at(&engine, namespace, text).await;
    }

    let view = brain.sources().await.unwrap();
    assert_eq!(view.root, "team:acme");
    assert_eq!(
        view.sources,
        [
            BrainSourceCount {
                source: "github".into(),
                documents: 2,
            },
            BrainSourceCount {
                source: "notion".into(),
                documents: 1,
            },
            BrainSourceCount {
                source: "pdf".into(),
                documents: 1,
            },
        ]
    );
    assert_eq!(view.unfiled, 3);
    assert_eq!(
        serde_json::to_value(view).unwrap(),
        serde_json::json!({
            "root": "team:acme",
            "sources": [
                { "source": "github", "documents": 2 },
                { "source": "notion", "documents": 1 },
                { "source": "pdf", "documents": 1 }
            ],
            "unfiled": 3
        })
    );
}

struct ExploreFixture {
    inner: ReferenceEngine,
    requests: Mutex<Vec<ExploreRequest>>,
}

#[async_trait]
impl tinymemory_api::MemoryEngine for ExploreFixture {
    fn descriptor(&self) -> &EngineDescriptor {
        self.inner.descriptor()
    }
    async fn health(&self) -> EngineHealth {
        self.inner.health().await
    }
    async fn recall(&self, request: RecallRequest) -> tinymemory_api::Result<RecallAnswer> {
        self.inner.recall(request).await
    }
    async fn fetch(&self, request: FetchRequest) -> tinymemory_api::Result<FetchPage> {
        self.inner.fetch(request).await
    }
    async fn store(&self, item: StoreItem) -> tinymemory_api::Result<StoreReceipt> {
        self.inner.store(item).await
    }
    async fn forget(&self, target: ForgetTarget) -> tinymemory_api::Result<ForgetReport> {
        self.inner.forget(target).await
    }
    async fn list(&self, request: ListRequest) -> tinymemory_api::Result<ListPage> {
        self.inner.list(request).await
    }
    async fn explore(&self, request: ExploreRequest) -> tinymemory_api::Result<ExplorePage> {
        self.requests.lock().unwrap().push(request);
        Ok(ExplorePage {
            facet: Facet::Namespace,
            buckets: vec![
                FacetBucket {
                    value: "root".into(),
                    count: 1,
                },
                FacetBucket {
                    value: "source:files".into(),
                    count: 3,
                },
                FacetBucket {
                    value: "malformed".into(),
                    count: 2,
                },
            ],
            total: 8,
            missing: 2,
            more_buckets: 0,
            truncated: false,
        })
    }
}

#[tokio::test]
async fn sources_include_missing_facet_items_and_keep_the_bounded_host_query() {
    let engine = Arc::new(ExploreFixture {
        inner: ReferenceEngine::new(),
        requests: Mutex::new(Vec::new()),
    });
    let brain = Brain::new(engine.clone(), MemoryLayout::default());
    let view = brain.sources().await.unwrap();

    assert_eq!(
        view.unfiled, 5,
        "root and malformed buckets plus missing facet"
    );
    assert_eq!(
        view.sources,
        [BrainSourceCount {
            source: "files".into(),
            documents: 3,
        }]
    );
    let requests = engine.requests.lock().unwrap();
    let request = &requests[0];
    assert_eq!(request.facet, Facet::Namespace);
    assert_eq!(request.filter.kinds, [tinymemory_api::ItemKind::Document]);
    assert_eq!(request.filter.reach, Some(Reach::subtree(Namespace::ROOT)));
    assert_eq!(request.limit, 200);
    assert_eq!(request.scan_limit, 20_000);
}

#[tokio::test]
async fn a_document_lands_at_its_source_node_without_an_agent() {
    let (engine, brain) = brain();
    let meta = MemoryMeta {
        namespace: Namespace::agent("sneaky"),
        agent_id: Some("sneaky".into()),
        file_path: Some("/docs/refunds.pdf".into()),
        ..MemoryMeta::default()
    };
    let ingested = brain
        .ingest(
            BrainDocument::new(BrainSource::Pdf, "Refunds take five days.")
                .titled("refunds.pdf")
                .with_meta(meta),
        )
        .await
        .unwrap();
    assert_eq!(
        ingested.job,
        Some(BackgroundJob::BuildBeliefs {
            request: ConsolidateRequest::new(Reach::subtree(Namespace::source("pdf")))
                .kinds([ItemKind::Document]),
        })
    );
    let listed = engine
        .list(ListRequest::new(Default::default(), 10))
        .await
        .unwrap();
    let meta = &listed.items[0].meta;
    assert_eq!(meta.namespace, Namespace::source("pdf"));
    assert_eq!(meta.agent_id, None);
    assert_eq!(meta.file_path.as_deref(), Some("/docs/refunds.pdf"));
    assert_eq!(meta.source.kind, SourceKind::File);
}

#[tokio::test]
async fn a_reader_s_source_is_kept() {
    let (engine, brain) = brain();
    let meta = MemoryMeta {
        source: SourceRef {
            kind: SourceKind::Folder,
            id: Some("handbook".into()),
        },
        ..MemoryMeta::default()
    };
    brain
        .ingest(BrainDocument::new(BrainSource::Markdown, "Be kind.").with_meta(meta))
        .await
        .unwrap();
    let listed = engine
        .list(ListRequest::new(Default::default(), 10))
        .await
        .unwrap();
    assert_eq!(listed.items[0].meta.source.kind, SourceKind::Folder);
}

#[tokio::test]
async fn ingest_many_batches_and_builds_once_per_source() {
    let (engine, brain) = brain();
    let documents: Vec<BrainDocument> = (0..MAX_STORE_MANY + 5)
        .map(|i| {
            let source = if i % 2 == 0 {
                BrainSource::Notion
            } else {
                BrainSource::Github
            };
            BrainDocument::new(source, format!("page {i}"))
        })
        .collect();
    let batch = brain.ingest_many(documents).await.unwrap();
    assert_eq!(batch.receipts.len(), MAX_STORE_MANY + 5);
    assert_eq!(engine.len(), MAX_STORE_MANY + 5);
    assert_eq!(batch.jobs.len(), 2);
    let again = brain
        .ingest_many(vec![BrainDocument::new(BrainSource::Notion, "page 0")])
        .await
        .unwrap();
    assert!(again.receipts[0].replayed);
}

#[tokio::test]
async fn a_blank_document_is_refused_and_nothing_is_stored() {
    let (engine, brain) = brain();
    let error = brain
        .ingest_many(vec![
            BrainDocument::new(BrainSource::Web, "fine"),
            BrainDocument::new(BrainSource::Web, "  "),
        ])
        .await
        .unwrap_err();
    assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");
    assert!(engine.is_empty());
}

#[tokio::test]
async fn search_and_forget_stay_inside_one_source() {
    let (engine, brain) = brain();
    for (source, text) in [
        (BrainSource::Pdf, "refund policy pdf"),
        (BrainSource::Notion, "refund policy notion"),
    ] {
        brain
            .ingest(BrainDocument::new(source, text))
            .await
            .unwrap();
    }
    assert_eq!(brain.search("refund", None, 10).await.unwrap().len(), 2);
    let notion = brain
        .search("refund", Some(&BrainSource::Notion), 10)
        .await
        .unwrap();
    assert_eq!(notion.len(), 1);
    assert!(notion[0].text.contains("notion"));

    let report = brain.forget(&BrainSource::Pdf).await.unwrap();
    assert_eq!(report.forgotten, 1);
    assert_eq!(engine.len(), 1);
    assert!(
        brain
            .search("refund", Some(&BrainSource::Pdf), 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn forgetting_a_source_forgets_its_collections() {
    let (engine, brain) = brain();
    brain
        .ingest(BrainDocument::new(BrainSource::Github, "repo-wide notes"))
        .await
        .unwrap();
    let repo = brain
        .layout()
        .brain_collection(&BrainSource::Github, "acme-api")
        .unwrap();
    assert_eq!(repo.to_string(), "source:github/project:acme-api");
    for (namespace, text) in [
        (repo.clone(), "issue in acme/api"),
        (Namespace::source("notion"), "a notion page"),
    ] {
        let meta = MemoryMeta {
            namespace,
            ..MemoryMeta::default()
        };
        engine.store(StoreItem::document(text, meta)).await.unwrap();
    }
    let hits = brain
        .search("issue", Some(&BrainSource::Github), 10)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1, "a source's search reads its collections");

    let report = brain.forget(&BrainSource::Github).await.unwrap();
    assert_eq!(report.forgotten, 2, "the source node and its collection");
    assert_eq!(engine.len(), 1, "another source is untouched");
}

#[tokio::test]
async fn an_engine_that_builds_on_its_own_gets_no_build_from_an_ingest() {
    let engine = Arc::new(ReferenceEngine::new().with_consolidation(Consolidation::Automatic));
    let brain = Brain::new(engine.clone(), MemoryLayout::default());
    let ingested = brain
        .ingest(BrainDocument::new(
            BrainSource::Pdf,
            "Refunds take five days.",
        ))
        .await
        .unwrap();
    assert_eq!(ingested.job, None);
    let batch = brain
        .ingest_many(vec![
            BrainDocument::new(BrainSource::Notion, "Deploys run on Fridays."),
            BrainDocument::new(BrainSource::Github, "CI runs on every push."),
        ])
        .await
        .unwrap();
    assert!(batch.jobs.is_empty(), "{:?}", batch.jobs);
    assert_eq!(engine.len(), 3, "the documents are still stored");

    let refresh = brain.build(&BrainSource::Pdf).unwrap();
    assert_eq!(
        refresh,
        BackgroundJob::BuildBeliefs {
            request: ConsolidateRequest::new(Reach::subtree(Namespace::source("pdf")))
                .kinds([ItemKind::Document]),
        }
    );
    let report = crate::background::BackgroundRunner::new(engine, MemoryLayout::default())
        .run(refresh)
        .await
        .unwrap();
    assert_eq!(report.outcome, crate::background::JobOutcome::Done);
}
