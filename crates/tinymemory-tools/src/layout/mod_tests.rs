//! Layout nodes, filters and brain source ids.

use super::*;
use tinymemory_api::SourceKind;

#[test]
fn places_every_part_below_the_root() {
    let layout = MemoryLayout::new("team:acme".parse().unwrap()).unwrap();
    assert_eq!(
        layout.brain(&BrainSource::Pdf).unwrap().to_string(),
        "team:acme/source:pdf"
    );
    assert_eq!(
        layout.conversations("coder-42").unwrap().to_string(),
        "team:acme/agent:coder-42"
    );
    assert_eq!(layout.learnings().to_string(), "team:acme");
    let odd = layout.conversations("agent 7").unwrap();
    assert!(odd.to_string().starts_with("team:acme/agent:agent-7-"));
}

#[test]
fn filters_read_their_scope_only() {
    let layout = MemoryLayout::default();
    let pdf = layout.brain_filter(Some(&BrainSource::Pdf));
    let pdf_reach = pdf.reach.clone().unwrap();
    assert!(pdf_reach.admits(&Namespace::source("pdf")));
    assert!(!pdf_reach.admits(&Namespace::source("notion")));
    assert!(!pdf_reach.admits(&Namespace::ROOT));
    assert_eq!(pdf.kinds, [ItemKind::Document]);

    let brain = layout.brain_filter(None).reach.unwrap();
    assert!(brain.admits(&Namespace::source("notion")));

    let support = layout.conversations_filter(Some("support")).reach.unwrap();
    assert!(support.admits(&Namespace::agent("support")));
    assert!(!support.admits(&Namespace::agent("coder")));
    let team = layout.conversations_filter(None);
    assert!(team.reach.unwrap().admits(&Namespace::agent("coder")));
    assert_eq!(team.kinds, [ItemKind::Conversation]);

    assert_eq!(layout.learnings_filter().kinds, [ItemKind::Learning]);
    assert!(layout.holistic_filter().kinds.is_empty());
}

#[test]
fn refuses_a_root_with_no_room_below() {
    let deep: Namespace = ["agent:a"; 8].join("/").parse().unwrap();
    assert!(matches!(
        MemoryLayout::new(deep),
        Err(Error::InvalidRequest(_))
    ));
    let deepest_allowed: Namespace = ["agent:a"; 7].join("/").parse().unwrap();
    let layout = MemoryLayout::new(deepest_allowed).unwrap();
    assert_eq!(layout.brain(&BrainSource::Web).unwrap().depth(), 8);
    assert!(
        layout
            .brain_collection(&BrainSource::Github, "acme-api")
            .is_err(),
        "no room for a collection below the deepest root"
    );
}

#[test]
fn a_collection_sits_below_its_source() {
    let layout = MemoryLayout::new("team:acme".parse().unwrap()).unwrap();
    let repo = layout
        .brain_collection(&BrainSource::Github, "tinyhumansai/openhuman")
        .unwrap();
    assert!(
        repo.to_string()
            .starts_with("team:acme/source:github/project:tinyhumansai-openhuman")
    );
    assert!(
        layout
            .brain_filter(Some(&BrainSource::Github))
            .reach
            .unwrap()
            .admits(&repo)
    );
}

#[test]
fn source_ids_round_trip() {
    for source in [
        BrainSource::Files,
        BrainSource::Pdf,
        BrainSource::Markdown,
        BrainSource::Notion,
        BrainSource::Github,
        BrainSource::Web,
        BrainSource::Other("confluence".into()),
    ] {
        let id = source.to_string();
        assert_eq!(id.parse::<BrainSource>().unwrap(), source);
        let json = serde_json::to_value(&source).unwrap();
        assert_eq!(json, serde_json::json!(id));
        assert_eq!(serde_json::from_value::<BrainSource>(json).unwrap(), source);
    }
    assert_eq!("MD".parse::<BrainSource>().unwrap(), BrainSource::Markdown);
    assert!(" ".parse::<BrainSource>().is_err());
    assert_eq!(BrainSource::Github.source_kind(), SourceKind::Github);
}

#[test]
fn admits_only_a_strict_ancestor_as_core() {
    let layout = MemoryLayout::new("ws:acme/team:hive".parse().unwrap()).unwrap();
    layout.admits_core(&Namespace::ROOT).unwrap();
    layout.admits_core(&"ws:acme".parse().unwrap()).unwrap();
    for refused in [
        "ws:acme/team:hive",
        "ws:acme/team:other",
        "ws:acme/team:hive/agent:a",
        "ws:other",
    ] {
        assert!(
            matches!(
                layout.admits_core(&refused.parse().unwrap()),
                Err(Error::InvalidRequest(_))
            ),
            "{refused}"
        );
    }
    assert!(
        MemoryLayout::default()
            .admits_core(&Namespace::ROOT)
            .is_err()
    );
}

#[test]
fn ancestors_lists_root_first() {
    let layout = MemoryLayout::new("ws:acme/team:hive".parse().unwrap()).unwrap();
    let ancestors: Vec<String> = layout.ancestors().iter().map(ToString::to_string).collect();
    assert_eq!(ancestors, ["root", "ws:acme"]);
    assert!(MemoryLayout::default().ancestors().is_empty());
}

#[test]
fn a_core_scope_reads_its_node_exactly() {
    let company = CoreScope::new("ws:acme".parse().unwrap(), "Company")
        .kinds([ItemKind::Learning])
        .limit(2);
    let filter = company.filter();
    let reach = filter.reach.unwrap();
    assert!(reach.admits(&"ws:acme".parse().unwrap()));
    assert!(!reach.admits(&Namespace::ROOT));
    assert!(!reach.admits(&"ws:acme/team:other".parse().unwrap()));
    assert_eq!(filter.kinds, [ItemKind::Learning]);
    assert_eq!(company.limit, 2);

    let brief = company.brief("What does the company know?");
    assert_eq!(brief.heading, "Company");
    assert_eq!(brief.filter, company.filter());

    let parsed: CoreScope =
        serde_json::from_value(serde_json::json!({"at": "ws:acme", "heading": "Company"})).unwrap();
    assert_eq!(
        parsed,
        CoreScope::new("ws:acme".parse().unwrap(), "Company")
    );
}

#[test]
fn pooled_conversations_share_one_node_told_apart_by_agent() {
    let chats: Namespace = "team:acme/ws:main".parse().unwrap();
    let layout = MemoryLayout::new("team:acme".parse().unwrap())
        .unwrap()
        .with_pooled_conversations(&chats)
        .unwrap();
    let pooled: Namespace = "team:acme/ws:main".parse().unwrap();
    assert_eq!(layout.conversations("coder").unwrap(), pooled);
    assert_eq!(layout.conversations("support").unwrap(), pooled);

    let coder = layout.conversations_filter(Some("coder"));
    assert_eq!(coder.agent_id.as_deref(), Some("coder"));
    assert_eq!(coder.reach, Some(Reach::exact(pooled.clone())));
    assert_eq!(coder.kinds, [ItemKind::Conversation]);
    let team = layout.conversations_filter(None);
    assert_eq!(team.agent_id, None);
    assert_eq!(team.reach, Some(Reach::exact(pooled)));

    // Everything else is where it was.
    assert_eq!(layout.learnings().to_string(), "team:acme");
    assert_eq!(
        layout.brain(&BrainSource::Pdf).unwrap().to_string(),
        "team:acme/source:pdf"
    );
}

#[test]
fn pooled_conversations_accept_the_absolute_node_below_a_non_root_layout() {
    let root: Namespace = "team:acme".parse().unwrap();
    let node: Namespace = "team:acme/ws:main".parse().unwrap();
    let layout = MemoryLayout::new(root).unwrap();

    let pooled = layout.with_pooled_conversations(&node).unwrap();

    assert_eq!(pooled.conversations("coder").unwrap(), node);
}

#[test]
fn pooled_conversations_need_a_node_that_fits() {
    let layout = MemoryLayout::default();
    assert!(matches!(
        layout.clone().with_pooled_conversations(&Namespace::ROOT),
        Err(Error::InvalidRequest(_))
    ));
    let scoped = MemoryLayout::new("team:acme".parse().unwrap()).unwrap();
    assert!(
        scoped
            .clone()
            .with_pooled_conversations(&Namespace::ROOT)
            .is_err()
    );
    for invalid in ["team:acme", "team:other/ws:main", "team:acme2/ws:main"] {
        assert!(
            scoped
                .clone()
                .with_pooled_conversations(&invalid.parse().unwrap())
                .is_err(),
            "expected {invalid:?} to be outside a strict descendant of team:acme"
        );
    }
}

#[test]
fn a_pooled_sub_agent_is_in_the_team_not_its_parents_history() {
    let chats: Namespace = "ws:main".parse().unwrap();
    let layout = MemoryLayout::default()
        .with_pooled_conversations(&chats)
        .unwrap();
    let scout_turn = tinymemory_api::MemoryMeta {
        namespace: layout.conversations("coder-scout").unwrap(),
        agent_id: Some("coder-scout".to_string()),
        ..tinymemory_api::MemoryMeta::default()
    };
    let history = layout.conversations_filter(Some("coder"));
    assert!(!history.matches(ItemKind::Conversation, &scout_turn));
    let team = layout.conversations_filter(None);
    assert!(team.matches(ItemKind::Conversation, &scout_turn));
}
