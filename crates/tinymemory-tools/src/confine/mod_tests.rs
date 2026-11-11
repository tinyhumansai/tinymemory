//! Tests for namespace confinement policies.

use super::*;
use std::sync::Arc;

use tinymemory_api::conformance::ReferenceEngine;
use tinymemory_api::{LearningKind, ListRequest, MemoryEngine, SourceKind, SourceRef};

fn ns(value: &str) -> Namespace {
    value.parse().unwrap()
}

fn confinement(policy: Policy) -> Confinement {
    Confinement {
        allowed: Reach::subtree(ns("team:acme")),
        policy,
    }
}

fn learning_at(text: &str, namespace: &str) -> StoreItem {
    StoreItem::learning(
        text,
        LearningKind::Fact,
        0.8,
        MemoryMeta {
            namespace: ns(namespace),
            source: SourceRef {
                kind: SourceKind::Agent,
                id: None,
            },
            ..MemoryMeta::default()
        },
    )
}

#[test]
fn refuse_uses_the_allowed_reach_and_rejects_wider_requests() {
    let scope = confinement(Policy::Refuse);
    assert_eq!(scope.reach(None).unwrap(), scope.allowed);

    let narrower = Reach::exact(ns("team:acme/agent:writer"));
    assert_eq!(scope.reach(Some(narrower.clone())).unwrap(), narrower);

    for wider in [
        Reach::subtree(Namespace::ROOT),
        Reach::subtree(ns("team:other")),
        Reach::of(ns("team:acme/agent:writer")),
    ] {
        assert!(matches!(
            scope.reach(Some(wider)),
            Err(Error::InvalidRequest(_))
        ));
    }
}

#[test]
fn clamp_keeps_inside_reads_but_removes_ancestors() {
    let scope = confinement(Policy::Clamp);
    assert_eq!(scope.reach(None).unwrap(), scope.allowed);
    assert_eq!(
        scope.reach(Some(Reach::of(Namespace::ROOT))).unwrap(),
        scope.allowed,
        "a reach from the global root would include other tenants"
    );
    assert_eq!(
        scope.reach(Some(Reach::subtree(ns("team:other")))).unwrap(),
        scope.allowed,
        "a sibling tenant is clamped to the allowed root"
    );

    let inside = scope
        .reach(Some(Reach::of(ns("team:acme/agent:writer"))))
        .unwrap();
    assert_eq!(inside.at, ns("team:acme/agent:writer"));
    assert!(!inside.inherit, "ancestors can escape the allowed root");

    let exact_root = scope.reach(Some(Reach::exact(ns("team:acme")))).unwrap();
    assert_eq!(exact_root, Reach::exact(ns("team:acme")));

    let subtree_root = scope.reach(Some(Reach::subtree(ns("team:acme")))).unwrap();
    assert_eq!(subtree_root, scope.allowed);

    let inherited_root = scope.reach(Some(Reach::of(ns("team:acme")))).unwrap();
    assert_eq!(inherited_root, Reach::exact(ns("team:acme")));
}

#[test]
fn filters_keep_other_clauses_while_confining_the_reach() {
    let scope = confinement(Policy::Clamp);
    let filter = MetaFilter {
        reach: Some(Reach::subtree(Namespace::ROOT)),
        workspace: Some("repo".into()),
        ..MetaFilter::default()
    };
    let clamped = scope.filter(Some(filter)).unwrap();
    assert_eq!(clamped.reach, Some(scope.allowed));
    assert_eq!(clamped.workspace.as_deref(), Some("repo"));

    let scope = confinement(Policy::Refuse);
    assert!(matches!(
        scope.filter(Some(MetaFilter {
            reach: Some(Reach::subtree(Namespace::ROOT)),
            ..MetaFilter::default()
        })),
        Err(Error::InvalidRequest(_))
    ));
}

#[test]
fn clamp_moves_out_of_scope_items_and_preserves_inside_items() {
    let scope = confinement(Policy::Clamp);
    let mut outside = learning_at("fact", "team:other");
    scope.item(&mut outside).unwrap();
    assert_eq!(outside.meta().namespace, scope.allowed.at);

    let mut inside = learning_at("fact", "team:acme/agent:writer");
    scope.item(&mut inside).unwrap();
    assert_eq!(inside.meta().namespace, ns("team:acme/agent:writer"));
}

#[test]
fn learn_metadata_obeys_the_selected_policy() {
    let clamp = confinement(Policy::Clamp);
    let default = ns("team:acme/agent:writer");
    let clamped = clamp.learn_meta(None, &default).unwrap();
    assert_eq!(clamped.namespace, default);

    let out_of_scope_default = clamp
        .learn_meta(None, &ns("team:other/agent:writer"))
        .unwrap();
    assert_eq!(out_of_scope_default.namespace, clamp.allowed.at);

    let root_meta = clamp
        .learn_meta(Some(MemoryMeta::default()), &ns("team:acme/agent:writer"))
        .unwrap();
    assert_eq!(root_meta.namespace, ns("team:acme/agent:writer"));

    let clamped = clamp
        .learn_meta(
            Some(MemoryMeta {
                namespace: ns("team:other"),
                ..MemoryMeta::default()
            }),
            &default,
        )
        .unwrap();
    assert_eq!(clamped.namespace, clamp.allowed.at);

    let refuse = confinement(Policy::Refuse);
    let default = ns("team:acme/agent:writer");
    let accepted = refuse.learn_meta(None, &default).unwrap();
    assert_eq!(accepted.namespace, default);

    let explicit_root = refuse
        .learn_meta(Some(MemoryMeta::default()), &default)
        .unwrap();
    assert_eq!(explicit_root.namespace, default);

    let explicit_inside = refuse
        .learn_meta(
            Some(MemoryMeta {
                namespace: ns("team:acme/agent:writer"),
                ..MemoryMeta::default()
            }),
            &ns("team:acme/agent:other"),
        )
        .unwrap();
    assert_eq!(explicit_inside.namespace, ns("team:acme/agent:writer"));

    assert!(matches!(
        refuse.learn_meta(None, &ns("team:other/agent:writer")),
        Err(Error::InvalidRequest(_))
    ));
    assert!(matches!(
        refuse.learn_meta(Some(MemoryMeta::default()), &ns("team:other/agent:writer")),
        Err(Error::InvalidRequest(_))
    ));
    assert!(matches!(
        refuse.learn_meta(
            Some(MemoryMeta {
                namespace: ns("team:other"),
                ..MemoryMeta::default()
            }),
            &default
        ),
        Err(Error::InvalidRequest(_))
    ));
}

#[test]
fn segment_prefixes_do_not_cross_namespace_boundaries() {
    let allowed = Reach::subtree(ns("user:a"));
    let clamp = Confinement {
        allowed: allowed.clone(),
        policy: Policy::Clamp,
    };
    let clamped = clamp.reach(Some(Reach::subtree(ns("user:ab")))).unwrap();
    assert_eq!(clamped, allowed);

    let refuse = Confinement {
        allowed,
        policy: Policy::Refuse,
    };
    assert!(matches!(
        refuse.reach(Some(Reach::subtree(ns("user:ab")))),
        Err(Error::InvalidRequest(_))
    ));
    assert!(matches!(
        refuse.learn_meta(
            Some(MemoryMeta {
                namespace: ns("user:ab"),
                ..MemoryMeta::default()
            }),
            &ns("user:a/agent:writer")
        ),
        Err(Error::InvalidRequest(_))
    ));
}

#[test]
fn clamp_respects_exact_and_inherited_allowed_reaches() {
    let exact = Confinement {
        allowed: Reach::exact(ns("team:acme/agent:writer")),
        policy: Policy::Clamp,
    };
    assert_eq!(
        exact
            .reach(Some(Reach::exact(ns("team:acme/agent:writer"))))
            .unwrap(),
        exact.allowed
    );
    assert_eq!(
        exact
            .reach(Some(Reach::subtree(ns("team:acme/agent:writer"))))
            .unwrap(),
        exact.allowed,
        "a descendant reach is wider than an exact allowed reach"
    );

    let inherited = Confinement {
        allowed: Reach::of(ns("team:acme/agent:writer")),
        policy: Policy::Clamp,
    };
    let ancestor = Reach::exact(ns("team:acme"));
    assert_eq!(inherited.reach(Some(ancestor.clone())).unwrap(), ancestor);
    assert_eq!(
        inherited
            .reach(Some(Reach::subtree(ns("team:acme"))))
            .unwrap(),
        inherited.allowed,
        "subtree access is wider than an inherited reach"
    );

    let refuse = Confinement {
        allowed: exact.allowed.clone(),
        policy: Policy::Refuse,
    };
    assert!(matches!(
        refuse.reach(Some(Reach::subtree(ns("team:acme/agent:writer")))),
        Err(Error::InvalidRequest(_))
    ));
}

/// Two users on one shared engine stay isolated after their filters are clamped.
#[tokio::test]
async fn clamping_keeps_users_apart_on_a_shared_engine() {
    let engine: Arc<dyn MemoryEngine> = Arc::new(ReferenceEngine::new());
    for (user, text) in [("user:a", "alice's fact"), ("user:b", "bob's fact")] {
        let confinement = Confinement {
            allowed: Reach::subtree(ns(user)),
            policy: Policy::Clamp,
        };
        let mut item = learning_at(text, "root");
        confinement.item(&mut item).unwrap();
        engine.store(item).await.unwrap();
    }

    let everyone = MetaFilter {
        reach: Some(Reach::subtree(Namespace::ROOT)),
        ..MetaFilter::default()
    };
    let unclamped = engine
        .list(ListRequest {
            filter: everyone.clone(),
            limit: 100,
            cursor: None,
        })
        .await
        .unwrap();
    assert_eq!(unclamped.items.len(), 2);

    let own = Confinement {
        allowed: Reach::subtree(ns("user:a")),
        policy: Policy::Clamp,
    }
    .filter(Some(everyone))
    .unwrap();
    let clamped = engine
        .list(ListRequest {
            filter: own,
            limit: 100,
            cursor: None,
        })
        .await
        .unwrap();
    assert_eq!(clamped.items.len(), 1);
    assert_eq!(clamped.items[0].meta.namespace, ns("user:a"));
}
