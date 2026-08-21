//! Tests for the reference null driver.
//!
//! These pin three separate contracts:
//!
//! 1. the mandatory-three set is genuinely implementable without a store;
//! 2. an unadvertised family is **unreachable** through the trait object, which
//!    is the degradation behaviour the kernel relies on;
//! 3. a direct call to an unadvertised family yields a typed `Unsupported`
//!    error that **names** the family, which is what the transport adapter's
//!    `501` mapping is checked against.
//!
//! ## No async runtime here, on purpose
//!
//! `tinymemory-api` must not depend on tokio (or any executor) — that is the
//! whole point of the crate. Every future in this module completes on its first
//! poll, so a six-line std-only [`block_on`] is sufficient and adds no
//! dependency.

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll};

use super::*;
use crate::provider::audit_provider;
use crate::types::MemoryCategory;

/// Drive a future that is ready on first poll to completion, without an
/// executor. Panics rather than spinning if a future ever returns `Pending`,
/// because in this module that would mean a supposedly-inert implementation
/// started doing real work.
fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut context = Context::from_waker(std::task::Waker::noop());
    match future.as_mut().poll(&mut context) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("null driver future must complete on first poll"),
    }
}

#[test]
fn null_driver_advertises_exactly_the_mandatory_families() {
    let driver = NullMemoryProvider::new();
    let capabilities = driver.capabilities();

    assert_eq!(driver.driver_id(), NULL_DRIVER_ID);
    assert_eq!(capabilities.len(), 3);
    for capability in Capability::MANDATORY {
        assert!(
            capabilities.contains(capability),
            "{capability} must be advertised"
        );
    }
}

#[test]
fn null_driver_passes_capability_validation() {
    // The mandatory-three set is the minimum bindable set, so the reference
    // driver must be bindable. If this ever fails, either the mandatory list
    // grew or the null driver stopped implementing it.
    let driver = NullMemoryProvider::new();
    assert_eq!(driver.capabilities().validate(), Ok(()));
}

#[test]
fn null_driver_is_self_consistent() {
    assert_eq!(audit_provider(&NullMemoryProvider::new()), Ok(()));
}

#[test]
fn null_driver_reports_ready() {
    let health = block_on(NullMemoryProvider::new().health());
    assert_eq!(health, MemoryHealth::Ready);
    assert!(health.is_usable());
}

#[test]
fn null_driver_shutdown_is_an_idempotent_no_op() {
    let driver = NullMemoryProvider::new();
    assert!(block_on(driver.shutdown()).is_ok());
    assert!(block_on(driver.shutdown()).is_ok());
}

#[test]
fn mandatory_core_accepts_writes_and_reads_back_empty() {
    let driver = NullMemoryProvider::new();

    block_on(driver.store(
        "global",
        "k",
        "v",
        MemoryCategory::Core,
        None,
        MemoryTaint::ExternalSync,
    ))
    .expect("null store must accept the write");

    assert!(block_on(driver.get("global", "k"))
        .expect("get must succeed")
        .is_none());
    assert!(!block_on(driver.forget("global", "k")).expect("forget must succeed"));
    assert!(block_on(driver.list(None, None, None))
        .expect("list must succeed")
        .is_empty());
    assert!(block_on(driver.namespaces())
        .expect("namespaces must succeed")
        .is_empty());
}

#[test]
fn mandatory_recall_returns_no_hits() {
    let driver = NullMemoryProvider::new();
    let hits = block_on(driver.recall("anything", 10, &OwnedRecallOpts::default(), None))
        .expect("recall must succeed");
    assert!(hits.is_empty());
}

#[test]
fn mandatory_portability_round_trips_as_an_empty_store() {
    let driver = NullMemoryProvider::new();

    let page = block_on(driver.export_page(None, 100)).expect("export must succeed");
    assert!(page.records.is_empty());
    assert!(
        page.next_cursor.is_none(),
        "the absent cursor is what terminates the caller's export loop"
    );

    let outcome = block_on(driver.import_records(vec![ExportRecord {
        kind: "entry".to_string(),
        id: "rec-1".to_string(),
        namespace: None,
        taint: MemoryTaint::Internal,
        payload: serde_json::Value::Null,
    }]))
    .expect("import must succeed");

    // Skipped, never imported: reporting an import would tell a migration its
    // data landed somewhere it did not.
    assert_eq!(outcome.imported, 0);
    assert_eq!(outcome.skipped, 1);
    assert_eq!(outcome.failed, 0);
}

#[test]
fn export_page_rejects_a_cursor_it_never_issued() {
    let driver = NullMemoryProvider::new();

    let err = block_on(driver.export_page(Some("unexpected"), 100))
        .expect_err("a cursor this driver never issued must be rejected, not silently accepted");
    assert!(
        matches!(err, MemoryError::Invalid(_)),
        "expected MemoryError::Invalid, got {err:?}"
    );
}

#[test]
fn every_unadvertised_family_is_unreachable_through_the_trait_object() {
    let driver = NullMemoryProvider::new();
    let provider: &dyn MemoryProvider = &driver;

    assert!(provider.as_ingest().is_none());
    assert!(provider.as_documents().is_none());
    assert!(provider.as_tree().is_none());
    assert!(provider.as_entities().is_none());
    assert!(provider.as_graph().is_none());
    assert!(provider.as_diff().is_none());
    assert!(provider.as_goals().is_none());
    assert!(provider.as_tool_memory().is_none());
    assert!(provider.as_sources().is_none());
    assert!(provider.as_maintenance().is_none());
}

#[test]
fn advertised_and_reachable_agree_for_every_family() {
    // The invariant that keeps the capability set honest, checked family by
    // family rather than only through the aggregate audit.
    let driver = NullMemoryProvider::new();
    let provider: &dyn MemoryProvider = &driver;
    let advertised = provider.capabilities();

    for capability in Capability::ALL {
        assert_eq!(
            advertised.contains(capability),
            provider.provides(capability),
            "{capability}: advertised and reachable must agree"
        );
    }
}

/// Assert a result is `Unsupported` and names the expected family.
fn assert_unsupported<T: std::fmt::Debug>(result: Result<T, MemoryError>, expected: Capability) {
    match result {
        Err(MemoryError::Unsupported { capability }) => {
            assert_eq!(capability, expected.as_str());
        }
        other => panic!("expected Unsupported({expected}), got {other:?}"),
    }
}

#[test]
fn unadvertised_families_return_unsupported_naming_their_capability() {
    let driver = NullMemoryProvider::new();

    assert_unsupported(block_on(driver.ingest_chat(Vec::new())), Capability::Ingest);
    assert_unsupported(
        block_on(driver.get_document("global", "k")),
        Capability::Documents,
    );
    assert_unsupported(block_on(driver.seal("global")), Capability::Tree);
    assert_unsupported(
        block_on(driver.entities("global", None, 10)),
        Capability::Entities,
    );
    assert_unsupported(block_on(driver.kv_get(None, "k")), Capability::Graph);
    assert_unsupported(
        block_on(driver.capture_snapshot("src-abc")),
        Capability::Diff,
    );
    assert_unsupported(block_on(driver.goals()), Capability::Goals);
    assert_unsupported(block_on(driver.tool_rules("shell")), Capability::ToolMemory);
    assert_unsupported(
        block_on(driver.forget_source("src-abc")),
        Capability::Sources,
    );
    assert_unsupported(block_on(driver.doctor()), Capability::Maintenance);
}

#[test]
fn every_optional_method_fails_with_its_advertised_family_name() {
    use crate::chunks::DataSource;
    use crate::goals::GoalsDoc;
    use crate::provider::types::{IngestItem, SourceItem};
    use crate::provider::{
        ChunkQuery, CoverWindowQuery, FacetType, FastRetrieveQuery, PersonHandle,
        PersonInteraction, SourceRetrievalQuery, UserState,
    };
    use crate::tool_memory::{ToolMemoryPriority, ToolMemoryRule, ToolMemorySource};
    use crate::tree::IngestRequest;
    use crate::types::{GraphRelationRecord, NamespaceDocumentInput};

    let driver = NullMemoryProvider::new();
    let ingest = IngestItem {
        namespace: Some("ns".into()),
        source: DataSource::Upload,
        source_id: "source".into(),
        owner: "owner".into(),
        source_ref: None,
        content: "body".into(),
        mime: Some("text/plain".into()),
        timestamp: None,
        tags: Vec::new(),
        taint: MemoryTaint::Internal,
        path_scope: None,
    };
    assert_unsupported(block_on(driver.ingest_document(ingest)), Capability::Ingest);

    let document = NamespaceDocumentInput {
        namespace: "ns".into(),
        key: "key".into(),
        title: "title".into(),
        content: "body".into(),
        source_type: "upload".into(),
        priority: "normal".into(),
        tags: Vec::new(),
        metadata: serde_json::Value::Null,
        category: "core".into(),
        session_id: None,
        document_id: None,
        taint: MemoryTaint::Internal,
    };
    assert_unsupported(
        block_on(driver.put_document(document)),
        Capability::Documents,
    );
    assert_unsupported(block_on(driver.list_documents(None)), Capability::Documents);
    assert_unsupported(block_on(driver.list_namespaces()), Capability::Documents);
    assert_unsupported(
        block_on(driver.delete_document("ns", "doc")),
        Capability::Documents,
    );
    assert_unsupported(
        block_on(driver.clear_namespace("ns")),
        Capability::Documents,
    );
    assert_unsupported(
        block_on(driver.query_documents("ns", "q", 5)),
        Capability::Documents,
    );
    assert_unsupported(
        block_on(driver.recall_documents("ns", 5)),
        Capability::Documents,
    );

    assert_unsupported(
        block_on(driver.append(IngestRequest {
            namespace: "ns".into(),
            content: "body".into(),
            timestamp: None,
            metadata: None,
        })),
        Capability::Tree,
    );
    assert_unsupported(
        block_on(driver.query_source("ns", "source", 5, None)),
        Capability::Tree,
    );
    assert_unsupported(block_on(driver.drill_down("ns", "node")), Capability::Tree);
    assert_unsupported(block_on(driver.cascade("ns")), Capability::Tree);

    assert_unsupported(
        block_on(driver.entity_edges("ns", "entity", 5)),
        Capability::Entities,
    );
    assert_unsupported(
        block_on(driver.touch_entities("ns", &["entity".into()])),
        Capability::Entities,
    );

    assert_unsupported(
        block_on(driver.kv_put(Some("ns"), "key", serde_json::json!(1))),
        Capability::Graph,
    );
    assert_unsupported(
        block_on(driver.kv_delete(Some("ns"), "key")),
        Capability::Graph,
    );
    assert_unsupported(
        block_on(driver.kv_list(Some("ns"), Some("k"), 5)),
        Capability::Graph,
    );
    assert_unsupported(
        block_on(driver.relations(Some("ns"), Some("a"), Some("p"), 5)),
        Capability::Graph,
    );
    assert_unsupported(
        block_on(driver.put_relation(GraphRelationRecord {
            namespace: Some("ns".into()),
            subject: "a".into(),
            predicate: "p".into(),
            object: "b".into(),
            attrs: serde_json::Value::Null,
            updated_at: 0.0,
            evidence_count: 0,
            order_index: None,
            document_ids: Vec::new(),
            chunk_ids: Vec::new(),
        })),
        Capability::Graph,
    );

    assert_unsupported(block_on(driver.snapshots("source", 5)), Capability::Diff);
    assert_unsupported(
        block_on(driver.diff("source", None, "snapshot")),
        Capability::Diff,
    );
    assert_unsupported(
        block_on(driver.set_goals(GoalsDoc::default())),
        Capability::Goals,
    );

    let rule = ToolMemoryRule::new(
        "shell",
        "be careful",
        ToolMemoryPriority::High,
        ToolMemorySource::UserExplicit,
    );
    assert_unsupported(block_on(driver.put_tool_rule(rule)), Capability::ToolMemory);
    assert_unsupported(
        block_on(driver.delete_tool_rule("shell", "rule")),
        Capability::ToolMemory,
    );

    assert_unsupported(
        block_on(driver.accept_source_items(
            "source",
            "folder",
            vec![SourceItem {
                item_id: "item".into(),
                title: "title".into(),
                content: "body".into(),
                mime: None,
                url: None,
                updated_at_ms: None,
                tags: Vec::new(),
            }],
            MemoryTaint::Internal,
        )),
        Capability::Sources,
    );
    for result in [
        block_on(driver.reembed()),
        block_on(driver.compact()),
        block_on(driver.consolidate()),
    ] {
        assert_unsupported(result, Capability::Maintenance);
    }

    let handle = PersonHandle::Email("person@example.com".into());
    assert_unsupported(block_on(driver.list_people(Some(5))), Capability::People);
    assert_unsupported(block_on(driver.get_person("person")), Capability::People);
    assert_unsupported(
        block_on(driver.resolve_handle(&handle, true)),
        Capability::People,
    );
    assert_unsupported(
        block_on(driver.add_handle_alias("person", &handle)),
        Capability::People,
    );
    assert_unsupported(block_on(driver.score_person("person")), Capability::People);
    assert_unsupported(
        block_on(driver.record_interaction(&PersonInteraction {
            person_id: "person".into(),
            at: "2026-01-01T00:00:00Z".into(),
            is_outbound: false,
            length: 10,
        })),
        Capability::People,
    );
    assert_unsupported(
        block_on(driver.seed_from_address_book()),
        Capability::People,
    );

    assert_unsupported(
        block_on(driver.list_chunks(&ChunkQuery::default(), None)),
        Capability::Chunks,
    );
    assert_unsupported(block_on(driver.get_chunk("chunk")), Capability::Chunks);
    assert_unsupported(block_on(driver.chunk_detail("chunk")), Capability::Chunks);
    assert_unsupported(block_on(driver.storage_kinds()), Capability::Chunks);
    assert_unsupported(
        block_on(driver.chunk_embeddings(&["chunk".into()], "model:8")),
        Capability::Chunks,
    );

    assert_unsupported(
        block_on(driver.fast_retrieve(
            "query",
            FastRetrieveQuery {
                limit: 5,
                max_hops: 1,
                time_window_days: None,
            },
            None,
        )),
        Capability::Retrieval,
    );
    assert_unsupported(
        block_on(driver.cover_window(&CoverWindowQuery::default(), None)),
        Capability::Retrieval,
    );
    assert_unsupported(
        block_on(driver.retrieve_source(&SourceRetrievalQuery::default(), None)),
        Capability::Retrieval,
    );
    assert_unsupported(
        block_on(driver.retrieve_children("node", 1, None, Some(5), None)),
        Capability::Retrieval,
    );
    assert_unsupported(
        block_on(driver.retrieve_leaves(&["chunk".into()], None)),
        Capability::Retrieval,
    );
    assert_unsupported(
        block_on(driver.recall_namespace_scored("ns", "query", 5, None)),
        Capability::Retrieval,
    );
    assert_unsupported(
        block_on(driver.search_entities("query", None, 5)),
        Capability::Retrieval,
    );

    assert_unsupported(block_on(driver.list_active_facets()), Capability::Profile);
    assert_unsupported(block_on(driver.list_all_facets()), Capability::Profile);
    assert_unsupported(block_on(driver.get_facet("key")), Capability::Profile);
    assert_unsupported(
        block_on(driver.facets_by_type(FacetType::Preference)),
        Capability::Profile,
    );
    assert_unsupported(
        block_on(driver.upsert_provider_facet(
            "facet",
            FacetType::Preference,
            "key",
            "value",
            0.8,
            None,
            0.0,
        )),
        Capability::Profile,
    );
    assert_unsupported(
        block_on(driver.set_facet_user_state("key", UserState::Pinned)),
        Capability::Profile,
    );
    assert_unsupported(block_on(driver.delete_facet("key")), Capability::Profile);
    assert_unsupported(
        block_on(driver.delete_facet_by_id("facet")),
        Capability::Profile,
    );
    assert_unsupported(block_on(driver.drop_facets_below(0.5)), Capability::Profile);
    assert!(!block_on(driver.workflow_identity_matches("*", "value")));
}

#[test]
fn provider_is_usable_as_a_shared_trait_object() {
    // The registry binds `Arc<dyn MemoryProvider>`, so the trait object must be
    // `Send + Sync` and every family trait must be object-safe. This test fails
    // to *compile* rather than to run if that ever regresses.
    fn assert_send_sync<T: Send + Sync>(_value: &T) {}

    let provider: std::sync::Arc<dyn MemoryProvider> =
        std::sync::Arc::new(NullMemoryProvider::new());
    assert_send_sync(&provider);
    assert_eq!(provider.driver_id(), NULL_DRIVER_ID);
    assert!(block_on(provider.list(None, None, None))
        .expect("list through the trait object")
        .is_empty());
}
