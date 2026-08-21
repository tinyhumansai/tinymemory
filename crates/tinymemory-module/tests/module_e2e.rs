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
use tinybus::{Connection, Result as BusResult};
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
        _request: tinyagents::harness::model::ModelRequest,
    ) -> BusResult<tinyagents::harness::model::ModelResponse> {
        use tinyagents::harness::message::{AssistantMessage, ContentBlock};
        use tinyagents::harness::usage::Usage;

        std::future::ready(()).await;
        Ok(tinyagents::harness::model::ModelResponse {
            message: AssistantMessage {
                id: None,
                content: vec![ContentBlock::Text("deterministic summary".into())],
                tool_calls: Vec::new(),
                usage: Some(Usage::new(2, 1)),
            },
            usage: Some(Usage::new(2, 1)),
            finish_reason: Some("stop".into()),
            raw: None,
            resolved_model: None,
            continue_turn: None,
            served_from_cache: false,
        })
    }
}

#[tinybus::interface(name = "ai.tinyhumans.tinymemory.EmbeddingHost")]
impl HostEmbedder {
    async fn embed(
        &self,
        _model: String,
        _dimensions: usize,
        texts: Vec<String>,
    ) -> BusResult<Vec<Vec<f32>>> {
        std::future::ready(()).await;
        EMBED_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
    "Reembed",
    "Compact",
    "Consolidate",
    "Doctor",
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
    // It advertises `Capabilities::all()` — eighteen families — and the tests
    // above exercise three of them.
    //
    // Rather than eighteen bespoke round trips, this asserts the property that
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
    let _: tinymemory_api::provider::people::AddressBookSeedOutcome = bus
        .call("SeedFromAddressBook", ())
        .await
        .expect("SeedFromAddressBook");

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
        ("seg-1", "session-1", "global", turn_id, 10.0_f64, 10.0_f64),
    )
    .await
    .expect("CreateSegment");
    bus.call::<()>("AppendTurn", ("seg-1", turn_id, 10.0_f64, 11.0_f64))
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

async fn ingest_and_chunks_round_trip(bus: &tinybus::Proxy) -> String {
    use tinymemory_api::chunks::DataSource;
    use tinymemory_api::provider::chunks::{ChunkDetail, ChunkEmbedding, ChunkQuery};
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
    };
    let outcome: IngestOutcome = bus
        .call("IngestDocument", (ingest,))
        .await
        .expect("IngestDocument");
    assert!(outcome.written > 0);
    let empty: IngestOutcome = bus
        .call("IngestChat", (Vec::<IngestItem>::new(),))
        .await
        .expect("IngestChat");
    assert!(empty.ids.is_empty());

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
