//! Tests for the surrounding module.

use super::*;

#[tokio::test]
async fn scope_sets_and_clears_thread_id() {
    assert!(current_thread_id().is_none(), "baseline outside scope");
    with_thread_id("thread-123", async {
        assert_eq!(current_thread_id().as_deref(), Some("thread-123"));
    })
    .await;
    assert!(
        current_thread_id().is_none(),
        "thread_id must not leak past scope"
    );
}

#[tokio::test]
async fn empty_or_whitespace_id_normalizes_to_none() {
    with_thread_id("   ", async {
        assert!(current_thread_id().is_none());
    })
    .await;
    with_thread_id("", async {
        assert!(current_thread_id().is_none());
    })
    .await;
}

#[tokio::test]
async fn nested_scope_overrides_outer() {
    with_thread_id("outer", async {
        assert_eq!(current_thread_id().as_deref(), Some("outer"));
        with_thread_id("inner", async {
            assert_eq!(current_thread_id().as_deref(), Some("inner"));
        })
        .await;
        assert_eq!(current_thread_id().as_deref(), Some("outer"));
    })
    .await;
}

#[tokio::test]
async fn spawned_task_inherits_via_explicit_propagation() {
    // tokio::task_local does not propagate across spawn by default.
    // Document the expected pattern: capture before spawning.
    with_thread_id("propagated", async {
        let captured = current_thread_id();
        let handle = tokio::spawn(async move {
            with_thread_id(captured.unwrap_or_default(), async { current_thread_id() }).await
        });
        let observed = handle.await.unwrap();
        assert_eq!(observed.as_deref(), Some("propagated"));
    })
    .await;
}
