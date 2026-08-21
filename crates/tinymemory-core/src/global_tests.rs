//! Tests for the surrounding module.

use super::*;
use tempfile::TempDir;

/// All tests that touch `GLOBAL_CLIENT` must contend with process-wide
/// state. We tolerate both branches so test ordering doesn't flake the
/// suite.
#[tokio::test]
async fn client_if_ready_is_some_after_init_or_remains_none() {
    crate::test_seams::init();
    let before = client_if_ready();
    let tmp = TempDir::new().unwrap();
    let _ = init(tmp.path().join("ws"));
    let after = client_if_ready();
    if before.is_some() {
        assert!(after.is_some(), "if global was set, it must remain set");
    } else {
        // First setter wins; if our init succeeded it's set now.
        assert!(after.is_some());
    }
}

#[tokio::test]
async fn init_returns_existing_client_when_already_set() {
    crate::test_seams::init();
    let slot = GlobalClientSlot::default();
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path().join("ws");

    let first = init_in_slot(&slot, workspace.clone()).unwrap();
    let second = init_in_slot(&slot, workspace).unwrap();

    assert!(Arc::ptr_eq(&first, &second));
}

#[tokio::test]
async fn init_rebinds_client_when_workspace_changes() {
    crate::test_seams::init();
    let slot = GlobalClientSlot::default();
    let tmp = TempDir::new().unwrap();

    let first = init_in_slot(&slot, tmp.path().join("ws-a")).unwrap();
    let second = init_in_slot(&slot, tmp.path().join("ws-b")).unwrap();
    let current = client_from(&slot).unwrap();

    assert!(!Arc::ptr_eq(&first, &second));
    assert!(Arc::ptr_eq(&second, &current));
}

#[tokio::test]
async fn switching_back_to_a_workspace_reuses_its_cached_client() {
    crate::test_seams::init();
    let slot = GlobalClientSlot::default();
    let tmp = TempDir::new().unwrap();
    let workspace_a = tmp.path().join("ws-a-cached");
    let workspace_b = tmp.path().join("ws-b-cached");

    let first_a = init_in_slot(&slot, workspace_a.clone()).unwrap();
    let _b = init_in_slot(&slot, workspace_b).unwrap();
    let second_a = init_in_slot(&slot, workspace_a).unwrap();

    assert!(Arc::ptr_eq(&first_a, &second_a));
    assert!(Arc::ptr_eq(&second_a, &client_from(&slot).unwrap()));
}

#[tokio::test]
async fn workspace_scoped_clients_are_cached_without_global_rebinding() {
    crate::test_seams::init();
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path().join("workspace-scoped");

    let first = client_for_workspace(&workspace).unwrap();
    let second = client_for_workspace(&workspace).unwrap();

    assert!(Arc::ptr_eq(&first, &second));
}

#[tokio::test]
async fn active_workspace_reports_an_explicit_global_binding() {
    crate::test_seams::init();
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path().join("active-workspace");
    init(workspace).unwrap();

    assert!(active_workspace_dir().is_some());
}

#[tokio::test]
async fn init_clears_existing_client_when_rebind_workspace_cannot_initialise() {
    crate::test_seams::init();
    let slot = GlobalClientSlot::default();
    let tmp = TempDir::new().unwrap();

    let _first = init_in_slot(&slot, tmp.path().join("ws-a")).unwrap();
    let file_path = tmp.path().join("not-a-directory");
    std::fs::write(&file_path, b"not a workspace").unwrap();

    let err = match init_in_slot(&slot, file_path) {
        Ok(_) => panic!("rebind to a file path must fail"),
        Err(err) => err,
    };

    assert!(err.contains("Create workspace dir"));
    assert!(client_from(&slot).is_err());
}

#[tokio::test]
async fn client_returns_a_handle_after_explicit_init() {
    crate::test_seams::init();
    // Bind TempDir at test scope so its directory outlives the global
    // client — the singleton holds the path and may be used later in
    // this test binary.
    let tmp = TempDir::new().unwrap();
    // Explicit init: client() no longer lazily initialises.
    let _ = client_if_ready().or_else(|| init(tmp.path().join("ws")).ok());
    let c = client().expect("global client should be available after init");
    let _arc: Arc<MemoryClient> = c;
}

#[tokio::test]
async fn client_errs_clearly_when_not_initialised() {
    crate::test_seams::init();
    // Use a fresh local `OnceLock` rather than the process-global one:
    // other tests may have already called `init()` on the singleton, so
    // an `is_none`-gated check on `GLOBAL_CLIENT` would race / silently
    // skip. `client_from` lets us assert the contract deterministically.
    let local = GlobalClientSlot::default();
    match client_from(&local) {
        Ok(_) => panic!("client_from(empty) must error"),
        Err(err) => assert!(
            err.contains("init"),
            "error should mention init contract, got: {err}"
        ),
    }
}
