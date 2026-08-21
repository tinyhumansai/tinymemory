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
//! OpenStore(memory_subdir)                          -> object_path
//!
//! Store(namespace, key, content, category, session_id, taint) -> ()
//! Get(namespace, key)                               -> Option<MemoryEntry>
//! Forget(namespace, key)                            -> bool
//! List(namespace, category, session_id)              -> [MemoryEntry]
//! Namespaces()                                      -> [NamespaceSummary]
//! Recall(query, limit, opts, scope)                 -> [MemoryEntry]
//! ExportPage(cursor, limit)                         -> ExportPage
//! ImportRecords(records)                            -> ImportOutcome
//!
//! ListPeople(limit)                                 -> [RankedPerson]
//! GetPerson(person_id)                              -> Option<PersonRecord>
//! ResolveHandle(handle, create_if_missing)          -> Option<ResolvedPerson>
//! AddHandleAlias(person_id, handle)                 -> ()
//! ScorePerson(person_id)                            -> Option<PersonScore>
//! RecordInteraction(interaction)                    -> ()
//! SeedFromAddressBook()                             -> AddressBookSeedOutcome
//!
//! ListChunks(query, scope)                          -> [Chunk]
//! GetChunk(chunk_id)                                -> Option<Chunk>
//! ChunkDetail(chunk_id)                             -> Option<ChunkDetail>
//! ChunkEmbeddings(chunk_ids, model_signature)       -> [ChunkEmbedding]
//! StorageKinds()                                    -> [String]
//!
//! ListActiveFacets() / ListAllFacets()               -> [ProfileFacet]
//! GetFacet(key) / FacetsByType(type)                 -> facet(s)
//! UpsertFacet(facet) / UpsertProviderFacet(…)        -> ()
//! SetFacetUserState(key, state) / DeleteFacet(key)   -> bool
//! DeleteFacetById(id) / DropFacetsBelow(threshold)   -> bool / usize
//! WorkflowIdentityMatches(pattern, value)            -> bool
//!
//! FastRetrieve(query, options, scope)               -> RetrievalResponse
//! CoverWindow(window, scope)                        -> RetrievalResponse
//! SearchEntities(query, kinds, limit)               -> [EntityMatch]
//! RecallNamespaceScored(ns, query, limit, exclude)  -> [NamespaceMemoryHit]
//! RetrieveSource(query, scope)                      -> RetrievalResponse
//! RetrieveChildren(node_id, max_depth, query, limit, scope) -> [RetrievalHit]
//! RetrieveLeaves(chunk_ids, scope)                   -> [RetrievalHit]
//! ```
//!
//! # Source scope crosses as an argument, never as ambient state
//!
//! Every scoped method above takes `scope` explicitly. In-process the engine
//! resolves it from a task-local; that task-local belongs to the *host's* task
//! and does not exist on this side of a bus call. Inferring it here would read
//! as absent, and absent means unrestricted — a source gate failing open.
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

use std::collections::HashMap;
use std::sync::Arc;

// Deliberately the async mutex, not `std::sync::Mutex`: the open path holds
// this guard across an `.await` (see `open_store`), which a std guard cannot
// be held across.
use tokio::sync::Mutex;

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
use tinymemory_api::provider::chunks::{ChunkDetail, ChunkEmbedding, ChunkQuery};
use tinymemory_api::provider::episodic::{ConversationSegment, EpisodicTurn};
use tinymemory_api::provider::people::{
    AddressBookSeedOutcome, PersonHandle, PersonInteraction, PersonRecord, PersonScore,
    RankedPerson, ResolvedPerson,
};
use tinymemory_api::provider::profile::{FacetType, ProfileFacet, UserState};
use tinymemory_api::provider::retrieval::{
    CoverWindowQuery, EntityMatch, FastRetrieveQuery, RetrievalHit, RetrievalResponse,
    SourceRetrievalQuery,
};
use tinymemory_api::provider::MemoryProvider;
use tinymemory_api::recall::OwnedRecallOpts;
use tinymemory_api::tool_memory::ToolMemoryRule;
use tinymemory_api::tree::{IngestRequest, QueryResult, TreeStatus};
use tinymemory_api::types::{
    GraphRelationRecord, MemoryCategory, MemoryEntry, MemoryKvRecord, MemoryTaint,
    NamespaceDocumentInput, NamespaceMemoryHit, NamespaceRetrievalContext, NamespaceSummary,
    StoredMemoryDocument,
};
use tinymemory_api::wire;

#[cfg(not(test))]
#[path = "instrumentation.rs"]
mod instrumentation;
#[cfg(test)]
#[path = "instrumentation_test.rs"]
mod instrumentation;

/// Well-known name exported by the `TinyMemory` module.
pub const BUS_NAME: &str = "ai.tinyhumans.tinymemory.Memory";

/// Object path exported by the `TinyMemory` module.
pub const OBJECT_PATH: &str = "/ai/tinyhumans/tinymemory/Memory";

/// How many stores one module process will open, across every subtree.
///
/// Sized for "a host with per-profile memory", which is the case `OpenStore`
/// exists for — one store per profile, and a host with sixty-four live profiles
/// in one process is already outside what this was built for. It is a backstop
/// against a caller that opens stores in a loop, not a quota anyone should
/// meet.
pub(crate) const MAX_OPEN_STORES: usize = 64;

/// The served object: a bound driver, plus what it needs to open a sibling
/// store on request.
pub(crate) struct MemoryService {
    provider: Arc<dyn MemoryProvider>,
    /// Everything needed to build a second store under a different subtree.
    ///
    /// `None` on the objects that `OpenStore` itself creates: a store opened
    /// this way cannot open further stores. That is not a limitation worth
    /// lifting — the host asks the root object, which knows the workspace — and
    /// it keeps the recursion finite by construction.
    opener: Option<Arc<StoreOpener>>,
}

/// The root object's ability to bring up additional stores under the same
/// workspace.
pub(crate) struct StoreOpener {
    connection: Connection,
    config: crate::config::ModuleConfig,
    /// Subtrees already served, so a second `OpenStore` for the same one
    /// returns the existing object instead of opening the database twice.
    ///
    /// Two live handles to one SQLite file is not a hypothetical problem: the
    /// engine runs migrations on open, and concurrent migration attempts on the
    /// same file are exactly the kind of corruption that is invisible until it
    /// is not.
    ///
    /// The guard is therefore held across the whole open, not just the lookup —
    /// a lock released between the check and the insert would let two callers
    /// through and produce exactly the double-open it is here to prevent. That
    /// is why this is a `tokio::sync::Mutex`.
    served: Mutex<HashMap<String, String>>,
    instrumentation: instrumentation::OpenStoreInstrumentation,
}

impl MemoryService {
    /// Serve `provider` as a leaf object — one store, no opener.
    pub(crate) fn new(provider: Arc<dyn MemoryProvider>) -> Self {
        Self {
            provider,
            opener: None,
        }
    }

    /// Serve `provider` as the root object, able to open sibling stores.
    pub(crate) fn root(provider: Arc<dyn MemoryProvider>, opener: Arc<StoreOpener>) -> Self {
        Self {
            provider,
            opener: Some(opener),
        }
    }
}

impl StoreOpener {
    pub(crate) fn new(connection: Connection, config: crate::config::ModuleConfig) -> Self {
        Self {
            connection,
            config,
            served: Mutex::new(HashMap::new()),
            instrumentation: instrumentation::OpenStoreInstrumentation::default(),
        }
    }
}

/// Object path for a store rooted at `memory_subdir`.
///
/// Derived rather than free-form so a caller cannot name an arbitrary bus path,
/// and sanitised to the characters an object path allows — a subdir reaches
/// this from a profile id, and an id that fails validation must produce a
/// refusal, not a malformed path.
fn object_path_for_subdir(memory_subdir: &str) -> Option<String> {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    if memory_subdir.is_empty()
        || memory_subdir.len() > 128
        || !memory_subdir
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }

    // TinyBus object-path elements accept ASCII alphanumerics and `_`, but a
    // profile id commonly contains `-`. Escape both punctuation characters so
    // the mapping remains injective (`a-b` cannot collide with `a_2db`).
    let mut component = String::with_capacity(memory_subdir.len());
    for byte in memory_subdir.bytes() {
        if byte.is_ascii_alphanumeric() {
            component.push(char::from(byte));
        } else {
            component.push('_');
            component.push(char::from(HEX[usize::from(byte >> 4)]));
            component.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    Some(format!("{OBJECT_PATH}/stores/{component}"))
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
    async fn driver_id(&self) -> BusResult<String> {
        std::future::ready(Ok(self.provider.driver_id().to_string())).await
    }

    /// The families this driver implements.
    ///
    /// The host caches this at bind time, exactly as it would for an in-process
    /// driver — the trait documents that the set is asked once and must not
    /// change afterwards.
    async fn capabilities(&self) -> BusResult<Capabilities> {
        std::future::ready(Ok(self.provider.capabilities())).await
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

    /// Bring up a store rooted at `<workspace>/<memory_subdir>` and return the
    /// object path serving it.
    ///
    /// # Why the module opens stores rather than the host selecting one per call
    ///
    /// A host with per-profile memory needs more than one store in a process.
    /// The alternative was a store selector threaded through every method on
    /// every capability family — a change to the shape of the whole contract,
    /// to express something that is not a property of a memory operation at
    /// all. Which store you are talking to is settled when you are handed a
    /// driver, exactly like which workspace you are bound to.
    ///
    /// So the root object opens stores and hands back object paths. Each is an
    /// ordinary [`MemoryService`] exporting the identical interface, and the
    /// contract does not change at all: `MemoryProvider` still describes one
    /// store, and a proxy still talks to one store.
    ///
    /// Idempotent per subtree — see [`StoreOpener::served`] for why opening the
    /// same database twice is worth going out of the way to avoid.
    async fn open_store(&self, memory_subdir: String) -> BusResult<String> {
        let Some(opener) = self.opener.as_ref() else {
            return Err(BusError::MethodFailed {
                name: "ai.tinyhumans.tinymemory.Error.Invalid".to_string(),
                message: "only the root memory object can open stores".to_string(),
            });
        };
        let Some(path) = object_path_for_subdir(&memory_subdir) else {
            // The subdir is rejected by shape, and the message says so without
            // echoing it: it derives from a profile id, which is user data.
            return Err(BusError::MethodFailed {
                name: "ai.tinyhumans.tinymemory.Error.Invalid".to_string(),
                message: "memory subdirectory is empty, over-long, or contains \
                          characters outside [A-Za-z0-9_-]"
                    .to_string(),
            });
        };

        // The guard is taken here and held to the end of the method, so the
        // check and the insert cannot be split by the open in between. An
        // earlier version dropped it before opening the store, which read as
        // idempotent but was not: two concurrent calls for the same subtree
        // both missed the map, both opened the database, and both ran
        // migrations against one file — the corruption this map exists to
        // prevent, arrived at through the map.
        //
        // It serializes opens of *different* subtrees too. That is accepted
        // rather than worked around: an open happens once per profile, and a
        // per-key lock map costs more complexity than the contention it saves.
        let mut served = opener.served.lock().await;
        if let Some(existing) = served.get(&memory_subdir) {
            log::debug!("[tinymemory:module] open_store reusing already-served subtree");
            return Ok(existing.clone());
        }

        // Each store is a SQLite file, an object path and a set of file
        // descriptors that live until the process exits — nothing here ever
        // closes one, because tinybus does not unserve. A caller that opens a
        // fresh subdir in a loop would therefore exhaust descriptors with no
        // way back short of a restart. The cap is far above any real host (one
        // store per profile) and exists so that a bug is refused by name
        // instead of degrading the whole process.
        if served.len() >= MAX_OPEN_STORES {
            log::error!(
                "[tinymemory:module] open_store refused: already serving {MAX_OPEN_STORES} stores"
            );
            return Err(BusError::MethodFailed {
                name: "ai.tinyhumans.tinymemory.Error.Invalid".to_string(),
                message: format!(
                    "this module already serves the maximum of {MAX_OPEN_STORES} memory stores"
                ),
            });
        }

        opener.instrumentation.record_allocation();
        let client = tinymemory_core::store::factories::create_memory_client_in_subdir(
            &opener.config.memory,
            None,
            "",
            &opener.config.embedding_routes,
            opener.config.storage_provider.as_ref(),
            &opener.config.workspace_dir,
            &memory_subdir,
        )
        .map_err(|error| {
            // Same reasoning as `setup`: the factory error names this process's
            // filesystem layout, which the caller has no business learning.
            log::error!("[tinymemory:module] open_store create store failed: {error}");
            BusError::MethodFailed {
                name: "ai.tinyhumans.tinymemory.Error.Other".to_string(),
                message: "could not open the requested memory store".to_string(),
            }
        })?;

        let provider = crate::provider::provider(&opener.config, Arc::new(client));
        opener.instrumentation.before_registration()?;
        opener
            .connection
            .serve_at(
                path.as_str().try_into()?,
                MemoryService::new(Arc::new(provider)),
            )
            .await?;

        // Recorded only after `serve_at` succeeds, so a failed open is retried
        // rather than caching a path nothing answers on. Both early returns
        // above leave the map untouched for the same reason.
        served.insert(memory_subdir, path.clone());
        log::info!("[tinymemory:module] open_store now serving an additional memory subtree");
        Ok(path)
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

    async fn list_documents(&self, namespace: Option<String>) -> BusResult<serde_json::Value> {
        require_family!(self, as_documents, Capability::Documents)
            .list_documents(namespace.as_deref())
            .await
            .map_err(|error| into_bus_error(&error))
    }

    async fn list_namespaces(&self) -> BusResult<Vec<String>> {
        require_family!(self, as_documents, Capability::Documents)
            .list_namespaces()
            .await
            .map_err(|error| into_bus_error(&error))
    }

    async fn delete_document(
        &self,
        namespace: String,
        document_id: String,
    ) -> BusResult<serde_json::Value> {
        require_family!(self, as_documents, Capability::Documents)
            .delete_document(&namespace, &document_id)
            .await
            .map_err(|error| into_bus_error(&error))
    }

    async fn clear_namespace(&self, namespace: String) -> BusResult<()> {
        require_family!(self, as_documents, Capability::Documents)
            .clear_namespace(&namespace)
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

    async fn recall_documents(
        &self,
        namespace: String,
        limit: usize,
    ) -> BusResult<NamespaceRetrievalContext> {
        let response = require_family!(self, as_documents, Capability::Documents)
            .recall_documents(&namespace, limit)
            .await
            .map_err(|error| into_bus_error(&error))?;
        ensure_response_fits(&response, "RecallDocuments")?;
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

    async fn kv_delete(&self, namespace: Option<String>, key: String) -> BusResult<bool> {
        require_family!(self, as_graph, Capability::Graph)
            .kv_delete(namespace.as_deref(), &key)
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

    // ── People ──────────────────────────────────────────────────────────────

    /// Known people, ranked by closeness.
    ///
    /// Size-checked like the other list-returning methods. `limit` bounds the
    /// *count* but not the bytes — a store of people each carrying many handles
    /// can still overflow a frame — so the ceiling is enforced on the encoded
    /// response rather than trusted to the caller's limit.
    async fn list_people(&self, limit: Option<usize>) -> BusResult<Vec<RankedPerson>> {
        let people = require_family!(self, as_people, Capability::People)
            .list_people(limit)
            .await
            .map_err(|error| into_bus_error(&error))?;
        ensure_response_fits(&people, "ListPeople")?;
        Ok(people)
    }

    async fn get_person(&self, person_id: String) -> BusResult<Option<PersonRecord>> {
        require_family!(self, as_people, Capability::People)
            .get_person(&person_id)
            .await
            .map_err(|error| into_bus_error(&error))
    }

    async fn resolve_handle(
        &self,
        handle: PersonHandle,
        create_if_missing: bool,
    ) -> BusResult<Option<ResolvedPerson>> {
        require_family!(self, as_people, Capability::People)
            .resolve_handle(&handle, create_if_missing)
            .await
            .map_err(|error| into_bus_error(&error))
    }

    async fn add_handle_alias(&self, person_id: String, handle: PersonHandle) -> BusResult<()> {
        require_family!(self, as_people, Capability::People)
            .add_handle_alias(&person_id, &handle)
            .await
            .map_err(|error| into_bus_error(&error))
    }

    async fn score_person(&self, person_id: String) -> BusResult<Option<PersonScore>> {
        require_family!(self, as_people, Capability::People)
            .score_person(&person_id)
            .await
            .map_err(|error| into_bus_error(&error))
    }

    async fn record_interaction(&self, interaction: PersonInteraction) -> BusResult<()> {
        require_family!(self, as_people, Capability::People)
            .record_interaction(&interaction)
            .await
            .map_err(|error| into_bus_error(&error))
    }

    async fn seed_from_address_book(&self) -> BusResult<AddressBookSeedOutcome> {
        require_family!(self, as_people, Capability::People)
            .seed_from_address_book()
            .await
            .map_err(|error| into_bus_error(&error))
    }

    // ── Chunks ──────────────────────────────────────────────────────────────

    /// Chunks matching the query, size-checked.
    ///
    /// `ChunkQuery::limit` bounds rows, not bytes, and a chunk carries full
    /// content — so this is one of the methods where the ceiling matters most.
    async fn list_chunks(
        &self,
        query: ChunkQuery,
        scope: Option<SourceScope>,
    ) -> BusResult<Vec<Chunk>> {
        let chunks = require_family!(self, as_chunks, Capability::Chunks)
            .list_chunks(&query, scope.as_ref())
            .await
            .map_err(|error| into_bus_error(&error))?;
        ensure_response_fits(&chunks, "ListChunks")?;
        Ok(chunks)
    }

    /// One chunk, size-checked.
    ///
    /// A single object is checked for the same reason a list is: the ceiling is
    /// a property of the frame, not of the row count, and one chunk carries
    /// full content with no bound of its own. A list of one that is refused
    /// while the singular read of the same chunk succeeds would be an odd
    /// contract to explain.
    async fn get_chunk(&self, chunk_id: String) -> BusResult<Option<Chunk>> {
        let chunk = require_family!(self, as_chunks, Capability::Chunks)
            .get_chunk(&chunk_id)
            .await
            .map_err(|error| into_bus_error(&error))?;
        ensure_response_fits(&chunk, "GetChunk")?;
        Ok(chunk)
    }

    /// One chunk plus its metadata, size-checked.
    async fn chunk_detail(&self, chunk_id: String) -> BusResult<Option<ChunkDetail>> {
        let detail = require_family!(self, as_chunks, Capability::Chunks)
            .chunk_detail(&chunk_id)
            .await
            .map_err(|error| into_bus_error(&error))?;
        ensure_response_fits(&detail, "ChunkDetail")?;
        Ok(detail)
    }

    async fn storage_kinds(&self) -> BusResult<Vec<String>> {
        require_family!(self, as_chunks, Capability::Chunks)
            .storage_kinds()
            .await
            .map_err(|error| into_bus_error(&error))
    }

    /// Embedding vectors are the largest thing this interface returns.
    ///
    /// A 1536-dimension vector encodes to roughly 10 KiB of JSON, so a few
    /// hundred chunks reach the frame ceiling on their own. Checked for the same
    /// reason `List` is, and refused by name rather than truncated — a short
    /// batch is indistinguishable from "those chunks have no vector".
    async fn chunk_embeddings(
        &self,
        chunk_ids: Vec<String>,
        model_signature: String,
    ) -> BusResult<Vec<ChunkEmbedding>> {
        let embeddings = require_family!(self, as_chunks, Capability::Chunks)
            .chunk_embeddings(&chunk_ids, &model_signature)
            .await
            .map_err(|error| into_bus_error(&error))?;
        ensure_response_fits(&embeddings, "ChunkEmbeddings")?;
        Ok(embeddings)
    }

    // ── Retrieval ───────────────────────────────────────────────────────────

    async fn fast_retrieve(
        &self,
        query: String,
        options: FastRetrieveQuery,
        scope: Option<SourceScope>,
    ) -> BusResult<RetrievalResponse> {
        let response = require_family!(self, as_retrieval, Capability::Retrieval)
            .fast_retrieve(&query, options, scope.as_ref())
            .await
            .map_err(|error| into_bus_error(&error))?;
        ensure_response_fits(&response, "FastRetrieve")?;
        Ok(response)
    }

    async fn cover_window(
        &self,
        window: CoverWindowQuery,
        scope: Option<SourceScope>,
    ) -> BusResult<RetrievalResponse> {
        let response = require_family!(self, as_retrieval, Capability::Retrieval)
            .cover_window(&window, scope.as_ref())
            .await
            .map_err(|error| into_bus_error(&error))?;
        ensure_response_fits(&response, "CoverWindow")?;
        Ok(response)
    }

    // ── Profile ─────────────────────────────────────────────────────────────

    async fn list_active_facets(&self) -> BusResult<Vec<ProfileFacet>> {
        let facets = require_family!(self, as_profile, Capability::Profile)
            .list_active_facets()
            .await
            .map_err(|error| into_bus_error(&error))?;
        ensure_response_fits(&facets, "ListActiveFacets")?;
        Ok(facets)
    }

    async fn list_all_facets(&self) -> BusResult<Vec<ProfileFacet>> {
        let facets = require_family!(self, as_profile, Capability::Profile)
            .list_all_facets()
            .await
            .map_err(|error| into_bus_error(&error))?;
        ensure_response_fits(&facets, "ListAllFacets")?;
        Ok(facets)
    }

    async fn get_facet(&self, key: String) -> BusResult<Option<ProfileFacet>> {
        require_family!(self, as_profile, Capability::Profile)
            .get_facet(&key)
            .await
            .map_err(|error| into_bus_error(&error))
    }

    async fn facets_by_type(&self, facet_type: FacetType) -> BusResult<Vec<ProfileFacet>> {
        let facets = require_family!(self, as_profile, Capability::Profile)
            .facets_by_type(facet_type)
            .await
            .map_err(|error| into_bus_error(&error))?;
        ensure_response_fits(&facets, "FacetsByType")?;
        Ok(facets)
    }

    // ── Episodic ────────────────────────────────────────────────────────────

    /// Record one turn, answering with the row id the engine assigned it.
    async fn insert_turn(&self, turn: EpisodicTurn) -> BusResult<i64> {
        require_family!(self, as_episodic, Capability::Episodic)
            .insert_turn(&turn)
            .await
            .map_err(|error| into_bus_error(&error))
    }

    /// Every recorded turn for one session, oldest first.
    async fn session_turns(&self, session_id: String) -> BusResult<Vec<EpisodicTurn>> {
        let turns = require_family!(self, as_episodic, Capability::Episodic)
            .session_turns(&session_id)
            .await
            .map_err(|error| into_bus_error(&error))?;
        ensure_response_fits(&turns, "SessionTurns")?;
        Ok(turns)
    }

    /// The open segment for a session, if there is one.
    async fn open_segment(&self, session_id: String) -> BusResult<Option<ConversationSegment>> {
        require_family!(self, as_episodic, Capability::Episodic)
            .open_segment(&session_id)
            .await
            .map_err(|error| into_bus_error(&error))
    }

    /// Start a new segment.
    #[allow(
        clippy::too_many_arguments,
        reason = "mirrors `MemoryEpisodic::create_segment`; the service layer must \
                  not reshape a contract signature"
    )]
    async fn create_segment(
        &self,
        segment_id: String,
        session_id: String,
        namespace: String,
        start_episodic_id: i64,
        start_timestamp: f64,
        now: f64,
    ) -> BusResult<()> {
        require_family!(self, as_episodic, Capability::Episodic)
            .create_segment(
                &segment_id,
                &session_id,
                &namespace,
                start_episodic_id,
                start_timestamp,
                now,
            )
            .await
            .map_err(|error| into_bus_error(&error))
    }

    /// Extend a segment to include one more turn.
    async fn append_turn(
        &self,
        segment_id: String,
        episodic_id: i64,
        timestamp: f64,
        now: f64,
    ) -> BusResult<()> {
        require_family!(self, as_episodic, Capability::Episodic)
            .append_turn(&segment_id, episodic_id, timestamp, now)
            .await
            .map_err(|error| into_bus_error(&error))
    }

    /// Mark a segment closed.
    async fn close_segment(&self, segment_id: String, now: f64) -> BusResult<()> {
        require_family!(self, as_episodic, Capability::Episodic)
            .close_segment(&segment_id, now)
            .await
            .map_err(|error| into_bus_error(&error))
    }

    /// Attach a summary to a closed segment.
    async fn set_segment_summary(
        &self,
        segment_id: String,
        summary: String,
        now: f64,
    ) -> BusResult<()> {
        require_family!(self, as_episodic, Capability::Episodic)
            .set_segment_summary(&segment_id, &summary, now)
            .await
            .map_err(|error| into_bus_error(&error))
    }

    /// Store a segment's embedding under `model_signature`.
    async fn upsert_segment_embedding(
        &self,
        segment_id: String,
        model_signature: String,
        embedding: Vec<f32>,
        created_at: f64,
    ) -> BusResult<()> {
        require_family!(self, as_episodic, Capability::Episodic)
            .upsert_segment_embedding(&segment_id, &model_signature, &embedding, created_at)
            .await
            .map_err(|error| into_bus_error(&error))
    }

    async fn upsert_facet(&self, facet: ProfileFacet) -> BusResult<()> {
        require_family!(self, as_profile, Capability::Profile)
            .upsert_facet(&facet)
            .await
            .map_err(|error| into_bus_error(&error))
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "mirrors `MemoryProfile::upsert_provider_facet`; the service layer \
                  must not reshape a contract signature"
    )]
    async fn upsert_provider_facet(
        &self,
        facet_id: String,
        facet_type: FacetType,
        key: String,
        value: String,
        confidence: f64,
        segment_id: Option<String>,
        observed_at: f64,
    ) -> BusResult<()> {
        require_family!(self, as_profile, Capability::Profile)
            .upsert_provider_facet(
                &facet_id,
                facet_type,
                &key,
                &value,
                confidence,
                segment_id.as_deref(),
                observed_at,
            )
            .await
            .map_err(|error| into_bus_error(&error))
    }

    async fn set_facet_user_state(&self, key: String, user_state: UserState) -> BusResult<bool> {
        require_family!(self, as_profile, Capability::Profile)
            .set_facet_user_state(&key, user_state)
            .await
            .map_err(|error| into_bus_error(&error))
    }

    async fn delete_facet(&self, key: String) -> BusResult<bool> {
        require_family!(self, as_profile, Capability::Profile)
            .delete_facet(&key)
            .await
            .map_err(|error| into_bus_error(&error))
    }

    async fn delete_facet_by_id(&self, facet_id: String) -> BusResult<bool> {
        require_family!(self, as_profile, Capability::Profile)
            .delete_facet_by_id(&facet_id)
            .await
            .map_err(|error| into_bus_error(&error))
    }

    async fn drop_facets_below(&self, threshold: f64) -> BusResult<usize> {
        require_family!(self, as_profile, Capability::Profile)
            .drop_facets_below(threshold)
            .await
            .map_err(|error| into_bus_error(&error))
    }

    /// Returns `bool`, not `BusResult<bool>` on the trait — but the wire needs a
    /// result, so an absent family answers `false` rather than erroring, which
    /// is the trait's documented reading of "cannot tell" for this predicate.
    async fn workflow_identity_matches(
        &self,
        key_pattern: String,
        canonical_value: String,
    ) -> BusResult<bool> {
        let Some(profile) = self.provider.as_profile() else {
            return Ok(false);
        };
        Ok(profile
            .workflow_identity_matches(&key_pattern, &canonical_value)
            .await)
    }

    async fn retrieve_source(
        &self,
        query: SourceRetrievalQuery,
        scope: Option<SourceScope>,
    ) -> BusResult<RetrievalResponse> {
        let response = require_family!(self, as_retrieval, Capability::Retrieval)
            .retrieve_source(&query, scope.as_ref())
            .await
            .map_err(|error| into_bus_error(&error))?;
        ensure_response_fits(&response, "RetrieveSource")?;
        Ok(response)
    }

    async fn retrieve_children(
        &self,
        node_id: String,
        max_depth: u32,
        query: Option<String>,
        limit: Option<usize>,
        scope: Option<SourceScope>,
    ) -> BusResult<Vec<RetrievalHit>> {
        let hits = require_family!(self, as_retrieval, Capability::Retrieval)
            .retrieve_children(&node_id, max_depth, query.as_deref(), limit, scope.as_ref())
            .await
            .map_err(|error| into_bus_error(&error))?;
        ensure_response_fits(&hits, "RetrieveChildren")?;
        Ok(hits)
    }

    async fn retrieve_leaves(
        &self,
        chunk_ids: Vec<String>,
        scope: Option<SourceScope>,
    ) -> BusResult<Vec<RetrievalHit>> {
        let hits = require_family!(self, as_retrieval, Capability::Retrieval)
            .retrieve_leaves(&chunk_ids, scope.as_ref())
            .await
            .map_err(|error| into_bus_error(&error))?;
        ensure_response_fits(&hits, "RetrieveLeaves")?;
        Ok(hits)
    }

    async fn recall_namespace_scored(
        &self,
        namespace: String,
        query: String,
        limit: usize,
        exclude_session_id: Option<String>,
    ) -> BusResult<Vec<NamespaceMemoryHit>> {
        let hits = require_family!(self, as_retrieval, Capability::Retrieval)
            .recall_namespace_scored(&namespace, &query, limit, exclude_session_id.as_deref())
            .await
            .map_err(|error| into_bus_error(&error))?;
        ensure_response_fits(&hits, "RecallNamespaceScored")?;
        Ok(hits)
    }

    async fn search_entities(
        &self,
        query: String,
        kinds: Option<Vec<String>>,
        limit: usize,
    ) -> BusResult<Vec<EntityMatch>> {
        let matches = require_family!(self, as_retrieval, Capability::Retrieval)
            .search_entities(&query, kinds.as_deref(), limit)
            .await
            .map_err(|error| into_bus_error(&error))?;
        ensure_response_fits(&matches, "SearchEntities")?;
        Ok(matches)
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
    config: crate::config::ModuleConfig,
) -> BusResult<()> {
    let opener = Arc::new(StoreOpener::new(connection.clone(), config));
    connection
        .serve_at(
            OBJECT_PATH.try_into()?,
            MemoryService::root(provider, opener),
        )
        .await?;
    connection.request_name(BUS_NAME).await?;
    Ok(())
}

#[cfg(test)]
#[path = "test.rs"]
mod test;
