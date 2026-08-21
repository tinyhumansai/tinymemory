//! Tests for active-connection filtering at the Composio sync boundary.

use super::*;

fn connection(toolkit: &str, status: &str) -> ComposioConnection {
    serde_json::from_value(serde_json::json!({
        "id": format!("connection-{toolkit}"),
        "toolkit": toolkit,
        "status": status
    }))
    .unwrap()
}

#[test]
fn connection_target_requires_active_registered_provider() {
    init_default_composio_sync_providers();
    assert!(connection_to_sync_target(connection("gmail", "inactive")).is_none());
    assert!(connection_to_sync_target(connection("unknown", "active")).is_none());
    let target = connection_to_sync_target(connection("GMAIL", "active")).unwrap();
    assert_eq!(target.toolkit, "gmail");
    assert_eq!(target.connection_id, "connection-GMAIL");
}
