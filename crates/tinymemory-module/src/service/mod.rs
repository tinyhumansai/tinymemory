//! `TinyBus` service boundary for the memory surface.
//!
//! One object, `/ai/tinyhumans/tinymemory/Memory`, exporting every capability
//! family plus the four driver-level methods.
//!
//! ```text
//! DriverId()                                        -> String
//! Capabilities()                                    -> Capabilities
//! Health()                                          -> MemoryHealth
//! Shutdown()                                        -> ()
//!
//! Store(namespace, key, content, category, session_id, taint) -> ()
//! Get(namespace, key)                               -> Option<MemoryEntry>
//! Forget(namespace, key)                            -> bool
//! List(namespace, category, session_id)              -> [MemoryEntry]
//! Namespaces()                                      -> [NamespaceSummary]
//! Recall(query, limit, opts, scope)                 -> [MemoryEntry]
//! ExportPage(cursor, limit)                         -> ExportPage
//! ImportRecords(records)                            -> ImportOutcome
//! ```
//!
//! # Why the method list mirrors a trait exactly
//!
//! These are `tinymemory_api`'s [`MemoryProvider`] and all of its capability
//! traits, with the borrows replaced by owned equivalents. That
//! is deliberate: the host binds an `Arc<dyn MemoryProvider>`, so a host-side
//! client that forwards each method one-for-one is a *complete* provider with no
//! translation layer in between. Anything cleverer — batching, a combined
//! "recall and store" call — would put engine semantics on the wire, where two
//! sides could disagree about them.
//!
//! # Everything travels inline
//!
//! A `TinyBus` frame is JSON capped at 16 MiB. That is a real constraint for a
//! generated document, where a byte array costs about 3.5 bytes per byte, and it
//! is not one here: memory entries are *text*, which costs about 1.1× as JSON.
//! So there is no blob store, no chunking and no held output — the apparatus the
//! `tinydocs` module needs does not appear in this one.
//!
//! Inline does not mean unbounded, though, and the three list-returning methods
//! are not all bounded the same way:
//!
//! - `ExportPage` is paged by contract, with the caller choosing the page size.
//!   Asking for a million records in one page gets an error, correctly.
//! - `Recall` takes a `limit`, so the caller bounds the count — but not the
//!   bytes, since fifty entries each holding a large document still overflow.
//! - `List` takes **neither**. It has no limit and no cursor, so entries can
//!   accumulate across individually valid `Store` calls until the response
//!   cannot cross a frame, and the caller has no way to ask for less.
//!
//! So `List` and `Recall` are checked against [`MAX_RESPONSE_BYTES`] and refuse
//! with a named `BudgetExceeded` rather than truncating. Truncating would be the
//! worse failure: with no cursor, a short list is indistinguishable from a
//! complete one, so a caller would conclude the missing entries do not exist.
//! `Namespaces` is left unchecked — it returns one small summary per namespace,
//! and a host with enough namespaces to fill 16 MiB of summaries has a different
//! problem.
//!
//! # Errors are named, and the names are the contract
//!
//! [`MemoryError`] is a rich enum, but a bus error is a name plus a string. The
//! table that maps between them lives in [`tinymemory_api::wire`] and is used by
//! **both** ends, so the module and the host cannot drift into disagreeing about
//! what a name means. See that module for why there is one name per variant
//! rather than one per outcome class.
//!
//! **No method here logs a namespace key, an entry's content, or a recall
//! query.** All three are user memory content, and a module error must not carry
//! payload values.

use std::sync::Arc;

use tinybus::{Connection, Error as BusError, Result as BusResult};
use tinymemory_api::capabilities::{Capabilities, Capability};
use tinymemory_api::chunks::Chunk;
use tinymemory_api::error::MemoryError;
use tinymemory_api::goals::GoalsDoc;
use tinymemory_api::health::MemoryHealth;
use tinymemory_api::provider::types::{
    DiffReport, EntityHit, ExportPage, ExportRecord, ImportOutcome, IngestItem, IngestOutcome,
    MaintenanceReport, SnapshotRef, SourceItem, SourceScope,
};
// `MemoryCore`, `MemoryRecall` and `MemoryPortability` are deliberately not
// imported: they are supertraits of `MemoryProvider`, so their methods are
// already callable on the trait object.
use tinymemory_api::provider::MemoryProvider;
use tinymemory_api::recall::OwnedRecallOpts;
use tinymemory_api::tool_memory::ToolMemoryRule;
use tinymemory_api::tree::{IngestRequest, QueryResult, TreeStatus};
use tinymemory_api::types::{
    GraphRelationRecord, MemoryCategory, MemoryEntry, MemoryKvRecord, MemoryTaint,
    NamespaceDocumentInput, NamespaceRetrievalContext, NamespaceSummary, StoredMemoryDocument,
};
use tinymemory_api::wire;

/// Well-known name exported by the `TinyMemory` module.
pub const BUS_NAME: &str = "ai.tinyhumans.tinymemory.Memory";

/// Object path exported by the `TinyMemory` module.
pub const OBJECT_PATH: &str = "/ai/tinyhumans/tinymemory/Memory";

/// The served object: a bound driver and nothing else.
pub(crate) struct MemoryService {
    provider: Arc<dyn MemoryProvider>,
}

impl MemoryService {
    /// Serve `provider`.
    pub(crate) fn new(provider: Arc<dyn MemoryProvider>) -> Self {
        Self { provider }
    }
}

macro_rules! require_family {
    ($service:expr, $accessor:ident, $capability:expr) => {
        $service
            .provider
            .$accessor()
            .ok_or_else(|| into_bus_error(&MemoryError::unsupported($capability)))?
    };
}

#[tinybus::interface(name = "ai.tinyhumans.tinymemory.Memory")]
impl MemoryService {
    /// The bound driver's stable identifier.
    #[allow(
        clippy::unused_async,
        reason = "tinybus::interface requires every method to be `async fn`"
    )]
    async fn driver_id(&self) -> BusResult<String> {
        Ok(self.provider.driver_id().to_string())
    }

    /// The families this driver implements.
    ///
    /// The host caches this at bind time, exactly as it would for an in-process
    /// driver — the trait documents that the set is asked once and must not
    /// change afterwards.
    #[allow(
        clippy::unused_async,
        reason = "tinybus::interface requires every method to be `async fn`"
    )]
    async fn capabilities(&self) -> BusResult<Capabilities> {
        Ok(self.provider.capabilities())
    }

    /// Current liveness, as the driver reports it.
    async fn health(&self) -> BusResult<MemoryHealth> {
        Ok(self.provider.health().await)
    }

    /// Release backend resources.
    ///
    /// Idempotent, as the trait requires. Note that this does **not** unload the
    /// module: `TinyBus` never unloads a library, so a host that shuts the
    /// driver down and rebinds gets a fresh engine inside the same mapped image.
    async fn shutdown(&self) -> BusResult<()> {
        self.provider
            .shutdown()
            .await
            .map_err(|error| into_bus_error(&error))
    }

    /// Upsert an entry keyed by `(namespace, key)`.
    ///
    /// `taint` is a required argument rather than a defaulted one, mirroring the
    /// contract: a driver that could default provenance would be able to launder
    /// externally-sourced content into internal-trust content, which is the one
    /// failure mode the host's policy guard exists to prevent.
    async fn store(
        &self,
        namespace: String,
        key: String,
        content: String,
        category: MemoryCategory,
        session_id: Option<String>,
        taint: MemoryTaint,
    ) -> BusResult<()> {
        self.provider
            .store(
                &namespace,
                &key,
                &content,
                category,
                session_id.as_deref(),
                taint,
            )
            .await
            .map_err(|error| into_bus_error(&error))
    }

    /// Fetch the entry at an exact `(namespace, key)`.
    async fn get(&self, namespace: String, key: String) -> BusResult<Option<MemoryEntry>> {
        self.provider
            .get(&namespace, &key)
            .await
            .map_err(|error| into_bus_error(&error))
    }

    /// Delete the entry at `(namespace, key)`, reporting whether it existed.
    async fn forget(&self, namespace: String, key: String) -> BusResult<bool> {
        self.provider
            .forget(&namespace, &key)
            .await
            .map_err(|error| into_bus_error(&error))
    }

    /// List entries, narrowing by namespace, category and session.
    ///
    /// Bounded by [`MAX_RESPONSE_BYTES`]: unlike `Recall` and `ExportPage`, this
    /// method takes no limit and no cursor, so the caller has no way to ask for
    /// less. See [`ensure_response_fits`] for why the answer is a named refusal
    /// rather than a truncation.
    async fn list(
        &self,
        namespace: Option<String>,
        category: Option<MemoryCategory>,
        session_id: Option<String>,
    ) -> BusResult<Vec<MemoryEntry>> {
        let entries = self
            .provider
            .list(
                namespace.as_deref(),
                category.as_ref(),
                session_id.as_deref(),
            )
            .await
            .map_err(|error| into_bus_error(&error))?;
        ensure_response_fits(&entries, "List")?;
        Ok(entries)
    }

    /// Enumerate namespaces with their aggregate counts.
    async fn namespaces(&self) -> BusResult<Vec<NamespaceSummary>> {
        self.provider
            .namespaces()
            .await
            .map_err(|error| into_bus_error(&error))
    }

    /// Ranked retrieval.
    ///
    /// `scope` is a query predicate the driver applies internally, not a filter
    /// the host may apply to the result: narrowing afterwards would let the
    /// driver spend its `limit` on entries the caller is not allowed to see and
    /// then return fewer than it could have.
    async fn recall(
        &self,
        query: String,
        limit: usize,
        opts: OwnedRecallOpts,
        scope: Option<SourceScope>,
    ) -> BusResult<Vec<MemoryEntry>> {
        let entries = self
            .provider
            .recall(&query, limit, &opts, scope.as_ref())
            .await
            .map_err(|error| into_bus_error(&error))?;
        // `limit` bounds the count but not the bytes: a caller asking for 50
        // entries that each hold a large document still overflows a frame.
        ensure_response_fits(&entries, "Recall")?;
        Ok(entries)
    }

    /// Read one page of the export, continuing from `cursor`.
    async fn export_page(&self, cursor: Option<String>, limit: usize) -> BusResult<ExportPage> {
        self.provider
            .export_page(cursor.as_deref(), limit)
            .await
            .map_err(|error| into_bus_error(&error))
    }

    /// Write a batch of previously-exported records.
    ///
    /// Partial success is reported inside [`ImportOutcome`] rather than as an
    /// error, so a million-record restore is not aborted by one bad record.
    async fn import_records(&self, records: Vec<ExportRecord>) -> BusResult<ImportOutcome> {
        self.provider
            .import_records(records)
            .await
            .map_err(|error| into_bus_error(&error))
    }

    async fn ingest_document(&self, item: IngestItem) -> BusResult<IngestOutcome> {
        require_family!(self, as_ingest, Capability::Ingest)
            .ingest_document(item)
            .await
            .map_err(|error| into_bus_error(&error))
    }

    async fn ingest_chat(&self, messages: Vec<IngestItem>) -> BusResult<IngestOutcome> {
        require_family!(self, as_ingest, Capability::Ingest)
            .ingest_chat(messages)
            .await
            .map_err(|error| into_bus_error(&error))
    }

    async fn put_document(&self, input: NamespaceDocumentInput) -> BusResult<String> {
        require_family!(self, as_documents, Capability::Documents)
            .put_document(input)
            .await
            .map_err(|error| into_bus_error(&error))
    }

    async fn get_document(
        &self,
        namespace: String,
        key: String,
    ) -> BusResult<Option<StoredMemoryDocument>> {
        require_family!(self, as_documents, Capability::Documents)
            .get_document(&namespace, &key)
            .await
            .map_err(|error| into_bus_error(&error))
    }

    async fn query_documents(
        &self,
        namespace: String,
        query: String,
        limit: usize,
    ) -> BusResult<NamespaceRetrievalContext> {
        let response = require_family!(self, as_documents, Capability::Documents)
            .query_documents(&namespace, &query, limit)
            .await
            .map_err(|error| into_bus_error(&error))?;
        ensure_response_fits(&response, "QueryDocuments")?;
        Ok(response)
    }

    async fn append(&self, request: IngestRequest) -> BusResult<()> {
        require_family!(self, as_tree, Capability::Tree)
            .append(request)
            .await
            .map_err(|error| into_bus_error(&error))
    }

    async fn query_source(
        &self,
        namespace: String,
        source_id: String,
        limit: usize,
        scope: Option<SourceScope>,
    ) -> BusResult<Vec<Chunk>> {
        let response = require_family!(self, as_tree, Capability::Tree)
            .query_source(&namespace, &source_id, limit, scope.as_ref())
            .await
            .map_err(|error| into_bus_error(&error))?;
        ensure_response_fits(&response, "QuerySource")?;
        Ok(response)
    }

    async fn drill_down(&self, namespace: String, node_id: String) -> BusResult<QueryResult> {
        require_family!(self, as_tree, Capability::Tree)
            .drill_down(&namespace, &node_id)
            .await
            .map_err(|error| into_bus_error(&error))
    }

    async fn seal(&self, namespace: String) -> BusResult<TreeStatus> {
        require_family!(self, as_tree, Capability::Tree)
            .seal(&namespace)
            .await
            .map_err(|error| into_bus_error(&error))
    }

    async fn cascade(&self, namespace: String) -> BusResult<TreeStatus> {
        require_family!(self, as_tree, Capability::Tree)
            .cascade(&namespace)
            .await
            .map_err(|error| into_bus_error(&error))
    }

    async fn entities(
        &self,
        namespace: String,
        query: Option<String>,
        limit: usize,
    ) -> BusResult<Vec<EntityHit>> {
        let response = require_family!(self, as_entities, Capability::Entities)
            .entities(&namespace, query.as_deref(), limit)
            .await
            .map_err(|error| into_bus_error(&error))?;
        ensure_response_fits(&response, "Entities")?;
        Ok(response)
    }

    async fn entity_edges(
        &self,
        namespace: String,
        entity_id: String,
        limit: usize,
    ) -> BusResult<Vec<GraphRelationRecord>> {
        require_family!(self, as_entities, Capability::Entities)
            .entity_edges(&namespace, &entity_id, limit)
            .await
            .map_err(|error| into_bus_error(&error))
    }

    async fn touch_entities(&self, namespace: String, entity_ids: Vec<String>) -> BusResult<()> {
        require_family!(self, as_entities, Capability::Entities)
            .touch_entities(&namespace, &entity_ids)
            .await
            .map_err(|error| into_bus_error(&error))
    }

    async fn kv_get(
        &self,
        namespace: Option<String>,
        key: String,
    ) -> BusResult<Option<MemoryKvRecord>> {
        require_family!(self, as_graph, Capability::Graph)
            .kv_get(namespace.as_deref(), &key)
            .await
            .map_err(|error| into_bus_error(&error))
    }

    async fn kv_put(
        &self,
        namespace: Option<String>,
        key: String,
        value: serde_json::Value,
    ) -> BusResult<()> {
        require_family!(self, as_graph, Capability::Graph)
            .kv_put(namespace.as_deref(), &key, value)
            .await
            .map_err(|error| into_bus_error(&error))
    }

    async fn kv_list(
        &self,
        namespace: Option<String>,
        prefix: Option<String>,
        limit: usize,
    ) -> BusResult<Vec<MemoryKvRecord>> {
        let response = require_family!(self, as_graph, Capability::Graph)
            .kv_list(namespace.as_deref(), prefix.as_deref(), limit)
            .await
            .map_err(|error| into_bus_error(&error))?;
        ensure_response_fits(&response, "KvList")?;
        Ok(response)
    }

    async fn relations(
        &self,
        namespace: Option<String>,
        subject: Option<String>,
        predicate: Option<String>,
        limit: usize,
    ) -> BusResult<Vec<GraphRelationRecord>> {
        let response = require_family!(self, as_graph, Capability::Graph)
            .relations(
                namespace.as_deref(),
                subject.as_deref(),
                predicate.as_deref(),
                limit,
            )
            .await
            .map_err(|error| into_bus_error(&error))?;
        ensure_response_fits(&response, "Relations")?;
        Ok(response)
    }

    async fn put_relation(&self, relation: GraphRelationRecord) -> BusResult<()> {
        require_family!(self, as_graph, Capability::Graph)
            .put_relation(relation)
            .await
            .map_err(|error| into_bus_error(&error))
    }

    async fn capture_snapshot(&self, source_id: String) -> BusResult<SnapshotRef> {
        require_family!(self, as_diff, Capability::Diff)
            .capture_snapshot(&source_id)
            .await
            .map_err(|error| into_bus_error(&error))
    }

    async fn snapshots(&self, source_id: String, limit: usize) -> BusResult<Vec<SnapshotRef>> {
        require_family!(self, as_diff, Capability::Diff)
            .snapshots(&source_id, limit)
            .await
            .map_err(|error| into_bus_error(&error))
    }

    async fn diff(
        &self,
        source_id: String,
        from: Option<String>,
        to: String,
    ) -> BusResult<DiffReport> {
        require_family!(self, as_diff, Capability::Diff)
            .diff(&source_id, from.as_deref(), &to)
            .await
            .map_err(|error| into_bus_error(&error))
    }

    async fn goals(&self) -> BusResult<GoalsDoc> {
        require_family!(self, as_goals, Capability::Goals)
            .goals()
            .await
            .map_err(|error| into_bus_error(&error))
    }

    async fn set_goals(&self, goals: GoalsDoc) -> BusResult<()> {
        require_family!(self, as_goals, Capability::Goals)
            .set_goals(goals)
            .await
            .map_err(|error| into_bus_error(&error))
    }

    async fn tool_rules(&self, tool_name: String) -> BusResult<Vec<ToolMemoryRule>> {
        require_family!(self, as_tool_memory, Capability::ToolMemory)
            .tool_rules(&tool_name)
            .await
            .map_err(|error| into_bus_error(&error))
    }

    async fn put_tool_rule(&self, rule: ToolMemoryRule) -> BusResult<()> {
        require_family!(self, as_tool_memory, Capability::ToolMemory)
            .put_tool_rule(rule)
            .await
            .map_err(|error| into_bus_error(&error))
    }

    async fn delete_tool_rule(&self, tool_name: String, rule_id: String) -> BusResult<bool> {
        require_family!(self, as_tool_memory, Capability::ToolMemory)
            .delete_tool_rule(&tool_name, &rule_id)
            .await
            .map_err(|error| into_bus_error(&error))
    }

    async fn accept_source_items(
        &self,
        source_id: String,
        source_kind: String,
        items: Vec<SourceItem>,
        taint: MemoryTaint,
    ) -> BusResult<IngestOutcome> {
        require_family!(self, as_sources, Capability::Sources)
            .accept_source_items(&source_id, &source_kind, items, taint)
            .await
            .map_err(|error| into_bus_error(&error))
    }

    async fn forget_source(&self, source_id: String) -> BusResult<u64> {
        require_family!(self, as_sources, Capability::Sources)
            .forget_source(&source_id)
            .await
            .map_err(|error| into_bus_error(&error))
    }

    async fn reembed(&self) -> BusResult<MaintenanceReport> {
        require_family!(self, as_maintenance, Capability::Maintenance)
            .reembed()
            .await
            .map_err(|error| into_bus_error(&error))
    }

    async fn compact(&self) -> BusResult<MaintenanceReport> {
        require_family!(self, as_maintenance, Capability::Maintenance)
            .compact()
            .await
            .map_err(|error| into_bus_error(&error))
    }

    async fn consolidate(&self) -> BusResult<MaintenanceReport> {
        require_family!(self, as_maintenance, Capability::Maintenance)
            .consolidate()
            .await
            .map_err(|error| into_bus_error(&error))
    }

    async fn doctor(&self) -> BusResult<MaintenanceReport> {
        require_family!(self, as_maintenance, Capability::Maintenance)
            .doctor()
            .await
            .map_err(|error| into_bus_error(&error))
    }
}

/// The response-size ceiling for a method that returns a list of entries.
///
/// A `TinyBus` frame is JSON capped at 16 MiB. 8 MiB of raw entry content leaves
/// room for the JSON structure around it and for escaping, which can double a
/// pathological string, so a response that passes this check fits with margin.
pub(crate) const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// Refuse a response that would not fit in a frame.
///
/// # Why a refusal and not a truncation
///
/// Truncating would be worse than failing. `List` has no cursor, so a caller
/// receiving a short list has no way to tell it apart from a complete one and no
/// way to ask for the rest — it would conclude those entries do not exist. A
/// named error tells the caller to narrow by namespace, category or session,
/// which is a query it can actually issue.
///
/// # Why `BudgetExceeded` and not a new name
///
/// The name has to be one both ends already agree on, and
/// [`tinymemory_api::wire`] is the table that makes that true. `BudgetExceeded`
/// is what it means — the result exceeded a size budget — and it round-trips to
/// the host as `MemoryError::BudgetExceeded` with no client change. A new name
/// would decode to `Other` on any host older than the module, turning an
/// actionable "narrow your query" into an opaque backend failure.
///
/// # Errors
///
/// [`wire::BUDGET_EXCEEDED`], when the estimate exceeds [`MAX_RESPONSE_BYTES`].
/// The message names the method and the sizes, never entry content.
fn ensure_response_fits<T: serde::Serialize>(response: &T, method: &str) -> BusResult<()> {
    let estimate = serde_json::to_vec(response)
        .map_err(|error| BusError::Protocol(error.to_string()))?
        .len();

    if estimate > MAX_RESPONSE_BYTES {
        log::warn!(
            "[tinymemory:module] {method} refused: response estimated at {estimate} bytes \
             exceeds the {MAX_RESPONSE_BYTES} byte response ceiling"
        );
        return Err(BusError::MethodFailed {
            name: wire::BUDGET_EXCEEDED.to_string(),
            message: format!(
                "{method} would return ~{estimate} bytes, over the \
                 {MAX_RESPONSE_BYTES} byte response ceiling; narrow the query by \
                 namespace, category or session"
            ),
        });
    }
    Ok(())
}

/// Map a [`MemoryError`] onto a named bus error.
///
/// Both the name and the message come from [`tinymemory_api::wire`], which the
/// host's client also uses to map them back. Deriving them here instead would
/// give the contract two definitions free to drift — and the drift that matters
/// is silent: a `PathEscape` arriving as an `Invalid` reclassifies a sandbox
/// escape as a caller mistake.
fn into_bus_error(error: &MemoryError) -> BusError {
    BusError::MethodFailed {
        name: wire::wire_name(error).to_string(),
        message: wire::wire_message(error),
    }
}

/// Serve the memory object and claim the well-known name.
pub(crate) async fn serve(
    connection: &Connection,
    provider: Arc<dyn MemoryProvider>,
) -> BusResult<()> {
    connection
        .serve_at(OBJECT_PATH.try_into()?, MemoryService::new(provider))
        .await?;
    connection.request_name(BUS_NAME).await?;
    Ok(())
}

#[cfg(test)]
#[path = "test.rs"]
mod test;
