//! Tests for the service's error mapping.
//!
//! This is the module's half of the wire contract: every `MemoryError` the
//! engine can raise has to leave as a named bus error the host's client can map
//! back. `tinymemory_api::wire_tests` pins the table itself; what is tested here
//! is that the service actually goes through it.
//!
//! Covered here rather than in the loader E2E deliberately. An E2E can only
//! provoke the errors the engine happens to raise for a given input, which makes
//! it a test of engine internals this port does not own — an earlier revision
//! tried `ExportPage` with a zero limit, and since a driver accepting a zero
//! limit is equally legitimate, the test asserted nothing when it passed. Here
//! every variant is reachable by construction.

use tinybus::Error as BusError;
use tinymemory_api::error::MemoryError;
use tinymemory_api::wire;

use super::into_bus_error;

fn test_provider() -> std::sync::Arc<dyn tinymemory_api::provider::MemoryProvider> {
    std::sync::Arc::new(tinymemory_tinycortex::provider(std::sync::Arc::new(
        tinycortex::memory::store::InMemoryMemoryStore::new(),
    )))
}

async fn test_connection() -> tinybus::Connection {
    use tinybus::transport::memory::MemoryBus;

    let bus = MemoryBus::new();
    let broker = tinybus::broker::Broker::new();
    let _broker_task = broker.spawn(bus.clone());
    let connection = tinybus::Connection::connect(bus.connect().await.expect("test transport"))
        .await
        .expect("test connection");
    connection
        .request_name(super::BUS_NAME)
        .await
        .expect("claim test service name");
    connection
}

fn test_config(workspace: &std::path::Path) -> crate::config::ModuleConfig {
    crate::config::ModuleConfig {
        workspace_dir: workspace.to_path_buf(),
        ..crate::config::ModuleConfig::default()
    }
}

/// Holds the embedding-host test mutex while a temporary host is installed.
///
/// Restoring in `Drop` keeps the process global correct even when an assertion
/// panics. The mutex guard is deliberately retained for the whole scope: the
/// factory reads the host during each `OpenStore`, not just during setup.
struct EmbeddingHostRestore {
    _lock: std::sync::MutexGuard<'static, ()>,
    previous: Option<std::sync::Arc<dyn tinymemory_core::embedding_host::EmbeddingHost>>,
}

impl EmbeddingHostRestore {
    fn install(connection: tinybus::Connection, config: &crate::config::ModuleConfig) -> Self {
        let lock = tinymemory_core::embedding_host::embedding_test_guard();
        let previous = tinymemory_core::embedding_host::embedding_host();
        tinymemory_core::embedding_host::set_embedding_host(std::sync::Arc::new(
            crate::embedding::BusEmbeddingHost::new(connection, config),
        ));
        Self {
            _lock: lock,
            previous,
        }
    }
}

impl Drop for EmbeddingHostRestore {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(previous) => tinymemory_core::embedding_host::set_embedding_host(previous),
            None => tinymemory_core::embedding_host::clear_embedding_host(),
        }
    }
}

fn test_opener(
    connection: tinybus::Connection,
    config: crate::config::ModuleConfig,
) -> std::sync::Arc<super::StoreOpener> {
    std::sync::Arc::new(super::StoreOpener::new(connection, config))
}

/// The name and message a mapped error carries on the wire.
fn mapped(error: &MemoryError) -> (String, String) {
    match into_bus_error(error) {
        BusError::MethodFailed { name, message } => (name, message),
        other => panic!("expected MethodFailed, got {other:?}"),
    }
}

#[test]
fn every_variant_leaves_under_its_contract_name() {
    // Exhaustive by construction: `wire::wire_name` is a total match over
    // `MemoryError`, so a new variant fails to compile there before it can
    // silently leave this list.
    let cases = [
        (MemoryError::NotFound("k".into()), wire::NOT_FOUND),
        (MemoryError::Invalid("bad".into()), wire::INVALID),
        (
            MemoryError::BudgetExceeded("too big".into()),
            wire::BUDGET_EXCEEDED,
        ),
        (
            MemoryError::PathEscape("../outside".into()),
            wire::PATH_ESCAPE,
        ),
        (MemoryError::unsupported_raw("tree"), wire::UNSUPPORTED),
        (
            MemoryError::Other(anyhow::anyhow!("engine fell over")),
            wire::OTHER,
        ),
    ];

    for (error, expected) in &cases {
        let (name, _) = mapped(error);
        assert_eq!(&name, expected, "{error:?} left under the wrong name");
    }
}

#[test]
fn a_path_escape_never_leaves_as_an_invalid() {
    // The security-relevant collapse. `Invalid` tells a caller its input was
    // malformed and invites a retry; a sandbox escape is not that, and the host
    // re-raises whatever it receives to its own callers.
    let (name, _) = mapped(&MemoryError::PathEscape("../../etc".into()));
    assert_eq!(name, wire::PATH_ESCAPE);
    assert_ne!(name, wire::INVALID);
}

#[test]
fn a_miss_never_leaves_as_an_invalid() {
    // `get`'s contract makes a miss `Ok(None)`, so a `NotFound` that arrived as
    // `Invalid` would turn an ordinary absence into a caller-visible failure.
    let (name, _) = mapped(&MemoryError::NotFound("absent".into()));
    assert_eq!(name, wire::NOT_FOUND);
    assert_ne!(name, wire::INVALID);
}

#[test]
fn the_names_the_service_emits_are_the_ones_the_host_decodes() {
    // The drift that matters is silent, so this closes the loop rather than
    // trusting the two tables to agree: map out through the service, back
    // through the client's decoder, and require the variant to survive.
    let originals = [
        MemoryError::NotFound("k".into()),
        MemoryError::Invalid("bad".into()),
        MemoryError::BudgetExceeded("too big".into()),
        MemoryError::PathEscape("../outside".into()),
        MemoryError::unsupported_raw("tree"),
    ];

    for original in &originals {
        let (name, message) = mapped(original);
        let decoded = wire::from_wire(&name, &message);
        assert_eq!(
            std::mem::discriminant(&decoded),
            std::mem::discriminant(original),
            "{original:?} did not survive the round trip, arrived as {decoded:?}"
        );
    }
}

#[test]
fn a_message_carries_no_user_content_beyond_what_the_engine_put_there() {
    // Not a redaction test — the engine owns its message. This pins that the
    // service adds nothing of its own, so the only thing that can leak is what
    // the engine already chose to say.
    let error = MemoryError::NotFound("some-key".into());
    let (_, message) = mapped(&error);
    assert_eq!(message, wire::wire_message(&error));
}

/// An entry whose content is `bytes` long.
fn entry_of(bytes: usize) -> tinymemory_api::types::MemoryEntry {
    tinymemory_api::types::MemoryEntry {
        id: "id".into(),
        key: "key".into(),
        content: "x".repeat(bytes),
        namespace: Some("ns".into()),
        category: tinymemory_api::types::MemoryCategory::Core,
        timestamp: "2026-01-01T00:00:00Z".into(),
        session_id: None,
        score: None,
        taint: tinymemory_api::types::MemoryTaint::Internal,
    }
}

#[test]
fn an_ordinary_list_response_is_not_refused() {
    // The ceiling must not be so tight that normal use trips it. A hundred
    // entries of a kilobyte each is an unremarkable namespace.
    let entries: Vec<_> = (0..100).map(|_| entry_of(1024)).collect();
    assert!(super::ensure_response_fits(&entries, "List").is_ok());
}

#[test]
fn an_empty_list_response_is_not_refused() {
    assert!(
        super::ensure_response_fits(&Vec::<tinymemory_api::types::MemoryEntry>::new(), "List")
            .is_ok()
    );
}

#[test]
fn a_response_over_the_ceiling_is_refused_as_a_budget_error() {
    // `List` takes no limit and no cursor, so entries accumulate across
    // individually valid `Store` calls until the response cannot cross a
    // 16 MiB frame. Without this check the caller gets a transport failure it
    // cannot act on; with it, a named error that says how to narrow the query.
    let entries: Vec<_> = (0..2)
        .map(|_| entry_of(super::MAX_RESPONSE_BYTES))
        .collect();

    let error = super::ensure_response_fits(&entries, "List")
        .expect_err("a response over the ceiling must be refused");
    match error {
        BusError::MethodFailed { name, message } => {
            assert_eq!(
                name,
                wire::BUDGET_EXCEEDED,
                "must use a name the host already decodes"
            );
            assert!(message.contains("List"), "{message}");
            assert!(
                message.contains("narrow"),
                "the message must tell the caller what to do: {message}"
            );
        }
        other => panic!("expected MethodFailed, got {other:?}"),
    }
}

#[test]
fn the_refusal_decodes_host_side_as_a_budget_error() {
    // The whole point of reusing an existing name: a new one would decode to
    // `Other` on any host older than the module, turning an actionable "narrow
    // your query" into an opaque backend failure.
    let entries: Vec<_> = (0..2)
        .map(|_| entry_of(super::MAX_RESPONSE_BYTES))
        .collect();

    let BusError::MethodFailed { name, message } =
        super::ensure_response_fits(&entries, "List").expect_err("refused")
    else {
        panic!("expected MethodFailed");
    };

    let decoded = wire::from_wire(&name, &message);
    assert!(
        matches!(decoded, MemoryError::BudgetExceeded(_)),
        "{decoded:?}"
    );
}

#[test]
fn the_refusal_message_carries_no_entry_content() {
    // Entry content is user memory. The message names sizes and the method, and
    // nothing that was stored.
    let secret = "correct-horse-battery-staple";
    let mut entries: Vec<_> = (0..2)
        .map(|_| entry_of(super::MAX_RESPONSE_BYTES))
        .collect();
    entries[0].content.push_str(secret);

    let BusError::MethodFailed { message, .. } =
        super::ensure_response_fits(&entries, "List").expect_err("refused")
    else {
        panic!("expected MethodFailed");
    };
    assert!(!message.contains(secret), "{message}");
}

#[test]
fn the_per_entry_overhead_is_counted_so_many_tiny_entries_still_trip_it() {
    // A million empty entries carry no content at all but still cannot cross a
    // frame — the JSON structure around each one is the payload. Counting only
    // `content.len()` would let this through.
    let encoded_entry = serde_json::to_vec(&entry_of(0))
        .expect("serializable")
        .len();
    let count = super::MAX_RESPONSE_BYTES / encoded_entry + 1;
    let entries: Vec<_> = (0..count).map(|_| entry_of(0)).collect();
    assert!(
        super::ensure_response_fits(&entries, "List").is_err(),
        "entries with no content must still be counted"
    );
}

#[test]
fn store_object_paths_accept_only_one_safe_identifier_component() {
    let valid = [
        ("profile-1", "profile_2d1".to_string()),
        ("profile_one", "profile_5fone".to_string()),
        ("A9", "A9".to_string()),
        (&"x".repeat(128), "x".repeat(128)),
    ];
    for (subdir, component) in valid {
        assert_eq!(
            super::object_path_for_subdir(subdir),
            Some(format!("{}/stores/{component}", super::OBJECT_PATH))
        );
    }

    assert_ne!(
        super::object_path_for_subdir("a-b"),
        super::object_path_for_subdir("a_2db"),
        "escaped identifiers must not collide"
    );

    for invalid in [
        "",
        ".",
        "..",
        "../escape",
        "nested/store",
        "nested\\store",
        "profile.name",
        "profile name",
        "pröfile",
        &"x".repeat(129),
    ] {
        assert!(
            super::object_path_for_subdir(invalid).is_none(),
            "unsafe subdirectory was admitted: {invalid:?}"
        );
    }
}

#[tokio::test]
async fn a_leaf_store_cannot_recursively_open_another_store() {
    let service = super::MemoryService::new(test_provider());
    let error = service
        .open_store("child".to_string())
        .await
        .expect_err("leaf stores must not recursively open stores");
    let tinybus::Error::MethodFailed { name, message } = error else {
        panic!("expected MethodFailed");
    };
    assert_eq!(name, tinymemory_api::wire::INVALID);
    assert!(message.contains("root"));
}

#[tokio::test]
async fn repeated_and_concurrent_opens_reuse_the_registered_object_path() {
    use std::sync::Arc;

    let workspace = tempfile::tempdir().expect("tempdir");
    let connection = test_connection().await;
    let config = test_config(workspace.path());
    let _embedding_host = EmbeddingHostRestore::install(connection.clone(), &config);
    let opener = test_opener(connection.clone(), config);
    let expected = format!("{}/stores/profile_2d1", super::OBJECT_PATH);
    let service = Arc::new(super::MemoryService::root(
        test_provider(),
        Arc::clone(&opener),
    ));

    let mut tasks = Vec::new();
    for _ in 0..16 {
        let service = Arc::clone(&service);
        tasks.push(tokio::spawn(async move {
            service.open_store("profile-1".to_string()).await
        }));
    }
    for task in tasks {
        assert_eq!(task.await.expect("join").expect("reused store"), expected);
    }
    assert_eq!(opener.instrumentation.allocation_attempts(), 1);
    assert_eq!(opener.instrumentation.registration_attempts(), 1);
    assert_eq!(opener.served.lock().await.len(), 1);

    let driver_id: String = connection
        .proxy(super::BUS_NAME, &expected, super::BUS_NAME)
        .expect("store proxy")
        .call("DriverId", ())
        .await
        .expect("the newly registered object must answer");
    assert_eq!(driver_id, "tinycortex");
}

#[tokio::test]
async fn a_failed_registration_is_retried_and_only_success_counts_toward_the_cap() {
    use std::sync::Arc;

    let workspace = tempfile::tempdir().expect("tempdir");
    let connection = test_connection().await;
    let config = test_config(workspace.path());
    let _embedding_host = EmbeddingHostRestore::install(connection.clone(), &config);
    let opener = test_opener(connection, config);
    opener.instrumentation.fail_registrations(1);
    let service = super::MemoryService::root(test_provider(), Arc::clone(&opener));
    service
        .open_store("retry".to_string())
        .await
        .expect_err("the first registration is injected to fail");
    assert!(opener.served.lock().await.is_empty());

    let path = service
        .open_store("retry".to_string())
        .await
        .expect("the same subtree must be retried");
    assert_eq!(path, format!("{}/stores/retry", super::OBJECT_PATH));
    assert_eq!(opener.instrumentation.allocation_attempts(), 2);
    assert_eq!(opener.instrumentation.registration_attempts(), 2);
    assert_eq!(opener.served.lock().await.len(), 1);
}

#[tokio::test]
async fn the_open_store_cap_is_reached_through_successful_opens() {
    use std::sync::Arc;

    let workspace = tempfile::tempdir().expect("tempdir");
    let connection = test_connection().await;
    let config = test_config(workspace.path());
    let _embedding_host = EmbeddingHostRestore::install(connection.clone(), &config);
    let opener = test_opener(connection, config);
    let service = super::MemoryService::root(test_provider(), Arc::clone(&opener));

    for index in 0..super::MAX_OPEN_STORES {
        service
            .open_store(format!("profile-{index}"))
            .await
            .unwrap_or_else(|error| panic!("successful open {index} failed: {error}"));
    }
    let error = service
        .open_store("one-more".to_string())
        .await
        .expect_err("the store cap must be enforced");
    let tinybus::Error::MethodFailed { name, message } = error else {
        panic!("expected MethodFailed");
    };
    assert_eq!(name, tinymemory_api::wire::INVALID);
    assert!(message.contains(&super::MAX_OPEN_STORES.to_string()));
    assert_eq!(opener.served.lock().await.len(), super::MAX_OPEN_STORES);
    assert_eq!(
        opener.instrumentation.allocation_attempts(),
        super::MAX_OPEN_STORES,
        "the refused open must not allocate"
    );
    assert_eq!(
        opener.instrumentation.registration_attempts(),
        super::MAX_OPEN_STORES,
        "the refused open must not register"
    );
}

/// Every method the service implements must also be declared in the manifest.
///
/// The manifest's `methods` list is admission surface: the host may only call a
/// member the artifact declared, so an implemented-but-undeclared method is
/// simply unreachable — no error, no warning, just a family that is silently
/// missing from the bus.
///
/// This is not hypothetical. Thirty-one methods sat in exactly that state: the
/// whole of People, Chunks, Retrieval and Profile, plus `RecallDocuments`,
/// which predates them. The E2E `the_manifest_declares_every_method_the_module
/// _serves` did not catch it, and could not — it compares the manifest against
/// a hand-written list, so a method missing from *both* is invisible to it, and
/// it is `#[ignore]`d besides because it needs a real dlopen'ed artifact.
///
/// Comparing against the implementation removes the hand-written list from the
/// loop entirely: `members()` is generated by `#[interface]` from the `impl`
/// block itself, so it cannot drift from what is really served. The manifest is
/// read out of `lib.rs` because the macro consumes those literals and offers no
/// constant to inspect.
#[test]
fn every_served_method_is_declared_in_the_manifest() {
    let source = include_str!("../lib.rs");
    let list = source
        .split_once("methods = [")
        .expect("the module_export! block declares methods")
        .1
        .split_once(']')
        .expect("the methods list is closed")
        .0;
    let declared: std::collections::BTreeSet<&str> = list
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            // Skip the group comments; only quoted names count.
            line.strip_prefix('"')?
                .split_once('"')
                .map(|(name, _)| name)
        })
        .collect();

    let service = super::MemoryService::new(std::sync::Arc::new(
        tinymemory_api::null::NullMemoryProvider,
    ));
    let served: std::collections::BTreeSet<String> = tinybus::service::Interface::members(&service)
        .iter()
        .map(|member| member.as_str().to_string())
        .collect();
    let served: std::collections::BTreeSet<&str> = served.iter().map(String::as_str).collect();

    let undeclared: Vec<_> = served.difference(&declared).collect();
    assert!(
        undeclared.is_empty(),
        "these methods are served but not declared in the manifest, so no host can call them: \
         {undeclared:?}"
    );

    // The converse is a different failure — a host admitted for a method that
    // answers `unknown_method` — so it is worth pinning in the same place.
    let unserved: Vec<_> = declared.difference(&served).collect();
    assert!(
        unserved.is_empty(),
        "these methods are declared in the manifest but not served: {unserved:?}"
    );
}

/// The members served here are exactly the ones `tinymemory-bus` publishes, in
/// the same order.
///
/// `tinymemory-bus` is what a host compiles against: it carries one constant
/// and one typed call struct per member. Nothing links the two — this crate
/// derives its members from the `#[tinybus::interface]` block, that one lists
/// them by hand — so a method added here without a matching entry there is a
/// capability no host can reach, and an entry there with no method here is a
/// call that fails at runtime with `UnknownMethod`.
///
/// Neither failure has a compile error anywhere, which is why it is asserted.
/// The comparison is on sequences rather than sets on purpose: `members()`
/// returns declaration order, `METHODS` is written in declaration order, and
/// pinning the order too means the two lists stay readable side by side.
#[test]
fn the_served_members_are_exactly_the_published_contract() {
    let service = super::MemoryService::new(std::sync::Arc::new(
        tinymemory_api::null::NullMemoryProvider,
    ));
    let served: Vec<String> = tinybus::service::Interface::members(&service)
        .iter()
        .map(|member| member.as_str().to_string())
        .collect();
    let published: Vec<String> = tinymemory_bus::METHODS
        .iter()
        .map(|member| (*member).to_string())
        .collect();

    // Reported as differences rather than as a 89-element inequality, so the
    // failure names the method that moved instead of printing both lists.
    let missing: Vec<&String> = served.iter().filter(|m| !published.contains(m)).collect();
    assert!(
        missing.is_empty(),
        "served here but absent from tinymemory-bus, so no host can call them: {missing:?}"
    );
    let extra: Vec<&String> = published.iter().filter(|m| !served.contains(m)).collect();
    assert!(
        extra.is_empty(),
        "published by tinymemory-bus but not served here, so a host calling them gets \
         UnknownMethod: {extra:?}"
    );
    assert_eq!(
        served, published,
        "the two lists hold the same members in different orders"
    );
}
