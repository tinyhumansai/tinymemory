//! The conformance suite over the FULL twenty-family driver (#18 §E1/§E3).
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
        // No cadence and no Composio mode: the conformance suite drives the
        // provider directly and starts no periodic loop, so the values that
        // matter to those loops are left at what a host that states nothing
        // sends.
        memory_sync_interval_secs: None,
        composio_mode: String::new(),
        composio_entity_id: String::new(),
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

/// The maintenance diagnostics answer from the store, not from their defaults.
///
/// `store_stats`, `queue_stats` and `latest_queue_failure` are defaulted on
/// the trait so a driver without a queue compiles and answers "nothing" rather
/// than refusing. That default is also the failure this test exists to catch:
/// an engine that inherits it reports an empty store and an idle queue forever,
/// which is indistinguishable from a healthy quiet one and is exactly the shape
/// a caller cannot detect. Asserting a zero would pass against the default, so
/// this stores something first and requires the numbers to move.
#[tokio::test(flavor = "multi_thread")]
async fn maintenance_diagnostics_read_the_store_rather_than_their_defaults() {
    use tinymemory_api::chunks::DataSource;
    use tinymemory_api::provider::{MemoryMaintenance, MemoryProvider};
    use tinymemory_api::types::MemoryTaint;

    let workspace = tempfile::tempdir().expect("workspace");
    let provider = provider_over(workspace.path());

    let before = provider.store_stats().await.expect("store stats");
    assert_eq!(before.chunks, 0, "a fresh workspace holds nothing");
    assert_eq!(
        before.most_recent_chunk_ms, None,
        "an empty store has no newest chunk — `None`, never `Some(0)`, which \
         would be a chunk stamped at the epoch"
    );

    // Ingested, not `store`d: `MemoryCore::store` writes a memory entry, and
    // `store_stats.chunks` counts the CHUNK tier, which only ingest fills.
    // Asserting against `store` here passed a zero against a zero and proved
    // nothing — the first version of this test did exactly that.
    let outcome = provider
        .as_ingest()
        .expect("Ingest")
        .ingest_document(tinymemory_api::provider::types::IngestItem {
            namespace: None,
            source: DataSource::Upload,
            source_id: "diagnostics-upload".into(),
            owner: "owner".into(),
            source_ref: None,
            content: "A deterministic sentence for the diagnostics test.".into(),
            mime: Some("text/plain".into()),
            timestamp: Some(
                chrono::DateTime::from_timestamp(1_700_000_000, 0).expect("fixed timestamp"),
            ),
            tags: Vec::new(),
            taint: MemoryTaint::Internal,
            path_scope: None,
            author: None,
            channel_label: None,
            platform: None,
            to: Vec::new(),
            cc: Vec::new(),
            subject: None,
            list_unsubscribe: None,
        })
        .await
        .expect("ingest a document");
    assert!(outcome.written > 0, "the ingest must persist a chunk");

    let after = provider.store_stats().await.expect("store stats");
    assert!(
        after.chunks > before.chunks,
        "the count must follow the store; got {} after writing to {}",
        after.chunks,
        before.chunks
    );
    assert!(
        after.chunks_with_structure <= after.chunks,
        "extracted chunks are a subset of stored ones; a numerator above its \
         denominator is a coverage over 100%, which is what reading the two \
         separately can produce"
    );

    // A queue this engine has not been asked to fill is legitimately empty, so
    // the assertion is that the call answers from the queue at all rather than
    // erroring — the numbers themselves are only meaningful once work exists.
    let queue = provider.queue_stats(None).await.expect("queue stats");
    assert_eq!(
        queue.eligible_now.min(queue.ready),
        queue.eligible_now,
        "jobs eligible now are a subset of jobs ready; conflating the two \
         reads a backlog of deferred work as a stall"
    );

    // Nothing has failed, and that must read as `None` rather than an error:
    // a healthy queue and a driver keeping no failure history give the same
    // answer, and neither is something a caller can act on.
    assert!(
        provider
            .latest_queue_failure()
            .await
            .expect("latest queue failure")
            .is_none(),
        "a store that has run no failing job reports no failure"
    );
}

/// Work the queue has deliberately parked is ready, but not eligible now.
///
/// This is the difference between a backlog and a stall. A job backing off
/// after a transient failure stays `ready` with its next attempt scheduled
/// forward; counting it as runnable means a queue that is behaving correctly
/// reports as one that has stopped, and the caller escalates on it. The
/// caller cannot make the distinction itself — it sees counts, not schedules
/// — so the split has to be made here.
#[tokio::test]
async fn deferred_work_stays_ready_without_becoming_eligible() {
    use tinymemory_api::provider::MemoryMaintenance;
    use tinymemory_core::queue::store as queue_store;
    use tinymemory_core::queue::types::{FlushStalePayload, NewJob};

    let workspace = tempfile::tempdir().expect("workspace");
    let provider = provider_over(workspace.path());
    let config = provider_config(workspace.path(), serde_json::Value::Null);

    let new_job =
        NewJob::flush_stale(&FlushStalePayload::default(), "2026-08-21", 1).expect("build a job");
    let id = queue_store::enqueue(&config, &new_job)
        .expect("enqueue")
        .expect("a fresh job is not a duplicate");
    let job = queue_store::get_job(&config, &id)
        .expect("read the job")
        .expect("the job exists");

    let queue = provider.queue_stats(None).await.expect("queue stats");
    assert_eq!(queue.ready, 1);
    assert_eq!(
        queue.eligible_now, 1,
        "precondition: a freshly enqueued job is runnable now"
    );

    // Park it the way a backing-off retry does: still `ready`, scheduled
    // forward. Far enough forward that no plausible clock lands after it.
    let until_ms = chrono::Utc::now().timestamp_millis() + 60 * 60 * 1000;
    queue_store::mark_deferred(&config, &job, until_ms, "backing off").expect("defer the job");

    let queue = provider.queue_stats(None).await.expect("queue stats");
    assert_eq!(
        queue.ready, 1,
        "deferred work is still queued — it has not been abandoned"
    );
    assert_eq!(
        queue.eligible_now, 0,
        "but nothing is runnable, so nothing is being held up"
    );
    assert_eq!(
        queue.oldest_eligible_ms, None,
        "and there is no waiting job to measure an idle window from"
    );
}

/// Retrying moves parked work back to ready, and says how much it moved.
///
/// The count is the point. A caller offering the user a "retry failed" control
/// has to tell them whether anything happened, and `0` from a queue with
/// nothing parked has to be distinguishable from a retry that silently did
/// not run — which is what the direct call this replaces gave them, since it
/// returned a count the host then had to pair with a separate wake.
#[tokio::test]
async fn retrying_moves_parked_work_back_to_ready_and_counts_it() {
    use tinymemory_api::provider::MemoryMaintenance;
    use tinymemory_core::queue::store as queue_store;
    use tinymemory_core::queue::types::{FlushStalePayload, NewJob};
    use tinymemory_core::tree::health::{FailureCode, PipelineFailure};

    let workspace = tempfile::tempdir().expect("workspace");
    let provider = provider_over(workspace.path());
    let config = provider_config(workspace.path(), serde_json::Value::Null);

    // Nothing parked: a retry is honest about having moved nothing rather than
    // refusing or inventing a number.
    let report = provider.retry_failed().await.expect("retry");
    assert_eq!(report.operation, "retry_failed");
    assert_eq!(report.changed, 0, "an empty queue has nothing to requeue");

    let new_job =
        NewJob::flush_stale(&FlushStalePayload::default(), "2026-08-23", 1).expect("build a job");
    let id = queue_store::enqueue(&config, &new_job)
        .expect("enqueue")
        .expect("a fresh job is not a duplicate");
    let job = queue_store::get_job(&config, &id)
        .expect("read the job")
        .expect("the job exists");
    queue_store::mark_failed_typed(
        &config,
        &job,
        "parked for the retry test",
        Some(&PipelineFailure::new(FailureCode::BudgetExhausted)),
    )
    .expect("park the job");

    let before = provider.queue_stats(None).await.expect("queue stats");
    assert_eq!(before.failed, 1, "precondition: one job is parked");
    assert_eq!(before.ready, 0);

    let report = provider.retry_failed().await.expect("retry");
    assert_eq!(
        report.changed, 1,
        "the parked job was given another attempt, and the caller is told so"
    );

    let after = provider.queue_stats(None).await.expect("queue stats");
    assert_eq!(after.failed, 0, "nothing is parked any more");
    assert_eq!(
        after.ready, 1,
        "and the job is queued again rather than lost"
    );
}

/// The backfill flag is answered through the contract, and its scope is the
/// driver's process rather than the store.
///
/// Not derivable from `queue_stats`: a backfill chain has an instant between
/// links with nothing ready and nothing running and the work unfinished, which
/// is exactly when a caller reasoning about an empty recall needs it. What
/// this pins is that the member reports the engine's state rather than the
/// trait's `false` default — the failure that closes a re-embed modal while
/// the driver is still preparing work.
#[tokio::test]
async fn the_backfill_flag_is_reported_through_the_contract() {
    use tinymemory_api::provider::MemoryMaintenance;

    // The flag is a process-global. A test that sets it puts it back on every
    // path, including a failing assertion, or it leaks into whatever runs next
    // in this process.
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            tinymemory_core::queue::set_backfill_in_progress(false);
        }
    }

    let workspace = tempfile::tempdir().expect("workspace");
    let provider = provider_over(workspace.path());

    assert!(
        !provider
            .backfill_in_progress()
            .await
            .expect("read the flag"),
        "a store with no backfill running says so"
    );

    let _restore = Restore;
    tinymemory_core::queue::set_backfill_in_progress(true);
    assert!(
        provider
            .backfill_in_progress()
            .await
            .expect("read the flag"),
        "the member reports the engine's state, not the trait's default"
    );

    // The pairing is the point: at this instant the counts alone would tell a
    // caller the queue is finished.
    let stats = provider.queue_stats(None).await.expect("queue stats");
    assert_eq!(
        (stats.ready, stats.running),
        (0, 0),
        "precondition: nothing ready and nothing running, yet a backfill is up"
    );
}

/// A reported failure carries the success watermark that decides whether it is
/// still worth showing.
///
/// A caller asking "has anything succeeded since this failed?" out of two
/// separate calls lets a job settle in between and flip the answer. So the
/// watermark rides along with the failure, and what this test pins is that it
/// is populated from the store rather than left at `None` — the shape that
/// would push the caller back into asking twice.
#[tokio::test]
async fn a_reported_failure_carries_the_success_watermark_that_supersedes_it() {
    use tinymemory_api::provider::MemoryMaintenance;
    use tinymemory_core::queue::store as queue_store;
    use tinymemory_core::queue::types::{FlushStalePayload, NewJob};
    use tinymemory_core::tree::health::{FailureCode, PipelineFailure};

    let workspace = tempfile::tempdir().expect("workspace");
    let provider = provider_over(workspace.path());
    let config = provider_config(workspace.path(), serde_json::Value::Null);

    // Fail one job for real rather than writing the row by hand: the query
    // filters on `failure_reason IS NOT NULL`, which only the typed failure
    // path fills, and a hand-written row would not prove that filter matches
    // what the engine actually persists.
    let failing =
        NewJob::flush_stale(&FlushStalePayload::default(), "2026-08-21", 1).expect("build a job");
    let failing_id = queue_store::enqueue(&config, &failing)
        .expect("enqueue")
        .expect("a fresh job is not a duplicate");
    let failing = queue_store::get_job(&config, &failing_id)
        .expect("read the job")
        .expect("the job exists");
    queue_store::mark_failed_typed(
        &config,
        &failing,
        "the failure the operator would see",
        Some(&PipelineFailure::new(FailureCode::BudgetExhausted)),
    )
    .expect("fail the job");

    let failure = provider
        .latest_queue_failure()
        .await
        .expect("latest queue failure")
        .expect("the failed job is reported");
    assert_eq!(failure.reason, "budget_exhausted");
    assert_eq!(
        failure.last_success_ms, None,
        "nothing has succeeded yet, so there is no watermark to supersede it"
    );

    let queue = provider.queue_stats(None).await.expect("queue stats");
    assert_eq!(queue.failed, 1, "the job is parked as failed");
    assert_eq!(
        queue.failed_unrecoverable, 1,
        "an exhausted budget is not retried on its own, and a caller that \
         escalates on `failed` alone cannot tell that apart from a failure \
         about to self-heal"
    );
    assert!(
        queue.last_completed_ms.is_some(),
        "a failure settles a job too. Counting only successes here reports a \
         queue that is failing fast — as fast as it can run — as one that has \
         gone idle, which is the opposite diagnosis"
    );

    // Now settle one successfully. Same queue, same connection the failure is
    // read on.
    let done =
        NewJob::flush_stale(&FlushStalePayload::default(), "2026-08-22", 1).expect("build a job");
    let done_id = queue_store::enqueue(&config, &done)
        .expect("enqueue")
        .expect("a fresh job is not a duplicate");
    let done = queue_store::get_job(&config, &done_id)
        .expect("read the job")
        .expect("the job exists");
    queue_store::mark_done(&config, &done).expect("settle the job");

    let failure = provider
        .latest_queue_failure()
        .await
        .expect("latest queue failure")
        .expect("the failed job is still the newest failure");
    let watermark = failure
        .last_success_ms
        .expect("a completed job must show up as the success watermark");
    let failed_at = failure
        .completed_at_ms
        .expect("the typed failure path stamps a completion time");
    // `>=`, not `>`: both settle from the wall clock and can land on the same
    // millisecond. What is under test is that the two values arrive together
    // and are comparable at all, not the resolution of the clock.
    assert!(
        watermark >= failed_at,
        "the success settled after the failure; got watermark {watermark} against failure \
         {failed_at}"
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
        .create_segment("seg-1", "session-1", "global", turn_id, Some(1), 10.0, 10.0)
        .await
        .expect("create segment");
    episodic
        .append_turn("seg-1", turn_id, Some(2), 10.0, 11.0)
        .await
        .expect("append turn");
    let segment = episodic
        .open_segment("session-1")
        .await
        .expect("open segment")
        .expect("segment present");
    assert_eq!(segment.turn_count, 2);
    assert_eq!(
        (segment.start_seq, segment.end_seq),
        (Some(1), Some(2)),
        "the per-session sequence pair survives the contract round trip — \
         segment selection prefers it over ms-rounded timestamps"
    );
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
    // An extracted event lands against its segment through the contract, and
    // the id is an upsert key: re-recording under the same id replaces the row
    // rather than duplicating it, so a re-run of extraction is idempotent.
    use tinymemory_api::provider::{EpisodicEvent, EventKind};
    let event = EpisodicEvent {
        event_id: "evt-1".into(),
        segment_id: "seg-1".into(),
        session_id: "session-1".into(),
        namespace: "global".into(),
        kind: EventKind::Decision,
        content: "the test decided to remember".into(),
        subject: Some("the test".into()),
        timestamp_ref: None,
        confidence: 0.9,
        embedding: None,
        source_turn_ids: Some(turn_id.to_string()),
        created_at: 16.0,
    };
    episodic.insert_event(&event).await.expect("insert event");
    episodic
        .insert_event(&EpisodicEvent {
            content: "the test revised its decision".into(),
            ..event
        })
        .await
        .expect("re-insert under the same id");
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
        author: None,
        channel_label: None,
        platform: None,
        to: Vec::new(),
        cc: Vec::new(),
        subject: None,
        list_unsubscribe: None,
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
            author: None,
            channel_label: None,
            platform: None,
            to: Vec::new(),
            cc: Vec::new(),
            subject: None,
            list_unsubscribe: None,
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
            // The widened trio: the speaking role is not the owner, the label
            // is not the dedupe key, and the platform string is the caller's
            // own. Set here so the mapping that used to collapse all three is
            // exercised by conformance rather than trusted.
            author: Some("assistant".into()),
            channel_label: Some("Agent session #1".into()),
            platform: Some("agent".into()),
            to: Vec::new(),
            cc: Vec::new(),
            subject: None,
            list_unsubscribe: None,
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
    // The provenance kind survives the engine-to-contract crossing. That
    // crossing is a serde round-trip, not a field-by-field mapping, so a
    // rename on either side loses the kind without failing anything: the hit
    // still decodes, just kindless, and openhuman's four retrieval RPCs would
    // start serving a body missing a field they have always emitted. Pinned to
    // the engine's leaf placeholder rather than to `is_some`, because a
    // crossing that invented a value would satisfy the weaker check.
    assert!(leaves
        .iter()
        .all(|hit| hit.tree_kind.as_deref() == Some("source")));
    // …and the field stays additive in both directions. A payload written by a
    // peer that predates it must still decode, and a hit with no kind must
    // encode without the key, or an older peer parsing this response sees a
    // shape it was not built against.
    let mut without_kind =
        serde_json::to_value(leaves.first().expect("a leaf hit")).expect("encode a hit");
    without_kind
        .as_object_mut()
        .expect("a hit encodes as a JSON object")
        .remove("tree_kind");
    let decoded: tinymemory_api::provider::RetrievalHit =
        serde_json::from_value(without_kind).expect("a payload without tree_kind still decodes");
    assert!(decoded.tree_kind.is_none());
    assert!(!serde_json::to_value(&decoded)
        .expect("re-encode a kindless hit")
        .as_object()
        .expect("a hit encodes as a JSON object")
        .contains_key("tree_kind"));
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

/// A second ingest of one source reports the gate, not a dropped chunk.
///
/// The driver refuses a document whose `source_id` it has already ingested, and
/// does not look at the content to decide — so even completely different
/// material writes nothing. What the contract has to carry is *why* nothing was
/// written. A caller re-ingesting on purpose (after a wipe, after a failed
/// import, after clearing a gate by hand) reads a refusal as "the claim is
/// still there, go clear it" and an empty result as "there was nothing to
/// write", and those lead to opposite actions.
///
/// The two facts are therefore asserted apart. `skipped` counting the refusal
/// as one unit — the reading this replaces — is indistinguishable from a single
/// genuinely dropped chunk, which is the whole defect.
#[tokio::test(flavor = "multi_thread")]
async fn a_repeated_source_reports_its_gate_rather_than_a_dropped_chunk() {
    use tinymemory_api::chunks::DataSource;
    use tinymemory_api::provider::types::IngestItem;
    use tinymemory_api::provider::MemoryProvider;
    use tinymemory_api::types::MemoryTaint;

    let workspace = tempfile::tempdir().expect("workspace");
    let provider = provider_over(workspace.path());
    let ingest = provider.as_ingest().expect("Ingest");

    let item = IngestItem {
        namespace: None,
        source: DataSource::Upload,
        source_id: "claimed-source".into(),
        owner: "owner".into(),
        source_ref: None,
        content: "The adapter is maintained from Kuwait and ships on Thursday.".into(),
        mime: Some("text/plain".into()),
        timestamp: Some(
            chrono::DateTime::from_timestamp(1_700_000_000, 0).expect("fixed timestamp"),
        ),
        tags: vec!["coverage".into()],
        taint: MemoryTaint::Internal,
        path_scope: None,
        author: None,
        channel_label: None,
        platform: None,
        to: Vec::new(),
        cc: Vec::new(),
        subject: None,
        list_unsubscribe: None,
    };

    let first = ingest
        .ingest_document(item.clone())
        .await
        .expect("first ingest");
    assert!(first.written > 0, "the first ingest of a source writes it");
    assert!(
        !first.already_ingested,
        "and is not itself a refusal, or the flag would be a constant"
    );
    assert!(
        first.extract_jobs_enqueued > 0,
        "rows written with nothing scheduled to derive from them is the failure \
         this count exists to make visible"
    );

    let second = ingest
        .ingest_document(IngestItem {
            // Different content under the same id: the gate is on the source,
            // so this must still write nothing.
            content: "Entirely different words under an id that is already claimed.".into(),
            ..item
        })
        .await
        .expect("second ingest");
    assert!(
        second.already_ingested,
        "the source gate refused the call, and the outcome has to say so"
    );
    assert_eq!(second.written, 0, "a refused call writes nothing");
    assert_eq!(
        second.skipped, 0,
        "nothing was dropped: folding the refusal in here reads as one \
         dropped chunk and the caller cannot tell the two apart"
    );
    assert!(second.ids.is_empty());
    assert_eq!(
        second.extract_jobs_enqueued, 0,
        "and it scheduled no follow-up work"
    );
}

/// An email thread ingests as mail, not as a document that happens to be text.
///
/// The stored chunk carries the rendered `From:` header, which is what makes
/// this assertion discriminate: routing the mail path through
/// `ingest_document` would store the same bodies with the per-message headers
/// gone, and a citation back to one message has nothing left to point at.
/// It also pins the sender mapping — `author` is the speaker, and attributing
/// every message to the owner is the failure the chat path already had.
#[tokio::test(flavor = "multi_thread")]
async fn an_email_thread_keeps_its_per_message_headers() {
    use tinymemory_api::chunks::DataSource;
    use tinymemory_api::provider::types::IngestItem;
    use tinymemory_api::provider::MemoryProvider;
    use tinymemory_api::types::MemoryTaint;

    let workspace = tempfile::tempdir().expect("workspace");
    let provider = provider_over(workspace.path());
    let ingest = provider.as_ingest().expect("Ingest");

    let message = |author: &str, text: &str, at: i64| IngestItem {
        namespace: None,
        source: DataSource::Gmail,
        source_id: "mail:thread-1".into(),
        owner: "owner@example.com".into(),
        source_ref: None,
        content: text.into(),
        mime: Some("text/plain".into()),
        timestamp: Some(chrono::DateTime::from_timestamp(at, 0).expect("fixed timestamp")),
        tags: vec!["coverage".into()],
        taint: MemoryTaint::Internal,
        path_scope: None,
        author: Some(author.into()),
        channel_label: Some("Adapter ship date".into()),
        platform: None,
        to: Vec::new(),
        cc: Vec::new(),
        subject: None,
        list_unsubscribe: None,
    };

    let outcome = ingest
        .ingest_email(vec![
            message(
                "alice@example.com",
                "The adapter ships on Thursday if the review lands.",
                1_700_000_000,
            ),
            message(
                "bob@example.com",
                "The review is done, so Thursday holds.",
                1_700_000_100,
            ),
            IngestItem {
                to: vec!["carol@example.com".into()],
                cc: vec!["dave@example.com".into()],
                subject: Some("Re: adapter, renamed".into()),
                list_unsubscribe: Some("<https://lists.example.com/u/9>".into()),
                ..message(
                    "carol@example.com",
                    "Renaming the thread so the ship date is findable.",
                    1_700_000_200,
                )
            },
        ])
        .await
        .expect("email ingest");
    assert!(outcome.written > 0, "the thread reached the pipeline");
    assert!(
        !outcome.ids.is_empty(),
        "and the driver identified what it wrote"
    );

    let stored = provider
        .as_chunks()
        .expect("Chunks")
        .get_chunk(&outcome.ids[0])
        .await
        .expect("read the first stored chunk")
        .expect("the id the outcome reported must resolve");
    assert!(
        stored.content.contains("From: alice@example.com"),
        "the sender header is what a per-message citation resolves against: {}",
        stored.content
    );

    // The headers the third message carries are not decoration. `To:`/`Cc:`
    // are what "who else saw this" resolves against, a per-message `Subject:`
    // is how a renamed thread stays findable under its new name, and
    // `List-Unsubscribe:` is the input an unsubscribe flow reads back out of
    // stored mail — a pipeline that drops it makes that flow impossible, not
    // merely less complete. So this asserts they survive the crossing rather
    // than trusting that they do.
    let thread = stored_thread_text(&provider, &outcome.ids).await;
    for header in [
        "To: carol@example.com",
        "Cc: dave@example.com",
        "Subject: Re: adapter, renamed",
        "List-Unsubscribe: <https://lists.example.com/u/9>",
    ] {
        assert!(
            thread.contains(header),
            "`{header}` must survive the crossing: {thread}"
        );
    }
    // And a message that names no subject of its own still inherits the
    // thread's, so the field being optional does not leave mail unlabelled.
    assert!(
        thread.contains("Subject: Adapter ship date"),
        "a message with no subject of its own keeps the thread's: {thread}"
    );
}

/// Every chunk the outcome named, concatenated, so a header assertion does not
/// depend on which chunk the splitter happened to put it in.
async fn stored_thread_text(
    provider: &impl tinymemory_api::provider::MemoryProvider,
    ids: &[String],
) -> String {
    let chunks = provider.as_chunks().expect("Chunks");
    let mut text = String::new();
    for id in ids {
        if let Some(chunk) = chunks.get_chunk(id).await.expect("read stored chunk") {
            text.push_str(&chunk.content);
            text.push('\n');
        }
    }
    text
}

/// Flushing twice inside one window schedules the work once, and says so.
///
/// The deduplication is the driver's, keyed on date and three-hour block, so a
/// user hitting "flush now" twice does not get the work queued twice. What
/// makes that safe to surface is the buffer count riding alongside: without
/// it, `enqueued: false` is ambiguous between "nothing to flush" and "already
/// scheduled", and a caller showing the first when it is the second is lying
/// about state the user is watching.
#[tokio::test(flavor = "multi_thread")]
async fn flushing_twice_in_a_window_schedules_the_work_once() {
    use tinymemory_api::provider::{MemoryMaintenance, MemoryProvider};
    use tinymemory_api::tree::IngestRequest;

    let workspace = tempfile::tempdir().expect("workspace");
    let provider = provider_over(workspace.path());

    provider
        .as_tree()
        .expect("Tree")
        .append(IngestRequest {
            namespace: "project".into(),
            content: "something buffered and waiting to be written out".into(),
            timestamp: Some(chrono::DateTime::from_timestamp(1_700_000_000, 0).expect("ts")),
            metadata: None,
        })
        .await
        .expect("append");

    let first = provider.flush_pending().await.expect("first flush");
    assert!(
        first.enqueued,
        "the first flush in a window schedules the work"
    );

    let second = provider.flush_pending().await.expect("second flush");
    assert!(
        !second.enqueued,
        "the second deduplicates against the first rather than queueing it twice"
    );
    assert_eq!(
        second.stale_buffers, first.stale_buffers,
        "and still reports what is pending, so `enqueued: false` is not mistaken \
         for an empty queue"
    );
}

/// Resetting the derived index keeps every chunk it was derived from.
///
/// This is the invariant that makes the operation safe to expose at all.
/// Summaries, buffers, entity indexes and trees are recomputable; the chunks
/// are the source and are never deleted. A reset that took them too would be
/// data loss wearing the word "reset".
#[tokio::test(flavor = "multi_thread")]
async fn resetting_the_derived_index_keeps_the_chunks_it_derives_from() {
    use tinymemory_api::provider::{MemoryMaintenance, MemoryProvider};
    use tinymemory_api::tree::IngestRequest;

    let workspace = tempfile::tempdir().expect("workspace");
    let provider = provider_over(workspace.path());

    provider
        .as_tree()
        .expect("Tree")
        .append(IngestRequest {
            namespace: "project".into(),
            content: "content the derived index is built from".into(),
            timestamp: Some(chrono::DateTime::from_timestamp(1_700_000_000, 0).expect("ts")),
            metadata: None,
        })
        .await
        .expect("append");

    let before = provider.store_stats().await.expect("stats before");

    let outcome = provider
        .reset_derived_index()
        .await
        .expect("reset the derived index");

    let after = provider.store_stats().await.expect("stats after");
    assert_eq!(
        after.chunks, before.chunks,
        "the reset deletes what was derived, never the source it was derived from"
    );
    assert!(
        outcome.jobs_enqueued <= outcome.chunks_requeued,
        "the enqueue is keyed, so scheduled jobs cannot exceed requeued chunks: \
         {} jobs for {} chunks",
        outcome.jobs_enqueued,
        outcome.chunks_requeued
    );
}

/// Recency recall answers when there is no query to rank against.
///
/// This is the member's whole reason for existing. `recall_namespace_scored`
/// looks like the same call with the query left blank, and handing it `""`
/// does not degrade to recency — it runs the ranking path against nothing. A
/// context-assembly step that has not seen a user query yet needs the
/// namespace's contents ordered by freshness, which is what this returns.
///
/// What is pinned: writes are visible through it, and an unknown namespace is
/// an empty answer rather than an error — a true statement about that
/// namespace, not a fault the caller can act on.
#[tokio::test(flavor = "multi_thread")]
async fn recency_recall_answers_without_a_query() {
    use tinymemory_api::provider::{MemoryCore, MemoryProvider};
    use tinymemory_api::types::{MemoryCategory, MemoryTaint};

    let workspace = tempfile::tempdir().expect("workspace");
    let provider = provider_over(workspace.path());

    for (key, content) in [
        ("first", "the earliest note in this namespace"),
        ("second", "a later note about something else entirely"),
    ] {
        provider
            .store(
                "global",
                key,
                content,
                MemoryCategory::Core,
                None,
                MemoryTaint::default(),
            )
            .await
            .expect("store");
    }

    let retrieval = provider.as_retrieval().expect("Retrieval");

    let recent = retrieval
        .recall_namespace_recent("global", 5)
        .await
        .expect("recency recall");
    assert!(
        !recent.is_empty(),
        "a caller with no query still gets the namespace's contents back"
    );
    assert!(
        recent.len() <= 5,
        "the limit is honoured: asked for 5, got {}",
        recent.len()
    );

    let empty = retrieval
        .recall_namespace_recent("a-namespace-nothing-was-written-to", 5)
        .await
        .expect("an unknown namespace is not an error");
    assert!(empty.is_empty());
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

    // The occurrence-index reads.
    //
    // Two entities, both on `chunk-1`, both seen once. That is enough to pin
    // every property the three members promise and the namespace-scoped
    // `entities` above does not: no namespace argument, a count instead of a
    // hotness, a surface instead of a name, and a join back to the chunk.
    let store_wide = entities
        .top_entities(None, 10)
        .await
        .expect("store-wide entity index");
    assert_eq!(store_wide.len(), 2);
    // Ordered by count, and both counts are 1 — so this asserts membership,
    // not position. Asserting an order the SQL breaks ties on by timestamp,
    // when both rows carry the same timestamp, would be a flaky test.
    let alice = store_wide
        .iter()
        .find(|row| row.entity_id == "person:alice")
        .expect("the seeded person is in the index");
    assert_eq!(alice.kind, "person");
    assert_eq!(alice.surface, "Alice");
    assert_eq!(alice.mentions, 1);

    let people_only = entities
        .top_entities(Some("person"), 10)
        .await
        .expect("kind-filtered entity index");
    assert_eq!(people_only.len(), 1);
    assert_eq!(people_only[0].entity_id, "person:alice");

    // The filter is validated, not applied blindly: an unknown kind that
    // matched nothing would read as an empty store.
    assert!(
        matches!(
            entities.top_entities(Some("not-a-kind"), 10).await,
            Err(tinymemory_api::error::MemoryError::Invalid(_))
        ),
        "an unrecognised kind must be refused rather than answered with []"
    );

    let of_chunk = entities
        .chunk_entities(&["chunk-1".to_string()], None)
        .await
        .expect("entities of one chunk");
    assert_eq!(of_chunk.len(), 2);
    // Equal counts break by entity id ascending, which is deterministic here.
    assert_eq!(of_chunk[0].occurrence.entity_id, "organization:tinymemory");
    assert_eq!(of_chunk[0].occurrence.surface, "TinyMemory");
    // The single-id call is the batched one with a slice of one, and it still
    // has to say which chunk each row came from.
    assert!(of_chunk.iter().all(|row| row.chunk_id == "chunk-1"));
    assert!(entities
        .chunk_entities(&["no-such-chunk".to_string()], None)
        .await
        .expect("an unknown chunk is not an error")
        .is_empty());

    let of_entity = entities
        .entity_chunk_ids("person:alice", 10)
        .await
        .expect("chunks of one entity");
    assert_eq!(of_entity, vec!["chunk-1".to_string()]);
    assert!(entities
        .entity_chunk_ids("person:nobody", 10)
        .await
        .expect("an unknown entity is not an error")
        .is_empty());

    // Summary nodes live in the same index and are not chunks. Indexing the
    // same entity against one must not add its node id to the chunk list —
    // a caller filtering a chunk list by these ids would find nothing behind
    // it.
    assert_eq!(
        tinymemory_core::store::entities::index_entities(
            &config,
            &indexed[..1],
            "summary-1",
            "summary",
            1_700_000_100_000,
            Some("project"),
        )
        .expect("seed a summary-node occurrence"),
        1
    );
    assert_eq!(
        entities
            .entity_chunk_ids("person:alice", 10)
            .await
            .expect("chunks of one entity, with a summary node indexed"),
        vec!["chunk-1".to_string()],
    );

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

/// The count answers the same question the page does.
///
/// `count_chunks` exists because a caller rendering "20 of 431" cannot derive
/// 431 from a page, and the only host-side way to get it — list everything and
/// measure — is the unbounded query the row limit exists to prevent. So the
/// total is computed where the `WHERE` clause is, and what has to be pinned is
/// that it is computed from *that* `WHERE` clause.
///
/// Three failure shapes are each asserted apart, because the plausible wrong
/// implementations differ:
///
/// - a count wired to the engine's unfiltered `count_chunks` returns the whole
///   table and looks right until a filter is applied, so the filtered count is
///   required to be strictly smaller than the unfiltered one;
/// - a count that reuses the listing's SQL wholesale keeps its `LIMIT`, so the
///   same query paged one row at a time must not move it;
/// - a count that skips the scope clause reports rows a scoped caller is not
///   allowed to see, which is the source gate failing open in a number.
///
/// The rows are seeded through the store rather than the ingest pipeline: the
/// point here is which rows a predicate matches, and the pipeline decides
/// chunk boundaries, which would make the expected totals a property of the
/// chunker instead of the filter.
#[tokio::test(flavor = "multi_thread")]
async fn the_chunk_count_matches_the_page_and_ignores_its_bounds() {
    use tinymemory_api::chunks::{Chunk, Metadata, SourceKind};
    use tinymemory_api::provider::types::SourceScope;
    use tinymemory_api::provider::{ChunkQuery, MemoryProvider};

    let workspace = tempfile::tempdir().expect("workspace");
    let provider = provider_over(workspace.path());
    let config = provider_config(workspace.path(), serde_json::Value::Null);
    let timestamp = chrono::DateTime::from_timestamp(1_700_000_000, 0).expect("timestamp");
    let seed = |id: &str, kind: SourceKind, source_id: &str, tags: Vec<String>, seq: u32| Chunk {
        id: id.into(),
        content: format!("Body of {id}."),
        metadata: Metadata {
            tags,
            ..Metadata::point_in_time(kind, source_id, "owner", timestamp)
        },
        token_count: 3,
        seq_in_source: seq,
        created_at: timestamp,
        partial_message: false,
    };
    // Four documents and one chat, so the kind filter has something to drop;
    // one of the documents is source-attributed, so the scope does too.
    let seeded = vec![
        seed("count-doc-0", SourceKind::Document, "doc-source", vec![], 0),
        seed("count-doc-1", SourceKind::Document, "doc-source", vec![], 1),
        seed("count-doc-2", SourceKind::Document, "doc-source", vec![], 2),
        seed(
            "count-doc-scoped",
            SourceKind::Document,
            "mem_src:src-a:item-1",
            vec!["memory_sources".into()],
            0,
        ),
        seed("count-chat-0", SourceKind::Chat, "chat-source", vec![], 0),
    ];
    assert_eq!(
        tinymemory_core::store::chunks::store::upsert_chunks(&config, &seeded)
            .expect("seed chunks"),
        seeded.len(),
    );

    let chunks = provider.as_chunks().expect("Chunks");
    // A limit well above the seeded rows: the listing this is compared against
    // must not be the truncated one, or the equality would hold for the wrong
    // reason.
    let documents = ChunkQuery {
        source_kind: Some(SourceKind::Document),
        limit: Some(1_000),
        ..ChunkQuery::default()
    };
    let listed = chunks
        .list_chunks(&documents, None)
        .await
        .expect("list documents");
    let counted = chunks
        .count_chunks(&documents, None)
        .await
        .expect("count documents");
    assert_eq!(
        listed.len(),
        4,
        "four of the five seeded rows are documents"
    );
    assert_eq!(counted as usize, listed.len());

    let everything = ChunkQuery {
        limit: Some(1_000),
        ..ChunkQuery::default()
    };
    let all = chunks
        .count_chunks(&everything, None)
        .await
        .expect("count everything");
    assert_eq!(all as usize, seeded.len());
    assert!(
        counted < all,
        "the filter must reach the count; an unfiltered count would report {all} for both"
    );

    // Same predicate, one row at a time: the page moves, the total does not.
    let second_page = ChunkQuery {
        limit: Some(1),
        offset: Some(2),
        ..documents.clone()
    };
    assert_eq!(
        chunks
            .list_chunks(&second_page, None)
            .await
            .expect("list one row")
            .len(),
        1
    );
    assert_eq!(
        chunks
            .count_chunks(&second_page, None)
            .await
            .expect("count with page bounds"),
        counted,
        "limit and offset must not change what the count reports"
    );

    // The scope is applied by both, identically. `src-b` allows nothing that
    // was ingested under `src-a`, and the unattributed rows stay visible —
    // the engine's fail-closed rule, which the count has to share or it
    // reports rows the caller may not see.
    let scope = SourceScope::new(["src-b"]);
    let scoped = chunks
        .list_chunks(&documents, Some(&scope))
        .await
        .expect("list scoped documents");
    assert_eq!(scoped.len(), 3, "the source-attributed row is out of scope");
    assert_eq!(
        chunks
            .count_chunks(&documents, Some(&scope))
            .await
            .expect("count scoped documents") as usize,
        scoped.len()
    );

    // No match is a zero, not an error.
    let unmatched = ChunkQuery {
        source_id: Some("no-such-source".into()),
        ..ChunkQuery::default()
    };
    assert!(chunks
        .list_chunks(&unmatched, None)
        .await
        .expect("list nothing")
        .is_empty());
    assert_eq!(
        chunks
            .count_chunks(&unmatched, None)
            .await
            .expect("count nothing"),
        0
    );
}

/// The forest walk and its leaf edge — the two structural tree reads.
///
/// Nothing here can pass vacuously. Both members default to
/// `MemoryError::Unsupported` on the trait, so every `expect` below is a
/// claim about this engine rather than about the contract's fallback, and a
/// build that dropped either implementation fails on the first call.
///
/// What is pinned: the owning tree's kind and scope arrive denormalised onto
/// each node; the parent link survives (it is the edge a caller draws a graph
/// from, and the one thing `retrieve_children` does not carry); the source
/// allowlist is applied to trees *and* to leaves; an empty allowlist denies
/// rather than waves through; a bound reports itself as `truncated` instead of
/// erroring or lying; a tombstoned summary is invisible; and a leaf carries the
/// summary that sealed it plus a preview capped at `LEAF_PREVIEW_CHARS`.
#[tokio::test(flavor = "multi_thread")]
async fn the_summary_forest_and_its_leaves_read_through_the_contract() {
    use chrono::{TimeZone, Utc};
    use tinymemory_api::chunks::{chunk_id, Chunk, Metadata, SourceKind};
    use tinymemory_api::provider::types::SourceScope;
    use tinymemory_api::provider::MemoryProvider;
    use tinymemory_api::tree::LEAF_PREVIEW_CHARS;
    use tinymemory_core::store::chunks::store::{upsert_chunks, with_connection};
    use tinymemory_core::store::trees::store::{insert_summary_tx, insert_tree};
    use tinymemory_core::store::trees::{SummaryNode, Tree, TreeKind, TreeStatus as TreeActivity};

    const BASE_MS: i64 = 1_700_000_000_000;

    let workspace = tempfile::tempdir().expect("workspace");
    let provider = provider_over(workspace.path());
    let config = provider_config(workspace.path(), serde_json::Value::Null);

    let at = |offset_ms: i64| {
        Utc.timestamp_millis_opt(BASE_MS + offset_ms)
            .single()
            .expect("timestamp")
    };

    // Two source trees. Scoping is only testable with more than one, and the
    // whole point of the forest walk is that it spans them.
    for (id, scope) in [("tree-alpha", "src-alpha"), ("tree-beta", "src-beta")] {
        insert_tree(
            &config,
            &Tree {
                id: id.into(),
                kind: TreeKind::Source,
                scope: scope.into(),
                root_id: None,
                max_level: 2,
                ask: None,
                status: TreeActivity::Active,
                created_at: at(0),
                last_sealed_at: Some(at(0)),
            },
        )
        .expect("insert tree");
    }

    // Leaves. The `memory_sources` tag is what makes a chunk source-attributed
    // — without it the allowlist lets the row through by design — so the two
    // scoped leaves carry it and the assertions below are about the predicate
    // rather than about an exemption from it.
    let leaves = [
        (
            "src-alpha",
            0u32,
            "Alpha leaf one\nand a second line nobody labels with",
        ),
        ("src-alpha", 1, "Alpha leaf two"),
        ("src-beta", 0, "Beta leaf one"),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, (source, seq, content))| {
        let ts = at(i64::try_from(index).unwrap_or(0) * 1_000);
        Chunk {
            id: chunk_id(SourceKind::Chat, source, seq, content),
            content: content.to_string(),
            metadata: Metadata {
                source_kind: SourceKind::Chat,
                source_id: source.into(),
                owner: "owner".into(),
                timestamp: ts,
                time_range: (ts, ts),
                tags: vec!["memory_sources".into()],
                source_ref: None,
                path_scope: None,
            },
            token_count: 8,
            seq_in_source: seq,
            created_at: ts,
            partial_message: false,
        }
    })
    .collect::<Vec<_>>();
    assert_eq!(
        upsert_chunks(&config, &leaves).expect("persist leaves"),
        leaves.len()
    );

    // Four summaries: an L1 and its L2 parent in alpha, an L1 in beta, and a
    // tombstone that must never surface.
    let summary = |id: &str,
                   tree_id: &str,
                   level: u32,
                   parent: Option<&str>,
                   children: Vec<String>,
                   deleted: bool| SummaryNode {
        id: id.into(),
        tree_id: tree_id.into(),
        tree_kind: TreeKind::Source,
        level,
        parent_id: parent.map(str::to_string),
        child_ids: children,
        content: format!("seal of {id}"),
        token_count: 32,
        entities: Vec::new(),
        topics: Vec::new(),
        time_range_start: at(0),
        time_range_end: at(2_000),
        score: 0.5,
        sealed_at: at(i64::from(level) * 10),
        deleted,
        embedding: None,
        doc_id: None,
        version_ms: None,
    };
    let alpha_leaf_ids = leaves[..2]
        .iter()
        .map(|chunk| chunk.id.clone())
        .collect::<Vec<_>>();
    let seeded = [
        summary(
            "s-alpha-1",
            "tree-alpha",
            1,
            Some("s-alpha-2"),
            alpha_leaf_ids.clone(),
            false,
        ),
        summary(
            "s-alpha-2",
            "tree-alpha",
            2,
            None,
            vec!["s-alpha-1".into()],
            false,
        ),
        summary(
            "s-beta-1",
            "tree-beta",
            1,
            None,
            vec![leaves[2].id.clone()],
            false,
        ),
        summary("s-beta-gone", "tree-beta", 1, None, Vec::new(), true),
    ];
    with_connection(&config, |conn| {
        let tx = conn.unchecked_transaction()?;
        for node in &seeded {
            insert_summary_tx(&tx, node, None, "test")?;
        }
        // The seal writes this column when it claims a leaf; written directly
        // here because running the real sealer needs a summarisation model, and
        // what is under test is the read, not the summariser.
        for id in &alpha_leaf_ids {
            tx.execute(
                "UPDATE mem_tree_chunks SET parent_summary_id = ?1 WHERE id = ?2",
                rusqlite::params!["s-alpha-1", id],
            )?;
        }
        tx.commit()?;
        Ok(())
    })
    .expect("seed summaries and their leaf claims");

    let tree = provider.as_tree().expect("Tree");

    // ── The whole forest ────────────────────────────────────────────────────
    let forest = tree
        .summary_forest(100, None)
        .await
        .expect("walk the forest");
    assert!(
        !forest.truncated,
        "a walk that reached the end of the store is not truncated"
    );
    assert_eq!(
        forest.summaries.len(),
        3,
        "the tombstoned summary is not a node a caller has to know about"
    );
    let alpha_1 = forest
        .summaries
        .iter()
        .find(|node| node.id == "s-alpha-1")
        .expect("the L1 node is in the walk");
    assert_eq!(alpha_1.tree_id, "tree-alpha");
    assert_eq!(
        alpha_1.tree_scope, "src-alpha",
        "the tree's scope is denormalised onto the node; a caller that had to \
         join for it would be reading the driver's tables again"
    );
    assert_eq!(alpha_1.tree_kind, "source");
    assert_eq!(alpha_1.level, 1);
    assert_eq!(
        alpha_1.parent_id.as_deref(),
        Some("s-alpha-2"),
        "the parent link is the edge; without it this is a list, not a graph"
    );
    assert_eq!(alpha_1.child_ids, alpha_leaf_ids);
    assert_eq!(alpha_1.time_range_start, at(0));
    assert_eq!(alpha_1.time_range_end, at(2_000));
    assert!(
        forest.summaries.iter().all(|node| node.id != "s-beta-gone"),
        "a tombstone must not reach the caller"
    );

    // ── A bound reports itself ──────────────────────────────────────────────
    let clipped = tree
        .summary_forest(1, None)
        .await
        .expect("walk one node of the forest");
    assert_eq!(clipped.summaries.len(), 1);
    assert!(
        clipped.truncated,
        "a walk stopped by the bound says so, rather than reading as a store \
         with one summary in it"
    );

    // ── Scope is a predicate, on trees ──────────────────────────────────────
    let alpha_only = tree
        .summary_forest(100, Some(&SourceScope::new(["src-alpha"])))
        .await
        .expect("scoped walk");
    assert_eq!(alpha_only.summaries.len(), 2);
    assert!(
        alpha_only
            .summaries
            .iter()
            .all(|node| node.tree_id == "tree-alpha"),
        "a scoped walk answers for the allowed trees only"
    );

    let denied = tree
        .summary_forest(100, Some(&SourceScope::default()))
        .await
        .expect("an empty allowlist is an answer, not an error");
    assert!(
        denied.summaries.is_empty(),
        "an empty allowlist denies everything — it is not 'unrestricted'"
    );
    assert!(
        !denied.truncated,
        "nothing was withheld by a bound, so asking again with a bigger one \
         would change nothing"
    );

    // ── The leaf edge ───────────────────────────────────────────────────────
    let recent = tree.recent_leaves(100, None).await.expect("recent leaves");
    assert_eq!(recent.len(), 3);
    assert!(
        recent
            .windows(2)
            .all(|pair| pair[0].time_range_start >= pair[1].time_range_start),
        "newest first, as the member promises"
    );
    let claimed = recent
        .iter()
        .find(|leaf| leaf.chunk_id == alpha_leaf_ids[0])
        .expect("the first alpha leaf is in the page");
    assert_eq!(
        claimed.parent_summary_id.as_deref(),
        Some("s-alpha-1"),
        "the summary that sealed a leaf is the fact `list_chunks` cannot report"
    );
    assert_eq!(claimed.source_id, "src-alpha");
    assert_eq!(
        claimed.preview, "Alpha leaf one",
        "the preview is the first line, not the whole body"
    );
    assert!(recent
        .iter()
        .all(|leaf| leaf.preview.chars().count() <= LEAF_PREVIEW_CHARS));
    let unclaimed = recent
        .iter()
        .find(|leaf| leaf.chunk_id == leaves[2].id)
        .expect("the beta leaf is in the page");
    assert!(
        unclaimed.parent_summary_id.is_none(),
        "a leaf nothing has sealed is unattached, which is a state and not a \
         fault"
    );

    // ── Scope is a predicate, on leaves too ─────────────────────────────────
    let scoped_leaves = tree
        .recent_leaves(100, Some(&SourceScope::new(["src-alpha"])))
        .await
        .expect("scoped leaves");
    assert_eq!(scoped_leaves.len(), 2);
    assert!(
        scoped_leaves
            .iter()
            .all(|leaf| leaf.source_id == "src-alpha"),
        "the allowlist is applied inside the query, not after the limit"
    );
    assert!(tree
        .recent_leaves(100, Some(&SourceScope::default()))
        .await
        .expect("an empty allowlist is an answer here too")
        .is_empty());
}

/// One chunk row, ready to be seeded straight into the store.
///
/// The rows in the tests below go in through the store rather than the ingest
/// pipeline, for the reason
/// `the_chunk_count_matches_the_page_and_ignores_its_bounds` gives: what is
/// under test is which rows a predicate matches, and the pipeline decides chunk
/// boundaries, which would make every expected number a property of the chunker
/// instead of the filter.
fn chunk_row(
    id: &str,
    kind: tinymemory_api::chunks::SourceKind,
    source_id: &str,
    timestamp_ms: i64,
) -> tinymemory_api::chunks::Chunk {
    let timestamp = chrono::DateTime::from_timestamp_millis(timestamp_ms).expect("timestamp");
    tinymemory_api::chunks::Chunk {
        id: id.into(),
        content: format!("Body of {id}."),
        metadata: tinymemory_api::chunks::Metadata::point_in_time(
            kind, source_id, "owner", timestamp,
        ),
        token_count: 3,
        seq_in_source: 0,
        created_at: timestamp,
        partial_message: false,
    }
}

/// One canonical entity, ready to be indexed against a node.
fn canonical_entity(
    canonical_id: &str,
    kind: tinymemory_core::engine::backend::store::entity_index::EntityKind,
    surface: &str,
) -> tinymemory_core::engine::backend::store::entity_index::CanonicalEntity {
    tinymemory_core::engine::backend::store::entity_index::CanonicalEntity {
        canonical_id: canonical_id.into(),
        kind,
        surface: surface.into(),
        span_start: 0,
        span_end: surface.len() as u32,
        score: 1.0,
    }
}

/// An entity filter matches a chunk once, however many of the asked-for
/// entities that chunk mentions.
///
/// This is the one filter in the family that reaches a second table, and the
/// obvious way to write it — `INNER JOIN mem_tree_entity_index` — multiplies
/// the chunk row by the number of matching index rows. The host's own SQL
/// carries a `SELECT DISTINCT` and a `COUNT(*)` over a subquery precisely to
/// undo that, and the two are separate spellings of one predicate: drop either
/// and the page and the total disagree, silently, and only for chunks that
/// mention more than one of the filtered entities. A semi-join (`EXISTS`)
/// cannot multiply in the first place, which is why the shared filter builder
/// has to use one.
///
/// So the assertion is not "two rows came back" — a de-duplicating listing
/// passes that with a multiplying count beside it. It is that the same
/// `ChunkQuery` produces the same total through `count_chunks`, through
/// `list_chunks`, and through `list_chunk_details`.
#[tokio::test(flavor = "multi_thread")]
async fn an_entity_filter_matches_a_chunk_once_however_many_entities_it_mentions() {
    use tinymemory_api::chunks::SourceKind;
    use tinymemory_api::provider::{ChunkQuery, MemoryProvider};
    use tinymemory_core::engine::backend::store::entity_index::{CanonicalEntity, EntityKind};

    let workspace = tempfile::tempdir().expect("workspace");
    let provider = provider_over(workspace.path());
    let config = provider_config(workspace.path(), serde_json::Value::Null);

    let seeded = vec![
        chunk_row(
            "ent-both",
            SourceKind::Document,
            "ent-source",
            1_700_000_003_000,
        ),
        chunk_row(
            "ent-one",
            SourceKind::Document,
            "ent-source",
            1_700_000_002_000,
        ),
        chunk_row(
            "ent-none",
            SourceKind::Document,
            "ent-source",
            1_700_000_001_000,
        ),
    ];
    assert_eq!(
        tinymemory_core::store::chunks::store::upsert_chunks(&config, &seeded).expect("seed"),
        seeded.len()
    );
    // `ent-both` mentions two of the two filtered entities; `ent-one` mentions
    // one; `ent-none` mentions an entity of a different kind, so it is in the
    // table but out of both filters.
    let index = |node: &str, entities: &[CanonicalEntity]| {
        tinymemory_core::store::entities::index_entities(
            &config,
            entities,
            node,
            "leaf",
            1_700_000_000_000,
            Some("project"),
        )
        .expect("seed the entity index")
    };
    index(
        "ent-both",
        &[
            canonical_entity("person:alice", EntityKind::Person, "Alice"),
            canonical_entity("person:bob", EntityKind::Person, "Bob"),
        ],
    );
    index(
        "ent-one",
        &[canonical_entity(
            "person:alice",
            EntityKind::Person,
            "Alice",
        )],
    );
    index(
        "ent-none",
        &[canonical_entity(
            "organization:acme",
            EntityKind::Organization,
            "Acme",
        )],
    );

    let chunks = provider.as_chunks().expect("Chunks");
    let by_entity = ChunkQuery {
        entity_ids: vec!["person:alice".into(), "person:bob".into()],
        limit: Some(1_000),
        ..ChunkQuery::default()
    };
    let listed = chunks
        .list_chunks(&by_entity, None)
        .await
        .expect("list by entity");
    assert_eq!(
        listed.iter().filter(|chunk| chunk.id == "ent-both").count(),
        1,
        "a chunk mentioning two of the filtered entities is still one chunk"
    );
    assert_eq!(listed.len(), 2, "ent-none mentions neither");

    let counted = chunks
        .count_chunks(&by_entity, None)
        .await
        .expect("count by entity");
    assert_eq!(
        counted as usize,
        listed.len(),
        "the total must not multiply where the page does not"
    );

    let detailed = chunks
        .list_chunk_details(&by_entity, None)
        .await
        .expect("detail rows by entity");
    assert_eq!(
        detailed.len() as u64,
        counted,
        "the detail listing and the total answer one predicate, not two that \
         happen to agree on the unfiltered case"
    );
    let mut detailed_ids = detailed
        .iter()
        .map(|row| row.chunk.id.as_str())
        .collect::<Vec<_>>();
    detailed_ids.sort_unstable();
    assert_eq!(detailed_ids, vec!["ent-both", "ent-one"]);

    // The kind filter reaches the same table by the same join and is subject
    // to the same multiplication: `ent-both` carries two `person` rows.
    let by_kind = ChunkQuery {
        entity_kinds: vec!["person".into()],
        limit: Some(1_000),
        ..ChunkQuery::default()
    };
    let by_kind_rows = chunks
        .list_chunk_details(&by_kind, None)
        .await
        .expect("detail rows by entity kind");
    assert_eq!(by_kind_rows.len(), 2);
    assert_eq!(
        chunks
            .count_chunks(&by_kind, None)
            .await
            .expect("count by entity kind") as usize,
        by_kind_rows.len()
    );

    // An id nothing was indexed under is an empty page, not every chunk: a
    // filter dropped on the way to the engine reads exactly like no filter.
    let unmatched = ChunkQuery {
        entity_ids: vec!["person:nobody".into()],
        limit: Some(1_000),
        ..ChunkQuery::default()
    };
    assert!(chunks
        .list_chunk_details(&unmatched, None)
        .await
        .expect("list nothing")
        .is_empty());
}

/// The detail row reports the embedding sidecar, and the content filter
/// matches text rather than a pattern.
///
/// Two failures that both look like working code:
///
/// - `has_embedding` read from `mem_tree_chunks.embedding` is `false` for every
///   chunk in a live store. That column is a migration artefact nothing has
///   written since embeddings moved to `mem_tree_chunk_embeddings`, so the
///   host's own SQL — `CASE WHEN c.embedding IS NULL THEN 0 ELSE 1 END` — is a
///   constant `0` wearing a `CASE`. The seeded chunk here has a sidecar row and
///   no legacy blob, which is what every real chunk looks like.
/// - `content_contains` handed to `LIKE` unescaped turns `%` and `_` in the
///   caller's text into wildcards. Nobody notices until a user searches for
///   `100%` or a `snake_case` identifier and gets rows that do not contain
///   what they typed — a false positive, which is the failure a search box
///   cannot recover from.
#[tokio::test(flavor = "multi_thread")]
async fn detail_rows_carry_the_embedding_sidecar_and_match_content_literally() {
    use tinymemory_api::chunks::SourceKind;
    use tinymemory_api::provider::{ChunkQuery, MemoryProvider};

    let workspace = tempfile::tempdir().expect("workspace");
    let provider = provider_over(workspace.path());
    let config = provider_config(workspace.path(), serde_json::Value::Null);

    let mut embedded = chunk_row(
        "lit-embedded",
        SourceKind::Document,
        "lit",
        1_700_000_004_000,
    );
    embedded.content = "100% sure about this".into();
    embedded.metadata.tags = vec!["alpha".into(), "beta".into()];
    let mut spelled_out = chunk_row(
        "lit-spelled",
        SourceKind::Document,
        "lit",
        1_700_000_003_000,
    );
    spelled_out.content = "100 percent sure about this".into();
    let mut underscored = chunk_row("lit-under", SourceKind::Document, "lit", 1_700_000_002_000);
    underscored.content = "the snake_case identifier".into();
    let mut single_char = chunk_row("lit-any", SourceKind::Document, "lit", 1_700_000_001_000);
    single_char.content = "the snakeXcase identifier".into();

    let seeded = vec![embedded, spelled_out, underscored, single_char];
    assert_eq!(
        tinymemory_core::store::chunks::store::upsert_chunks(&config, &seeded).expect("seed"),
        seeded.len()
    );
    tinymemory_core::store::chunks::set_chunk_embedding(&config, "lit-embedded", &[0.5, 0.25])
        .expect("write one embedding sidecar row");

    let chunks = provider.as_chunks().expect("Chunks");
    let everything = ChunkQuery {
        limit: Some(1_000),
        ..ChunkQuery::default()
    };
    let rows = chunks
        .list_chunk_details(&everything, None)
        .await
        .expect("detail rows");
    assert_eq!(rows.len(), seeded.len());
    let embedded_row = rows
        .iter()
        .find(|row| row.chunk.id == "lit-embedded")
        .expect("the embedded chunk is listed");
    assert!(
        embedded_row.has_embedding,
        "a chunk with a row in mem_tree_chunk_embeddings has an embedding, \
         whatever the legacy blob column says"
    );
    assert!(
        rows.iter()
            .filter(|row| row.chunk.id != "lit-embedded")
            .all(|row| !row.has_embedding),
        "and a chunk without one does not"
    );

    // The rest of the row is what makes this member worth a round trip at all:
    // a caller that has to fetch these separately is back to five reads a row.
    assert_eq!(
        embedded_row.chunk.metadata.source_kind,
        SourceKind::Document
    );
    assert_eq!(embedded_row.chunk.metadata.source_id, "lit");
    assert_eq!(embedded_row.chunk.metadata.owner, "owner");
    assert_eq!(
        embedded_row.chunk.metadata.timestamp.timestamp_millis(),
        1_700_000_004_000
    );
    assert_eq!(embedded_row.chunk.token_count, 3);
    assert_eq!(embedded_row.lifecycle_status.as_deref(), Some("admitted"));
    assert_eq!(
        embedded_row.chunk.metadata.tags,
        vec!["alpha".to_string(), "beta".to_string()],
        "tags arrive decoded, not as the stored JSON text"
    );
    assert_eq!(embedded_row.chunk.content, "100% sure about this");
    assert!(
        embedded_row.content_path.is_none(),
        "an inline chunk has no vault path, and the list must not invent one"
    );

    // `%` is a literal. Read as a wildcard it also matches "100 percent sure",
    // which is the row a user searching for "100%" must not be shown.
    let percent = ChunkQuery {
        content_contains: Some("100% sure".into()),
        limit: Some(1_000),
        ..ChunkQuery::default()
    };
    let percent_rows = chunks
        .list_chunk_details(&percent, None)
        .await
        .expect("literal percent");
    assert_eq!(
        percent_rows
            .iter()
            .map(|row| row.chunk.id.as_str())
            .collect::<Vec<_>>(),
        vec!["lit-embedded"],
        "% must not match ' percent'"
    );
    assert_eq!(
        chunks
            .count_chunks(&percent, None)
            .await
            .expect("count literal percent"),
        percent_rows.len() as u64
    );

    // `_` is a literal too. Read as a wildcard it also matches "snakeXcase".
    let underscore = ChunkQuery {
        content_contains: Some("snake_case".into()),
        limit: Some(1_000),
        ..ChunkQuery::default()
    };
    assert_eq!(
        chunks
            .list_chunk_details(&underscore, None)
            .await
            .expect("literal underscore")
            .iter()
            .map(|row| row.chunk.id.as_str())
            .collect::<Vec<_>>(),
        vec!["lit-under"],
        "_ must not match any single character"
    );

    // The id filter is the recall-hydration path: N ids in, those rows out.
    let by_ids = ChunkQuery {
        ids: vec!["lit-under".into(), "lit-any".into()],
        limit: Some(1_000),
        ..ChunkQuery::default()
    };
    let mut hydrated = chunks
        .list_chunk_details(&by_ids, None)
        .await
        .expect("hydrate by id")
        .into_iter()
        .map(|row| row.chunk.id)
        .collect::<Vec<_>>();
    hydrated.sort();
    assert_eq!(
        hydrated,
        vec!["lit-any".to_string(), "lit-under".to_string()]
    );
}

/// Source totals bound sources, count chunks, and apply the scope.
///
/// Three things a caller cannot check for itself. The `limit` bounds the
/// number of *sources* — bound to chunks instead and a store with one busy
/// source returns one row and looks empty. The count is an aggregate over the
/// whole source, not over the page the browser happens to be showing. And the
/// allowlist is the same one `list_chunks` applies: a scoped caller that must
/// not see a source's rows must not learn the source exists from its total
/// either.
#[tokio::test(flavor = "multi_thread")]
async fn source_totals_bound_sources_and_apply_the_scope() {
    use tinymemory_api::chunks::SourceKind;
    use tinymemory_api::provider::types::SourceScope;
    use tinymemory_api::provider::MemoryProvider;

    let workspace = tempfile::tempdir().expect("workspace");
    let provider = provider_over(workspace.path());
    let config = provider_config(workspace.path(), serde_json::Value::Null);

    let mut scoped = chunk_row(
        "tot-scoped",
        SourceKind::Document,
        "mem_src:src-a:item-1",
        1_700_000_500_000,
    );
    scoped.metadata.tags = vec!["memory_sources".into()];
    let seeded = vec![
        chunk_row("tot-a-0", SourceKind::Document, "doc-a", 1_700_000_100_000),
        chunk_row("tot-a-1", SourceKind::Document, "doc-a", 1_700_000_200_000),
        chunk_row("tot-a-2", SourceKind::Document, "doc-a", 1_700_000_300_000),
        chunk_row("tot-b-0", SourceKind::Document, "doc-b", 1_700_000_400_000),
        chunk_row("tot-c-0", SourceKind::Chat, "chat-a", 1_700_000_050_000),
        scoped,
    ];
    assert_eq!(
        tinymemory_core::store::chunks::store::upsert_chunks(&config, &seeded).expect("seed"),
        seeded.len()
    );

    let chunks = provider.as_chunks().expect("Chunks");
    let totals = chunks
        .source_totals(1_000, None)
        .await
        .expect("every source total");
    assert_eq!(totals.len(), 4, "six chunks across four distinct sources");
    assert_eq!(
        totals
            .iter()
            .map(|total| total.source_id.as_str())
            .collect::<Vec<_>>(),
        vec!["mem_src:src-a:item-1", "doc-b", "doc-a", "chat-a"],
        "most recently written source first"
    );
    let busiest = totals
        .iter()
        .find(|total| total.source_id == "doc-a")
        .expect("doc-a is a source");
    assert_eq!(busiest.chunk_count, 3);
    assert_eq!(busiest.most_recent_ms, 1_700_000_300_000);
    assert_eq!(busiest.source_kind, SourceKind::Document);
    // Same source id under two kinds would be two rows; `chat-a` proves the
    // kind is part of the group key rather than decoration on it.
    assert_eq!(
        totals
            .iter()
            .find(|total| total.source_id == "chat-a")
            .expect("chat-a is a source")
            .source_kind,
        SourceKind::Chat
    );

    let bounded = chunks
        .source_totals(2, None)
        .await
        .expect("bounded source totals");
    assert_eq!(
        bounded.len(),
        2,
        "the bound is on sources; two chunks would be one source here"
    );
    assert_eq!(bounded[0].source_id, "mem_src:src-a:item-1");

    // The engine's fail-closed rule, unchanged: `src-b` allows nothing
    // ingested under `src-a`, and content with no source provenance at all is
    // outside the predicate and stays visible.
    let scoped_totals = chunks
        .source_totals(1_000, Some(&SourceScope::new(["src-b"])))
        .await
        .expect("scoped source totals");
    assert_eq!(scoped_totals.len(), 3);
    assert!(
        scoped_totals
            .iter()
            .all(|total| total.source_id != "mem_src:src-a:item-1"),
        "a scoped caller must not learn a forbidden source exists from its total"
    );
}

/// Each `forget_matching` selector removes what it names and nothing else.
///
/// Four selectors over one door, and the door is the whole reason for the
/// shape: the alternative is four members, three of which differ from the
/// others only by which column the predicate reads. What that buys — and what
/// this test exists for — is that the four arms are wired to four different
/// deletes, and a mis-wired arm is silent. An `Owner` routed to the
/// source-prefix delete matches nothing and reports `0`, which reads exactly
/// like "there was nothing to remove"; routed to the exact-source delete it
/// removes the wrong rows and still reports a plausible number.
///
/// The per-chunk arm additionally has to cascade. A bare
/// `DELETE FROM mem_tree_chunks` leaves the entity-index row behind, and the
/// chunk keeps appearing in every entity read that never joins back to the
/// chunk table — a deleted chunk that is still findable by name.
#[tokio::test(flavor = "multi_thread")]
async fn each_forget_selector_removes_what_it_names_and_leaves_its_siblings() {
    use tinymemory_api::chunks::SourceKind;
    use tinymemory_api::error::MemoryError;
    use tinymemory_api::provider::types::ForgetSelector;
    use tinymemory_api::provider::MemoryProvider;
    use tinymemory_core::engine::backend::store::entity_index::EntityKind;

    let workspace = tempfile::tempdir().expect("workspace");
    let provider = provider_over(workspace.path());
    let config = provider_config(workspace.path(), serde_json::Value::Null);

    let mut alice = chunk_row(
        "own-alice",
        SourceKind::Document,
        "owned-a",
        1_700_000_001_000,
    );
    alice.metadata.owner = "alice".into();
    let mut bob = chunk_row(
        "own-bob",
        SourceKind::Document,
        "owned-b",
        1_700_000_002_000,
    );
    bob.metadata.owner = "bob".into();
    let seeded = vec![
        chunk_row(
            "del-0",
            SourceKind::Document,
            "del-source",
            1_700_000_010_000,
        ),
        chunk_row(
            "del-1",
            SourceKind::Document,
            "del-source",
            1_700_000_011_000,
        ),
        chunk_row(
            "del-2",
            SourceKind::Document,
            "del-source",
            1_700_000_012_000,
        ),
        chunk_row(
            "pfx-one",
            SourceKind::Document,
            "pfx:one",
            1_700_000_020_000,
        ),
        chunk_row(
            "pfx-two",
            SourceKind::Document,
            "pfx:two",
            1_700_000_021_000,
        ),
        chunk_row(
            "pfx-other",
            SourceKind::Document,
            "other",
            1_700_000_022_000,
        ),
        alice,
        bob,
    ];
    assert_eq!(
        tinymemory_core::store::chunks::store::upsert_chunks(&config, &seeded).expect("seed"),
        seeded.len()
    );
    assert_eq!(
        tinymemory_core::store::entities::index_entities(
            &config,
            &[canonical_entity(
                "person:carol",
                EntityKind::Person,
                "Carol"
            )],
            "del-1",
            "leaf",
            1_700_000_011_000,
            Some("project"),
        )
        .expect("seed an occurrence on the chunk about to be deleted"),
        1
    );

    let sources = provider.as_sources().expect("Sources");
    let chunks = provider.as_chunks().expect("Chunks");
    let entities = provider.as_entities().expect("Entities");

    // ── One chunk ───────────────────────────────────────────────────────────
    let one = sources
        .forget_matching(&ForgetSelector::Chunk {
            chunk_id: "del-1".into(),
        })
        .await
        .expect("forget one chunk");
    assert_eq!(one.chunks_removed, 1);
    assert_eq!(
        one.trees_cleaned, 0,
        "a chunk id names no source scope, so there is no orphaned tree to \
         report"
    );
    assert!(chunks
        .get_chunk("del-1")
        .await
        .expect("read the deleted chunk")
        .is_none());
    for sibling in ["del-0", "del-2"] {
        assert!(
            chunks
                .get_chunk(sibling)
                .await
                .expect("read a sibling")
                .is_some(),
            "{sibling} shares a source with the deleted chunk and must survive"
        );
    }
    assert!(
        entities
            .entity_chunk_ids("person:carol", 10)
            .await
            .expect("read the occurrence index")
            .is_empty(),
        "the occurrence index must not keep naming a chunk that is gone"
    );
    // Deleting it again is `0`, not an error — the end state is the same.
    assert_eq!(
        sources
            .forget_matching(&ForgetSelector::Chunk {
                chunk_id: "del-1".into(),
            })
            .await
            .expect("forget it twice")
            .chunks_removed,
        0
    );

    // ── One exact source ────────────────────────────────────────────────────
    let source = sources
        .forget_matching(&ForgetSelector::Source {
            source_kind: "document".into(),
            source_id: "del-source".into(),
        })
        .await
        .expect("forget one source");
    assert_eq!(source.chunks_removed, 2, "the two survivors of that source");
    for gone in ["del-0", "del-2"] {
        assert!(chunks
            .get_chunk(gone)
            .await
            .expect("read a removed chunk")
            .is_none());
    }

    // ── A source prefix ─────────────────────────────────────────────────────
    let prefix = sources
        .forget_matching(&ForgetSelector::SourcePrefix {
            source_kind: "document".into(),
            source_id_prefix: "pfx:".into(),
        })
        .await
        .expect("forget a source prefix");
    assert_eq!(prefix.chunks_removed, 2);
    assert!(
        chunks
            .get_chunk("pfx-other")
            .await
            .expect("read the unprefixed chunk")
            .is_some(),
        "the prefix is a prefix, not a substring or a wildcard"
    );

    // ── One owner ───────────────────────────────────────────────────────────
    let owner = sources
        .forget_matching(&ForgetSelector::Owner {
            source_kind: "document".into(),
            owner: "alice".into(),
        })
        .await
        .expect("forget one owner");
    assert_eq!(owner.chunks_removed, 1);
    assert!(chunks
        .get_chunk("own-alice")
        .await
        .expect("read the removed owner's chunk")
        .is_none());
    assert!(
        chunks
            .get_chunk("own-bob")
            .await
            .expect("read the other owner's chunk")
            .is_some(),
        "an owner delete that reached the source column would take bob too"
    );

    // A kind this engine does not store is refused rather than answered with
    // a zero. On a destructive call the two readings send an operator opposite
    // ways: one says "fix the argument", the other says "it was already gone".
    assert!(
        matches!(
            sources
                .forget_matching(&ForgetSelector::Source {
                    source_kind: "not-a-kind".into(),
                    source_id: "del-source".into(),
                })
                .await,
            Err(MemoryError::Invalid(_))
        ),
        "an unrecognised source kind must not read as nothing to remove"
    );
}

/// Purging clears the chunk tier and everything keyed to it, in one call.
///
/// The operation exists because the caller cannot assemble it: the derived
/// tables are keyed by tree ids and job ids a chunk-shaped delete cannot
/// enumerate, so sweeping every source leaves summaries and trees standing over
/// chunks that no longer exist. That half-wiped store is worse than the full
/// one — recall walks a tree whose leaves are gone, and re-ingest is refused by
/// gates whose content was deleted.
///
/// What is pinned is that one call is enough: after it, every read in the
/// contract that touches the tier answers empty, and the reported row count
/// covers the derived rows as well as the chunks. A caller that had to issue
/// this table by table could stop halfway; a driver that wipes table by table
/// outside a transaction can too.
#[tokio::test(flavor = "multi_thread")]
async fn purging_clears_the_chunk_tier_and_everything_keyed_to_it() {
    use tinymemory_api::chunks::SourceKind;
    use tinymemory_api::provider::{ChunkQuery, MemoryProvider};
    use tinymemory_core::engine::backend::store::entity_index::EntityKind;

    let workspace = tempfile::tempdir().expect("workspace");
    let provider = provider_over(workspace.path());
    let config = provider_config(workspace.path(), serde_json::Value::Null);

    let seeded = vec![
        chunk_row(
            "purge-0",
            SourceKind::Document,
            "purge-source",
            1_700_000_001_000,
        ),
        chunk_row(
            "purge-1",
            SourceKind::Document,
            "purge-source",
            1_700_000_002_000,
        ),
        chunk_row("purge-2", SourceKind::Chat, "purge-chat", 1_700_000_003_000),
    ];
    assert_eq!(
        tinymemory_core::store::chunks::store::upsert_chunks(&config, &seeded).expect("seed"),
        seeded.len()
    );
    assert_eq!(
        tinymemory_core::store::entities::index_entities(
            &config,
            &[
                canonical_entity("person:dave", EntityKind::Person, "Dave"),
                canonical_entity("topic:migration", EntityKind::Topic, "migration"),
            ],
            "purge-0",
            "leaf",
            1_700_000_001_000,
            Some("project"),
        )
        .expect("seed the entity index"),
        2
    );

    let chunks = provider.as_chunks().expect("Chunks");
    let entities = provider.as_entities().expect("Entities");
    let maintenance = provider.as_maintenance().expect("Maintenance");
    let everything = ChunkQuery {
        limit: Some(1_000),
        ..ChunkQuery::default()
    };
    assert_eq!(
        chunks
            .count_chunks(&everything, None)
            .await
            .expect("count before"),
        seeded.len() as u64,
        "nothing below means anything if the store was empty to begin with"
    );

    let outcome = maintenance.purge_all().await.expect("purge the store");
    assert!(
        outcome.rows_deleted >= seeded.len() as u64,
        "a purge that reported less than it removed would let a caller tell a \
         user their store was already empty: got {}",
        outcome.rows_deleted
    );

    assert_eq!(
        chunks
            .count_chunks(&everything, None)
            .await
            .expect("count after"),
        0
    );
    assert!(chunks
        .list_chunks(&everything, None)
        .await
        .expect("list after")
        .is_empty());
    assert!(chunks
        .list_chunk_details(&everything, None)
        .await
        .expect("detail rows after")
        .is_empty());
    assert!(chunks
        .source_totals(1_000, None)
        .await
        .expect("source totals after")
        .is_empty());
    assert!(
        entities
            .top_entities(None, 10)
            .await
            .expect("entity index after")
            .is_empty(),
        "a purge that left the occurrence index standing would keep naming \
         entities extracted from chunks that no longer exist"
    );
    assert_eq!(
        maintenance
            .store_stats()
            .await
            .expect("store stats after")
            .chunks,
        0
    );

    // Purging an empty store is a no-op, not a failure: the end state is the
    // one that was asked for.
    assert_eq!(
        maintenance
            .purge_all()
            .await
            .expect("purge again")
            .rows_deleted,
        0
    );
}

/// Occurrences read for many chunks at once stay attached to their own chunk.
///
/// The member takes a set because its callers have one — a page of rows to
/// label, a contacts graph of fifteen hundred nodes. Answering that by looping
/// the single-chunk read is fifteen hundred bus messages, which is why the
/// signature carries the set rather than the caller.
///
/// The regression a batched read invites is the row losing its owner. The index
/// column is `node_id`, and the two ways to drop it are both easy to write and
/// impossible to see afterwards: not selecting it at all and stamping every row
/// with `chunk_ids[0]`, or concatenating per-chunk results and letting the
/// caller assume input order survived. Either way every entity in the page
/// appears to belong to the first chunk in it.
#[tokio::test(flavor = "multi_thread")]
async fn occurrences_read_in_a_batch_stay_attached_to_their_own_chunk() {
    use tinymemory_api::error::MemoryError;
    use tinymemory_api::provider::MemoryProvider;
    use tinymemory_core::engine::backend::store::entity_index::{CanonicalEntity, EntityKind};

    let workspace = tempfile::tempdir().expect("workspace");
    let provider = provider_over(workspace.path());
    let config = provider_config(workspace.path(), serde_json::Value::Null);

    let index = |node: &str, entities: &[CanonicalEntity], at_ms: i64| {
        tinymemory_core::store::entities::index_entities(
            &config,
            entities,
            node,
            "leaf",
            at_ms,
            Some("project"),
        )
        .expect("seed the entity index")
    };
    index(
        "batch-a",
        &[
            canonical_entity("person:erin", EntityKind::Person, "Erin"),
            canonical_entity("organization:acme", EntityKind::Organization, "Acme"),
        ],
        1_700_000_001_000,
    );
    index(
        "batch-b",
        &[canonical_entity(
            "person:frank",
            EntityKind::Person,
            "Frank",
        )],
        1_700_000_002_000,
    );
    // A third chunk nobody asks about, so "returned everything in the table"
    // is distinguishable from "returned what was asked for".
    index(
        "batch-c",
        &[canonical_entity(
            "person:grace",
            EntityKind::Person,
            "Grace",
        )],
        1_700_000_003_000,
    );

    let entities = provider.as_entities().expect("Entities");
    let asked = ["batch-a".to_string(), "batch-b".to_string()];
    let rows = entities
        .chunk_entities(&asked, None)
        .await
        .expect("occurrences for two chunks");
    assert_eq!(rows.len(), 3, "two on the first chunk, one on the second");
    // `Option` rather than an unwrap: an entity missing from the result reads
    // as `None` against the expected chunk instead of panicking out of the
    // closure, so the assertion below names which entity went missing.
    let owner_of = |entity_id: &str| {
        rows.iter()
            .find(|row| row.occurrence.entity_id == entity_id)
            .map(|row| row.chunk_id.as_str())
    };
    assert_eq!(owner_of("person:erin"), Some("batch-a"));
    assert_eq!(owner_of("organization:acme"), Some("batch-a"));
    assert_eq!(
        owner_of("person:frank"),
        Some("batch-b"),
        "a row stamped with the first requested id would say batch-a here"
    );
    assert!(
        rows.iter()
            .all(|row| row.occurrence.entity_id != "person:grace"),
        "only the chunks that were asked about"
    );

    // The kind filter narrows without losing the attachment.
    let people = entities
        .chunk_entities(&asked, Some(&["person".to_string()]))
        .await
        .expect("people only");
    assert_eq!(people.len(), 2);
    assert!(people.iter().all(|row| row.occurrence.kind == "person"));
    assert_eq!(
        people
            .iter()
            .map(|row| row.chunk_id.as_str())
            .collect::<std::collections::HashSet<_>>(),
        ["batch-a", "batch-b"].into_iter().collect()
    );

    // An empty request is an empty answer, not the whole index — and so is a
    // kind filter that admits no kind, which the store would otherwise read as
    // the unfiltered case.
    assert!(entities
        .chunk_entities(&[], None)
        .await
        .expect("no chunks asked about")
        .is_empty());
    assert!(
        entities
            .chunk_entities(&asked, Some(&[]))
            .await
            .expect("a filter admitting no kind is an answer, not a fault")
            .is_empty(),
        "Some(&[]) is not a second spelling of None"
    );

    // And the kind filter is validated rather than applied blindly, for
    // `top_entities`' reason: a misspelled kind that matched nothing would read
    // as "these chunks mention nobody", which is an answer the caller acts on.
    assert!(matches!(
        entities
            .chunk_entities(&asked, Some(&["not-a-kind".to_string()]))
            .await,
        Err(MemoryError::Invalid(_))
    ));
}
