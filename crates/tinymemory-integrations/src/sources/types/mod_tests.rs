//! Tests for source type contracts, serde wire strings, and validation.

use super::*;

#[test]
fn source_kind_round_trips_via_serde() {
    for kind in [
        SourceKind::Conversation,
        SourceKind::Folder,
        SourceKind::File,
        SourceKind::GithubRepo,
        SourceKind::RssFeed,
        SourceKind::WebPage,
    ] {
        let json = serde_json::to_string(&kind).unwrap();
        let decoded: SourceKind = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, kind);
    }
}

#[test]
fn source_kind_as_str_matches_wire_strings() {
    assert_eq!(SourceKind::Conversation.as_str(), "conversation");
    assert_eq!(SourceKind::Folder.as_str(), "folder");
    assert_eq!(SourceKind::GithubRepo.as_str(), "github_repo");
    assert_eq!(SourceKind::File.as_str(), "file");
    assert_eq!(SourceKind::RssFeed.as_str(), "rss_feed");
    assert_eq!(SourceKind::WebPage.as_str(), "web_page");
}

#[test]
fn validate_folder_requires_path() {
    let entry = MemorySourceEntry {
        id: "src_2".into(),
        kind: SourceKind::Folder,
        label: "Notes".into(),
        enabled: true,
        path: None,
        ..default_entry()
    };
    assert!(entry.validate().is_err());
}

#[test]
fn validate_github_requires_url() {
    let entry = MemorySourceEntry {
        id: "src_3".into(),
        kind: SourceKind::GithubRepo,
        label: "Repo".into(),
        enabled: true,
        url: Some("https://github.com/org/repo".into()),
        ..default_entry()
    };
    assert!(entry.validate().is_ok());
}

#[test]
fn validate_file_requires_path() {
    let entry = MemorySourceEntry::new("src_file", SourceKind::File, "One file");
    assert!(entry.validate().is_err());
    let valid = MemorySourceEntry {
        path: Some("notes/plan.md".into()),
        ..entry
    };
    assert!(valid.validate().is_ok());
}

#[test]
fn the_removed_twitter_query_kind_no_longer_decodes() {
    for removed in ["twitter_query", "composio"] {
        let decoded = serde_json::from_str::<SourceKind>(&format!("\"{removed}\""));
        assert!(decoded.is_err(), "{removed} must not decode");
    }
}

#[test]
fn every_config_kind_maps_onto_a_contract_source_kind() {
    use tinymemory_api::SourceKind as Api;
    let mapped: Vec<Api> = SourceKind::ALL.iter().map(SourceKind::api_kind).collect();
    assert_eq!(
        mapped,
        vec![
            Api::Conversation,
            Api::Folder,
            Api::File,
            Api::Github,
            Api::Rss,
            Api::Link,
        ]
    );
}

#[test]
fn validate_rss_and_web_page_require_url() {
    let rss = MemorySourceEntry {
        id: "src_rss".into(),
        kind: SourceKind::RssFeed,
        label: "Feed".into(),
        enabled: true,
        url: None,
        ..default_entry()
    };
    assert!(rss.validate().is_err());

    let web = MemorySourceEntry {
        id: "src_web".into(),
        kind: SourceKind::WebPage,
        label: "Page".into(),
        enabled: true,
        url: Some("https://example.com".into()),
        ..default_entry()
    };
    assert!(web.validate().is_ok());
}

#[test]
fn validate_conversation_needs_only_id_and_label() {
    let entry = MemorySourceEntry {
        id: "src_conv".into(),
        kind: SourceKind::Conversation,
        label: "Agent Conversations".into(),
        enabled: true,
        ..default_entry()
    };
    assert!(entry.validate().is_ok());
}

#[test]
fn validate_conversation_fails_with_empty_id() {
    let entry = MemorySourceEntry {
        id: "".into(),
        kind: SourceKind::Conversation,
        label: "Convos".into(),
        enabled: true,
        ..default_entry()
    };
    assert!(entry.validate().is_err());
}

#[test]
fn validate_conversation_fails_with_empty_label() {
    let entry = MemorySourceEntry {
        id: "src_conv".into(),
        kind: SourceKind::Conversation,
        label: "".into(),
        enabled: true,
        ..default_entry()
    };
    assert!(entry.validate().is_err());
}

#[test]
fn conversation_kind_serializes_to_snake_case() {
    let json = serde_json::to_string(&SourceKind::Conversation).unwrap();
    assert_eq!(json, "\"conversation\"");
}

#[test]
fn content_type_serializes_to_snake_case() {
    assert_eq!(
        serde_json::to_string(&ContentType::Markdown).unwrap(),
        "\"markdown\""
    );
    assert_eq!(
        serde_json::to_string(&ContentType::Html).unwrap(),
        "\"html\""
    );
    assert_eq!(
        serde_json::to_string(&ContentType::Plaintext).unwrap(),
        "\"plaintext\""
    );
}

#[test]
fn toml_round_trip() {
    let entry = MemorySourceEntry {
        id: "src_1".into(),
        kind: SourceKind::Folder,
        label: "My notes".into(),
        enabled: true,
        path: Some("/tmp/notes".into()),
        glob: Some("**/*.md".into()),
        ..default_entry()
    };
    let toml_str = toml::to_string_pretty(&entry).unwrap();
    let decoded: MemorySourceEntry = toml::from_str(&toml_str).unwrap();
    assert_eq!(decoded.id, "src_1");
    assert_eq!(decoded.kind, SourceKind::Folder);
    assert_eq!(decoded.path.as_deref(), Some("/tmp/notes"));
}

#[test]
fn conversation_toml_round_trip() {
    let entry = MemorySourceEntry {
        id: "src_conv".into(),
        kind: SourceKind::Conversation,
        label: "Conversations".into(),
        enabled: true,
        ..default_entry()
    };
    let toml_str = toml::to_string_pretty(&entry).unwrap();
    let decoded: MemorySourceEntry = toml::from_str(&toml_str).unwrap();
    assert_eq!(decoded.id, "src_conv");
    assert_eq!(decoded.kind, SourceKind::Conversation);
    assert_eq!(decoded.label, "Conversations");
    assert!(decoded.enabled);
}

#[test]
fn enabled_defaults_to_true_when_absent() {
    let toml_str = r#"
id = "src_x"
kind = "conversation"
label = "Convos"
"#;
    let decoded: MemorySourceEntry = toml::from_str(toml_str).unwrap();
    assert!(decoded.enabled);
}

/// A fully-`None` entry used as a `..default_entry()` base in the tests above.
pub(super) fn default_entry() -> MemorySourceEntry {
    MemorySourceEntry {
        id: String::new(),
        kind: SourceKind::Folder,
        label: String::new(),
        enabled: true,
        path: None,
        glob: None,
        url: None,
        branch: None,
        paths: Vec::new(),
        max_commits: None,
        max_issues: None,
        max_prs: None,
        max_items: None,
        selector: None,
        max_tokens_per_sync: None,
        max_cost_per_sync_usd: None,
        sync_depth_days: None,
    }
}

/// Hosts persist these types in their configuration and exchange them over
/// RPC as JSON, so a renamed field or a new `SourceKind` variant is not a
/// compile error anywhere: it is a runtime failure the first time a host reads
/// a config written by another version.
///
/// These pin the full serialised shape. A failure here means the wire format
/// changed and hosts need a migration, never a local edit to the expectation.
#[test]
fn source_entry_wire_format_is_pinned() {
    let entry = MemorySourceEntry {
        id: "src_pinned".into(),
        kind: SourceKind::GithubRepo,
        label: "Pinned".into(),
        enabled: false,
        path: Some("/notes".into()),
        glob: Some("**/*.md".into()),
        url: Some("https://github.com/tinyhumansai/tinymemory".into()),
        branch: Some("main".into()),
        paths: vec!["core/src".into()],
        max_commits: Some(10),
        max_issues: Some(20),
        max_prs: Some(30),
        max_items: Some(40),
        selector: Some("article".into()),
        max_tokens_per_sync: Some(50_000),
        max_cost_per_sync_usd: Some(1.5),
        sync_depth_days: Some(90),
    };

    assert_eq!(
        serde_json::to_value(&entry).unwrap(),
        serde_json::json!({
            "id": "src_pinned",
            "kind": "github_repo",
            "label": "Pinned",
            "enabled": false,
            "path": "/notes",
            "glob": "**/*.md",
            "url": "https://github.com/tinyhumansai/tinymemory",
            "branch": "main",
            "paths": ["core/src"],
            "max_commits": 10,
            "max_issues": 20,
            "max_prs": 30,
            "max_items": 40,
            "selector": "article",
            "max_tokens_per_sync": 50000,
            "max_cost_per_sync_usd": 1.5,
            "sync_depth_days": 90
        })
    );
}

/// Every optional field is skipped when absent, so an entry carrying only its
/// required fields is a four-key object. A `skip_serializing_if` dropped from
/// one copy and not the other changes what the seam sends without changing
/// what either side compiles.
#[test]
fn an_empty_source_entry_serialises_to_its_required_fields_only() {
    let entry = MemorySourceEntry {
        id: "src_min".into(),
        label: "Minimal".into(),
        ..default_entry()
    };

    assert_eq!(
        serde_json::to_value(&entry).unwrap(),
        serde_json::json!({
            "id": "src_min",
            "kind": "folder",
            "label": "Minimal",
            "enabled": true
        })
    );
}

/// `SourceItem` crosses the same seam, on the `list_items` direction.
#[test]
fn source_item_wire_format_is_pinned() {
    assert_eq!(
        serde_json::to_value(SourceItem {
            id: "item-1".into(),
            title: "Quarterly planning".into(),
            updated_at_ms: Some(1_777_000_000_000),
        })
        .unwrap(),
        serde_json::json!({
            "id": "item-1",
            "title": "Quarterly planning",
            "updated_at_ms": 1_777_000_000_000i64
        })
    );

    assert_eq!(
        serde_json::to_value(SourceItem {
            id: "item-2".into(),
            title: "No timestamp".into(),
            updated_at_ms: None,
        })
        .unwrap(),
        serde_json::json!({ "id": "item-2", "title": "No timestamp" })
    );
}

/// `SourceContent` crosses the same seam, on the `read_item` direction.
#[test]
fn source_content_wire_format_is_pinned() {
    assert_eq!(
        serde_json::to_value(SourceContent {
            id: "item-1".into(),
            title: "Quarterly planning".into(),
            body: "# Roadmap".into(),
            content_type: ContentType::Markdown,
            metadata: serde_json::json!({ "author": "shanu" }),
        })
        .unwrap(),
        serde_json::json!({
            "id": "item-1",
            "title": "Quarterly planning",
            "body": "# Roadmap",
            "content_type": "markdown",
            "metadata": { "author": "shanu" }
        })
    );
}

#[test]
fn validate_treats_an_empty_string_field_as_missing() {
    let entry = MemorySourceEntry {
        id: "src_folder".into(),
        label: "Folder".into(),
        path: Some(String::new()),
        ..default_entry()
    };
    assert!(entry.validate().is_err());
}

#[test]
fn validate_rejects_an_id_with_a_colon_or_control_character() {
    for id in ["src:x", "src\nx"] {
        let entry = MemorySourceEntry {
            id: id.into(),
            label: "Conversation".into(),
            kind: SourceKind::Conversation,
            ..default_entry()
        };
        assert!(entry.validate().is_err(), "{id:?} must be rejected");
    }
}

#[test]
fn host_source_config_keeps_its_persisted_wire_defaults() {
    let config: MemorySourceConfig = serde_json::from_value(serde_json::json!({
        "id": "src-1",
        "kind": "github",
        "target": "https://github.com/acme/repo"
    }))
    .unwrap();
    assert_eq!(config.kind, MemorySourceKind::Github);
    assert_eq!(config.label, "");
    assert_eq!(config.schedule_mins, None);
    assert_eq!(config.namespace, None);
    assert_eq!(
        serde_json::to_value(config).unwrap(),
        serde_json::json!({
            "id": "src-1",
            "kind": "github",
            "target": "https://github.com/acme/repo",
            "label": ""
        })
    );
}

#[test]
fn memory_source_kind_parses_the_host_wire_names_case_insensitively() {
    for kind in MemorySourceKind::ALL {
        assert_eq!(MemorySourceKind::parse(kind.as_str()), Some(kind));
        assert_eq!(
            serde_json::from_str::<MemorySourceKind>(&format!("\"{}\"", kind.as_str())).unwrap(),
            kind
        );
    }
    assert_eq!(
        MemorySourceKind::parse(" FOLDER "),
        Some(MemorySourceKind::Folder)
    );
    assert_eq!(MemorySourceKind::parse("composio"), None);
}
