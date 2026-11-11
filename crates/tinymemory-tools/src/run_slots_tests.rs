//! Tests for shared start gates and per-driver run-slot reservations.

use super::*;

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Barrier, mpsc};
use std::thread;
use std::time::Duration;

fn shared_gate() -> impl StartGate + Clone {
    super::RunStartGate::new()
}

#[test]
fn default_gate_forwards_false_and_remains_usable() {
    let gate = super::RunStartGate::default();
    let mut called = false;

    assert_eq!(format!("{gate:?}"), "RunStartGate { .. }");
    assert!(!gate.with_gate(&mut || {
        called = true;
        false
    }));
    assert!(called, "the callback result is returned");
    assert!(gate.with_gate(&mut || true), "the gate remains reusable");
}

fn reserve_if_peer_idle<G: StartGate>(
    gate: &G,
    own: &RunSlots<String>,
    peer: &RunSlots<String>,
    key: String,
) -> Option<RunGuard<String>> {
    let mut reservation = None;
    gate.with_gate(&mut || {
        if peer.is_running(&key) {
            return false;
        }
        reservation = own.try_acquire(key.clone());
        reservation.is_some()
    });
    reservation
}

#[test]
fn shared_gate_excludes_peer_runs_across_separate_slot_registries() {
    let gate = shared_gate();
    let import_slots = RunSlots::new();
    let migration_slots = RunSlots::new();
    let key = String::from("workspace");
    let barrier = Arc::new(Barrier::new(2));
    let (import_tx, import_rx) = mpsc::channel();
    let (migration_tx, migration_rx) = mpsc::channel();

    let import_thread = {
        let gate = gate.clone();
        let own = import_slots.clone();
        let peer = migration_slots.clone();
        let barrier = Arc::clone(&barrier);
        let key = key.clone();
        thread::spawn(move || {
            barrier.wait();
            import_tx
                .send(reserve_if_peer_idle(&gate, &own, &peer, key))
                .unwrap();
        })
    };
    let migration_thread = {
        let gate = gate.clone();
        let own = migration_slots.clone();
        let peer = import_slots.clone();
        let barrier = Arc::clone(&barrier);
        let key = key.clone();
        thread::spawn(move || {
            barrier.wait();
            migration_tx
                .send(reserve_if_peer_idle(&gate, &own, &peer, key))
                .unwrap();
        })
    };

    let import_reservation = import_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("import reservation should finish without deadlock");
    let migration_reservation = migration_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("migration reservation should finish without deadlock");
    import_thread.join().unwrap();
    migration_thread.join().unwrap();

    assert_ne!(
        import_reservation.is_some(),
        migration_reservation.is_some()
    );
    assert_ne!(
        import_slots.is_running(&key),
        migration_slots.is_running(&key),
        "each driver's status reflects only its own registry"
    );
    assert_eq!(import_reservation.is_some(), import_slots.is_running(&key));
    assert_eq!(
        migration_reservation.is_some(),
        migration_slots.is_running(&key)
    );
}

#[test]
fn shared_gate_recovers_from_a_panicking_reservation_callback() {
    let gate = shared_gate();
    let panic = catch_unwind(AssertUnwindSafe(|| {
        gate.with_gate(&mut || panic!("reservation callback failed"));
    }));
    assert!(panic.is_err(), "the callback panic propagates");

    let mut called = false;
    assert!(gate.with_gate(&mut || {
        called = true;
        true
    }));
    assert!(called, "the poisoned gate remains usable");
}

#[test]
fn released_run_slot_can_be_reserved_again_through_the_shared_gate() {
    let gate = shared_gate();
    let own = RunSlots::new();
    let peer = RunSlots::new();
    let key = String::from("workspace");

    let first = reserve_if_peer_idle(&gate, &own, &peer, key.clone());
    assert!(first.is_some());
    assert!(own.is_running(&key));
    drop(first);
    assert!(!own.is_running(&key));

    let second = reserve_if_peer_idle(&gate, &own, &peer, key.clone());
    assert!(second.is_some());
    assert!(own.is_running(&key));
}

#[test]
fn shared_gate_allows_runs_for_unrelated_keys_in_separate_registries() {
    let gate = shared_gate();
    let import_slots = RunSlots::new();
    let migration_slots = RunSlots::new();
    let import_key = String::from("import-workspace");
    let migration_key = String::from("migration-workspace");

    let import_reservation =
        reserve_if_peer_idle(&gate, &import_slots, &migration_slots, import_key.clone());
    let migration_reservation = reserve_if_peer_idle(
        &gate,
        &migration_slots,
        &import_slots,
        migration_key.clone(),
    );

    assert!(import_reservation.is_some());
    assert!(migration_reservation.is_some());
    assert!(import_slots.is_running(&import_key));
    assert!(migration_slots.is_running(&migration_key));
}
