//! Tests for the surrounding module.

use super::*;
use crate::thread_context::with_thread_id;

#[tokio::test]
async fn resolves_the_ambient_thread_id_inside_a_turn() {
    let resolved = with_thread_id("thread-xyz", async { current_self_echo_exclusion() }).await;
    assert_eq!(resolved.as_deref(), Some("thread-xyz"));
}

#[tokio::test]
async fn resolves_to_none_outside_any_turn() {
    assert_eq!(current_self_echo_exclusion(), None);
}
