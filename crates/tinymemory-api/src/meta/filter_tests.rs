//! [`MetaFilter`] matching rules.

use chrono::TimeZone;

use super::*;

fn meta() -> MemoryMeta {
    MemoryMeta {
        namespace: "team:t/agent:a1".parse().unwrap(),
        workspace: Some("/ws".into()),
        folder: Some("/ws/src".into()),
        file_path: Some("/ws/src/lib.rs".into()),
        language: Some("rust".into()),
        repo: Some("o/r".into()),
        commit: Some("abc".into()),
        url: Some("https://x.test".into()),
        thread_id: Some("t1".into()),
        channel: Some("telegram".into()),
        turns: Some(TurnRange { first: 0, last: 3 }),
        agent_id: Some("a1".into()),
        tool_call: Some(crate::ToolCallRef {
            name: "grep".into(),
            id: Some("c1".into()),
        }),
        source: crate::SourceRef {
            kind: SourceKind::Folder,
            id: Some("src_1".into()),
        },
        tags: vec!["x".into(), "y".into()],
        observed_at: Some(Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap()),
        derive: None,
        observed_actor: None,
    }
}

#[test]
fn channel_filters_are_exact_and_leave_other_channels_out() {
    let filter = MetaFilter {
        channel: Some("telegram".into()),
        ..MetaFilter::default()
    };
    assert!(filter.matches(ItemKind::Learning, &meta()));

    let mut another = meta();
    another.channel = Some("discord".into());
    assert!(!filter.matches(ItemKind::Learning, &another));

    let mut unstamped = meta();
    unstamped.channel = None;
    assert!(!filter.matches(ItemKind::Learning, &unstamped));
}

#[test]
fn an_empty_filter_matches_everything() {
    let filter = MetaFilter::default();
    assert!(filter.is_empty());
    assert!(filter.matches(ItemKind::Document, &meta()));
    assert!(filter.matches(ItemKind::Learning, &MemoryMeta::default()));
}

#[test]
fn tags_any_constructor_matches_any_tag_and_leaves_other_fields_unset() {
    let filter = MetaFilter::tags_any(["flow:f1", "flows"]);
    assert_eq!(
        filter.tags_any,
        ["flow:f1".to_string(), "flows".to_string()]
    );
    assert_eq!(filter.reach, None);
    assert_eq!(filter.workspace, None);

    let matching = MemoryMeta {
        tags: vec!["flows".into()],
        ..MemoryMeta::default()
    };
    assert!(filter.matches(ItemKind::Learning, &matching));

    let nonmatching = MemoryMeta {
        tags: vec!["flow:f2".into()],
        ..MemoryMeta::default()
    };
    assert!(!filter.matches(ItemKind::Learning, &nonmatching));
}

#[test]
fn every_exact_field_must_match() {
    let item = meta();
    let probes: Vec<(MetaFilter, MetaFilter)> = vec![
        (
            MetaFilter {
                workspace: Some("/ws".into()),
                ..Default::default()
            },
            MetaFilter {
                workspace: Some("/other".into()),
                ..Default::default()
            },
        ),
        (
            MetaFilter {
                language: Some("rust".into()),
                ..Default::default()
            },
            MetaFilter {
                language: Some("go".into()),
                ..Default::default()
            },
        ),
        (
            MetaFilter {
                repo: Some("o/r".into()),
                ..Default::default()
            },
            MetaFilter {
                repo: Some("o/x".into()),
                ..Default::default()
            },
        ),
        (
            MetaFilter {
                commit: Some("abc".into()),
                ..Default::default()
            },
            MetaFilter {
                commit: Some("def".into()),
                ..Default::default()
            },
        ),
        (
            MetaFilter {
                url: Some("https://x.test".into()),
                ..Default::default()
            },
            MetaFilter {
                url: Some("https://y.test".into()),
                ..Default::default()
            },
        ),
        (
            MetaFilter {
                thread_id: Some("t1".into()),
                ..Default::default()
            },
            MetaFilter {
                thread_id: Some("t2".into()),
                ..Default::default()
            },
        ),
        (
            MetaFilter {
                turns: Some(TurnRange { first: 0, last: 3 }),
                ..Default::default()
            },
            MetaFilter {
                turns: Some(TurnRange { first: 0, last: 4 }),
                ..Default::default()
            },
        ),
        (
            MetaFilter {
                agent_id: Some("a1".into()),
                ..Default::default()
            },
            MetaFilter {
                agent_id: Some("a2".into()),
                ..Default::default()
            },
        ),
        (
            MetaFilter {
                tool_call: Some("grep".into()),
                ..Default::default()
            },
            MetaFilter {
                tool_call: Some("ls".into()),
                ..Default::default()
            },
        ),
        (
            MetaFilter {
                source_id: Some("src_1".into()),
                ..Default::default()
            },
            MetaFilter {
                source_id: Some("src_2".into()),
                ..Default::default()
            },
        ),
        (
            MetaFilter {
                sources: vec![SourceKind::Folder],
                ..Default::default()
            },
            MetaFilter {
                sources: vec![SourceKind::Rss],
                ..Default::default()
            },
        ),
        (
            MetaFilter {
                tags_any: vec!["nope".into(), "y".into()],
                ..Default::default()
            },
            MetaFilter {
                tags_any: vec!["nope".into()],
                ..Default::default()
            },
        ),
        (
            MetaFilter::kinds([ItemKind::Document]),
            MetaFilter::kinds([ItemKind::Learning]),
        ),
    ];
    for (hit, miss) in probes {
        assert!(hit.matches(ItemKind::Document, &item), "{hit:?}");
        assert!(!miss.matches(ItemKind::Document, &item), "{miss:?}");
    }
}

#[test]
fn a_set_field_never_matches_an_item_without_it() {
    let filter = MetaFilter {
        repo: Some("o/r".into()),
        ..Default::default()
    };
    assert!(!filter.matches(ItemKind::Document, &MemoryMeta::default()));
}

#[test]
fn folder_and_file_path_match_on_a_path_boundary() {
    let item = meta();
    for prefix in ["/ws", "/ws/", "/ws/src", "/ws/src/"] {
        let filter = MetaFilter {
            folder: Some(prefix.into()),
            ..Default::default()
        };
        assert!(filter.matches(ItemKind::Document, &item), "{prefix}");
    }
    let filter = MetaFilter {
        folder: Some("/ws/sr".into()),
        ..Default::default()
    };
    assert!(!filter.matches(ItemKind::Document, &item));
    let filter = MetaFilter {
        file_path: Some("/ws/src".into()),
        ..Default::default()
    };
    assert!(filter.matches(ItemKind::Document, &item));
    let filter = MetaFilter {
        file_path: Some("/ws/src/lib".into()),
        ..Default::default()
    };
    assert!(!filter.matches(ItemKind::Document, &item));
}

#[test]
fn the_window_is_half_open_and_excludes_undated_items() {
    let at = Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
    let inclusive = MetaFilter {
        observed_after: Some(at),
        ..Default::default()
    };
    assert!(inclusive.matches(ItemKind::Document, &meta()));
    let exclusive = MetaFilter {
        observed_before: Some(at),
        ..Default::default()
    };
    assert!(!exclusive.matches(ItemKind::Document, &meta()));
    assert!(!inclusive.matches(ItemKind::Document, &MemoryMeta::default()));
}

#[test]
fn reach_admits_own_node_and_ancestors_never_a_sibling() {
    let reach = |at: &str| MetaFilter {
        reach: Some(crate::Reach::of(at.parse().unwrap())),
        ..MetaFilter::default()
    };
    assert!(reach("team:t/agent:a1").matches(ItemKind::Learning, &meta()));
    assert!(
        !reach("team:t").matches(ItemKind::Learning, &meta()),
        "a team does not read its members"
    );
    assert!(!reach("team:t/agent:a2").matches(ItemKind::Learning, &meta()));
    assert!(
        reach("team:t/agent:a2").matches(ItemKind::Learning, &MemoryMeta::default()),
        "root memory is shared"
    );
    assert!(!reach("team:t").is_empty(), "a reach constrains a forget");
}

#[test]
fn no_reach_never_matches_a_service_sandbox() {
    let in_flow = MemoryMeta {
        namespace: "ws:main/service:newsletter".parse().unwrap(),
        ..MemoryMeta::default()
    };
    assert!(!MetaFilter::default().matches(ItemKind::Learning, &in_flow));
    assert!(MetaFilter::default().matches(ItemKind::Learning, &meta()));
    let at_flow = MetaFilter {
        reach: Some(crate::Reach::exact(in_flow.namespace.clone())),
        ..MetaFilter::default()
    };
    assert!(at_flow.matches(ItemKind::Learning, &in_flow));
}
