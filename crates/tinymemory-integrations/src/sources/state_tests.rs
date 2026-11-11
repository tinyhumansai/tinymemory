//! Source state wire format and restart recovery.

use super::*;

#[test]
fn source_state_round_trips_with_host_defaults() {
    let json = serde_json::json!({
        "status": "error",
        "error": "reader failed",
        "items": 4
    });
    let state: SourceState = serde_json::from_value(json).unwrap();
    assert_eq!(state.status, SourceStatus::Error);
    assert_eq!(state.last_sync_at, None);
    assert_eq!(state.error.as_deref(), Some("reader failed"));
    assert_eq!(state.items, 4);
    assert_eq!(serde_json::to_value(state).unwrap()["status"], "error");
}

#[test]
fn reset_interrupted_only_changes_syncing_status() {
    let mut all = BTreeMap::from([
        (
            "running".to_string(),
            SourceState {
                status: SourceStatus::Syncing,
                items: 9,
                ..SourceState::default()
            },
        ),
        (
            "failed".to_string(),
            SourceState {
                status: SourceStatus::Error,
                error: Some("kept".into()),
                ..SourceState::default()
            },
        ),
    ]);
    reset_interrupted(&mut all);
    assert_eq!(all["running"].status, SourceStatus::Idle);
    assert_eq!(all["running"].items, 9);
    assert_eq!(all["failed"].status, SourceStatus::Error);
    assert_eq!(all["failed"].error.as_deref(), Some("kept"));
}
