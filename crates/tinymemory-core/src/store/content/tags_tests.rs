//! Tests for source-tag preservation and atomic summary replacement.

use chrono::{TimeZone, Utc};
use tinymemory_api::host::MemoryHostConfig;

use super::{augment_with_source_tag, update_summary_tags, write_atomically};
use crate::engine::backend::score::extract::EntityKind;
use crate::engine::backend::score::resolver::CanonicalEntity;
use crate::store::chunks::store::with_connection;
use crate::store::content::{atomic, compose, StagedSummary};
use crate::store::trees::store::{get_tree, insert_summary_tx, insert_tree};
use crate::store::trees::{SummaryNode, Tree, TreeKind, TreeStatus};
use crate::tree::score::store::index_entities;

fn test_config() -> (
    tempfile::TempDir,
    tinymemory_api::host::test_support::TestHostConfig,
) {
    crate::test_seams::init();
    let directory = tempfile::tempdir().expect("temporary workspace");
    let mut config = tinymemory_api::host::test_support::TestHostConfig::default();
    config.workspace_dir = directory.path().to_path_buf();
    (directory, config)
}

fn insert_staged_summary(
    config: &crate::Config,
    id: &str,
    relative_path: &str,
    expected_sha: &str,
) {
    let timestamp = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    if get_tree(config, "tree-1")
        .expect("read fixture tree")
        .is_none()
    {
        insert_tree(
            config,
            &Tree {
                id: "tree-1".into(),
                kind: TreeKind::Source,
                scope: "github:acme/widget".into(),
                root_id: None,
                max_level: 0,
                status: TreeStatus::Active,
                created_at: timestamp,
                last_sealed_at: None,
                ask: None,
            },
        )
        .expect("insert fixture tree");
    }
    let summary = SummaryNode {
        id: id.into(),
        tree_id: "tree-1".into(),
        tree_kind: TreeKind::Source,
        level: 1,
        parent_id: None,
        child_ids: vec!["child-1".into()],
        content: "summary body".into(),
        token_count: 2,
        entities: Vec::new(),
        topics: Vec::new(),
        time_range_start: timestamp,
        time_range_end: timestamp,
        score: 0.8,
        sealed_at: timestamp,
        deleted: false,
        embedding: None,
        doc_id: None,
        version_ms: None,
    };
    let staged = StagedSummary {
        summary_id: id.into(),
        content_path: relative_path.into(),
        content_sha256: expected_sha.into(),
    };
    with_connection(config, |connection| {
        let transaction = connection.unchecked_transaction()?;
        insert_summary_tx(&transaction, &summary, Some(&staged), "test")?;
        transaction.commit()?;
        Ok(())
    })
    .expect("insert staged summary");
}

fn entity(id: &str, kind: EntityKind, surface: &str) -> CanonicalEntity {
    CanonicalEntity {
        canonical_id: id.into(),
        kind,
        surface: surface.into(),
        span_start: 0,
        span_end: surface.len() as u32,
        score: 0.9,
    }
}

#[test]
fn update_summary_tags_uses_authoritative_index_and_preserves_body_integrity() {
    let (_directory, config) = test_config();
    let body = "The body must remain byte-for-byte identical.\n";
    let relative_path = "summaries/tree-1/summary-1.md";
    let absolute_path = config.memory_tree_content_root().join(relative_path);
    std::fs::create_dir_all(absolute_path.parent().expect("content parent"))
        .expect("create content parent");
    let original = format!(
        "---\ntree_kind: source\ntree_scope: github:acme/widget\ntags:\n  - stale/tag\naliases: []\n---\n{body}"
    );
    std::fs::write(&absolute_path, original).expect("write summary");
    let expected_sha = atomic::sha256_hex(body.as_bytes());
    insert_staged_summary(&config, "summary-1", relative_path, &expected_sha);
    index_entities(
        &config,
        &[
            entity("person:Zed Person", EntityKind::Person, "Zed Person"),
            entity("organization:Acme Co", EntityKind::Organization, "Acme Co"),
            entity("person:Zed Person", EntityKind::Person, "Zed Person"),
        ],
        "summary-1",
        "summary",
        200,
        Some("tree-1"),
    )
    .expect("index authoritative entities");

    update_summary_tags(&config, "summary-1").expect("rewrite summary tags");

    let rewritten = std::fs::read_to_string(&absolute_path).expect("read rewritten summary");
    let (front_matter, rewritten_body) =
        compose::split_front_matter(&rewritten).expect("rewritten front matter must remain valid");
    assert_eq!(rewritten_body, body);
    assert_eq!(atomic::sha256_hex(rewritten_body.as_bytes()), expected_sha);
    let source_tag = compose::source_tag("github:acme/widget");
    let source_position = front_matter
        .find(&format!("  - {source_tag}"))
        .expect("source tag");
    let organization_position = front_matter
        .find("  - organization/Acme-Co")
        .expect("organization tag");
    let person_position = front_matter
        .find("  - person/Zed-Person")
        .expect("person tag");
    assert!(source_position < organization_position && organization_position < person_position);
    assert_eq!(front_matter.matches("person/Zed-Person").count(), 1);
    assert!(!front_matter.contains("stale/tag"));
    let files = std::fs::read_dir(absolute_path.parent().expect("content parent"))
        .expect("content directory")
        .collect::<Result<Vec<_>, _>>()
        .expect("content entries");
    assert_eq!(files.len(), 1, "atomic rewrite must leave no tempfile");
}

#[test]
fn update_summary_tags_skips_missing_rows_and_files_but_rejects_sha_mismatch() {
    let (_directory, config) = test_config();
    update_summary_tags(&config, "not-in-database").expect("missing summary is a no-op");

    let body = "intact body\n";
    let expected_sha = atomic::sha256_hex(body.as_bytes());
    insert_staged_summary(
        &config,
        "missing-file",
        "summaries/missing.md",
        &expected_sha,
    );
    update_summary_tags(&config, "missing-file").expect("missing file is a no-op");

    let relative_path = "summaries/mismatch.md";
    let absolute_path = config.memory_tree_content_root().join(relative_path);
    std::fs::create_dir_all(absolute_path.parent().expect("content parent"))
        .expect("create content parent");
    std::fs::write(
        &absolute_path,
        format!("---\ntree_kind: source\ntree_scope: scope\ntags: []\n---\n{body}"),
    )
    .expect("write mismatched summary");
    insert_staged_summary(&config, "sha-mismatch", relative_path, "deadbeef");

    let error = update_summary_tags(&config, "sha-mismatch")
        .expect_err("stored SHA mismatch must be reported");
    assert!(error.to_string().contains("body mutated after rewrite"));
    let after = std::fs::read_to_string(&absolute_path).expect("read mismatched summary");
    let (_, after_body) = compose::split_front_matter(&after).expect("front matter");
    assert_eq!(after_body, body);
    assert_eq!(atomic::sha256_hex(after_body.as_bytes()), expected_sha);
}

#[test]
fn source_front_matter_prepends_scope_tag_and_deduplicates_it() {
    let markdown = b"---\ntree_kind: source\ntree_scope: github/acme/widget\n---\nbody\n";
    let source = crate::store::content::compose::source_tag("github/acme/widget");
    let tags = vec!["entity/person/alice".to_string(), source.clone()];

    assert_eq!(
        augment_with_source_tag(markdown, &tags),
        vec![source, "entity/person/alice".to_string()]
    );
}

#[test]
fn source_tag_is_not_invented_for_invalid_or_non_source_documents() {
    let tags = vec!["entity/topic/rust".to_string()];
    for markdown in [
        b"not front matter".as_slice(),
        b"---\ntree_kind: summary\ntree_scope: scope\n---\nbody".as_slice(),
        b"---\ntree_kind: source\n---\nbody".as_slice(),
        &[0xff, 0xfe],
    ] {
        assert_eq!(augment_with_source_tag(markdown, &tags), tags);
    }
}

#[test]
fn atomic_write_replaces_existing_content_without_leaving_tempfiles() {
    let directory = tempfile::tempdir().expect("temp directory");
    let path = directory.path().join("summary.md");
    std::fs::write(&path, b"old").expect("seed summary");

    write_atomically(&path, b"new content").expect("atomic replacement");

    assert_eq!(std::fs::read(&path).expect("read summary"), b"new content");
    let entries = std::fs::read_dir(directory.path())
        .expect("read directory")
        .collect::<Result<Vec<_>, _>>()
        .expect("directory entries");
    assert_eq!(entries.len(), 1);
}

#[test]
fn atomic_write_reports_missing_parent_without_leaving_a_file() {
    let directory = tempfile::tempdir().expect("temp directory");
    let path = directory.path().join("missing").join("summary.md");

    let error = write_atomically(&path, b"content").expect_err("missing parent must fail");

    assert!(error.to_string().contains("create tag tempfile"));
    assert!(!path.exists());
}
