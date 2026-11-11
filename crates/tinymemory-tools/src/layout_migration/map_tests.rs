use super::*;
use crate::layout::BrainSource;
use tinymemory_api::{LearningKind, MemoryMeta, Role, SourceKind, SourceRef, Turn};

fn ns(path: &str) -> Namespace {
    path.parse().unwrap()
}

fn placement(root: &str) -> Placement {
    Placement {
        layout: MemoryLayout::new(ns(root)).unwrap(),
        chat_node: ns("ws:main"),
        flows: FlowPlacement::WithRoot,
        split_github_by_repo: false,
    }
}

fn document(namespace: &str, kind: SourceKind, file_path: Option<&str>) -> StoreItem {
    StoreItem::document(
        "a body",
        MemoryMeta {
            namespace: ns(namespace),
            source: SourceRef { kind, id: None },
            file_path: file_path.map(str::to_string),
            agent_id: Some("orchestrator".into()),
            ..MemoryMeta::default()
        },
    )
}

fn flow_learning() -> StoreItem {
    StoreItem::learning(
        "already sent item 5531",
        LearningKind::Fact,
        0.9,
        MemoryMeta {
            tags: vec![
                "flow:nl".into(),
                FLOWS_TAG.into(),
                "flow:nl:key:sent".into(),
            ],
            ..MemoryMeta::default()
        },
    )
}

#[test]
fn a_per_format_document_moves_to_the_files_node() {
    let placement = placement("root");
    let placed = placement
        .place(document(
            "source:pdf",
            SourceKind::File,
            Some("handbook.pdf"),
        ))
        .unwrap();
    assert_eq!(
        placed.meta().namespace,
        placement.layout.brain(&files_source()).unwrap()
    );
    assert_eq!(
        placed.meta().agent_id,
        None,
        "the brain belongs to every agent"
    );
}

#[test]
fn a_web_link_stays_web_and_an_uploaded_page_is_a_file() {
    let placement = placement("root");
    let link = placement
        .place(document("source:web", SourceKind::Link, None))
        .unwrap();
    assert_eq!(
        link.meta().namespace,
        placement.layout.brain(&BrainSource::Web).unwrap()
    );
    let page = placement
        .place(document("source:web", SourceKind::Link, Some("page.html")))
        .unwrap();
    assert_eq!(
        page.meta().namespace,
        placement.layout.brain(&files_source()).unwrap()
    );
}

#[test]
fn a_connector_document_keeps_its_node() {
    let placement = placement("root");
    let placed = placement
        .place(document("source:gmail", SourceKind::Import, None))
        .unwrap();
    assert_eq!(placed.meta().namespace, ns("source:gmail"));
}

#[test]
fn a_github_document_goes_to_its_repository_when_the_setting_splits_them() {
    let mut item = document("source:github", SourceKind::Import, None);
    item.meta_mut().repo = Some("Acme/Widgets".into());
    let mut placement = placement("root");
    assert_eq!(
        placement.place(item.clone()).unwrap().meta().namespace,
        ns("source:github")
    );
    placement.split_github_by_repo = true;
    assert_eq!(
        placement.place(item).unwrap().meta().namespace,
        placement
            .layout
            .brain_collection(&BrainSource::Github, "acme--widgets")
            .unwrap()
    );
}

#[test]
fn a_conversation_is_pooled_at_the_chat_node_with_its_agent() {
    let placement = placement("root");
    let turn = StoreItem::Conversation {
        turns: vec![Turn {
            role: Role::User,
            text: "hi".into(),
            at: None,
            tool_calls: Vec::new(),
        }],
        meta: MemoryMeta {
            namespace: ns("agent:orchestrator"),
            agent_id: Some("orchestrator".into()),
            thread_id: Some("thread-1".into()),
            ..MemoryMeta::default()
        },
    };
    let placed = placement.place(turn).unwrap();
    assert_eq!(placed.meta().namespace, ns("ws:main"));
    assert_eq!(placed.meta().agent_id.as_deref(), Some("orchestrator"));
    assert_eq!(placed.meta().thread_id.as_deref(), Some("thread-1"));
}

#[test]
fn workflow_items_stay_with_the_root_until_the_switch_moves_them() {
    let item = flow_learning();
    let placed = placement("root").place(item.clone()).unwrap();
    assert_eq!(placed, item, "with the root's learnings, unchanged");

    fn flow_node(id: &str) -> Result<Namespace> {
        Ok(format!("ws:main/service:{id}").parse().unwrap())
    }
    let mut moved = placement("root");
    moved.flows = FlowPlacement::InNode(flow_node);
    let placed = moved.place(item).unwrap();
    assert_eq!(placed.meta().namespace, ns("ws:main/service:nl"));
}

#[test]
fn a_flow_digest_document_is_a_workflow_item_not_a_brain_document() {
    let mut digest = document("root", SourceKind::Agent, None);
    digest.meta_mut().tags = vec!["flow:nl".into(), FLOWS_TAG.into(), "flow_run_digest".into()];
    let placed = placement("root").place(digest.clone()).unwrap();
    assert_eq!(placed, digest);
}

#[test]
fn a_learning_keeps_its_namespace() {
    let learning = StoreItem::learning(
        "prefers short answers",
        LearningKind::Preference,
        0.8,
        MemoryMeta::default(),
    );
    assert_eq!(placement("root").place(learning.clone()).unwrap(), learning);
}

#[test]
fn a_hosts_root_is_kept_below_the_wire_root() {
    let placement = placement("team:acme");
    let placed = placement
        .place(document(
            "team:acme/source:markdown",
            SourceKind::File,
            None,
        ))
        .unwrap();
    assert_eq!(placed.meta().namespace, ns("team:acme/source:files"));
    let outside = document("source:pdf", SourceKind::File, None);
    assert_eq!(
        placement.place(outside.clone()).unwrap(),
        outside,
        "a node outside the host's root is not its brain"
    );
}
