//! Tests for the surrounding module.

use super::*;
use crate::store::chunks::store::upsert_chunks;
use crate::store::chunks::types::{Chunk, Metadata};
use chrono::{TimeZone, Utc};
use tempfile::TempDir;
use tinymemory_api::host::NoopEmbedding;

fn test_config() -> (TempDir, TestHostConfig) {
    crate::test_seams::init();
    let tmp = TempDir::new().unwrap();
    let mut cfg = TestHostConfig::default();
    cfg.workspace_dir = tmp.path().to_path_buf();
    (tmp, cfg)
}

fn test_facade(tmp: &TempDir) -> RetrievalFacade {
    let unified = UnifiedMemory::new(tmp.path(), Arc::new(NoopEmbedding), None).unwrap();
    RetrievalFacade::new(Arc::new(unified))
}

fn chunk(id: &str, source_kind: SourceKind, source_id: &str, owner: &str, tags: &[&str]) -> Chunk {
    chunk_at(id, source_kind, source_id, owner, tags, Utc::now())
}

fn chunk_at(
    id: &str,
    source_kind: SourceKind,
    source_id: &str,
    owner: &str,
    tags: &[&str],
    ts: chrono::DateTime<Utc>,
) -> Chunk {
    Chunk {
        id: id.into(),
        content: format!("content for {id}"),
        metadata: Metadata {
            source_kind,
            source_id: source_id.into(),
            owner: owner.into(),
            timestamp: ts,
            time_range: (ts, ts),
            tags: tags.iter().map(|s| (*s).to_string()).collect(),
            source_ref: None,
            path_scope: None,
        },
        token_count: 3,
        seq_in_source: 0,
        created_at: ts,
        partial_message: false,
    }
}

#[test]
fn param_tag_filters_default_to_no_constraints() {
    let filters = ParamTagFilters::default();
    assert!(filters.source_kind.is_none());
    assert!(filters.source_id.is_none());
    assert!(filters.owner.is_none());
    assert!(filters.since_ms.is_none());
    assert!(filters.until_ms.is_none());
    assert!(filters.tags_all_of.is_none());
    assert!(filters.limit.is_none());
}

#[test]
fn param_tag_search_filters_by_tags_all_of() {
    let (tmp, cfg) = test_config();
    let facade = test_facade(&tmp);
    upsert_chunks(
        &cfg,
        &[
            chunk(
                "c1",
                SourceKind::Chat,
                "slack:#eng",
                "alice",
                &["person:alice", "deploy"],
            ),
            chunk(
                "c2",
                SourceKind::Chat,
                "slack:#eng",
                "alice",
                &["person:alice"],
            ),
            chunk(
                "c3",
                SourceKind::Email,
                "gmail:thread-1",
                "bob",
                &["deploy"],
            ),
        ],
    )
    .unwrap();

    let filters = ParamTagFilters {
        tags_all_of: Some(vec!["person:alice".into(), "deploy".into()]),
        ..ParamTagFilters::default()
    };
    let hits = facade.param_tag_search(&cfg, &filters).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, "c1");
}

#[test]
fn param_tag_search_respects_source_kind_filter() {
    let (tmp, cfg) = test_config();
    let facade = test_facade(&tmp);
    upsert_chunks(
        &cfg,
        &[
            chunk("c1", SourceKind::Chat, "slack:#eng", "alice", &[]),
            chunk("c2", SourceKind::Email, "gmail:thread-1", "alice", &[]),
        ],
    )
    .unwrap();

    let filters = ParamTagFilters {
        source_kind: Some(SourceKind::Email),
        ..ParamTagFilters::default()
    };
    let hits = facade.param_tag_search(&cfg, &filters).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, "c2");
}

#[test]
fn param_tag_search_respects_source_id_owner_and_limit() {
    let (tmp, cfg) = test_config();
    let facade = test_facade(&tmp);
    upsert_chunks(
        &cfg,
        &[
            chunk("c1", SourceKind::Chat, "slack:#eng", "alice", &[]),
            chunk("c2", SourceKind::Chat, "slack:#eng", "bob", &[]),
            chunk("c3", SourceKind::Chat, "slack:#ops", "alice", &[]),
        ],
    )
    .unwrap();

    let filters = ParamTagFilters {
        source_id: Some("slack:#eng".into()),
        owner: Some("alice".into()),
        limit: Some(1),
        ..ParamTagFilters::default()
    };
    let hits = facade.param_tag_search(&cfg, &filters).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, "c1");
    assert_eq!(hits[0].metadata.source_id, "slack:#eng");
    assert_eq!(hits[0].metadata.owner, "alice");
}

#[test]
fn param_tag_search_empty_required_tags_is_noop() {
    let (tmp, cfg) = test_config();
    let facade = test_facade(&tmp);
    upsert_chunks(
        &cfg,
        &[
            chunk("c1", SourceKind::Chat, "slack:#eng", "alice", &["deploy"]),
            chunk(
                "c2",
                SourceKind::Email,
                "gmail:thread-1",
                "bob",
                &["person:bob"],
            ),
        ],
    )
    .unwrap();

    let hits = facade
        .param_tag_search(
            &cfg,
            &ParamTagFilters {
                tags_all_of: Some(vec![]),
                ..ParamTagFilters::default()
            },
        )
        .unwrap();
    assert_eq!(hits.len(), 2);
}

#[test]
fn param_tag_search_respects_since_and_until_bounds() {
    let (tmp, cfg) = test_config();
    let facade = test_facade(&tmp);
    let older = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let newer = Utc.timestamp_millis_opt(1_700_100_000_000).unwrap();
    upsert_chunks(
        &cfg,
        &[
            chunk_at("c1", SourceKind::Chat, "slack:#eng", "alice", &[], older),
            chunk_at("c2", SourceKind::Chat, "slack:#eng", "alice", &[], newer),
        ],
    )
    .unwrap();

    let hits = facade
        .param_tag_search(
            &cfg,
            &ParamTagFilters {
                since_ms: Some(newer.timestamp_millis()),
                until_ms: Some(newer.timestamp_millis()),
                ..ParamTagFilters::default()
            },
        )
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, "c2");
}

#[test]
fn param_tag_search_returns_empty_when_required_tag_is_missing() {
    let (tmp, cfg) = test_config();
    let facade = test_facade(&tmp);
    upsert_chunks(
        &cfg,
        &[chunk(
            "c1",
            SourceKind::Chat,
            "slack:#eng",
            "alice",
            &["deploy"],
        )],
    )
    .unwrap();

    let hits = facade
        .param_tag_search(
            &cfg,
            &ParamTagFilters {
                tags_all_of: Some(vec!["person:bob".into()]),
                ..ParamTagFilters::default()
            },
        )
        .unwrap();
    assert!(hits.is_empty());
}
