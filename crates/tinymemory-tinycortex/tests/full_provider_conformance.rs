//! The conformance suite over the FULL eighteen-family driver (#18 §E1/§E3).
//!
//! `conformance_test.rs` (in-lib) covers `crate::provider` — the mandatory
//! three families over any engine backend. This target covers
//! [`tinymemory_tinycortex::engine::TinycortexProvider`], which the in-lib
//! test cannot: the provider needs a `MemoryClient`, and a `MemoryClient`
//! needs the host's process-global embedding seam installed. A process global
//! makes tests order-dependent inside a shared binary, so this lives in its
//! own integration target that owns the global for its whole lifetime — the
//! arrangement the in-lib test's module doc promised.
//!
//! The seam is the same noop shape the §B5 acceptance test uses: recall
//! quality is not under test here, contract shape is.

// A panic in a test IS the failure report — same allowance the in-lib
// conformance test carries.
#![allow(clippy::expect_used)]

use std::sync::Arc;

use tinymemory_tinycortex::engine::{EngineRuntimeConfig, TinycortexProvider};

/// The one piece of host wiring `MemoryClient` requires.
#[derive(Debug)]
struct NoopEmbeddingHost;

impl tinymemory_api::host::EmbeddingHost for NoopEmbeddingHost {
    fn resolve_api_key(&self, _provider: &str) -> Option<String> {
        None
    }

    fn ollama_base_url(&self) -> String {
        "http://127.0.0.1:1".into()
    }

    fn default_embedding_provider(&self) -> Arc<dyn tinymemory_api::host::EmbeddingProvider> {
        Arc::new(tinymemory_api::host::NoopEmbedding)
    }

    fn create_embedding_provider_with_credentials(
        &self,
        _provider: &str,
        _model: &str,
        _dims: usize,
        _api_key: &str,
        _custom_endpoint: Option<&str>,
    ) -> Result<Box<dyn tinymemory_api::host::EmbeddingProvider>, String> {
        Ok(Box::new(tinymemory_api::host::NoopEmbedding))
    }

    fn model_supports_dimensions(&self, _model: &str) -> bool {
        false
    }

    fn cloud_embedding_provider(
        &self,
        _model: &str,
        _dims: usize,
    ) -> Result<Box<dyn tinymemory_api::host::EmbeddingProvider>, String> {
        Ok(Box::new(tinymemory_api::host::NoopEmbedding))
    }

    fn default_cloud_embedding_model(&self) -> &str {
        "noop"
    }

    fn default_cloud_embedding_dimensions(&self) -> usize {
        8
    }

    fn ollama_embedding_provider(
        &self,
        _base_url: &str,
        _model: &str,
        _dims: usize,
    ) -> Result<Box<dyn tinymemory_api::host::EmbeddingProvider>, String> {
        Ok(Box::new(tinymemory_api::host::NoopEmbedding))
    }
}

fn provider_over(workspace: &std::path::Path) -> TinycortexProvider {
    provider_over_sources(workspace, serde_json::Value::Null)
}

fn provider_over_sources(
    workspace: &std::path::Path,
    memory_sources: serde_json::Value,
) -> TinycortexProvider {
    tinymemory_core::embedding_host::set_embedding_host(Arc::new(NoopEmbeddingHost));
    let client = Arc::new(
        tinymemory_core::store::MemoryClient::from_workspace_dir(workspace.to_path_buf())
            .expect("open the workspace store"),
    );
    let config = provider_config(workspace, memory_sources);
    TinycortexProvider::new("tinycortex".into(), config, client)
}

fn provider_config(
    workspace: &std::path::Path,
    memory_sources: serde_json::Value,
) -> EngineRuntimeConfig {
    EngineRuntimeConfig {
        workspace_dir: workspace.to_path_buf(),
        config_path: workspace.join("config.toml"),
        memory: Default::default(),
        memory_tree: Default::default(),
        scheduler_gate: Default::default(),
        local_ai: Default::default(),
        embeddings_provider: None,
        memory_provider: None,
        default_model: None,
        default_temperature: 0.2,
        output_language: None,
        memory_sources,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_full_tinycortex_provider_upholds_the_contract() {
    let workspace = tempfile::tempdir().expect("workspace");
    let provider = provider_over(workspace.path());
    tinymemory_conformance::assert_provider(Arc::new(provider)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_full_provider_actually_retains() {
    let workspace = tempfile::tempdir().expect("workspace");
    let provider = provider_over(workspace.path());
    assert!(
        tinymemory_conformance::retains_writes(&provider).await,
        "the workspace store must retain writes, or the suite above asserts \
         almost nothing"
    );
}

/// The KV write path canonicalizes identifiers (the shim in `tinymemory-core`
/// routes every `set_*`/`delete_*` through `canonical_identifier`), so a read
/// path that compares the raw caller key misses every rewritten key: put→get
/// answered `None` while put→delete answered `true`. `kv_get` and `kv_list`
/// must apply the same transform the write path did.
#[tokio::test(flavor = "multi_thread")]
async fn kv_reads_find_a_key_the_canonicalizer_rewrites() {
    use tinymemory_api::provider::MemoryProvider;
    use tinymemory_core::store::safety::canonical_identifier;

    let workspace = tempfile::tempdir().expect("workspace");
    let provider = provider_over(workspace.path());
    let graph = provider.as_graph().expect("the full provider serves Graph");

    // A formatted national ID is strict-gated PII, so the write path rewrites
    // it. (A bare Luhn-valid digit run would NOT do here: the strict gate
    // deliberately ignores bare-numeric shapes so scanner-built identifiers —
    // timestamps, phone-shaped JIDs — keep their identity.)
    let key = "ssn-123-45-6789";
    let canonical = canonical_identifier(key);
    assert_ne!(
        canonical, key,
        "fixture must be a key the canonicalizer rewrites"
    );

    let value = serde_json::json!({"ticket": 42});
    graph
        .kv_put(None, key, value.clone())
        .await
        .expect("kv_put");

    let record = graph
        .kv_get(None, key)
        .await
        .expect("kv_get")
        .expect("kv_get must find the key it just put under the same raw key");
    assert_eq!(record.value, value, "kv_get surfaced another record");
    assert_eq!(
        record.key, canonical,
        "the stored key is the canonical form, and reads surface it as stored"
    );

    // Prefix matching is over canonical stored keys, so the raw caller key
    // works as a prefix of its own record.
    let listed = graph.kv_list(None, Some(key), 16).await.expect("kv_list");
    assert!(
        listed.iter().any(|r| r.key == canonical),
        "kv_list under the raw-key prefix must reach the rewritten record, got {listed:?}"
    );

    // Delete already routed through the canonicalizing shim; the fix must not
    // break that half of the symmetry.
    assert!(
        graph.kv_delete(None, key).await.expect("kv_delete"),
        "kv_delete must find the rewritten key"
    );
    assert!(
        graph
            .kv_get(None, key)
            .await
            .expect("kv_get after delete")
            .is_none(),
        "the record must be gone after kv_delete reported true"
    );
}

/// The same symmetry holds for namespaced KV rows: namespace and key are both
/// canonicalized on write, so both must be canonicalized on read.
#[tokio::test(flavor = "multi_thread")]
async fn namespaced_kv_reads_apply_the_write_path_canonicalization() {
    use tinymemory_api::provider::MemoryProvider;

    let workspace = tempfile::tempdir().expect("workspace");
    let provider = provider_over(workspace.path());
    let graph = provider.as_graph().expect("the full provider serves Graph");

    // A namespace the canonicalizer rewrites, guarded like the key leg — a
    // no-op namespace would prove only key symmetry under a namespace.
    let ns = "ssn-123-45-6789";
    assert_ne!(
        tinymemory_core::store::safety::canonical_identifier(ns),
        ns,
        "the fixture namespace must be one the canonicalizer rewrites"
    );
    let key = "cliente-RFC-VECJ880326XK4";
    let value = serde_json::json!("rewritten");
    graph
        .kv_put(Some(ns), key, value.clone())
        .await
        .expect("kv_put");

    let record = graph
        .kv_get(Some(ns), key)
        .await
        .expect("kv_get")
        .expect("a namespaced put must be readable back under the same raw key");
    assert_eq!(record.value, value);
    assert!(
        graph.kv_delete(Some(ns), key).await.expect("kv_delete"),
        "namespaced kv_delete must stay symmetric with kv_put"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn document_source_graph_goals_and_tool_rule_state_transitions_round_trip() {
    use tinymemory_api::goals::{GoalItem, GoalsDoc};
    use tinymemory_api::provider::types::SourceItem;
    use tinymemory_api::provider::MemoryProvider;
    use tinymemory_api::tool_memory::{ToolMemoryPriority, ToolMemoryRule, ToolMemorySource};
    use tinymemory_api::types::{GraphRelationRecord, MemoryTaint, NamespaceDocumentInput};

    let workspace = tempfile::tempdir().expect("workspace");
    let provider = provider_over(workspace.path());

    let documents = provider.as_documents().expect("Documents");
    let input = NamespaceDocumentInput {
        namespace: "project".into(),
        key: "brief".into(),
        title: "Brief".into(),
        content: "Ship the deterministic test suite".into(),
        source_type: "upload".into(),
        priority: "high".into(),
        tags: vec!["tests".into()],
        metadata: serde_json::json!({"ticket": 81}),
        category: "core".into(),
        session_id: Some("session-1".into()),
        document_id: None,
        taint: MemoryTaint::ExternalSync,
    };
    let document_id = documents.put_document(input).await.expect("put document");
    let document = documents
        .get_document("project", "brief")
        .await
        .expect("get document")
        .expect("document present");
    assert_eq!(document.document_id, document_id);
    assert_eq!(document.metadata, serde_json::json!({"ticket": 81}));
    assert_eq!(document.taint, MemoryTaint::ExternalSync);
    assert!(documents
        .list_namespaces()
        .await
        .expect("namespaces")
        .contains(&"project".into()));
    let listed = documents
        .list_documents(Some("project"))
        .await
        .expect("list documents");
    let listed = listed["documents"].as_array().expect("document list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["documentId"], document_id);
    assert_eq!(listed[0]["key"], "brief");
    let queried = documents
        .query_documents("project", "deterministic", 4)
        .await
        .expect("query documents");
    assert_eq!(queried.namespace, "project");
    assert!(queried.context_text.contains("deterministic"));
    let recalled = documents
        .recall_documents("project", 4)
        .await
        .expect("recall documents");
    assert_eq!(recalled.namespace, "project");
    documents
        .delete_document("project", &document_id)
        .await
        .expect("delete document");
    assert!(documents
        .get_document("project", "brief")
        .await
        .expect("get after delete")
        .is_none());
    documents
        .clear_namespace("project")
        .await
        .expect("clear empty namespace");

    let source = provider.as_sources().expect("SourceSink");
    let outcome = source
        .accept_source_items(
            "drive-1",
            "drive",
            vec![SourceItem {
                item_id: "item-1".into(),
                title: "Source item".into(),
                content: "source body".into(),
                mime: Some("text/plain".into()),
                url: Some("https://example.invalid/item-1".into()),
                updated_at_ms: Some(42),
                tags: vec!["source".into()],
            }],
            MemoryTaint::ExternalSync,
        )
        .await
        .expect("accept source item");
    assert_eq!(outcome.written, 1);
    assert_eq!(
        source
            .forget_source("drive-1")
            .await
            .expect("forget source"),
        1
    );

    let graph = provider.as_graph().expect("Graph");
    let relation = GraphRelationRecord {
        namespace: Some("project".into()),
        subject: "suite".into(),
        predicate: "covers".into(),
        object: "adapter".into(),
        attrs: serde_json::json!({"confidence": 1.0}),
        updated_at: 0.0,
        evidence_count: 0,
        order_index: None,
        document_ids: Vec::new(),
        chunk_ids: Vec::new(),
    };
    graph.put_relation(relation).await.expect("put relation");
    let relations = graph
        .relations(Some("project"), Some("suite"), Some("covers"), 1)
        .await
        .expect("relations");
    assert_eq!(relations.len(), 1);
    assert_eq!(relations[0].object, "ADAPTER");
    assert!(graph
        .relations(Some("project"), None, None, 0)
        .await
        .expect("zero limit")
        .is_empty());

    let goals = provider.as_goals().expect("Goals");
    let expected_goals = GoalsDoc {
        items: vec![GoalItem::new("g1", "finish coverage")],
    };
    goals
        .set_goals(expected_goals.clone())
        .await
        .expect("set goals");
    assert_eq!(goals.goals().await.expect("goals"), expected_goals);

    let tools = provider.as_tool_memory().expect("ToolMemory");
    let rule = ToolMemoryRule::new(
        "shell",
        "never delete broad paths",
        ToolMemoryPriority::Critical,
        ToolMemorySource::UserExplicit,
    );
    let rule_id = rule.id.clone();
    tools.put_tool_rule(rule).await.expect("put tool rule");
    let rules = tools.tool_rules("shell").await.expect("tool rules");
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].id, rule_id);
    assert!(tools
        .delete_tool_rule("shell", &rule_id)
        .await
        .expect("delete rule"));
    assert!(!tools
        .delete_tool_rule("shell", &rule_id)
        .await
        .expect("delete missing rule"));
}

#[tokio::test(flavor = "multi_thread")]
async fn people_profile_and_episodic_lifecycles_are_real_and_typed() {
    use tinymemory_api::error::MemoryError;
    use tinymemory_api::provider::{
        EpisodicTurn, FacetType, MemoryProvider, PersonHandle, PersonInteraction, UserState,
    };

    let workspace = tempfile::tempdir().expect("workspace");
    let provider = provider_over(workspace.path());

    let people = provider.as_people().expect("People");
    let handle = PersonHandle::Email(" Friend@Example.com ".into());
    assert!(people
        .resolve_handle(&handle, false)
        .await
        .expect("resolve absent")
        .is_none());
    let resolved = people
        .resolve_handle(&handle, true)
        .await
        .expect("resolve/create")
        .expect("person created");
    assert!(resolved.created);
    assert_eq!(
        people
            .resolve_handle(&PersonHandle::Email("friend@example.com".into()), false)
            .await
            .expect("resolve canonical")
            .expect("same person")
            .id,
        resolved.id
    );
    let named = people
        .resolve_handle(&PersonHandle::DisplayName("Ada Lovelace".into()), true)
        .await
        .expect("resolve display name")
        .expect("named person created");
    assert!(named.created);
    assert!(matches!(
        people
            .record_interaction(&PersonInteraction {
                person_id: resolved.id.clone(),
                at: "not-a-time".into(),
                is_outbound: true,
                length: 10,
            })
            .await,
        Err(MemoryError::Invalid(_))
    ));
    people
        .record_interaction(&PersonInteraction {
            person_id: resolved.id.clone(),
            at: "2026-08-21T00:00:00Z".into(),
            is_outbound: true,
            length: 120,
        })
        .await
        .expect("record interaction");
    assert_eq!(
        people
            .score_person(&resolved.id)
            .await
            .expect("score")
            .expect("person score")
            .interaction_count,
        1
    );
    let people_list = people.list_people(Some(10)).await.expect("list people");
    assert_eq!(people_list.len(), 2);
    assert_eq!(people_list[0].person.id, resolved.id);
    assert_eq!(
        people
            .get_person(&resolved.id)
            .await
            .expect("get person")
            .expect("person present")
            .id,
        resolved.id
    );
    let alias = PersonHandle::IMessage("+15551234567".into());
    people
        .add_handle_alias(&resolved.id, &alias)
        .await
        .expect("add alias");
    assert_eq!(
        people
            .resolve_handle(&alias, false)
            .await
            .expect("resolve alias")
            .expect("alias present")
            .id,
        resolved.id
    );

    let profile = provider.as_profile().expect("Profile");
    profile
        .upsert_provider_facet(
            "facet-1",
            FacetType::Preference,
            "style/verbosity",
            "concise",
            0.9,
            Some("segment-1"),
            100.0,
        )
        .await
        .expect("upsert facet");
    let facet = profile
        .get_facet("style/verbosity")
        .await
        .expect("get facet")
        .expect("facet present");
    assert_eq!(facet.value, "concise");
    let mut complete_facet = facet.clone();
    complete_facet.facet_id = "facet-complete".into();
    complete_facet.key = "style/complete".into();
    profile
        .upsert_facet(&complete_facet)
        .await
        .expect("upsert complete facet");
    assert_eq!(
        profile
            .get_facet("style/complete")
            .await
            .expect("get complete facet")
            .expect("complete facet present")
            .facet_id,
        "facet-complete"
    );
    assert_eq!(profile.list_active_facets().await.expect("active").len(), 2);
    assert_eq!(
        profile.list_all_facets().await.expect("all facets").len(),
        2
    );
    assert_eq!(
        profile
            .facets_by_type(FacetType::Preference)
            .await
            .expect("facets by type")
            .len(),
        2
    );
    assert!(
        !profile
            .workflow_identity_matches("identity/*", "missing")
            .await
    );
    assert!(profile
        .set_facet_user_state("style/verbosity", UserState::Pinned)
        .await
        .expect("pin facet"));
    assert_eq!(
        profile
            .get_facet("style/verbosity")
            .await
            .expect("get pinned facet")
            .expect("facet present")
            .user_state,
        UserState::Pinned
    );
    assert!(profile
        .delete_facet("style/verbosity")
        .await
        .expect("delete facet"));
    assert!(profile
        .delete_facet_by_id("facet-complete")
        .await
        .expect("delete complete facet"));
    profile
        .upsert_provider_facet(
            "facet-low",
            FacetType::Preference,
            "style/tone",
            "plain",
            0.1,
            None,
            101.0,
        )
        .await
        .expect("upsert low-confidence facet");
    assert!(profile.drop_facets_below(0.2).await.expect("drop weak") <= 1);
    profile
        .upsert_provider_facet(
            "facet-delete-id",
            FacetType::Preference,
            "style/format",
            "markdown",
            0.8,
            None,
            102.0,
        )
        .await
        .expect("upsert deletable facet");
    assert!(profile
        .delete_facet_by_id("facet-delete-id")
        .await
        .expect("delete facet id"));

    let episodic = provider.as_episodic().expect("Episodic");
    let turn_id = episodic
        .insert_turn(&EpisodicTurn {
            id: None,
            session_id: "session-1".into(),
            timestamp: 10.0,
            role: "user".into(),
            content: "remember the test".into(),
            lesson: Some("verify state".into()),
            tool_calls_json: None,
            cost_microdollars: -1,
        })
        .await
        .expect("insert turn");
    let turns = episodic
        .session_turns("session-1")
        .await
        .expect("session turns");
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].id, Some(turn_id));
    assert_eq!(turns[0].cost_microdollars, 0, "negative costs clamp");
    episodic
        .create_segment("seg-1", "session-1", "global", turn_id, 10.0, 10.0)
        .await
        .expect("create segment");
    episodic
        .append_turn("seg-1", turn_id, 10.0, 11.0)
        .await
        .expect("append turn");
    let segment = episodic
        .open_segment("session-1")
        .await
        .expect("open segment")
        .expect("segment present");
    assert_eq!(segment.turn_count, 2);
    episodic
        .close_segment("seg-1", 13.0)
        .await
        .expect("close segment");
    episodic
        .set_segment_summary("seg-1", "one remembered turn", 14.0)
        .await
        .expect("set summary");
    episodic
        .upsert_segment_embedding("seg-1", "noop:8", &[0.0; 8], 15.0)
        .await
        .expect("upsert segment embedding");
    assert!(episodic
        .open_segment("session-1")
        .await
        .expect("open segment after close")
        .is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn ingest_chunks_and_retrieval_cover_success_and_validation_without_network() {
    use tinymemory_api::chunks::DataSource;
    use tinymemory_api::error::MemoryError;
    use tinymemory_api::provider::types::IngestItem;
    use tinymemory_api::provider::{ChunkQuery, FastRetrieveQuery, MemoryProvider};
    use tinymemory_api::types::MemoryTaint;

    let workspace = tempfile::tempdir().expect("workspace");
    let provider = provider_over(workspace.path());
    let ingest = provider.as_ingest().expect("Ingest");
    let invalid = IngestItem {
        namespace: None,
        source: DataSource::Upload,
        source_id: "upload-1".into(),
        owner: "owner".into(),
        source_ref: None,
        content: "binary".into(),
        mime: Some("application/pdf".into()),
        timestamp: None,
        tags: Vec::new(),
        taint: MemoryTaint::Internal,
        path_scope: None,
    };
    assert!(matches!(
        ingest.ingest_document(invalid).await,
        Err(MemoryError::Invalid(_))
    ));
    assert!(ingest
        .ingest_chat(Vec::new())
        .await
        .expect("empty chat")
        .ids
        .is_empty());

    let outcome = ingest
        .ingest_document(IngestItem {
            namespace: None,
            source: DataSource::Upload,
            source_id: "successful-upload".into(),
            owner: "owner".into(),
            source_ref: None,
            content: "Alice maintains the TinyMemory adapter in Kuwait.".into(),
            mime: Some("text/plain".into()),
            timestamp: Some(
                chrono::DateTime::from_timestamp(1_700_000_000, 0).expect("fixed timestamp"),
            ),
            tags: vec!["coverage".into()],
            taint: MemoryTaint::Internal,
            path_scope: None,
        })
        .await
        .expect("successful deterministic ingest");
    assert!(outcome.written > 0, "ingest must persist a chunk");
    assert!(!outcome.ids.is_empty(), "ingest must identify its chunks");
    let chat = ingest
        .ingest_chat(vec![IngestItem {
            namespace: Some("chat".into()),
            source: DataSource::Conversation,
            source_id: "chat-session".into(),
            owner: "owner".into(),
            source_ref: None,
            content: "A deterministic chat message about TinyMemory.".into(),
            mime: Some("text/plain".into()),
            timestamp: Some(
                chrono::DateTime::from_timestamp(1_700_000_100, 0).expect("fixed timestamp"),
            ),
            tags: vec!["chat".into()],
            taint: MemoryTaint::Internal,
            path_scope: None,
        }])
        .await
        .expect("successful chat ingest");
    assert!(chat.written > 0);

    let chunks = provider.as_chunks().expect("Chunks");
    let stored = chunks
        .list_chunks(&ChunkQuery::default(), None)
        .await
        .expect("chunk list after ingest");
    assert!(!stored.is_empty());
    let chunk = chunks
        .get_chunk(&outcome.ids[0])
        .await
        .expect("get ingested chunk")
        .expect("chunk present");
    assert!(chunk.content.contains("TinyMemory"));
    let detail = chunks
        .chunk_detail(&outcome.ids[0])
        .await
        .expect("chunk detail")
        .expect("detail present");
    assert_eq!(detail.chunk.id, outcome.ids[0]);
    assert!(chunks
        .get_chunk("missing")
        .await
        .expect("missing chunk")
        .is_none());
    assert!(!chunks
        .storage_kinds()
        .await
        .expect("storage kinds")
        .is_empty());
    assert!(chunks
        .chunk_embeddings(&outcome.ids, "missing-signature")
        .await
        .expect("missing embeddings are empty")
        .is_empty());

    let retrieval = provider.as_retrieval().expect("Retrieval");
    assert!(matches!(
        retrieval
            .fast_retrieve(
                "   ",
                FastRetrieveQuery {
                    limit: 5,
                    max_hops: 1,
                    time_window_days: None,
                },
                None,
            )
            .await,
        Err(MemoryError::Invalid(_))
    ));
    let fast = retrieval
        .fast_retrieve(
            "TinyMemory adapter",
            FastRetrieveQuery {
                limit: 5,
                max_hops: 2,
                time_window_days: Some(30),
            },
            None,
        )
        .await
        .expect("fast retrieval");
    assert!(fast.hits.len() <= 5);
    let entities = retrieval
        .search_entities("Alice", None, 5)
        .await
        .expect("unrestricted entity search");
    assert!(entities.len() <= 5);
    assert!(matches!(
        retrieval
            .search_entities("x", Some(&["not-a-kind".to_string()]), 5)
            .await,
        Err(MemoryError::Invalid(_))
    ));
    let leaves = retrieval
        .retrieve_leaves(&outcome.ids, None)
        .await
        .expect("retrieve ingested leaves");
    assert!(!leaves.is_empty());
    assert!(leaves.iter().any(|hit| hit.content.contains("TinyMemory")));
    let source = retrieval
        .retrieve_source(
            &tinymemory_api::provider::SourceRetrievalQuery {
                source_id: Some("successful-upload".into()),
                source_kind: None,
                time_window_days: None,
                query: Some("TinyMemory".into()),
                limit: 5,
            },
            None,
        )
        .await
        .expect("retrieve source");
    assert!(source.hits.len() <= 5);
    let cover = retrieval
        .cover_window(
            &tinymemory_api::provider::CoverWindowQuery {
                since_ms: 1_699_999_000_000,
                until_ms: 1_700_001_000_000,
                source_id: Some("successful-upload".into()),
                source_kind: None,
                limit: Some(5),
            },
            None,
        )
        .await
        .expect("cover window");
    assert!(cover.hits.len() <= 5);
    assert!(retrieval
        .retrieve_children("missing-node", 2, Some("TinyMemory"), Some(5), None)
        .await
        .expect("missing node children")
        .is_empty());
    assert!(
        retrieval
            .recall_namespace_scored("global", "TinyMemory", 5, None)
            .await
            .expect("namespace recall")
            .len()
            <= 5
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn tree_entities_and_maintenance_execute_real_workspace_transitions() {
    use tinymemory_api::provider::MemoryProvider;
    use tinymemory_api::tree::IngestRequest;

    let workspace = tempfile::tempdir().expect("workspace");
    let provider = provider_over(workspace.path());

    let tree = provider.as_tree().expect("Tree");
    tree.append(IngestRequest {
        namespace: "project".into(),
        content: "A deterministic tree buffer entry".into(),
        timestamp: Some(chrono::DateTime::from_timestamp(1_700_000_000, 0).expect("timestamp")),
        metadata: Some(serde_json::json!({"source": "test"})),
    })
    .await
    .expect("append tree content");
    let config = provider_config(workspace.path(), serde_json::Value::Null);
    let buffered = tinymemory_core::tree::tree_runtime::store::buffer_read(&config, "project")
        .expect("read persisted buffer");
    assert_eq!(buffered.len(), 1);
    assert!(buffered[0].1.contains("deterministic tree buffer"));
    let empty_status = tree.seal("empty-tree").await.expect("seal empty tree");
    assert_eq!(empty_status.total_nodes, 0);
    let cascaded = tree
        .cascade("empty-tree")
        .await
        .expect("cascade empty tree");
    assert_eq!(cascaded.namespace, empty_status.namespace);
    assert_eq!(cascaded.total_nodes, empty_status.total_nodes);
    assert_eq!(cascaded.depth, empty_status.depth);
    assert!(tree
        .query_source("project", "missing-source", 5, None)
        .await
        .expect("query missing source")
        .is_empty());
    assert!(tree.drill_down("project", "missing-node").await.is_err());

    use tinymemory_core::engine::backend::store::entity_index::{CanonicalEntity, EntityKind};
    let indexed = [
        CanonicalEntity {
            canonical_id: "person:alice".into(),
            kind: EntityKind::Person,
            surface: "Alice".into(),
            span_start: 0,
            span_end: 5,
            score: 1.0,
        },
        CanonicalEntity {
            canonical_id: "organization:tinymemory".into(),
            kind: EntityKind::Organization,
            surface: "TinyMemory".into(),
            span_start: 16,
            span_end: 26,
            score: 1.0,
        },
    ];
    assert_eq!(
        tinymemory_core::store::entities::index_entities(
            &config,
            &indexed,
            "chunk-1",
            "leaf",
            1_700_000_000_000,
            Some("project"),
        )
        .expect("seed entity index"),
        2
    );
    let entities = provider.as_entities().expect("Entities");
    let hits = entities
        .entities("project", Some("alice"), 10)
        .await
        .expect("query entities");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].entity.id, "person:alice");
    assert_eq!(hits[0].mentions, 1);
    let edges = entities
        .entity_edges("project", "person:alice", 10)
        .await
        .expect("entity edges");
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].object, "organization:tinymemory");
    entities
        .touch_entities("project", &["person:alice".into()])
        .await
        .expect("touch entity hotness");
    let touched = entities
        .entities("project", Some("alice"), 10)
        .await
        .expect("query touched entity");
    assert!(touched[0].hotness > 0.0);

    let maintenance = provider.as_maintenance().expect("Maintenance");
    let reembed = maintenance.reembed().await.expect("reembed");
    assert_eq!(reembed.operation, "reembed");
    let compact = maintenance.compact().await.expect("compact");
    assert_eq!(compact.operation, "compact");
    let first = maintenance.consolidate().await.expect("consolidate");
    let second = maintenance.consolidate().await.expect("consolidate again");
    assert_eq!(first.operation, "consolidate");
    assert!(first.changed <= 1 && second.changed <= 1);
    let doctor = maintenance.doctor().await.expect("doctor");
    assert_eq!(doctor.operation, "doctor");
    assert_eq!(doctor.changed, 0);
}

#[cfg(feature = "memory-git")]
#[tokio::test(flavor = "multi_thread")]
async fn diff_captures_and_compares_real_source_snapshots() {
    use tinymemory_api::chunks::{Chunk, Metadata, SourceKind};
    use tinymemory_api::provider::types::ChangeKind;
    use tinymemory_api::provider::MemoryProvider;

    let workspace = tempfile::tempdir().expect("workspace");
    let source_path = workspace.path().join("source");
    std::fs::create_dir(&source_path).expect("source directory");
    let sources = serde_json::json!([{
        "id": "src_diff",
        "kind": "folder",
        "label": "Diff folder",
        "enabled": true,
        "path": source_path,
    }]);
    let provider = provider_over_sources(workspace.path(), sources.clone());
    let config = provider_config(workspace.path(), sources);
    let timestamp = chrono::DateTime::from_timestamp(1_700_000_000, 0).expect("timestamp");
    let chunk = |content: &str| Chunk {
        id: "stable-chunk-id".into(),
        content: content.into(),
        metadata: Metadata::point_in_time(
            SourceKind::Document,
            "mem_src:src_diff:item-1",
            "owner",
            timestamp,
        ),
        token_count: 3,
        seq_in_source: 0,
        created_at: timestamp,
        partial_message: false,
    };
    assert_eq!(
        tinymemory_core::store::chunks::store::upsert_chunks(&config, &[chunk("first body")])
            .expect("seed source chunk"),
        1
    );

    let diff = provider.as_diff().expect("Diff enabled by memory-git");
    let first = diff
        .capture_snapshot("src_diff")
        .await
        .expect("first snapshot");
    assert_eq!(first.item_count, 1);
    assert_eq!(
        tinymemory_core::store::chunks::store::upsert_chunks(&config, &[chunk("second body")])
            .expect("modify source chunk"),
        1
    );
    let second = diff
        .capture_snapshot("src_diff")
        .await
        .expect("second snapshot");
    let snapshots = diff
        .snapshots("src_diff", 10)
        .await
        .expect("list snapshots");
    assert_eq!(snapshots.len(), 2);
    let report = diff
        .diff("src_diff", Some(&first.id), &second.id)
        .await
        .expect("diff snapshots");
    assert_eq!(report.modified, 1);
    assert_eq!(report.added, 0);
    assert_eq!(report.removed, 0);
    assert_eq!(report.changes.len(), 1);
    assert_eq!(report.changes[0].item_id, "item-1");
    assert_eq!(report.changes[0].kind, ChangeKind::Modified);
}
