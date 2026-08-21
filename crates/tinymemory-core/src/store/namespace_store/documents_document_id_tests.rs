//! Tests for the surrounding module.

use super::UnifiedMemory;

/// Two concurrent first-writes of one key must choose the SAME document
/// id. If they do not, each writes `vector_chunks` under its own id, the
/// `ON CONFLICT(namespace, key)` row keeps only one of them, and the
/// loser's chunks outlive `forget` — deleted content stays recallable.
#[test]
fn the_id_is_derived_from_namespace_and_key_not_random() {
    let a = UnifiedMemory::derive_document_id("notes", "q3-plan");
    let b = UnifiedMemory::derive_document_id("notes", "q3-plan");
    assert_eq!(a, b, "the same key must derive the same id");
    assert_ne!(
        a,
        UnifiedMemory::derive_document_id("notes", "q4-plan"),
        "different keys must not collide"
    );
    assert_ne!(
        a,
        UnifiedMemory::derive_document_id("other", "q3-plan"),
        "the namespace must participate"
    );
}

/// The guard must be per key, not global: two different keys writing at
/// once must not serialise, or every concurrent write in the process
/// queues behind one slow embedding.
#[test]
fn the_write_lock_is_per_key_and_shared_per_key() {
    let db = std::path::Path::new("/w/memory/memory.db");
    let a1 = UnifiedMemory::document_write_lock(db, "notes", "k1");
    let a2 = UnifiedMemory::document_write_lock(db, "notes", "k1");
    let b = UnifiedMemory::document_write_lock(db, "notes", "k2");
    let other_ns = UnifiedMemory::document_write_lock(db, "other", "k1");
    let other_db = UnifiedMemory::document_write_lock(
        std::path::Path::new("/w2/memory/memory.db"),
        "notes",
        "k1",
    );
    assert!(
        std::sync::Arc::ptr_eq(&a1, &a2),
        "same key must share one lock"
    );
    assert!(
        !std::sync::Arc::ptr_eq(&a1, &b),
        "different keys must not contend"
    );
    assert!(
        !std::sync::Arc::ptr_eq(&a1, &other_ns),
        "the namespace must participate"
    );
    assert!(
        !std::sync::Arc::ptr_eq(&a1, &other_db),
        "two workspaces must not contend"
    );
}

/// The separator matters: without it ("a","bc") and ("ab","c") hash the
/// same bytes and two distinct records share one id.
#[test]
fn the_namespace_key_boundary_cannot_be_shifted() {
    assert_ne!(
        UnifiedMemory::derive_document_id("a", "bc"),
        UnifiedMemory::derive_document_id("ab", "c")
    );
}
