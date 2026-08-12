//! The real thing: a `dlopen`ed `cdylib`, a real broker, a real store.
//!
//! # Why every test here is `#[ignore]`d
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
//! process. Run them one at a time:
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
    BUS_NAME, EMBEDDING_HOST_BUS_NAME, EMBEDDING_HOST_OBJECT_PATH, OBJECT_PATH,
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

/// Stands in for the host's embedder so recall has something to work with.
///
/// Deterministic rather than random: a recall assertion that depended on a
/// random vector would pass or fail for reasons unrelated to the module.
struct HostEmbedder;

#[tinybus::interface(name = "ai.tinyhumans.tinymemory.EmbeddingHost")]
impl HostEmbedder {
    #[allow(clippy::unused_async, reason = "the interface macro requires async")]
    async fn embed(
        &self,
        _model: String,
        _dimensions: usize,
        texts: Vec<String>,
    ) -> BusResult<Vec<Vec<f32>>> {
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
    let config = serde_json::json!({
        "workspace_dir": workspace,
        "cloud_embedding_model": "e2e-model",
        "cloud_embedding_dimensions": DIMS,
        "models_supporting_dimensions": ["e2e-model"],
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
    "Store",
    "Get",
    "Forget",
    "List",
    "Namespaces",
    "Recall",
    "ExportPage",
    "ImportRecords",
    "IngestDocument",
    "IngestChat",
    "PutDocument",
    "GetDocument",
    "QueryDocuments",
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
