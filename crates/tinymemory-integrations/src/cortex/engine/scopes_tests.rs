//! Which scopes a filter reads, with and without discovery.

use super::*;

fn ns(value: &str) -> Namespace {
    value.parse().unwrap()
}

#[test]
fn an_agent_reads_its_node_and_ancestors_per_kind() {
    let reach = Reach::of(ns("team:acme/agent:writer"));
    let paths: Vec<String> = known(
        &ScopeRoot::direct(&crate::cortex::CortexTenancy::SingleUser),
        &reach,
        &[ItemKind::Learning, ItemKind::Document],
    )
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
fn a_pinned_engine_reads_its_nodes_under_the_pin() {
    let root =
        ScopeRoot::direct(&crate::cortex::CortexTenancy::pinned("org:acme/user:alice").unwrap());
    let paths: Vec<String> = known(&root, &Reach::of(ns("agent:writer")), &[ItemKind::Learning])
        .into_iter()
        .map(|scope| scope.path)
        .collect();
    assert_eq!(
        paths,
        [
            "org:acme/user:alice/app:tinymemory/app:learnings",
            "org:acme/user:alice/app:tinymemory/agent:writer/app:learnings",
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
