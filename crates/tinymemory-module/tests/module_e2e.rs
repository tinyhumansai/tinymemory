//! The real thing: a `dlopen`ed `cdylib`, a real broker, a real store.
//!
//! # Why loader cases are marked `#[ignore]`
//!
//! Not flakiness — a runtime constraint that cannot be worked around inside a
//! single test binary.
//!
//! `Broker::spawn` binds its tasks to whichever tokio runtime created it, and
//! `#[tokio::test]` builds a fresh runtime per test function. The module is
//! loaded once per process and never unloaded (`TinyBus` deliberately never
//! unloads a library), so the second test to drive it finds a broker whose tasks
//! died with the first runtime, and the call **hangs** until some deadline above
//! it fires rather than failing cleanly.
//!
//! So a test that drives a real module must be the only one running in its
//! process. [`all_loader_cases_run_in_isolated_processes`] is part of the normal
//! suite and re-executes this test binary once per ignored loader case. To run a
//! single case manually:
//!
//! ```sh
//! # Both paths are the module's own workspace, not the repo root: this crate is
//! # `exclude`d from the root workspace (see the root Cargo.toml comment), so
//! # `-p tinymemory-module` does not resolve there and the artifact is written
//! # under `crates/tinymemory-module/target`, not `./target`.
//! cargo build --release --manifest-path crates/tinymemory-module/Cargo.toml
//! TINYMEMORY_TEST_MODULE=$PWD/crates/tinymemory-module/target/release/libtinymemory_module.so \
//!   cargo test --manifest-path crates/tinymemory-module/Cargo.toml \
//!   --test module_e2e -- --ignored --exact <one test name>
//! ```
//!
//! `--ignored` alone runs them all in one process and the second will hang. This
//! is the same constraint the `tinywallet` module's loader tests carry.

// All-features coverage builds the linked mode, whose ABI entries are Rust
// symbols. This suite loads the default cdylib by its C symbols instead.
#![cfg(not(feature = "static-link"))]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::cast_precision_loss,
    reason = "test code may panic, and the fake embedder derives a vector from a length"
)]

use tinybus::broker::Broker;
use tinybus::module::ModuleHost;
use tinybus::transport::memory::MemoryBus;
use tinybus::{Connection, Error as BusError, Result as BusResult};
use tinymemory_api::capabilities::{Capabilities, Capability};
use tinymemory_api::types::{MemoryCategory, MemoryEntry, MemoryTaint};
use tinymemory_module::{
    BUS_NAME, CHAT_HOST_BUS_NAME, CHAT_HOST_OBJECT_PATH, EMBEDDING_HOST_BUS_NAME,
    EMBEDDING_HOST_OBJECT_PATH, OBJECT_PATH,
};

/// The interface the module dispatches on.
const MEMORY_INTERFACE: &str = "ai.tinyhumans.tinymemory.Memory";

/// Width of the vectors this fake host returns.
const DIMS: usize = 8;

/// Counts embed calls the module made, across the process.
///
/// A process-global rather than a field because the served object is moved into
/// the connection and there is no handle left to read afterwards. One module per
/// process is already a hard constraint here (see the module docs), so a global
/// is not shared between tests in practice.
static EMBED_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

const LOADER_CASES: &[&str] = &[
    "the_module_advertises_the_complete_tinymemory_api",
    "an_entry_stored_over_the_bus_is_read_back",
    "a_missing_entry_is_none_and_not_an_error",
    "recall_reaches_the_host_embedder",
    "an_export_page_terminates_on_a_none_cursor",
    "a_rejected_request_comes_back_under_its_contract_name",
    "the_module_matches_the_in_process_engine_for_the_same_input",
    "the_manifest_declares_every_method_the_module_serves",
    "what_is_written_lands_in_the_workspace_it_was_given",
    "every_declared_method_is_actually_routed",
    "stateful_optional_families_round_trip_over_the_bus",
    "query_and_maintenance_families_dispatch_typed_requests",
    "a_source_sync_runs_over_the_bus_and_lands_in_the_store",
];

#[test]
fn all_loader_cases_run_in_isolated_processes() {
    let test_binary = std::env::current_exe().expect("current test executable");
    let artifact = std::env::var_os("TINYMEMORY_TEST_MODULE").unwrap_or_else(|| {
        test_binary
            .parent()
            .expect("test executable lives under target/<profile>/deps")
            .join(format!(
                "{}tinymemory_module{}",
                std::env::consts::DLL_PREFIX,
                std::env::consts::DLL_SUFFIX
            ))
            .into_os_string()
    });
    assert!(
        std::path::Path::new(&artifact).is_file(),
        "module artifact does not exist at {}",
        std::path::Path::new(&artifact).display()
    );

    for case in LOADER_CASES {
        let status = std::process::Command::new(&test_binary)
            .args(["--ignored", "--exact", case, "--nocapture"])
            .env("TINYMEMORY_TEST_MODULE", &artifact)
            .status()
            .unwrap_or_else(|error| panic!("could not run {case}: {error}"));
        assert!(status.success(), "isolated loader case {case} failed");
    }
}

/// Stands in for the host's embedder so recall has something to work with.
///
/// Deterministic rather than random: a recall assertion that depended on a
/// random vector would pass or fail for reasons unrelated to the module.
struct HostEmbedder;

struct HostChat;

#[tinybus::interface(name = "ai.tinyhumans.tinymemory.ChatHost")]
impl HostChat {
    async fn complete(
        &self,
        _role: String,
        _request: tinyinference_llm::model::ModelRequest,
    ) -> BusResult<tinyinference_llm::model::ModelResponse> {
        use tinyinference_llm::message::{AssistantMessage, ContentBlock};
        use tinyinference_llm::usage::Usage;

        std::future::ready(()).await;
        Ok(tinyinference_llm::model::ModelResponse {
            message: AssistantMessage {
                id: None,
                content: vec![ContentBlock::Text("deterministic summary".into())],
                tool_calls: Vec::new(),
                usage: Some(Usage::new(2, 1)),
                origin: None,
            },
            usage: Some(Usage::new(2, 1)),
            finish_reason: Some("stop".into()),
            raw: None,
            resolved_model: None,
            continue_turn: None,
            served_from_cache: false,
            correlation: None,
            resolved_route: None,
        })
    }
}

#[tinybus::interface(name = "ai.tinyhumans.tinymemory.EmbeddingHost")]
impl HostEmbedder {
    /// The host's real signature, in the host's order: `provider` first,
    /// because it is what the host selects credential and endpoint by. A fake
    /// declaring the module's old three-argument form passed while the real
    /// host refused every batch at decode (openhuman#5820).
    async fn embed(
        &self,
        provider: String,
        _model: String,
        _dimensions: usize,
        texts: Vec<String>,
    ) -> BusResult<Vec<Vec<f32>>> {
        std::future::ready(()).await;
        EMBED_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // The engine's default embedder must announce itself under a slug the
        // host has a factory arm for; `cloud` is the managed embedder.
        assert_eq!(provider, "cloud", "unknown provider slug on the Embed wire");
        // A crude content-derived vector: enough that identical text embeds
        // identically and different text does not, which is all recall needs
        // here.
        Ok(texts
            .iter()
            .map(|text| {
                let seed = text.len() as f32;
                (0..DIMS)
                    .map(|index| (seed + index as f32).sin())
                    .collect::<Vec<f32>>()
            })
            .collect())
    }
}

/// Load the module, serve the host embedder, and hand back a client connection
/// together with the admitted `ModuleInfo`.
///
/// The returned `ModuleHost` and broker task must be kept alive by the caller:
/// dropping the host is what would release the module's transport.
///
/// The manifest is only observable through a real admission — it is produced by
/// the `cdylib`'s exported `tinybus_module_manifest_v1` and parsed by the host —
/// so a test that wants to inspect the declared surface has to go through here.
/// Most tests do not, and use [`admit_module`] instead.
async fn admit_module_detailed(
    workspace: &std::path::Path,
) -> (
    Connection,
    ModuleHost,
    tokio::task::JoinHandle<BusResult<()>>,
    tinybus::module::ModuleInfo,
) {
    let artifact = std::env::var_os("TINYMEMORY_TEST_MODULE")
        .expect("TINYMEMORY_TEST_MODULE must point at the built cdylib");

    let bus = MemoryBus::new();
    let broker = Broker::new();
    let broker_task = broker.spawn(bus.clone());

    // The host's half: serve the embedder *before* loading the module, because
    // the module builds its store during initialization and a store built
    // without a reachable embedder would bind the inert provider.
    let host_side = Connection::connect(bus.connect().await.expect("host transport"))
        .await
        .expect("host connection");
    host_side
        .serve_at(
            EMBEDDING_HOST_OBJECT_PATH.try_into().expect("valid path"),
            HostEmbedder,
        )
        .await
        .expect("serve embedder");
    host_side
        .request_name(EMBEDDING_HOST_BUS_NAME)
        .await
        .expect("claim embedder name");
    host_side
        .serve_at(
            CHAT_HOST_OBJECT_PATH.try_into().expect("valid chat path"),
            HostChat,
        )
        .await
        .expect("serve chat host");
    host_side
        .request_name(CHAT_HOST_BUS_NAME)
        .await
        .expect("claim chat host name");
    // Deliberately leaked: dropping this releases the well-known name, and the
    // module needs it for the whole test.
    std::mem::forget(host_side);

    let modules = ModuleHost::new(broker);
    // The `memory` block is set explicitly rather than left to default, because
    // the engine sizes its vector store from `memory.embedding_dimensions` (1024
    // by default) while the bus provider reports the `cloud_embedding_dimensions`
    // above. Left mismatched, the store and the embedder disagree about the width
    // of the space and recall returns nothing — which is exactly the failure the
    // provider's width check exists to make loud, so the two are pinned equal
    // here on purpose.
    //
    // `min_relevance_score` is dropped to 0 because the fake embedder's vectors
    // are content-derived noise, not real semantics; the default 0.4 floor would
    // filter out a correct match for reasons that have nothing to do with the
    // module.
    let diff_source = workspace.join("diff-source");
    std::fs::create_dir_all(&diff_source).expect("create diff source fixture");
    let config = serde_json::json!({
        "workspace_dir": workspace,
        "cloud_embedding_model": "e2e-model",
        "cloud_embedding_dimensions": DIMS,
        "models_supporting_dimensions": ["e2e-model"],
        "memory_sources": [{
            "id": "src_diff",
            "kind": "folder",
            "label": "Diff source",
            "enabled": true,
            "path": diff_source,
        }],
        "memory": {
            "embedding_provider": "cloud",
            "embedding_model": "e2e-model",
            "embedding_dimensions": DIMS,
            "min_relevance_score": 0.0,
        },
    });

    let loaded = modules
        .load_file_with_config(&artifact, config)
        .expect("module should load");
    assert_eq!(loaded.name, "tinymemory-module");
    assert_eq!(loaded.manifest.bus_name.as_str(), BUS_NAME);
    assert_eq!(loaded.manifest.object_path.as_str(), OBJECT_PATH);

    let client = Connection::connect(bus.connect().await.expect("client transport"))
        .await
        .expect("client connection");
    (client, modules, broker_task, loaded)
}

/// Load the module under a fresh workspace and return a client connection to it.
async fn admit_module(
    workspace: &std::path::Path,
) -> (
    Connection,
    ModuleHost,
    tokio::task::JoinHandle<BusResult<()>>,
) {
    let (client, host, task, _info) = admit_module_detailed(workspace).await;
    (client, host, task)
}

fn proxy(connection: &Connection) -> tinybus::Proxy {
    connection
        .proxy(BUS_NAME, OBJECT_PATH, MEMORY_INTERFACE)
        .expect("proxy")
}

#[tokio::test]
#[ignore = "drives a real dlopen'ed module; must be the only such test in the process — see the module docs"]
async fn the_module_advertises_the_complete_tinymemory_api() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let (client, _host, _task) = admit_module(workspace.path()).await;

    let capabilities: Capabilities = proxy(&client)
        .call("Capabilities", ())
        .await
        .expect("Capabilities");

    assert_eq!(
        capabilities,
        Capabilities::all(),
        "the compiled module must own every TinyMemory capability family"
    );
    for mandatory in Capability::MANDATORY {
        assert!(
            capabilities.contains(mandatory),
            "{mandatory:?} must be advertised"
        );
    }

    let driver_id: String = proxy(&client).call("DriverId", ()).await.expect("DriverId");
    assert_eq!(driver_id, "tinycortex");
}

#[tokio::test]
#[ignore = "drives a real dlopen'ed module; must be the only such test in the process — see the module docs"]
async fn an_entry_stored_over_the_bus_is_read_back() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let (client, _host, _task) = admit_module(workspace.path()).await;
    let bus = proxy(&client);

    bus.call::<()>(
        "Store",
        (
            "e2e",
            "greeting",
            "the cat sat on the mat",
            MemoryCategory::Core,
            Option::<String>::None,
            MemoryTaint::default(),
        ),
    )
    .await
    .expect("Store");

    let entry: Option<MemoryEntry> = bus.call("Get", ("e2e", "greeting")).await.expect("Get");
    let entry = entry.expect("the entry was just stored");
    assert_eq!(entry.content, "the cat sat on the mat");

    // Idempotent by contract: forgetting reports whether it existed, and a
    // second forget is `false` rather than an error.
    let forgotten: bool = bus
        .call("Forget", ("e2e", "greeting"))
        .await
        .expect("Forget");
    assert!(forgotten);
    let again: bool = bus
        .call("Forget", ("e2e", "greeting"))
        .await
        .expect("Forget is idempotent");
    assert!(!again);
}

#[tokio::test]
#[ignore = "drives a real dlopen'ed module; must be the only such test in the process — see the module docs"]
async fn a_missing_entry_is_none_and_not_an_error() {
    // `get`'s contract: absence is `Ok(None)`. A host that received an error here
    // would surface a failure for an ordinary cache miss.
    let workspace = tempfile::tempdir().expect("tempdir");
    let (client, _host, _task) = admit_module(workspace.path()).await;

    let entry: Option<MemoryEntry> = proxy(&client)
        .call("Get", ("e2e", "never-written"))
        .await
        .expect("a miss is not an error");
    assert!(entry.is_none());
}

#[tokio::test]
#[ignore = "drives a real dlopen'ed module; must be the only such test in the process — see the module docs"]
async fn recall_reaches_the_host_embedder() {
    // The load-bearing test, and it asserts the seam rather than the ranking.
    //
    // What this module is responsible for is that the engine inside it resolves
    // its embedder to the bus and calls out to the host. Whether a given query
    // then *ranks* a given entry above `min_relevance_score` is engine retrieval
    // behaviour, tuned by chunking, the vector store and the relevance floor —
    // none of which this port changes, and all of which would make this test fail
    // for reasons unrelated to the boundary. `tinycortex` covers that.
    //
    // So the assertion is on the host embedder's call count. That can only be
    // non-zero if the module built its store against `BusEmbeddingHost`, the
    // engine asked it to embed, the request crossed the bus, and the reply passed
    // the width check.
    let workspace = tempfile::tempdir().expect("tempdir");
    let (client, _host, _task) = admit_module(workspace.path()).await;
    let bus = proxy(&client);

    bus.call::<()>(
        "Store",
        (
            "e2e",
            "fact",
            "the deployment runs on port 7788",
            MemoryCategory::Core,
            Option::<String>::None,
            MemoryTaint::default(),
        ),
    )
    .await
    .expect("Store");

    // Must not error: a recall that reached the bus and got a bad-width reply
    // would fail here, which is the negative half of the same property.
    let _entries: Vec<MemoryEntry> = bus
        .call(
            "Recall",
            (
                "the deployment runs on port 7788",
                5_usize,
                tinymemory_api::recall::OwnedRecallOpts::default(),
                Option::<tinymemory_api::provider::types::SourceScope>::None,
            ),
        )
        .await
        .expect("Recall must succeed, not merely return nothing");

    assert!(
        EMBED_CALLS.load(std::sync::atomic::Ordering::SeqCst) > 0,
        "the engine inside the module never asked the host to embed, so its \
         embedder is not wired to the bus"
    );
}

#[tokio::test]
#[ignore = "drives a real dlopen'ed module; must be the only such test in the process — see the module docs"]
async fn an_export_page_terminates_on_a_none_cursor() {
    // An empty `records` vector is explicitly *not* a terminator — a driver may
    // return an empty page while skipping a range — so a host must key on the
    // cursor. This pins that the module reports it the same way.
    let workspace = tempfile::tempdir().expect("tempdir");
    let (client, _host, _task) = admit_module(workspace.path()).await;
    let bus = proxy(&client);

    bus.call::<()>(
        "Store",
        (
            "e2e",
            "exported",
            "content worth keeping",
            MemoryCategory::Core,
            Option::<String>::None,
            MemoryTaint::default(),
        ),
    )
    .await
    .expect("Store");

    let mut cursor: Option<String> = None;
    let mut seen = 0_usize;
    // Bounded so a driver that never terminates fails the test instead of
    // hanging it.
    for _ in 0..32 {
        let page: tinymemory_api::provider::types::ExportPage = bus
            .call("ExportPage", (cursor.clone(), 16_usize))
            .await
            .expect("ExportPage");
        seen += page.records.len();
        cursor = page.next_cursor.clone();
        if cursor.is_none() {
            break;
        }
    }
    assert!(cursor.is_none(), "the export never terminated");
    assert!(seen >= 1, "the stored entry should appear in the export");
}

#[tokio::test]
#[ignore = "drives a real dlopen'ed module; must be the only such test in the process — see the module docs"]
async fn a_rejected_request_comes_back_under_its_contract_name() {
    // The error name is the contract, and the host reconstructs a `MemoryError`
    // variant from it. This asserts a real refusal carries a name from the
    // `tinymemory` table rather than a bare transport failure.
    let workspace = tempfile::tempdir().expect("tempdir");
    let (client, _host, _task) = admit_module(workspace.path()).await;

    // A method the module does not serve. This is the one refusal that is
    // guaranteed regardless of engine behaviour, which is what makes it worth
    // asserting here: it proves the served object rejects an unknown member
    // rather than hanging or answering something.
    //
    // Note what this deliberately does *not* claim. The refusal comes from the
    // bus's dispatch layer, so its name is tinybus's, not one from the contract
    // table — asserting a `ai.tinyhumans.tinymemory.Error.*` name here would be
    // asserting the wrong thing. The contract table's own mapping is covered
    // exhaustively and deterministically in `service::test`, where every
    // `MemoryError` variant is reachable by construction. An earlier revision
    // tried to provoke a contract error through `ExportPage` with a zero limit;
    // since a driver that accepts a zero limit is equally legitimate, that test
    // asserted nothing whenever it passed.
    let outcome: Result<serde_json::Value, _> = proxy(&client).call("NoSuchMethod", ()).await;

    let error = outcome.expect_err("an unknown member must be refused");
    let name = error.wire_name();
    assert!(
        !name.is_empty(),
        "a refusal must carry a wire name, got {error:?}"
    );
}

#[tokio::test]
#[ignore = "drives a real dlopen'ed module; must be the only such test in the process — see the module docs"]
async fn the_module_matches_the_in_process_engine_for_the_same_input() {
    // The port must not change behaviour. Store the same entry through the module
    // and assert the read-back is byte-identical to what the entry went in as,
    // which is the property a host depends on when it swaps an embedded driver
    // for this one.
    let workspace = tempfile::tempdir().expect("tempdir");
    let (client, _host, _task) = admit_module(workspace.path()).await;
    let bus = proxy(&client);

    let content =
        "unicode survives: \u{e9}\u{4e2d}\u{6587} \u{1f600} and \"quotes\" and \\slashes\\";
    bus.call::<()>(
        "Store",
        (
            "e2e",
            "roundtrip",
            content,
            MemoryCategory::Custom("notes".to_string()),
            Some("session-1".to_string()),
            MemoryTaint::default(),
        ),
    )
    .await
    .expect("Store");

    let entry: Option<MemoryEntry> = bus.call("Get", ("e2e", "roundtrip")).await.expect("Get");
    let entry = entry.expect("stored");
    assert_eq!(
        entry.content, content,
        "JSON transport must not alter content"
    );

    // The custom category's `custom:` wire prefix has to survive too: without it
    // `Custom("core")` and `Core` would collide.
    let listed: Vec<MemoryEntry> = bus
        .call(
            "List",
            (
                Some("e2e".to_string()),
                Some(MemoryCategory::Custom("notes".to_string())),
                Option::<String>::None,
            ),
        )
        .await
        .expect("List");
    assert!(
        listed.iter().any(|found| found.key == "roundtrip"),
        "a custom category must round-trip through the wire form"
    );
}

/// Not `#[ignore]`d: it loads nothing and so is safe alongside the suite.
#[test]
fn the_routing_constants_are_the_ones_the_host_dials() {
    // Only the constants. What the manifest actually declares is checked against
    // a real admission in `the_manifest_declares_every_method_the_module_serves`
    // below — these two used to be one test, and the constants alone cannot
    // catch a method missing from the manifest.
    assert_eq!(BUS_NAME, "ai.tinyhumans.tinymemory.Memory");
    assert_eq!(OBJECT_PATH, "/ai/tinyhumans/tinymemory/Memory");
    assert_eq!(MEMORY_INTERFACE, BUS_NAME);
}

/// Every method the service dispatches, as the manifest must declare it.
///
/// Kept beside the assertion rather than derived: the `module_export!` macro
/// takes string literals, so there is no constant for a test to share with it.
/// This list is therefore the second opinion — if the two disagree, one of them
/// is wrong and the test says which names differ.
const EXPECTED_METHODS: &[&str] = &[
    "DriverId",
    "Capabilities",
    "Health",
    "Shutdown",
    "OpenStore",
    "InsertTurn",
    "SessionTurns",
    "OpenSegment",
    "CreateSegment",
    "AppendTurn",
    "CloseSegment",
    "SetSegmentSummary",
    "UpsertSegmentEmbedding",
    "InsertEvent",
    "Store",
    "Get",
    "Forget",
    "List",
    "Namespaces",
    "Recall",
    "ExportPage",
    "ImportRecords",
    // People.
    "ListPeople",
    "GetPerson",
    "ResolveHandle",
    "AddHandleAlias",
    "ScorePerson",
    "RecordInteraction",
    "SeedFromAddressBook",
    // Chunks.
    "ListChunks",
    "GetChunk",
    "ChunkDetail",
    "StorageKinds",
    "ChunkEmbeddings",
    "CountChunks",
    "ListChunkDetails",
    "SourceTotals",
    // Retrieval.
    "FastRetrieve",
    "CoverWindow",
    "RetrieveSource",
    "RetrieveChildren",
    "RetrieveLeaves",
    "RecallNamespaceScored",
    "SearchEntities",
    // Profile.
    "ListActiveFacets",
    "ListAllFacets",
    "GetFacet",
    "FacetsByType",
    "UpsertFacet",
    "UpsertProviderFacet",
    "SetFacetUserState",
    "DeleteFacet",
    "DeleteFacetById",
    "DropFacetsBelow",
    "WorkflowIdentityMatches",
    "IngestDocument",
    "IngestChat",
    "IngestEmail",
    "PutDocument",
    "GetDocument",
    "ListDocuments",
    "ListNamespaces",
    "DeleteDocument",
    "ClearNamespace",
    "QueryDocuments",
    "RecallDocuments",
    "Append",
    "QuerySource",
    "DrillDown",
    "Seal",
    "Cascade",
    "Entities",
    "EntityEdges",
    "TouchEntities",
    "TopEntities",
    "ChunkEntities",
    "EntityChunkIds",
    "KvGet",
    "KvPut",
    "KvDelete",
    "KvList",
    "Relations",
    "PutRelation",
    "CaptureSnapshot",
    "Snapshots",
    "Diff",
    "Goals",
    "SetGoals",
    "ToolRules",
    "PutToolRule",
    "DeleteToolRule",
    "AcceptSourceItems",
    "ForgetSource",
    "ForgetMatching",
    "Reembed",
    "Compact",
    "Consolidate",
    "Doctor",
    "RetryFailed",
    "StoreStats",
    "QueueStats",
    "LatestQueueFailure",
    "BackfillInProgress",
    "FlushPending",
    // openhuman#6186. Set-compared like the member below, so it files with its
    // family rather than at the tail.
    "SegmentsPendingSummary",
    // openhuman#6012. Compared as a set (`BTreeSet`), so this sits with its
    // family rather than at the tail — unlike `tinymemory_bus::METHODS` and the
    // `#[tinybus::interface]` impl block, which are positional and append-only.
    "BackfillConnectorTrees",
    "ResetDerivedIndex",
    "PurgeAll",
    "RecallNamespaceRecent",
    "SummaryForest",
    "RecentLeaves",
    "FlushSourceTree",
    "Diagnose",
    "RunConnectionSync",
    "RunSourceSync",
    "BootstrapConnection",
    "IsToolkitSyncable",
    "SourceSyncState",
    "SyncAuditLog",
    "EstimateSyncCostUsd",
    "SyncStatuses",
    "RawArchiveCoverage",
    "RebuildFromRawArchive",
    "CodingSessionStatus",
    "IngestCodingSessions",
    // Scoring family.
    "ExtractEntities",
    "EmbedText",
    "EmbedderSlug",
    // The tree family's summariser door and its root read.
    "Summarise",
    "RootSummaries",
    // The maintenance hot-path read and the two chunk-family doors that
    // landed beside it.
    "DegradedState",
    "ChunkScore",
    "SourceIngestStatus",
    // The runtime-tree doors and the flavoured-root profile read — the final
    // round of the engine shed.
    "RuntimeBufferWrite",
    "RuntimeReadNode",
    "RuntimeReadChildren",
    "RuntimeTreeStatus",
    "RuntimeSummarize",
    "RuntimeRebuild",
    "FlavourProfile",
    // Granular ingestion and agentic retrieval, appended after all previously
    // released wire slots.
    "IngestLearning",
    "IngestEvent",
    "Answer",
    // Scheduler-gate round: appended at the wire tail with its declaration.
    "OverrideSchedulerGate",
];

#[tokio::test]
#[ignore = "drives a real dlopen'ed module; must be the only such test in the process — see the module docs"]
async fn the_manifest_declares_every_method_the_module_serves() {
    // The manifest's `methods` list is admission surface: a host can be refused
    // a method the module actually serves, or admitted for one it does not.
    //
    // This inspects the manifest the loaded artifact really exported, which is
    // the only way to see it — the list is baked into `tinybus_module_manifest_v1`
    // by the macro and parsed by the host during admission. Comparing routing
    // constants, as an earlier revision did, passes with `ImportRecords` missing
    // from the declaration entirely.
    let workspace = tempfile::tempdir().expect("tempdir");
    let (_client, _host, _task, info) = admit_module_detailed(workspace.path()).await;

    let provided = info
        .manifest
        .provides
        .iter()
        .find(|interface| interface.version.interface.as_str() == MEMORY_INTERFACE)
        .expect("the memory interface must be declared");

    let declared: std::collections::BTreeSet<&str> = provided
        .methods
        .iter()
        .map(tinybus::MemberName::as_str)
        .collect();
    let expected: std::collections::BTreeSet<&str> = EXPECTED_METHODS.iter().copied().collect();

    assert_eq!(
        declared,
        expected,
        "manifest methods drifted; missing={:?} unexpected={:?}",
        expected.difference(&declared).collect::<Vec<_>>(),
        declared.difference(&expected).collect::<Vec<_>>()
    );
}

#[tokio::test]
#[ignore = "drives a real dlopen'ed module; must be the only such test in the process — see the module docs"]
async fn what_is_written_lands_in_the_workspace_it_was_given() {
    // Issue #18 §E5 asks for a shutdown/restart cycle. It cannot be written
    // here, and the reason is structural rather than an omission: TinyBus never
    // unloads a library, so a second admission in the same process is refused —
    // `ModuleRefused { reason: "module initialization failed" }` — and every
    // test in this file must be the only one in its process for the same
    // reason. A genuine restart needs a second *process* against a shared
    // workspace, which is a change to the CI loop rather than a test.
    //
    // What is assertable in one process is the property that restart would be
    // checking: that a write goes to the durable workspace the host supplied,
    // and not somewhere that disappears. A module that stored into a temporary
    // directory of its own, or in memory, passes every other test in this file
    // — each one stores and reads back inside a single admission, so the
    // difference never shows. It would show on the user's next launch.
    let workspace = tempfile::tempdir().expect("tempdir");

    // Nothing has been asked of it yet.
    let before = std::fs::read_dir(workspace.path())
        .expect("workspace readable")
        .count();

    let (client, _host, _task) = admit_module(workspace.path()).await;
    proxy(&client)
        .call::<()>(
            "Store",
            (
                "e2e",
                "durable",
                "written to the host's workspace",
                MemoryCategory::Core,
                Option::<String>::None,
                MemoryTaint::default(),
            ),
        )
        .await
        .expect("Store");

    let entry: Option<MemoryEntry> = proxy(&client)
        .call("Get", ("e2e", "durable"))
        .await
        .expect("Get");
    assert_eq!(
        entry.expect("just stored").content,
        "written to the host's workspace"
    );

    // The assertion that a same-admission round trip cannot make: the bytes are
    // in the directory the host named.
    let after = std::fs::read_dir(workspace.path())
        .expect("workspace readable")
        .count();
    assert!(
        after > before,
        "the module answered correctly but wrote nothing into the workspace it \
         was given — a store that does not land here does not survive a restart"
    );
}

#[tokio::test]
#[ignore = "drives a real dlopen'ed module; must be the only such test in the process — see the module docs"]
async fn every_declared_method_is_actually_routed() {
    // Issue #18 §E5 asks the E2E to cover every family the module advertises.
    // It advertises `Capabilities::all()` — twenty families — and the tests
    // above exercise three of them.
    //
    // Rather than twenty bespoke round trips, this asserts the property that
    // makes the advertisement honest at this layer: every method the manifest
    // declares is actually *reachable*. `the_manifest_declares_every_method_the
    // _module_serves` compares two lists and would pass for a method that is
    // declared, routed, and answers "unknown member" — which is the bus-level
    // version of a capability set that overstates its accessors.
    //
    // Each method is called with no arguments, so most fail. That is fine and is
    // the point: what is asserted is the *kind* of failure. A method that is
    // wired rejects the arguments; a method that is not wired rejects the
    // member, and those carry different wire names.
    let workspace = tempfile::tempdir().expect("tempdir");
    let (client, _host, _task) = admit_module(workspace.path()).await;

    // Establish the shape of a genuine not-routed refusal from this very build,
    // rather than hard-coding tinybus's spelling of it. The two outcomes are
    // distinct names — `ai.tinyhumans.tinybus.Error.UnknownMethod` for a member
    // that is not there, `…Error.BadArguments` for one that is and did not like
    // the empty argument list — which is what makes this test discriminate
    // rather than pass vacuously.
    let unrouted = proxy(&client)
        .call::<serde_json::Value>("NoSuchMethodAtAll", ())
        .await
        .expect_err("an unknown member must be refused")
        .wire_name()
        .to_string();

    let mut missing = Vec::new();
    for method in EXPECTED_METHODS {
        // `Shutdown` would stop the module and strand every later iteration.
        if *method == "Shutdown" {
            continue;
        }
        let outcome = proxy(&client).call::<serde_json::Value>(method, ()).await;
        if let Err(error) = outcome {
            if error.wire_name() == unrouted {
                missing.push(*method);
            }
        }
    }

    assert!(
        missing.is_empty(),
        "declared in the manifest but not routed: {missing:?}"
    );
}

#[tokio::test]
#[ignore = "drives a real dlopen'ed module; must be the only such test in the process — see the module docs"]
async fn stateful_optional_families_round_trip_over_the_bus() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let (client, _host, _task) = admit_module(workspace.path()).await;
    let bus = proxy(&client);

    documents_and_graph_round_trip(&bus).await;
    goals_tools_and_sources_round_trip(&bus).await;
    people_and_profile_round_trip(&bus).await;
    episodic_round_trip(&bus).await;
}

async fn documents_and_graph_round_trip(bus: &tinybus::Proxy) {
    use tinymemory_api::types::{GraphRelationRecord, MemoryKvRecord, NamespaceDocumentInput};

    let document = NamespaceDocumentInput {
        namespace: "project".into(),
        key: "brief".into(),
        title: "Brief".into(),
        content: "Ship deterministic module coverage".into(),
        source_type: "upload".into(),
        priority: "high".into(),
        tags: vec!["coverage".into()],
        metadata: serde_json::json!({"ticket": 81}),
        category: "core".into(),
        session_id: Some("session-1".into()),
        document_id: None,
        taint: MemoryTaint::ExternalSync,
    };
    let document_id: String = bus
        .call("PutDocument", (document,))
        .await
        .expect("PutDocument");
    let stored: Option<tinymemory_api::types::StoredMemoryDocument> = bus
        .call("GetDocument", ("project", "brief"))
        .await
        .expect("GetDocument");
    assert_eq!(stored.expect("stored document").document_id, document_id);
    let _: serde_json::Value = bus
        .call("ListDocuments", (Some("project"),))
        .await
        .expect("ListDocuments");
    let namespaces: Vec<String> = bus
        .call("ListNamespaces", ())
        .await
        .expect("ListNamespaces");
    assert!(namespaces.contains(&"project".to_string()));
    let _: tinymemory_api::types::NamespaceRetrievalContext = bus
        .call("QueryDocuments", ("project", "coverage", 8_usize))
        .await
        .expect("QueryDocuments");
    let _: tinymemory_api::types::NamespaceRetrievalContext = bus
        .call("RecallDocuments", ("project", 8_usize))
        .await
        .expect("RecallDocuments");

    bus.call::<()>(
        "KvPut",
        (Some("project"), "status", serde_json::json!("green")),
    )
    .await
    .expect("KvPut");
    let kv: Option<MemoryKvRecord> = bus
        .call("KvGet", (Some("project"), "status"))
        .await
        .expect("KvGet");
    assert_eq!(kv.expect("KV row").value, serde_json::json!("green"));
    let listed: Vec<MemoryKvRecord> = bus
        .call("KvList", (Some("project"), Some("status"), 8_usize))
        .await
        .expect("KvList");
    assert_eq!(listed.len(), 1);
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
    bus.call::<()>("PutRelation", (relation,))
        .await
        .expect("PutRelation");
    let relations: Vec<GraphRelationRecord> = bus
        .call(
            "Relations",
            (Some("project"), Some("suite"), Some("covers"), 8_usize),
        )
        .await
        .expect("Relations");
    assert_eq!(relations.len(), 1);

    let _: serde_json::Value = bus
        .call("DeleteDocument", ("project", document_id))
        .await
        .expect("DeleteDocument");
    assert!(bus
        .call::<bool>("KvDelete", (Some("project"), "status"))
        .await
        .expect("KvDelete"));
    bus.call::<()>("ClearNamespace", ("project",))
        .await
        .expect("ClearNamespace");
}

async fn goals_tools_and_sources_round_trip(bus: &tinybus::Proxy) {
    use tinymemory_api::goals::{GoalItem, GoalsDoc};
    use tinymemory_api::provider::types::SourceItem;
    use tinymemory_api::tool_memory::{ToolMemoryPriority, ToolMemoryRule, ToolMemorySource};

    let goals = GoalsDoc {
        items: vec![GoalItem::new("g1", "finish coverage")],
    };
    bus.call::<()>("SetGoals", (goals.clone(),))
        .await
        .expect("SetGoals");
    let actual_goals: GoalsDoc = bus.call("Goals", ()).await.expect("Goals");
    assert_eq!(actual_goals, goals);
    let rule = ToolMemoryRule::new(
        "shell",
        "never delete broad paths",
        ToolMemoryPriority::Critical,
        ToolMemorySource::UserExplicit,
    );
    let rule_id = rule.id.clone();
    bus.call::<()>("PutToolRule", (rule,))
        .await
        .expect("PutToolRule");
    let rules: Vec<ToolMemoryRule> = bus.call("ToolRules", ("shell",)).await.expect("ToolRules");
    assert_eq!(rules.len(), 1);
    assert!(bus
        .call::<bool>("DeleteToolRule", ("shell", rule_id))
        .await
        .expect("DeleteToolRule"));

    let source = SourceItem {
        item_id: "item-1".into(),
        title: "Source item".into(),
        content: "source body".into(),
        mime: Some("text/plain".into()),
        url: Some("https://example.invalid/item-1".into()),
        updated_at_ms: Some(42),
        tags: vec!["source".into()],
    };
    let outcome: tinymemory_api::provider::types::IngestOutcome = bus
        .call(
            "AcceptSourceItems",
            ("drive-1", "drive", vec![source], MemoryTaint::ExternalSync),
        )
        .await
        .expect("AcceptSourceItems");
    assert_eq!(outcome.written, 1);
    let forgotten: u64 = bus
        .call("ForgetSource", ("drive-1",))
        .await
        .expect("ForgetSource");
    assert_eq!(forgotten, 1);
}

async fn people_and_profile_round_trip(bus: &tinybus::Proxy) {
    use tinymemory_api::provider::people::{PersonHandle, PersonInteraction, ResolvedPerson};
    use tinymemory_api::provider::profile::{FacetType, UserState};

    let handle = PersonHandle::Email("friend@example.com".into());
    let resolved: Option<ResolvedPerson> = bus
        .call("ResolveHandle", (handle.clone(), true))
        .await
        .expect("ResolveHandle");
    let person = resolved.expect("created person");
    bus.call::<()>(
        "AddHandleAlias",
        (
            person.id.clone(),
            PersonHandle::Email("alias@example.com".into()),
        ),
    )
    .await
    .expect("AddHandleAlias");
    let _: Option<tinymemory_api::provider::people::PersonRecord> = bus
        .call("GetPerson", (person.id.clone(),))
        .await
        .expect("GetPerson");
    bus.call::<()>(
        "RecordInteraction",
        (PersonInteraction {
            person_id: person.id.clone(),
            at: "2026-08-21T00:00:00Z".into(),
            is_outbound: true,
            length: 120,
        },),
    )
    .await
    .expect("RecordInteraction");
    let _: Option<tinymemory_api::provider::people::PersonScore> = bus
        .call("ScorePerson", (person.id.clone(),))
        .await
        .expect("ScorePerson");
    let _: Vec<tinymemory_api::provider::people::RankedPerson> = bus
        .call("ListPeople", (Some(8_usize),))
        .await
        .expect("ListPeople");
    // `SeedFromAddressBook` is the one member here that reaches outside the
    // process for its answer: with `contacts` on it opens the platform address
    // book, and on macOS that is a per-application privacy grant the test
    // runner may not hold. A denial is the address book answering, not the
    // module failing to route, so the assertion is that the call reaches the
    // driver and comes back under a contract error — never that this host
    // happens to have granted Contacts access.
    match bus
        .call::<tinymemory_api::provider::people::AddressBookSeedOutcome>("SeedFromAddressBook", ())
        .await
    {
        Ok(_) => {}
        Err(BusError::MethodFailed { name, message })
            if message.contains("contacts access denied") =>
        {
            assert!(
                name.starts_with("ai.tinyhumans.tinymemory.Error."),
                "a denial must still come back under a contract error name, got {name}"
            );
            eprintln!(
                "SeedFromAddressBook: address book access not granted on this host — {message}"
            );
        }
        Err(error) => panic!("SeedFromAddressBook: {error:?}"),
    }

    bus.call::<()>(
        "UpsertProviderFacet",
        (
            "facet-1",
            FacetType::Preference,
            "style/verbosity",
            "concise",
            0.9_f64,
            Some("segment-1"),
            100.0_f64,
        ),
    )
    .await
    .expect("UpsertProviderFacet");
    let facet: Option<tinymemory_api::provider::profile::ProfileFacet> = bus
        .call("GetFacet", ("style/verbosity",))
        .await
        .expect("GetFacet");
    let facet = facet.expect("facet");
    let _: Vec<tinymemory_api::provider::profile::ProfileFacet> = bus
        .call("ListActiveFacets", ())
        .await
        .expect("ListActiveFacets");
    let _: Vec<tinymemory_api::provider::profile::ProfileFacet> =
        bus.call("ListAllFacets", ()).await.expect("ListAllFacets");
    let _: Vec<tinymemory_api::provider::profile::ProfileFacet> = bus
        .call("FacetsByType", (FacetType::Preference,))
        .await
        .expect("FacetsByType");
    assert!(bus
        .call::<bool>("SetFacetUserState", ("style/verbosity", UserState::Pinned),)
        .await
        .expect("SetFacetUserState"));
    let _: bool = bus
        .call("WorkflowIdentityMatches", ("style/*", "concise"))
        .await
        .expect("WorkflowIdentityMatches");
    assert!(bus
        .call::<bool>("DeleteFacetById", (facet.facet_id,))
        .await
        .expect("DeleteFacetById"));
    let _: usize = bus
        .call("DropFacetsBelow", (0.5_f64,))
        .await
        .expect("DropFacetsBelow");
}

async fn episodic_round_trip(bus: &tinybus::Proxy) {
    use tinymemory_api::provider::EpisodicTurn;

    let turn = EpisodicTurn {
        id: None,
        session_id: "session-1".into(),
        timestamp: 10.0,
        role: "user".into(),
        content: "remember the test".into(),
        lesson: Some("verify state".into()),
        tool_calls_json: None,
        cost_microdollars: 1,
    };
    let turn_id: i64 = bus.call("InsertTurn", (turn,)).await.expect("InsertTurn");
    let _: Vec<EpisodicTurn> = bus
        .call("SessionTurns", ("session-1",))
        .await
        .expect("SessionTurns");
    bus.call::<()>(
        "CreateSegment",
        (
            "seg-1",
            "session-1",
            "global",
            turn_id,
            Option::<u32>::None,
            10.0_f64,
            10.0_f64,
        ),
    )
    .await
    .expect("CreateSegment");
    bus.call::<()>(
        "AppendTurn",
        ("seg-1", turn_id, Option::<u32>::None, 10.0_f64, 11.0_f64),
    )
    .await
    .expect("AppendTurn");
    let _: Option<tinymemory_api::provider::episodic::ConversationSegment> = bus
        .call("OpenSegment", ("session-1",))
        .await
        .expect("OpenSegment");
    bus.call::<()>("CloseSegment", ("seg-1", 13.0_f64))
        .await
        .expect("CloseSegment");
    bus.call::<()>("SetSegmentSummary", ("seg-1", "summary", 14.0_f64))
        .await
        .expect("SetSegmentSummary");
    bus.call::<()>(
        "UpsertSegmentEmbedding",
        ("seg-1", "test:8", vec![0.0_f32; DIMS], 15.0_f64),
    )
    .await
    .expect("UpsertSegmentEmbedding");
}

#[tokio::test]
#[ignore = "drives a real dlopen'ed module; must be the only such test in the process — see the module docs"]
async fn query_and_maintenance_families_dispatch_typed_requests() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let (client, _host, _task) = admit_module(workspace.path()).await;
    let bus = proxy(&client);

    let chunk_id = ingest_and_chunks_round_trip(&bus).await;
    retrieval_round_trip(&bus, chunk_id).await;
    tree_and_entities_round_trip(&bus).await;
    maintenance_and_diff_round_trip(&bus).await;
    portability_and_lifecycle_round_trip(&bus).await;
}

#[allow(
    clippy::too_many_lines,
    reason = "one linear bus round trip: ingest, then every chunk read it enables, asserted in call order"
)]
async fn ingest_and_chunks_round_trip(bus: &tinybus::Proxy) -> String {
    use tinymemory_api::chunks::DataSource;
    use tinymemory_api::provider::chunks::{
        ChunkDetail, ChunkEmbedding, ChunkListRow, ChunkQuery, SourceTotal,
    };
    use tinymemory_api::provider::types::{IngestItem, IngestOutcome};

    let ingest = IngestItem {
        namespace: Some("project".into()),
        source: DataSource::Upload,
        source_id: "mem_src:src_diff:item-1".into(),
        owner: "owner".into(),
        source_ref: None,
        content: "Alice maintains the TinyMemory adapter in Kuwait.".into(),
        mime: Some("text/plain".into()),
        timestamp: chrono::DateTime::from_timestamp(1_700_000_000, 0),
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
    let outcome: IngestOutcome = bus
        .call("IngestDocument", (ingest.clone(),))
        .await
        .expect("IngestDocument");
    assert!(outcome.written > 0);

    // The same source again. The gate refuses it, and the refusal has to reach
    // the caller as itself: this is the one place the field is asserted after a
    // real serialize/deserialize round trip, and a shape that dropped it would
    // still answer `written: 0` here and look like an empty ingest.
    let repeat: IngestOutcome = bus
        .call("IngestDocument", (ingest,))
        .await
        .expect("repeated IngestDocument");
    assert!(
        repeat.already_ingested,
        "a claimed source must say so over the bus, not just write nothing"
    );
    assert_eq!(repeat.written, 0, "and it must not have written anything");
    assert_eq!(
        repeat.skipped, 0,
        "`skipped` counts dropped units; the no-op belongs in `already_ingested`"
    );

    let empty: IngestOutcome = bus
        .call("IngestChat", (Vec::<IngestItem>::new(),))
        .await
        .expect("IngestChat");
    assert!(empty.ids.is_empty());

    let mail: IngestOutcome = bus
        .call(
            "IngestEmail",
            (vec![IngestItem {
                namespace: Some("project".into()),
                source: DataSource::Gmail,
                source_id: "mail:thread-1".into(),
                owner: "owner@example.com".into(),
                source_ref: None,
                content: "The adapter ships on Thursday, subject to the review.".into(),
                mime: Some("text/plain".into()),
                timestamp: chrono::DateTime::from_timestamp(1_700_000_200, 0),
                tags: vec!["coverage".into()],
                taint: MemoryTaint::Internal,
                path_scope: None,
                author: Some("alice@example.com".into()),
                channel_label: Some("Adapter ship date".into()),
                platform: None,
                to: vec!["carol@example.com".into()],
                cc: vec!["dave@example.com".into()],
                subject: Some("Re: adapter, renamed".into()),
                list_unsubscribe: Some("<https://lists.example.com/u/9>".into()),
            }],),
        )
        .await
        .expect("IngestEmail");
    assert!(
        mail.written > 0,
        "the mail path must reach the pipeline, not just route"
    );

    // The mail headers, asserted after a real serialize/deserialize across the
    // loaded module rather than only against the driver. `List-Unsubscribe` is
    // the one that matters most: it is the input an unsubscribe flow reads back
    // out of stored mail, so a shape that dropped it in transit would still
    // answer `written > 0` above and look like a healthy ingest.
    let stored: Option<tinymemory_api::chunks::Chunk> = bus
        .call("GetChunk", (mail.ids[0].clone(),))
        .await
        .expect("GetChunk for the stored mail");
    let stored = stored.expect("the id `IngestEmail` reported must resolve");
    for header in [
        "To: carol@example.com",
        "Cc: dave@example.com",
        "Subject: Re: adapter, renamed",
        "List-Unsubscribe: <https://lists.example.com/u/9>",
    ] {
        assert!(
            stored.content.contains(header),
            "`{header}` must survive the crossing: {}",
            stored.content
        );
    }

    let chunks: Vec<tinymemory_api::chunks::Chunk> = bus
        .call(
            "ListChunks",
            (
                ChunkQuery::default(),
                Option::<tinymemory_api::provider::types::SourceScope>::None,
            ),
        )
        .await
        .expect("ListChunks");
    assert!(!chunks.is_empty());
    let chunk_id = outcome.ids[0].clone();
    let _: Option<tinymemory_api::chunks::Chunk> = bus
        .call("GetChunk", (chunk_id.clone(),))
        .await
        .expect("GetChunk");
    let _: Option<ChunkDetail> = bus
        .call("ChunkDetail", (chunk_id.clone(),))
        .await
        .expect("ChunkDetail");
    let kinds: Vec<String> = bus.call("StorageKinds", ()).await.expect("StorageKinds");
    assert!(!kinds.is_empty());
    let _: Vec<ChunkEmbedding> = bus
        .call("ChunkEmbeddings", (vec![chunk_id.clone()], "test:8"))
        .await
        .expect("ChunkEmbeddings");
    // The count is asked over the wire with the same query the list used. This
    // workspace holds far fewer chunks than the default page, so the two must
    // agree exactly — a count answered from an unfiltered `SELECT COUNT(*)`,
    // or one that let the page bounds through, would not.
    let total: u64 = bus
        .call(
            "CountChunks",
            (
                ChunkQuery::default(),
                Option::<tinymemory_api::provider::types::SourceScope>::None,
            ),
        )
        .await
        .expect("CountChunks");
    assert_eq!(
        total,
        chunks.len() as u64,
        "CountChunks must agree with the ListChunks page it accompanies"
    );

    // The detail list answers the page's own filter in one read rather than in
    // a `ChunkDetail` loop, so it has to describe exactly the page `ListChunks`
    // returned. A detail list built on a second predicate would not.
    let details: Vec<ChunkListRow> = bus
        .call(
            "ListChunkDetails",
            (
                ChunkQuery::default(),
                Option::<tinymemory_api::provider::types::SourceScope>::None,
            ),
        )
        .await
        .expect("ListChunkDetails");
    assert_eq!(
        details.len(),
        chunks.len(),
        "ListChunkDetails must describe the same page ListChunks returned"
    );

    // Shape over the wire is what is under test here — a workspace with one
    // source is a legitimate answer — so this asserts the decode and the
    // argument tuple, which is what a host gets wrong.
    let _: Vec<SourceTotal> = bus
        .call(
            "SourceTotals",
            (
                16_usize,
                Option::<tinymemory_api::provider::types::SourceScope>::None,
            ),
        )
        .await
        .expect("SourceTotals");

    chunk_id
}

async fn retrieval_round_trip(bus: &tinybus::Proxy, chunk_id: String) {
    use tinymemory_api::provider::retrieval::{
        CoverWindowQuery, FastRetrieveQuery, RetrievalHit, RetrievalResponse, SourceRetrievalQuery,
    };

    let leaves: Vec<RetrievalHit> = bus
        .call(
            "RetrieveLeaves",
            (
                vec![chunk_id.clone()],
                Option::<tinymemory_api::provider::types::SourceScope>::None,
            ),
        )
        .await
        .expect("RetrieveLeaves");
    assert!(!leaves.is_empty());
    let _: RetrievalResponse = bus
        .call(
            "FastRetrieve",
            (
                "TinyMemory adapter",
                FastRetrieveQuery {
                    limit: 8,
                    max_hops: 1,
                    time_window_days: None,
                },
                Option::<tinymemory_api::provider::types::SourceScope>::None,
            ),
        )
        .await
        .expect("FastRetrieve");
    let _: RetrievalResponse = bus
        .call(
            "CoverWindow",
            (
                CoverWindowQuery {
                    since_ms: 0,
                    until_ms: i64::MAX,
                    source_id: None,
                    source_kind: None,
                    limit: Some(8),
                },
                Option::<tinymemory_api::provider::types::SourceScope>::None,
            ),
        )
        .await
        .expect("CoverWindow");
    let _: RetrievalResponse = bus
        .call(
            "RetrieveSource",
            (
                SourceRetrievalQuery {
                    source_id: Some("mem_src:src_diff:item-1".into()),
                    source_kind: None,
                    time_window_days: None,
                    query: None,
                    limit: 8,
                },
                Option::<tinymemory_api::provider::types::SourceScope>::None,
            ),
        )
        .await
        .expect("RetrieveSource");
    let _: Vec<RetrievalHit> = bus
        .call(
            "RetrieveChildren",
            (
                "root",
                1_u32,
                Option::<String>::None,
                Some(8_usize),
                Option::<tinymemory_api::provider::types::SourceScope>::None,
            ),
        )
        .await
        .expect("RetrieveChildren");
    let _: Vec<tinymemory_api::types::NamespaceMemoryHit> = bus
        .call(
            "RecallNamespaceScored",
            ("project", "adapter", 8_usize, Option::<String>::None),
        )
        .await
        .expect("RecallNamespaceScored");
    let _: Vec<tinymemory_api::provider::retrieval::EntityMatch> = bus
        .call(
            "SearchEntities",
            ("Alice", Option::<Vec<String>>::None, 8_usize),
        )
        .await
        .expect("SearchEntities");
}

async fn tree_and_entities_round_trip(bus: &tinybus::Proxy) {
    use tinymemory_api::tree::{IngestRequest, TreeStatus};

    bus.call::<()>(
        "Append",
        (IngestRequest {
            namespace: "tree-project".into(),
            content: "A deterministic tree buffer entry".into(),
            timestamp: chrono::DateTime::from_timestamp(1_700_000_000, 0),
            metadata: Some(serde_json::json!({"source": "test"})),
        },),
    )
    .await
    .expect("Append");
    let _: Vec<tinymemory_api::chunks::Chunk> = bus
        .call(
            "QuerySource",
            (
                "tree-project",
                "mem_src:src_diff:item-1",
                8_usize,
                Option::<tinymemory_api::provider::types::SourceScope>::None,
            ),
        )
        .await
        .expect("QuerySource");
    let sealed: TreeStatus = bus.call("Seal", ("tree-project",)).await.expect("Seal");
    assert!(sealed.total_nodes > 0);
    let cascaded: TreeStatus = bus
        .call("Cascade", ("tree-project",))
        .await
        .expect("Cascade");
    assert_eq!(cascaded.namespace, "tree-project");
    let drill: Result<tinymemory_api::tree::QueryResult, _> =
        bus.call("DrillDown", ("empty-tree", "missing")).await;
    assert!(drill.is_err(), "missing tree nodes must be named errors");

    let _: Vec<tinymemory_api::provider::types::EntityHit> = bus
        .call("Entities", ("project", Some("Alice"), 8_usize))
        .await
        .expect("Entities");
    let _: Vec<tinymemory_api::types::GraphRelationRecord> = bus
        .call("EntityEdges", ("project", "person:alice", 8_usize))
        .await
        .expect("EntityEdges");
    bus.call::<()>("TouchEntities", ("project", vec!["person:alice"]))
        .await
        .expect("TouchEntities");

    // The occurrence-index reads. Shape over the wire is what is under test —
    // an empty index is a legitimate answer to all three — so these assert the
    // decode and the argument tuples, which is what a host gets wrong.
    let _: Vec<tinymemory_api::provider::types::EntityOccurrence> = bus
        .call("TopEntities", (Option::<String>::None, 8_usize))
        .await
        .expect("TopEntities");
    let _: Vec<tinymemory_api::provider::types::ChunkEntityOccurrence> = bus
        .call(
            "ChunkEntities",
            (vec!["chunk-1".to_string()], Option::<Vec<String>>::None),
        )
        .await
        .expect("ChunkEntities");
    let _: Vec<String> = bus
        .call("EntityChunkIds", ("person:alice", 8_usize))
        .await
        .expect("EntityChunkIds");
    // A kind the extractor's vocabulary does not hold is a caller mistake, not
    // an empty store: the module must refuse it rather than answer with `[]`.
    let refused: Result<Vec<tinymemory_api::provider::types::EntityOccurrence>, _> = bus
        .call("TopEntities", (Some("not-a-kind".to_string()), 8_usize))
        .await;
    assert!(
        refused.is_err(),
        "an unknown entity kind must be refused, not answered with an empty index"
    );
}

async fn maintenance_and_diff_round_trip(bus: &tinybus::Proxy) {
    use tinymemory_api::chunks::DataSource;
    use tinymemory_api::provider::types::{
        DiffReport, IngestItem, IngestOutcome, MaintenanceReport, SnapshotRef,
    };

    for method in ["Reembed", "Compact", "Consolidate", "Doctor"] {
        let report: MaintenanceReport =
            tokio::time::timeout(std::time::Duration::from_secs(2), bus.call(method, ()))
                .await
                .unwrap_or_else(|_| panic!("{method} timed out"))
                .expect(method);
        assert_eq!(
            report.operation.to_ascii_lowercase(),
            method.to_ascii_lowercase()
        );
    }

    let first: SnapshotRef = bus
        .call("CaptureSnapshot", ("src_diff",))
        .await
        .expect("first CaptureSnapshot");
    let changed = IngestItem {
        namespace: Some("project".into()),
        source: DataSource::Upload,
        source_id: "mem_src:src_diff:item-2".into(),
        owner: "owner".into(),
        source_ref: None,
        content: "A second deterministic source item changes the snapshot.".into(),
        mime: Some("text/plain".into()),
        timestamp: chrono::DateTime::from_timestamp(1_700_000_100, 0),
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
    let _: IngestOutcome = bus
        .call("IngestDocument", (changed,))
        .await
        .expect("changed IngestDocument");
    let second: SnapshotRef = bus
        .call("CaptureSnapshot", ("src_diff",))
        .await
        .expect("second CaptureSnapshot");
    let snapshots: Vec<SnapshotRef> = bus
        .call("Snapshots", ("src_diff", 8_usize))
        .await
        .expect("Snapshots");
    assert_eq!(snapshots.len(), 2);
    let diff: DiffReport = bus
        .call("Diff", ("src_diff", Some(first.id), second.id))
        .await
        .expect("Diff");
    assert!(diff.added + diff.modified + diff.removed > 0);
    let missing_capture: Result<SnapshotRef, _> =
        bus.call("CaptureSnapshot", ("missing-source",)).await;
    assert!(missing_capture.is_err());
}

async fn portability_and_lifecycle_round_trip(bus: &tinybus::Proxy) {
    use tinymemory_api::provider::types::ExportPage;

    let _: Vec<tinymemory_api::types::NamespaceSummary> =
        bus.call("Namespaces", ()).await.expect("Namespaces");
    let page: ExportPage = bus
        .call("ExportPage", (Option::<String>::None, 16_usize))
        .await
        .expect("ExportPage");
    let _: tinymemory_api::provider::types::ImportOutcome = bus
        .call("ImportRecords", (page.records,))
        .await
        .expect("ImportRecords");

    let invalid_store: Result<String, _> = bus.call("OpenStore", ("../escape",)).await;
    assert!(invalid_store.is_err());
    let _: bool = bus
        .call("DeleteFacet", ("missing-facet",))
        .await
        .expect("DeleteFacet");
    let _: Option<tinymemory_api::provider::profile::ProfileFacet> = bus
        .call("GetFacet", ("missing-facet",))
        .await
        .expect("GetFacet missing");
    let _: tinymemory_api::health::MemoryHealth = bus.call("Health", ()).await.expect("Health");
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        bus.call::<()>("Shutdown", ()),
    )
    .await
    .expect("Shutdown timed out")
    .expect("Shutdown");
}

#[tokio::test]
#[ignore = "drives a real dlopen'ed module; must be the only such test in the process — see the module docs"]
async fn bootstrap_connection_finds_its_provider_registry_inside_the_module() {
    // The Composio provider registry is a process-global that the *host's* boot
    // used to fill. This module is a `cdylib` with its own statics, so unless
    // its startup calls `init_default_providers` the registry here is empty —
    // and `get_provider` answers `None` rather than erroring, so every
    // `BootstrapConnection` would report "no composio provider registered" over
    // a perfectly good connection. Nothing in a build, a type check or a unit
    // test in the module's own workspace sees that, because they all run in a
    // process the host has already initialised.
    //
    // So this asserts against the *loaded artifact*, and it asserts the
    // distinction rather than the outcome. `Ok` means the provider resolved
    // and its bootstrap ran; any other error means it resolved and the run
    // failed on its own terms. Exactly one result says the registry was never
    // populated, and that is the regression.
    //
    // An earlier revision required failure here, on the assumption that a temp
    // workspace has no Composio — and the call succeeded, because the module's
    // proxied `ComposioHost` answers through the test harness and the default
    // bootstrap is content with that. Asserting the symptom instead of the
    // mechanism made the test wrong about the one thing it exists to pin.
    let workspace = tempfile::tempdir().expect("tempdir");
    let (client, _host, _task) = admit_module(workspace.path()).await;

    let result: Result<(), _> = proxy(&client)
        .call(
            "BootstrapConnection",
            ("gmail".to_string(), "conn-1".to_string()),
        )
        .await;

    if let Err(error) = result {
        let rendered = format!("{error:?}");
        assert!(
            !rendered.contains("no composio provider registered"),
            "the module's provider registry is empty — its startup did not call \
             init_default_providers. Error was: {rendered}"
        );
    }
}

/// A source sync runs through the loaded module and its result lands in the store.
///
/// `every_declared_method_is_actually_routed` walks the module's declared
/// method list and checks each one dispatches. Six sync members are in that
/// list and, before this test, none was ever called: routing is not behaviour,
/// and the sync pipeline is what a connector-backed install spends its time
/// doing (#149).
///
/// # Why this needs a loaded module at all
///
/// The pipeline has ~397 tests across this workspace — `sources/sync_tests.rs`,
/// `sync/audit_tests.rs`, the `tinymemory-sync` post-processor suites — and
/// every one runs in-process. What none of them reaches is the same pipeline
/// **over the bus**, which is a real difference rather than a formality: the
/// module is a separately compiled `cdylib` with its own statics, so a
/// task-local set host-side reads as *absent* inside it, and the host's sync
/// entry points are where scope and credentials cross that boundary.
///
/// # Hermetic by construction
///
/// A `folder` source over a temp directory holding one file. No network, no
/// credentials, no fixture beyond `tempfile` — the same approach
/// `sources/sync_tests.rs` takes for its own folder cases.
///
/// # Two things this got wrong first, both worth keeping
///
/// The registry is **not** `admit_module`'s `memory_sources` config key: that
/// key is the serialized registry snapshot operations read, while
/// `run_source_sync` resolves ids through `get_source_in` against the host's
/// `config.toml`. Registering in the wrong one is openhuman#5820's "no memory
/// source registered as src_…" strand, and it is exactly what this test hit.
///
/// And `RunSourceSync` does **not** write a sync-audit row. The audit appends
/// live in `sources::sync::sync_source`, the periodic path; this member goes
/// through `engine::run_source_pipeline`, which has none. Asserting an audit
/// row here — the obvious reading of "assert its effect" — would have been
/// asserting something the member does not do. What it does do is land chunks,
/// so that is what is checked, through a second bus call.
#[tokio::test]
#[ignore = "loads a real module; must be the only test in its process"]
async fn a_source_sync_runs_over_the_bus_and_lands_in_the_store() {
    use tinymemory_api::provider::{SourceSyncState, SyncAuditEntry, SyncRunOutcome};

    let workspace = tempfile::tempdir().expect("tempdir");

    let source_dir = workspace.path().join("sync-fixture");
    std::fs::create_dir_all(&source_dir).expect("create source dir");
    std::fs::write(
        source_dir.join("note.md"),
        "# Sync fixture\n\nA folder source the module can read without a network.\n",
    )
    .expect("write fixture file");

    // `admit_module` sends no `config_path`, and the tempdir's parent holds no
    // `config.toml`, so `provider::host_config_path` falls through to
    // `workspace_dir/config.toml`. Written before admission, because the module
    // builds its store during initialization.
    std::fs::write(
        workspace.path().join("config.toml"),
        format!(
            "[[memory_sources]]\n\
             id = \"src_sync_fixture\"\n\
             kind = \"folder\"\n\
             label = \"Sync fixture\"\n\
             enabled = true\n\
             path = \"{}\"\n",
            source_dir.display()
        ),
    )
    .expect("write source registry");

    let (client, _host, _task) = admit_module(workspace.path()).await;
    let bus = proxy(&client);

    // ── the run ─────────────────────────────────────────────────────────────
    let outcome: SyncRunOutcome = bus
        .call("RunSourceSync", ("src_sync_fixture",))
        .await
        .expect("RunSourceSync must reach the pipeline over the bus");
    assert_eq!(
        outcome.records_ingested, 1,
        "the folder holds exactly one file; got {outcome:?}"
    );

    // ── the effect, read back over the same bus ─────────────────────────────
    //
    // The count above is the run's own report. This is the store's, and the two
    // being separate is the point: a member that returned a plausible
    // `SyncRunOutcome` without writing anything would satisfy the assertion
    // above and fail this one.
    let statuses: Vec<tinymemory_api::provider::SourceIngestStatus> = bus
        .call(
            "SourceIngestStatus",
            (vec![tinymemory_api::provider::SourceIngestQuery {
                source_id: "src_sync_fixture".to_string(),
                // The convention the engine's own readers write, trailing
                // separator included — without it a source keyed `src_a:` also
                // counts the chunks of `src_ab:`.
                chunk_id_prefix: "mem_src:src_sync_fixture:".to_string(),
            }],),
        )
        .await
        .expect("SourceIngestStatus");
    let status = statuses
        .iter()
        .find(|row| row.source_id == "src_sync_fixture")
        .unwrap_or_else(|| panic!("no ingest status for the synced source; got {statuses:?}"));
    assert!(
        status.chunks_synced > 0,
        "the sync reported {} records but the store holds no chunks under the \
         source prefix: {status:?}",
        outcome.records_ingested
    );

    // ── the second member ───────────────────────────────────────────────────
    //
    // `SourceSyncState` is driven and refuses, which is the answer worth
    // pinning: this engine does not own composio connection state at all —
    // "reading a composio connection's sync state is synced through the
    // connector module, not this engine". A refusal that names where the answer
    // lives is a better contract than a `None` that looks like "no state yet",
    // and a caller polling for a cursor needs to be able to tell those apart.
    //
    // The class matters as much as the refusal. `Invalid` says the request was
    // wrong; `Unsupported` would say the family is absent, and this driver does
    // serve `SourceSync` — it just does not serve this member's subject.
    let state: Result<Option<SourceSyncState>, _> = bus
        .call("SourceSyncState", ("folder", "src_sync_fixture"))
        .await;
    let refusal = state.expect_err("this engine does not own composio connection state");
    assert!(
        refusal.wire_name().ends_with("Invalid"),
        "the refusal must be Invalid — the request is wrong, the family is not \
         missing — got {refusal:?}"
    );

    // ── the audit log, and what it does not contain ─────────────────────────
    //
    // Reachable over the bus, and empty: `RunSourceSync` goes through
    // `engine::run_source_pipeline`, which appends no audit row. The rows come
    // from `sources::sync::sync_source`, the periodic path. Pinned because the
    // two entry points look interchangeable from the wire and are not — a
    // caller that ran a manual sync and then read the audit log for it would
    // wait forever.
    let audit: Vec<SyncAuditEntry> = bus
        .call("SyncAuditLog", (Some(16usize),))
        .await
        .expect("SyncAuditLog must be reachable");
    assert!(
        !audit.iter().any(|row| row.source_id == "src_sync_fixture"),
        "RunSourceSync is not an audited path; if it has become one, this test \
         should assert the row rather than its absence. Got: {audit:?}"
    );

    // ── the NotFound contract ───────────────────────────────────────────────
    //
    // Documented as "deliberately distinct from a sync that ran and found
    // nothing, because a caller retrying a deleted source should learn that
    // rather than see an empty success". Nothing checked it.
    let missing: Result<SyncRunOutcome, _> =
        bus.call("RunSourceSync", ("src_does_not_exist",)).await;
    let error = missing.expect_err("an unregistered source id must not be an empty success");
    assert!(
        error.wire_name().ends_with("NotFound"),
        "an unregistered id must refuse as NotFound, got {error:?}"
    );
}
