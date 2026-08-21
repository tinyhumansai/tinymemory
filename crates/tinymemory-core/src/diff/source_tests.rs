//! Tests for the surrounding module.

use super::*;
use crate::sources::types::SourceKind;

fn folder_source(id: &str) -> MemorySourceEntry {
    MemorySourceEntry {
        id: id.into(),
        kind: SourceKind::Folder,
        label: "Docs".into(),
        enabled: true,
        toolkit: None,
        connection_id: None,
        path: Some("/tmp".into()),
        glob: None,
        url: None,
        branch: None,
        paths: Vec::new(),
        query: None,
        since_days: None,
        max_items: None,
        max_commits: None,
        max_issues: None,
        max_prs: None,
        selector: None,
        max_tokens_per_sync: None,
        max_cost_per_sync_usd: None,
        sync_depth_days: None,
    }
}

/// The prefix scheme itself is covered where it is defined
/// (`sources::status::source_id_prefix_dispatch`). What matters here is
/// that the adapter resolves through *that* definition, so a snapshot and
/// a status agree on which chunks belong to a source.
#[test]
fn the_adapter_resolves_prefixes_through_the_shared_definition() {
    let source = folder_source("src_abc");
    let adapter =
        ChunkStoreItemSource::single(std::sync::Arc::new(TestHostConfig::default()), &source);
    assert_eq!(
        adapter.prefixes.get("src_abc").map(String::as_str),
        Some(crate::sources::status::source_id_prefix(&source).as_str())
    );
}

#[test]
fn read_only_adapter_never_yields_items() {
    let source = ChunkStoreItemSource::read_only(
        std::sync::Arc::new(TestHostConfig::default()) as std::sync::Arc<crate::Config>
    );
    assert!(source.items_for_source("anything").is_empty());
}
