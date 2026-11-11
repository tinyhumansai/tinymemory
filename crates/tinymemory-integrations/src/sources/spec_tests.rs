//! Validation and layout rules for host-configured source specs.

use super::*;

fn spec(kind: MemorySourceKind, target: &str) -> SourceSpec {
    SourceSpec {
        kind,
        target: target.into(),
        schedule_mins: None,
        namespace: None,
    }
}

#[test]
fn target_normalization_preserves_local_paths_and_expands_network_targets() {
    assert_eq!(
        SourceSpec::normalize_target(MemorySourceKind::Folder, "  /home/me/notes ").unwrap(),
        "/home/me/notes"
    );
    assert_eq!(
        SourceSpec::normalize_target(MemorySourceKind::File, " notes.md ").unwrap(),
        "notes.md"
    );
    assert_eq!(
        SourceSpec::normalize_target(MemorySourceKind::Github, "owner/repo").unwrap(),
        "https://github.com/owner/repo"
    );
    assert_eq!(
        SourceSpec::normalize_target(MemorySourceKind::Github, "https://github.com/o/r").unwrap(),
        "https://github.com/o/r"
    );
    assert_eq!(
        SourceSpec::normalize_target(MemorySourceKind::Rss, "https://example.com/feed.xml")
            .unwrap(),
        "https://example.com/feed.xml"
    );
    assert_eq!(
        SourceSpec::normalize_target(MemorySourceKind::Link, "https://example.com").unwrap(),
        "https://example.com/"
    );
}

#[test]
fn target_normalization_rejects_blank_malformed_and_non_http_targets() {
    for (kind, target) in [
        (MemorySourceKind::Folder, "   "),
        (MemorySourceKind::Github, "just-one-part"),
        (MemorySourceKind::Github, "a/b/c"),
        (MemorySourceKind::Github, "/repo"),
        (MemorySourceKind::Link, "not a url"),
        (MemorySourceKind::Link, "ftp://example.com/x"),
        (MemorySourceKind::Rss, "file:///etc/passwd"),
    ] {
        assert!(
            SourceSpec::normalize_target(kind, target).is_err(),
            "{kind:?} {target}"
        );
    }
}

#[test]
fn validate_new_enforces_minimum_schedule_and_normalized_duplicate_identity() {
    let existing = [MemorySourceConfig {
        id: "existing".into(),
        kind: MemorySourceKind::Github,
        target: "https://github.com/owner/repo".into(),
        label: "Repo".into(),
        schedule_mins: None,
        namespace: None,
    }];
    let duplicate = spec(MemorySourceKind::Github, "owner/repo");
    assert!(duplicate.validate_new(&existing).is_err());

    let mut too_frequent = spec(MemorySourceKind::Folder, "/notes");
    too_frequent.schedule_mins = Some(MIN_SCHEDULE_MINS - 1);
    assert!(too_frequent.validate_new(&existing).is_err());

    let valid = spec(MemorySourceKind::Folder, "/notes");
    assert_eq!(valid.validate_new(&existing).unwrap(), "/notes");
}

#[test]
fn namespace_layout_uses_source_root_or_configured_root() {
    let configured = MemoryLayout::new("team:configured".parse().unwrap()).unwrap();
    let source = MemorySourceConfig {
        id: "s".into(),
        kind: MemorySourceKind::Folder,
        target: "/notes".into(),
        label: "Notes".into(),
        schedule_mins: None,
        namespace: Some("team:source".into()),
    };
    assert_eq!(
        layout_of_source(&configured, &source).root().to_string(),
        "team:source"
    );
    assert_eq!(
        layout_of(&configured, std::slice::from_ref(&source), "s")
            .root()
            .to_string(),
        "team:source"
    );
    assert_eq!(
        layout_of(&configured, &[source], "unknown")
            .root()
            .to_string(),
        "team:configured"
    );
}

#[test]
fn invalid_saved_namespace_falls_back_to_configured_layout() {
    let configured = MemoryLayout::new("team:configured".parse().unwrap()).unwrap();
    let source = MemorySourceConfig {
        id: "s".into(),
        kind: MemorySourceKind::Folder,
        target: "/notes".into(),
        label: "Notes".into(),
        schedule_mins: None,
        namespace: Some("not-a-namespace".into()),
    };
    assert_eq!(
        layout_of_source(&configured, &source).root(),
        configured.root()
    );
}

#[test]
fn namespace_validation_accepts_blank_as_unset_but_rejects_bad_values() {
    let mut source = spec(MemorySourceKind::Folder, "/notes");
    source.namespace = Some("   ".into());
    assert!(source.validate_new(&[]).is_ok());
    assert_eq!(source.namespace().unwrap(), None);
    source.namespace = Some("bad".into());
    assert!(source.validate_new(&[]).is_err());
    assert!(source.namespace().is_err());
}

#[test]
fn validate_new_rejects_a_namespace_that_cannot_form_a_memory_layout() {
    let mut source = spec(MemorySourceKind::Folder, "/notes");
    source.namespace =
        Some("team:a/project:b/source:c/agent:d/ws:e/user:f/project:g/source:h".into());
    assert_eq!(source.namespace().unwrap().unwrap().depth(), 8);
    assert!(source.validate_new(&[]).is_err());
}
