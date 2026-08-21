//! Tests for the surrounding module.

use super::*;

#[test]
fn memory_store_reexports_expected_memory_kind_catalog() {
    assert!(MemoryKind::ALL.contains(&MemoryKind::Chunk));
    assert!(MemoryKind::ALL.contains(&MemoryKind::Tree));
    assert!(MemoryKind::ALL.contains(&MemoryKind::Contact));
}
