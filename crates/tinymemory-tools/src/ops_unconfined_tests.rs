//! Unconfined `MemoryOps` behavior: caller reach, namespaces, and deletion.

use super::*;
use tinymemory_api::GetRequest;

#[test]
fn unconfined_mode_is_explicit_and_has_no_confinement() {
    let confined = operations(Policy::Clamp);
    assert_eq!(
        confined.confinement(),
        Some(&Confinement {
            allowed: Reach::subtree(namespace("team:acme")),
            policy: Policy::Clamp,
        })
    );
    assert!(MemoryOps::unconfined().confinement().is_none());
}

#[test]
fn unconfined_read_builders_preserve_caller_reach_and_filter() {
    let ops = MemoryOps::unconfined();
    let service = namespace("team:acme/service:workflow");
    let reach = Reach::of(service);
    let filter = MetaFilter {
        reach: Some(reach.clone()),
        workspace: Some("repo".into()),
        ..MetaFilter::default()
    };

    let recall = ops
        .recall_request(RecallOptions {
            question: "workflow notes".into(),
            filter: Some(filter.clone()),
            limit: None,
            instructions: None,
            refers_to: None,
            exclude_thread: None,
            mode: RecallMode::Answer,
        })
        .unwrap();
    assert_eq!(recall.filter.reach, Some(reach.clone()));
    assert_eq!(recall.filter.workspace.as_deref(), Some("repo"));

    let descriptor = ReferenceEngine::new().descriptor().clone();
    let fetch = ops
        .fetch_request(
            FetchOptions {
                query: "workflow notes".into(),
                mode: None,
                filter: Some(filter.clone()),
                limit: None,
                cursor: None,
                refers_to: None,
                exclude_thread: None,
            },
            &descriptor,
        )
        .unwrap();
    assert_eq!(fetch.filter.reach, Some(reach.clone()));
    assert_eq!(fetch.filter.workspace.as_deref(), Some("repo"));

    let list = ops
        .list_request(ListOptions {
            filter: Some(filter.clone()),
            path: Vec::new(),
            limit: None,
            cursor: None,
        })
        .unwrap();
    assert_eq!(list.filter.reach, Some(reach.clone()));
    assert_eq!(list.filter.workspace.as_deref(), Some("repo"));

    let explore = ops
        .explore_request(ExploreOptions {
            facet: Facet::Workspace,
            path: Vec::new(),
            filter: Some(filter),
            limit: None,
            scan_limit: None,
        })
        .unwrap();
    assert_eq!(explore.filter.reach, Some(reach));
    assert_eq!(explore.filter.workspace.as_deref(), Some("repo"));

    assert_eq!(
        ops.recall_request(RecallOptions {
            question: "all ordinary memory".into(),
            filter: None,
            limit: None,
            instructions: None,
            refers_to: None,
            exclude_thread: None,
            mode: RecallMode::Answer,
        })
        .unwrap()
        .filter
        .reach,
        None
    );
}

#[test]
fn unconfined_get_request_preserves_optional_inherited_and_service_reaches() {
    let ops = MemoryOps::unconfined();
    let inherited = Reach::of(namespace("team:acme/agent:writer"));
    let service = Reach::of(namespace("team:acme/service:workflow"));

    assert_eq!(
        ops.items_get_request(vec![ItemId::new("id")], None)
            .unwrap()
            .reach,
        None
    );
    assert_eq!(
        ops.items_get_request(vec![ItemId::new("id")], Some(inherited.clone()))
            .unwrap()
            .reach,
        Some(inherited)
    );
    assert_eq!(
        ops.items_get_request(vec![ItemId::new("id")], Some(service.clone()))
            .unwrap()
            .reach,
        Some(service)
    );
}

#[test]
fn unconfined_learning_uses_default_and_preserves_effective_namespace() {
    let ops = MemoryOps::unconfined();
    let service = namespace("team:acme/service:workflow");
    let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();

    let defaulted = ops
        .learning_item(LearningOptions {
            text: "  workflow preference  ".into(),
            kind: Some(LearningKind::Preference),
            confidence: None,
            meta: None,
            host_meta: Some(MemoryMeta {
                thread_id: Some("host-thread".into()),
                ..MemoryMeta::default()
            }),
            default_namespace: service.clone(),
            now,
        })
        .unwrap();
    assert_eq!(defaulted.meta().namespace, service);
    assert_eq!(defaulted.meta().thread_id.as_deref(), Some("host-thread"));
    assert_eq!(defaulted.meta().observed_at, Some(now));

    let explicit = namespace("team:other/agent:worker");
    let item = ops
        .learning_item(LearningOptions {
            text: "explicit namespace".into(),
            kind: None,
            confidence: None,
            meta: Some(MemoryMeta {
                namespace: explicit.clone(),
                ..MemoryMeta::default()
            }),
            host_meta: None,
            default_namespace: namespace("team:acme"),
            now,
        })
        .unwrap();
    assert_eq!(item.meta().namespace, explicit);

    let caller_namespace = namespace("team:other/agent:worker");
    let host_namespace = namespace("team:acme/service:workflow");
    let host_overlaid = ops
        .learning_item(LearningOptions {
            text: "host namespace wins overlay".into(),
            kind: None,
            confidence: None,
            meta: Some(MemoryMeta {
                namespace: caller_namespace,
                ..MemoryMeta::default()
            }),
            host_meta: Some(MemoryMeta {
                namespace: host_namespace.clone(),
                channel: Some("host-channel".into()),
                ..MemoryMeta::default()
            }),
            default_namespace: namespace("team:acme"),
            now,
        })
        .unwrap();
    assert_eq!(host_overlaid.meta().namespace, host_namespace);
    assert_eq!(
        host_overlaid.meta().channel.as_deref(),
        Some("host-channel")
    );
    assert_eq!(item.meta().channel.as_deref(), None);

    assert!(
        ops.learning_item(LearningOptions {
            text: "invalid confidence".into(),
            kind: None,
            confidence: Some(1.1),
            meta: None,
            host_meta: None,
            default_namespace: namespace("team:acme"),
            now,
        })
        .is_err()
    );
}

#[test]
fn unconfined_store_preserves_ordinary_and_service_namespaces() {
    let ops = MemoryOps::unconfined();
    for namespace in [
        namespace("team:acme/agent:writer"),
        namespace("team:acme/service:workflow"),
    ] {
        let stored = ops
            .store_item(StoreItem::learning(
                "keep caller namespace",
                LearningKind::Fact,
                0.8,
                MemoryMeta {
                    namespace: namespace.clone(),
                    ..MemoryMeta::default()
                },
            ))
            .unwrap();
        assert_eq!(stored.meta().namespace, namespace);
    }
}

#[tokio::test]
async fn unconfined_forget_without_reach_removes_ids_across_namespaces() {
    let ops = MemoryOps::unconfined();
    let engine = CountingEngine::new(false);
    let normal_ns = namespace("team:acme/agent:writer");
    let service_ns = namespace("team:acme/service:workflow");
    let normal = engine
        .store(StoreItem::learning(
            "normal memory",
            LearningKind::Fact,
            0.8,
            MemoryMeta {
                namespace: normal_ns.clone(),
                ..MemoryMeta::default()
            },
        ))
        .await
        .unwrap()
        .id;
    let service = engine
        .store(StoreItem::learning(
            "service memory",
            LearningKind::Fact,
            0.8,
            MemoryMeta {
                namespace: service_ns.clone(),
                ..MemoryMeta::default()
            },
        ))
        .await
        .unwrap()
        .id;

    let report = ops
        .forget(
            &engine,
            vec![normal.as_str().into(), service.as_str().into()],
            None,
        )
        .await
        .unwrap();
    assert_eq!(report.forgotten, 2);
    for (id, ns) in [(normal, normal_ns), (service, service_ns)] {
        assert!(
            engine
                .get(GetRequest {
                    ids: vec![id],
                    reach: Some(Reach::exact(ns)),
                })
                .await
                .unwrap()
                .is_empty()
        );
    }
}

#[tokio::test]
async fn unconfined_forget_with_explicit_reach_preserves_outside_ids() {
    let ops = MemoryOps::unconfined();
    let engine = CountingEngine::new(false);
    let service_ns = namespace("team:acme/service:workflow");
    let outside_ns = namespace("team:other/agent:writer");
    let inside = engine
        .store(StoreItem::learning(
            "inside explicit reach",
            LearningKind::Fact,
            0.8,
            MemoryMeta {
                namespace: service_ns.clone(),
                ..MemoryMeta::default()
            },
        ))
        .await
        .unwrap()
        .id;
    let outside = engine
        .store(StoreItem::learning(
            "outside explicit reach",
            LearningKind::Fact,
            0.8,
            MemoryMeta {
                namespace: outside_ns.clone(),
                ..MemoryMeta::default()
            },
        ))
        .await
        .unwrap()
        .id;

    let report = ops
        .forget(
            &engine,
            vec![inside.as_str().into(), outside.as_str().into()],
            Some(Reach::subtree(service_ns.clone())),
        )
        .await
        .unwrap();
    assert_eq!(report.forgotten, 1);
    assert!(
        engine
            .get(GetRequest {
                ids: vec![inside],
                reach: Some(Reach::exact(service_ns)),
            })
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        engine
            .get(GetRequest {
                ids: vec![outside],
                reach: Some(Reach::exact(outside_ns)),
            })
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn unconfined_pending_deletion_preserves_filter_and_queue_contracts() {
    let ops = MemoryOps::unconfined();
    let engine = CountingEngine::new(false);
    let ordinary = namespace("team:acme/agent:writer");
    let service = namespace("team:acme/service:workflow");
    let mut conversation_ids = Vec::new();
    let mut source_ids = Vec::new();
    let mut non_conversation_ids = Vec::new();

    for ns in [ordinary.clone(), service.clone()] {
        let mut conversation_meta = MemoryMeta {
            namespace: ns.clone(),
            thread_id: Some("shared-thread".into()),
            ..MemoryMeta::default()
        };
        conversation_meta.source.kind = SourceKind::Conversation;
        conversation_ids.push(
            engine
                .store(StoreItem::Conversation {
                    turns: vec![tinymemory_api::Turn::new(
                        tinymemory_api::Role::User,
                        "conversation turn",
                    )],
                    meta: conversation_meta,
                })
                .await
                .unwrap()
                .id,
        );
        non_conversation_ids.push(
            engine
                .store(StoreItem::learning(
                    "same thread id, not a conversation",
                    LearningKind::Fact,
                    0.8,
                    MemoryMeta {
                        namespace: ns.clone(),
                        thread_id: Some("shared-thread".into()),
                        ..MemoryMeta::default()
                    },
                ))
                .await
                .unwrap()
                .id,
        );
        source_ids.push(
            engine
                .store(StoreItem::learning(
                    "shared source item",
                    LearningKind::Fact,
                    0.8,
                    MemoryMeta {
                        namespace: ns,
                        source: SourceRef {
                            kind: SourceKind::File,
                            id: Some("shared-source".into()),
                        },
                        ..MemoryMeta::default()
                    },
                ))
                .await
                .unwrap()
                .id,
        );
    }

    assert_eq!(
        ops.forget_thread(Some(&engine), "shared-thread", |_| panic!("must not queue"))
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        ops.forget_source(Some(&engine), "shared-source", |_| panic!("must not queue"))
            .await
            .unwrap(),
        1
    );

    for (id, ns) in conversation_ids
        .into_iter()
        .zip([ordinary.clone(), service.clone()])
        .chain(
            source_ids
                .into_iter()
                .zip([ordinary.clone(), service.clone()]),
        )
    {
        let should_remain = ns == service;
        let hits = engine
            .get(GetRequest {
                ids: vec![id],
                reach: Some(Reach::exact(ns)),
            })
            .await
            .unwrap();
        assert_eq!(hits.is_empty(), !should_remain);
    }
    for (id, ns) in non_conversation_ids
        .into_iter()
        .zip([ordinary.clone(), service.clone()])
    {
        assert_eq!(
            engine
                .get(GetRequest {
                    ids: vec![id],
                    reach: Some(Reach::exact(ns)),
                })
                .await
                .unwrap()
                .len(),
            1
        );
    }

    let mut queued = Vec::new();
    assert_eq!(
        ops.forget_thread(None, "offline-thread", |d| queued.push(d))
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        queued,
        [crate::deletion::PendingDeletion::Thread {
            thread_id: "offline-thread".into(),
        }]
    );
    assert_eq!(
        ops.forget_source(None, "offline-source", |d| queued.push(d))
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        queued,
        [
            crate::deletion::PendingDeletion::Thread {
                thread_id: "offline-thread".into(),
            },
            crate::deletion::PendingDeletion::Source {
                source_id: "offline-source".into(),
            },
        ]
    );

    let failing = CountingEngine::new(true);
    let mut failed_deletions = Vec::new();
    let error = ops
        .forget_thread(Some(&failing), "failed-thread", |d| {
            failed_deletions.push(d)
        })
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        tinymemory_api::Error::InvalidRequest(message) if message == "engine down"
    ));
    assert_eq!(
        failed_deletions,
        [crate::deletion::PendingDeletion::Thread {
            thread_id: "failed-thread".into(),
        }]
    );

    failed_deletions.clear();
    let error = ops
        .forget_source(Some(&failing), "failed-source", |d| {
            failed_deletions.push(d)
        })
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        tinymemory_api::Error::InvalidRequest(message) if message == "engine down"
    ));
    assert_eq!(
        failed_deletions,
        [crate::deletion::PendingDeletion::Source {
            source_id: "failed-source".into(),
        }]
    );
}
