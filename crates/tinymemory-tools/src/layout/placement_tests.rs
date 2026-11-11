use super::*;
use tinymemory_api::{MemoryMeta, SourceRef};

fn document(kind: SourceKind, file_path: Option<&str>) -> StoreItem {
    StoreItem::document(
        "a body",
        MemoryMeta {
            namespace: "source:web".parse().unwrap(),
            source: SourceRef { kind, id: None },
            file_path: file_path.map(str::to_string),
            agent_id: Some("orchestrator".into()),
            ..MemoryMeta::default()
        },
    )
}

#[test]
fn local_files_are_filed_under_the_files_source() {
    assert_eq!(files_source(), BrainSource::Files);
    assert_eq!(files_source().id(), "files");
}

#[test]
fn filing_a_document_drops_the_agent_and_sets_the_node() {
    let node: Namespace = "team:acme/source:files".parse().unwrap();
    let filed = file_into(node.clone(), document(SourceKind::File, None));
    assert_eq!(filed.meta().namespace, node);
    assert_eq!(filed.meta().agent_id, None);
}

#[test]
fn github_splits_by_repository_only_when_asked() {
    let layout = MemoryLayout::default();
    let mut item = document(SourceKind::Github, None);
    item.meta_mut().repo = Some("Acme/Widgets".into());
    let split = brain_node_with(true, &layout, &BrainSource::Github, &item).unwrap();
    assert_eq!(split.to_string(), "source:github/project:acme--widgets");
    let flat = brain_node_with(false, &layout, &BrainSource::Github, &item).unwrap();
    assert_eq!(flat.to_string(), "source:github");
    let other = brain_node_with(true, &layout, &BrainSource::Web, &item).unwrap();
    assert_eq!(other.to_string(), "source:web");
}

#[test]
fn a_legacy_files_node_parses_to_the_same_source_as_files() {
    let item = document(SourceKind::File, None);
    assert_eq!(legacy_brain_node("files", &item), BrainSource::Files);
    assert_eq!(legacy_brain_node("docx", &item), BrainSource::Files);
}

#[test]
fn legacy_nodes_map_to_todays_sources() {
    let file = document(SourceKind::File, None);
    assert_eq!(legacy_brain_node("pdf", &file), files_source());
    assert_eq!(legacy_brain_node("web", &file), files_source());
    let link = document(SourceKind::Link, None);
    assert_eq!(legacy_brain_node("web", &link), BrainSource::Web);
    assert_eq!(legacy_brain_node("notion", &link), BrainSource::Notion);
    assert_eq!(
        legacy_brain_node("google_drive", &link),
        BrainSource::Other("googledrive".into())
    );
}

#[test]
fn a_github_collection_is_the_lowercase_owner_and_repo() {
    let mut item = document(SourceKind::Import, None);
    item.meta_mut().repo = Some("Acme/Widgets".into());
    assert_eq!(github_collection(&item).as_deref(), Some("acme--widgets"));
    item.meta_mut().repo = None;
    item.meta_mut().url = Some("https://github.com/foo-bar/repo.git?tab=1".into());
    assert_eq!(github_collection(&item).as_deref(), Some("foo-bar--repo"));
    item.meta_mut().url = Some("https://example.com".into());
    assert_eq!(github_collection(&item), None);
}
