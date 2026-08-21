//! Tests for the surrounding module.

use super::*;

#[test]
fn memory_kind_as_str_matches_all_catalog_entries() {
    let kinds = [
        MemoryKind::Raw,
        MemoryKind::Chunk,
        MemoryKind::Entity,
        MemoryKind::Tree,
        MemoryKind::Vector,
        MemoryKind::Kv,
        MemoryKind::Contact,
    ];
    let labels: Vec<&str> = kinds.iter().map(|k| k.as_str()).collect();
    let all: Vec<&str> = MemoryKind::ALL.iter().map(|k| k.as_str()).collect();
    assert_eq!(labels, all);
}

#[test]
fn memory_kind_serde_uses_snake_case() {
    let raw = serde_json::to_string(&MemoryKind::Raw).unwrap();
    let tree = serde_json::to_string(&MemoryKind::Tree).unwrap();
    assert_eq!(raw, "\"raw\"");
    assert_eq!(tree, "\"tree\"");

    let decoded: MemoryKind = serde_json::from_str("\"contact\"").unwrap();
    assert_eq!(decoded, MemoryKind::Contact);
}
