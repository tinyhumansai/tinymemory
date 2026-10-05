//! Which scopes a filter reads, with and without discovery.

use super::*;

fn ns(value: &str) -> Namespace {
    value.parse().unwrap()
}

#[test]
fn an_agent_reads_its_node_and_ancestors_per_kind() {
    let reach = Reach::of(ns("team:acme/agent:writer"));
    let paths: Vec<String> = known(&reach, &[ItemKind::Learning, ItemKind::Document])
        .into_iter()
        .map(|scope| scope.path)
        .collect();
    assert_eq!(
        paths,
        [
            "app:tinymemory/app:documents",
            "app:tinymemory/team:acme/app:documents",
            "app:tinymemory/team:acme/agent:writer/app:documents",
            "app:tinymemory/app:learnings",
            "app:tinymemory/team:acme/app:learnings",
            "app:tinymemory/team:acme/agent:writer/app:learnings",
        ]
    );
}

#[test]
fn only_a_subtree_or_unscoped_read_needs_discovery() {
    assert!(needs_discovery(None));
    assert!(needs_discovery(Some(&Reach::subtree(Namespace::ROOT))));
    assert!(!needs_discovery(Some(&Reach::of(ns("agent:a")))));
    assert!(!needs_discovery(Some(&Reach::exact(Namespace::ROOT))));
}
