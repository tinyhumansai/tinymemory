use super::*;

#[test]
fn a_new_store_holds_a_fresh_state() {
    assert_eq!(
        MemoryStateStore::default().load().unwrap(),
        MigrationState::default()
    );
}

#[test]
fn a_saved_state_loads_back() {
    let store = MemoryStateStore::default();
    let state = MigrationState {
        phase: Phase::Paused,
        cursor: Some("l00ff".into()),
        copied: 7,
        replayed: 2,
        failures: vec![Failure {
            id: "abc".into(),
            reason: "invalid_request".into(),
        }],
        incomplete: vec!["def".into()],
        error: Some("background work is paused".into()),
        cleaning: true,
        switched: true,
        caught_up: false,
        takeover: true,
        rechecks: 1,
        rechecked: true,
    };
    store.save(&state).unwrap();
    assert_eq!(store.load().unwrap(), state);
}

#[test]
fn the_state_round_trips_through_json_and_omits_empty_fields() {
    let state = MigrationState::default();
    let json = serde_json::to_value(&state).unwrap();
    assert!(json.get("cursor").is_none());
    assert!(json.get("failures").is_none());
    assert!(json.get("rechecks").is_none());
    assert_eq!(
        serde_json::from_value::<MigrationState>(json).unwrap(),
        state
    );
}
