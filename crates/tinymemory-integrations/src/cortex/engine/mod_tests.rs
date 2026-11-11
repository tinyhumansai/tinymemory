//! Round trips of every operation on both wires, through the doubles.

use super::*;
use crate::cortex::testing::{both, direct_engine, sample_items as items};
use std::sync::atomic::Ordering;
use tinymemory_api::{FetchMode, ItemKind, MemoryMeta, MetaFilter, Namespace, Reach};

#[tokio::test]
async fn every_kind_round_trips_through_store_list_fetch_and_forget() {
    for (engine, state) in both().await {
        let wire = engine.wire();
        for item in items() {
            let receipt = engine.store(item.clone()).await.unwrap();
            assert_eq!(receipt.id.as_str(), item.fingerprint(), "{wire:?}");
            assert!(!receipt.replayed);

            let listed = engine
                .list(ListRequest::new(MetaFilter::kinds([item.kind()]), 10))
                .await
                .unwrap();
            assert_eq!(listed.items.len(), 1, "{wire:?}: {listed:?}");
            let hit = &listed.items[0];
            assert_eq!(hit.id, receipt.id);
            assert_eq!(hit.text, item.render_text());
            assert_eq!(hit.meta, *item.meta());
            assert_eq!(hit.confidence, item.confidence());
            assert_eq!(hit.score, 0.0);
        }
        let fetched = engine
            .fetch(FetchRequest::new("helix", FetchMode::Hybrid, 10))
            .await
            .unwrap();
        let kinds: Vec<_> = fetched.hits.iter().map(|h| h.kind).collect();
        assert!(
            kinds.contains(&ItemKind::Conversation),
            "{wire:?}: {fetched:?}"
        );
        assert!(kinds.contains(&ItemKind::Learning));
        let chat = fetched
            .hits
            .iter()
            .find(|h| h.kind == ItemKind::Conversation)
            .unwrap();
        assert_eq!(
            chat.text,
            items()[1].render_text(),
            "the whole conversation"
        );
        assert!(fetched.hits.windows(2).all(|w| w[0].score > w[1].score));

        let ids: Vec<_> = items().iter().map(|i| i.fingerprint().into()).collect();
        let report = engine.forget(ForgetTarget::Ids(ids)).await.unwrap();
        assert_eq!(report.forgotten, 3, "{wire:?}");
        assert_eq!(state.event_count(), 0, "{wire:?}: every turn removed too");
        let empty = engine
            .list(ListRequest::new(MetaFilter::default(), 10))
            .await
            .unwrap();
        assert!(empty.items.is_empty());
    }
}

#[tokio::test]
async fn an_identical_store_is_a_replay_that_writes_nothing() {
    for (engine, state) in both().await {
        for item in items() {
            engine.store(item.clone()).await.unwrap();
            let writes = state.count("POST");
            let again = engine.store(item.clone()).await.unwrap();
            assert!(again.replayed, "{:?}", engine.wire());
            assert_eq!(again.id.as_str(), item.fingerprint());
            let new_posts: Vec<_> = state.requests()[..]
                .iter()
                .filter(|r| r.starts_with("POST"))
                .skip(writes)
                .cloned()
                .collect();
            assert!(
                new_posts.is_empty(),
                "a replay writes nothing: {new_posts:?}"
            );
        }
    }
}

#[tokio::test]
async fn an_item_forgotten_and_stored_again_is_written_again() {
    for (engine, _state) in both().await {
        let item = items().remove(2);
        engine.store(item.clone()).await.unwrap();
        engine
            .forget(ForgetTarget::Ids(vec![item.fingerprint().into()]))
            .await
            .unwrap();
        let again = engine.store(item).await.unwrap();
        assert!(
            !again.replayed,
            "fresh keys: the engine's kept idempotency record must not swallow it"
        );
        let listed = engine
            .list(ListRequest::new(MetaFilter::default(), 5))
            .await
            .unwrap();
        assert_eq!(listed.items.len(), 1);
    }
}

#[tokio::test]
async fn keyword_and_vector_fetch_are_unsupported_without_a_request() {
    for (engine, state) in both().await {
        for mode in [FetchMode::Keyword, FetchMode::Vector] {
            let error = engine
                .fetch(FetchRequest::new("q", mode, 5))
                .await
                .unwrap_err();
            assert!(matches!(error, Error::Unsupported(_)), "{error:?}");
        }
        assert!(state.requests().is_empty());
    }
}

#[tokio::test]
async fn invalid_requests_are_refused_before_any_request() {
    for (engine, state) in both().await {
        let blank = StoreItem::document("  ", MemoryMeta::default());
        assert!(matches!(
            engine.store(blank).await,
            Err(Error::InvalidRequest(_))
        ));
        assert!(matches!(
            engine
                .forget(ForgetTarget::Filter(MetaFilter::default()))
                .await,
            Err(Error::InvalidRequest(_))
        ));
        assert!(matches!(
            engine
                .list(ListRequest::new(MetaFilter::default(), 0))
                .await,
            Err(Error::InvalidRequest(_))
        ));
        assert!(matches!(
            engine.recall(RecallRequest::new(" ", 3)).await,
            Err(Error::InvalidRequest(_))
        ));
        assert!(state.requests().is_empty());
    }
}

#[tokio::test]
async fn forget_by_filter_removes_only_what_matches() {
    for (engine, _state) in both().await {
        for item in items() {
            engine.store(item).await.unwrap();
        }
        let filter = MetaFilter {
            thread_id: Some("t-chat".into()),
            ..MetaFilter::default()
        };
        let report = engine.forget(ForgetTarget::Filter(filter)).await.unwrap();
        assert_eq!(report.forgotten, 1);
        let left = engine
            .list(ListRequest::new(MetaFilter::default(), 10))
            .await
            .unwrap();
        let kinds: Vec<_> = left.items.iter().map(|h| h.kind).collect();
        assert_eq!(kinds, vec![ItemKind::Document, ItemKind::Learning]);
    }
}

#[tokio::test]
async fn an_unscoped_recall_packs_each_held_scope_exactly_and_answers_once() {
    for (engine, state) in both().await {
        for item in items() {
            engine.store(item).await.unwrap();
        }
        state.seen.lock().unwrap().recalls.clear();
        let mut req = RecallRequest::new("which editor helix", 2);
        req.filter = MetaFilter::kinds([ItemKind::Learning, ItemKind::Conversation]);
        let answer = engine.recall(req).await.unwrap();
        assert_eq!(answer.answer, "grounded answer for which editor helix");
        assert_eq!(answer.model.as_deref(), Some("reasoning"));
        assert!(!answer.citations.is_empty() && answer.citations.len() <= 2);
        assert!(
            answer
                .citations
                .iter()
                .all(|c| c.kind != ItemKind::Document && c.score.is_none())
        );
        let seen = state.seen.lock().unwrap();
        let mut scopes: Vec<&str> = seen
            .recalls
            .iter()
            .map(|body| body["scope"].as_str().unwrap())
            .collect();
        scopes.sort_unstable();
        assert_eq!(
            scopes,
            [
                "app:tinymemory/app:conversations",
                "app:tinymemory/app:learnings",
            ],
            "one pack per held scope of the admitted kinds, never the root"
        );
        for body in &seen.recalls {
            assert_eq!(body["view"], "granular", "{body}");
            assert_eq!(
                body["include"],
                serde_json::json!(["events", "facts", "beliefs", "episodes", "understanding"])
            );
            let events = body["budgets"]["per_layer_limits"]["events"]
                .as_u64()
                .unwrap();
            let whole = (events * crate::cortex::envelope::chunks::MAX_EVENT_TEXT_BYTES as u64)
                .min(super::fetch::MAX_PACK_TOKENS as u64);
            assert!(
                body["budgets"]["max_tokens"].as_u64() >= Some(whole),
                "room for every event whole: {body}"
            );
            assert!(body.get("temporal").is_none(), "{body}");
        }
        assert_eq!(seen.answers.len(), 1, "one answer");
        assert_eq!(seen.answers[0]["use_pack_id"], "pack_test");
    }
}

#[tokio::test]
async fn recall_applies_channel_before_the_pack_event_budget() {
    for (engine, state) in both().await {
        let workspace = "/tmp/channel-recall-budget";
        let marker = "shared-channel-recall-marker";
        let metadata = |channel: &str| MemoryMeta {
            workspace: Some(workspace.to_string()),
            channel: Some(channel.to_string()),
            ..MemoryMeta::default()
        };
        let wanted = StoreItem::document(format!("{marker} wanted item"), metadata("wanted"));
        let wanted_id = wanted.fingerprint();
        engine.store(wanted).await.unwrap();

        // A one-citation pack budgets two events. Put two equally relevant
        // items from another channel after the wanted item so top-k recall
        // returns those two first unless the channel label narrows the server
        // request before it applies that budget.
        for index in 0..2 {
            engine
                .store(StoreItem::document(
                    format!("{marker} other item {index}"),
                    metadata("other"),
                ))
                .await
                .unwrap();
        }

        state.seen.lock().unwrap().recalls.clear();
        let mut request = RecallRequest::new(marker, 1);
        request.filter = MetaFilter {
            workspace: Some(workspace.to_string()),
            channel: Some("wanted".to_string()),
            ..MetaFilter::kinds([ItemKind::Document])
        };
        let answer = engine.recall(request).await.unwrap();
        assert_eq!(
            answer
                .citations
                .iter()
                .map(|citation| citation.id.as_str())
                .collect::<Vec<_>>(),
            vec![wanted_id.as_str()],
            "{:?}: a filtered item outside the unfiltered pack's top two is still recalled",
            engine.wire()
        );
        let mut evidence_request = RecallRequest::new(marker, 1);
        evidence_request.mode = tinymemory_api::RecallMode::Evidence;
        evidence_request.filter = MetaFilter {
            workspace: Some(workspace.to_string()),
            channel: Some("wanted".to_string()),
            ..MetaFilter::kinds([ItemKind::Document])
        };
        let evidence = engine.recall(evidence_request).await.unwrap();
        assert_eq!(
            evidence
                .citations
                .iter()
                .map(|citation| citation.id.as_str())
                .collect::<Vec<_>>(),
            vec![wanted_id.as_str()],
            "{:?}: Evidence mode also supplements the broad pack before top-k",
            engine.wire()
        );
        let mut fetch = FetchRequest::new(marker, FetchMode::Hybrid, 1);
        fetch.filter = MetaFilter {
            workspace: Some(workspace.to_string()),
            channel: Some("wanted".to_string()),
            ..MetaFilter::kinds([ItemKind::Document])
        };
        let fetched = engine.fetch(fetch).await.unwrap();
        assert_eq!(
            fetched
                .hits
                .iter()
                .map(|hit| hit.id.as_str())
                .collect::<Vec<_>>(),
            vec![wanted_id.as_str()],
            "{:?}: fetch also supplements its broad pack with the channel pack",
            engine.wire()
        );
        let seen = state.seen.lock().unwrap();
        let filters: Vec<_> = seen
            .recalls
            .iter()
            .filter_map(|body| body.pointer("/filters/metadata/labels"))
            .filter_map(serde_json::Value::as_array)
            .collect();
        assert!(
            filters.iter().any(|labels| labels.iter().any(|label| {
                label
                    .as_str()
                    .is_some_and(|label| label.starts_with("tm:w:"))
            })),
            "{:?}: the broad workspace pack is retained for legacy records: {filters:?}",
            engine.wire()
        );
        assert!(
            filters.iter().any(|labels| labels.iter().any(|label| {
                label
                    .as_str()
                    .is_some_and(|label| label.starts_with("tm:cw:"))
            })),
            "{:?}: a channel/workspace pack narrows newly labelled records: {filters:?}",
            engine.wire()
        );
    }
}

#[tokio::test]
async fn channel_filters_keep_mixed_legacy_items_without_channel_labels() {
    for (engine, state) in both().await {
        let workspace = "mixed-channel-label-versions";
        let meta = || MemoryMeta {
            workspace: Some(workspace.to_string()),
            channel: Some("support".to_string()),
            ..MemoryMeta::default()
        };
        let legacy_text = "legacy channel item";
        let legacy = StoreItem::document(legacy_text, meta());
        let legacy_id = legacy.fingerprint();
        engine.store(legacy).await.unwrap();

        // A v3 envelope written before channel lookup labels were introduced
        // still has the channel in its envelope metadata, but only carries
        // the older item/workspace labels.
        {
            let mut log = state.log.lock().unwrap();
            let event = log
                .events
                .iter_mut()
                .find(|event| {
                    event
                        .pointer("/content/text")
                        .and_then(serde_json::Value::as_str)
                        == Some(legacy_text)
                })
                .expect("the legacy fixture was stored");
            event["context"]["labels"] = serde_json::Value::Array(
                event["context"]["labels"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|label| {
                        !label.as_str().is_some_and(|label| {
                            label.starts_with("tm:c:") || label.starts_with("tm:cw:")
                        })
                    })
                    .cloned()
                    .collect(),
            );
        }

        let current = StoreItem::document("current channel item", meta());
        let current_id = current.fingerprint();
        engine.store(current).await.unwrap();
        engine
            .store(StoreItem::document(
                "other channel item",
                MemoryMeta {
                    workspace: Some(workspace.to_string()),
                    channel: Some("other".to_string()),
                    ..MemoryMeta::default()
                },
            ))
            .await
            .unwrap();

        let filter = MetaFilter {
            workspace: Some(workspace.to_string()),
            channel: Some("support".to_string()),
            ..MetaFilter::kinds([ItemKind::Document])
        };
        let listed = engine
            .list(ListRequest::new(filter.clone(), 10))
            .await
            .unwrap();
        let mut actual: Vec<String> = listed
            .items
            .iter()
            .map(|hit| hit.id.as_str().to_string())
            .collect();
        actual.sort();
        let mut expected = vec![legacy_id, current_id];
        expected.sort();
        assert_eq!(
            actual,
            expected,
            "{:?}: channel narrowing must retain readable pre-label records",
            engine.wire()
        );

        let mut request = RecallRequest::new("channel item", 2);
        request.filter = filter;
        let answer = engine.recall(request).await.unwrap();
        let mut citations: Vec<String> = answer
            .citations
            .iter()
            .map(|citation| citation.id.as_str().to_string())
            .collect();
        citations.sort();
        assert_eq!(
            citations,
            expected,
            "{:?}: a broad legacy pack keeps old channel items readable beside newly labelled ones",
            engine.wire()
        );
    }
}

#[tokio::test]
async fn recall_over_one_kind_uses_that_kind_scope() {
    for (engine, state) in both().await {
        let mut req = RecallRequest::new("anything", 3);
        req.filter = MetaFilter {
            reach: Some(Reach::exact(Namespace::ROOT)),
            ..MetaFilter::kinds([ItemKind::Document])
        };
        let answer = engine.recall(req).await.unwrap();
        assert!(
            answer.citations.is_empty(),
            "no decodable events, still an answer"
        );
        assert!(!answer.answer.is_empty());
        let seen = state.seen.lock().unwrap();
        assert_eq!(seen.recalls.len(), 1);
        assert_eq!(seen.recalls[0]["scope"], "app:tinymemory/app:documents");
        assert_eq!(seen.recalls[0]["view"], "granular");
    }
}

#[tokio::test]
async fn an_unscoped_recall_with_nothing_held_answers_empty_without_a_pack() {
    for (engine, state) in both().await {
        let answer = engine
            .recall(RecallRequest::new("anything", 3))
            .await
            .unwrap();
        assert!(answer.answer.is_empty() && answer.citations.is_empty());
        assert_eq!(answer.model, None);
        let seen = state.seen.lock().unwrap();
        assert!(seen.recalls.is_empty(), "{:?}", seen.recalls);
        assert!(seen.answers.is_empty());
    }
}

#[tokio::test]
async fn health_is_ok_degraded_or_down_with_a_redacted_reason() {
    for (engine, state) in both().await {
        assert_eq!(engine.health().await, EngineHealth::Ok);
        *state.fail_all.lock().unwrap() = Some((503, "UNAVAILABLE"));
        assert!(matches!(engine.health().await, EngineHealth::Degraded(_)));
        *state.fail_all.lock().unwrap() = Some((401, "UNAUTHORIZED"));
        let EngineHealth::Down(reason) = engine.health().await else {
            panic!("a rejected credential is down");
        };
        assert!(reason.contains("withheld"), "{reason}");
        assert!(!reason.contains("failed: UNAUTHORIZED"), "{reason}");
        assert!(!reason.contains(crate::cortex::testing::TEST_TOKEN));
    }
}

#[tokio::test]
async fn a_pack_without_a_pack_id_or_answer_text_is_an_engine_error() {
    use axum::routing::post;
    use axum::{Json, Router};
    let app = Router::new().route(
        "/v1/recall",
        post(|| async { Json(serde_json::json!({ "layers": {} })) }),
    );
    let endpoint = crate::cortex::testing::serve(app).await;
    let mut req = RecallRequest::new("q", 1);
    req.filter = MetaFilter {
        reach: Some(Reach::exact(Namespace::ROOT)),
        ..MetaFilter::kinds([ItemKind::Document])
    };
    let error = direct_engine(&endpoint).recall(req).await.unwrap_err();
    assert!(matches!(error, Error::Engine(_)), "{error:?}");
}

#[test]
fn debug_names_the_engine_but_never_the_credential() {
    let engine = CortexEngine::direct(
        "https://db.example",
        CortexCredential::api_key("ctx_secret"),
    )
    .unwrap();
    let rendered = format!("{engine:?}");
    assert!(rendered.contains("cortexdb") && rendered.contains("db.example"));
    assert!(!rendered.contains("ctx_secret"));
    assert_eq!(engine.descriptor().id, crate::cortex::CORTEXDB_ENGINE_ID);
    assert_eq!(engine.wire(), CortexWire::Direct);
}

#[tokio::test]
async fn a_store_succeeds_when_ranked_recall_is_down() {
    for (engine, state) in both().await {
        state.recall_down.store(true, Ordering::SeqCst);
        engine.store(items().remove(0)).await.unwrap();
        let listed = engine
            .list(ListRequest::new(MetaFilter::default(), 5))
            .await
            .unwrap();
        assert_eq!(listed.items.len(), 1, "the settle probe is best-effort");
    }
}

#[tokio::test]
async fn an_answer_whose_pack_expired_recalls_that_scope_again() {
    for (engine, state) in both().await {
        for item in items() {
            engine.store(item).await.unwrap();
        }
        let mut req = RecallRequest::new("which editor helix", 2);
        req.filter = MetaFilter::kinds([ItemKind::Learning]);
        state.seen.lock().unwrap().recalls.clear();
        state.expire_packs.store(1, Ordering::SeqCst);
        let answer = engine.recall(req.clone()).await.unwrap();
        assert_eq!(answer.answer, "grounded answer for which editor helix");
        {
            let seen = state.seen.lock().unwrap();
            assert_eq!(seen.recalls.len(), 2, "the one scope, packed again");
            assert_eq!(seen.recalls[0], seen.recalls[1], "the same pack request");
            assert_eq!(seen.answers.len(), 2, "answered from the fresh pack");
        }

        // With several scopes every pack is read again, so the citations
        // come from packs read after whatever dropped them, not before.
        let mut wide = RecallRequest::new("which editor helix", 2);
        wide.filter = MetaFilter::kinds([ItemKind::Learning, ItemKind::Conversation]);
        state.seen.lock().unwrap().recalls.clear();
        state.expire_packs.store(1, Ordering::SeqCst);
        engine.recall(wide).await.unwrap();
        assert_eq!(
            state.seen.lock().unwrap().recalls.len(),
            4,
            "two scopes, each packed in both rounds"
        );

        state.seen.lock().unwrap().answers.clear();
        state.expire_packs.store(3, Ordering::SeqCst);
        assert!(
            matches!(engine.recall(req).await, Err(Error::NotFound(_))),
            "a bounded retry, not forever"
        );
        assert_eq!(state.seen.lock().unwrap().answers.len(), 3);
        state.expire_packs.store(0, Ordering::SeqCst);
    }
}

/// A bulk import that reads nothing back until it ends stores accepted:
/// every item gets its receipt, in order, with no visibility polling, on
/// either wire. The same batch stored visible times out on the hidden
/// listing, which is the wait this skips.
#[tokio::test]
async fn an_accepted_batch_returns_without_waiting_to_be_listed() {
    for (engine, state) in both().await {
        let wire = engine.wire();
        let engine = engine.with_test_timing(std::time::Duration::from_millis(50));
        // Listings never show the writes: a visible store would time out.
        state.hide_listing_for.store(usize::MAX, Ordering::SeqCst);
        let listings = |state: &crate::cortex::testing::Shared| {
            state.count("GET /v1/events") + state.count("GET /memory/events")
        };
        let before = listings(&state);
        let receipts = engine
            .store_many_with(items(), tinymemory_api::WriteOptions::accepted())
            .await
            .unwrap();
        let ids: Vec<String> = receipts.iter().map(|r| r.id.as_str().to_string()).collect();
        let wanted: Vec<String> = items().iter().map(StoreItem::fingerprint).collect();
        assert_eq!(ids, wanted, "{wire:?}: a receipt per item, in order");
        let polled = listings(&state) - before;
        assert_eq!(
            polled,
            items().len() + 1,
            "{wire:?}: one lookup per scope plus the conversation's legacy thread scan, no visibility polling"
        );
        let requests = state.requests();
        let read_requests: Vec<_> = requests
            .iter()
            .enumerate()
            .filter(|(_, request)| {
                request.starts_with("GET /v1/events") || request.starts_with("GET /memory/events")
            })
            .collect();
        let first_write = requests
            .iter()
            .position(|request| {
                request.starts_with("POST /v1/experience")
                    || request.starts_with("POST /memory/experience")
            })
            .unwrap();
        assert!(read_requests.iter().all(|(index, _)| *index < first_write));
        for item in items() {
            let item_digest = crate::cortex::envelope::labels::digest(&item.fingerprint());
            let scope = crate::cortex::envelope::scope_path(&item.meta().namespace, item.kind());
            let encoded_scope = crate::cortex::transport::urlencode(&scope);
            assert!(
                read_requests.iter().any(|(_, request)| {
                    request.contains(&item_digest)
                        && request.contains(&format!("scope={encoded_scope}"))
                }),
                "{wire:?}: current id queried in its own scope: {scope}"
            );
        }
        let thread_scope = crate::cortex::transport::urlencode(
            &crate::cortex::envelope::scope_path(&items()[1].meta().namespace, items()[1].kind()),
        );
        assert!(
            read_requests.iter().any(|(_, request)| {
                request.contains(&crate::cortex::envelope::labels::digest("t-chat"))
                    && request.contains(&format!("scope={thread_scope}"))
            }),
            "{wire:?}: compatibility fallback is confined to the conversation scope"
        );

        let fresh = vec![StoreItem::document(
            "Visible stores wait for the listing.",
            MemoryMeta::default(),
        )];
        let visible = engine
            .store_many_with(fresh, tinymemory_api::WriteOptions::visible())
            .await;
        assert!(
            matches!(visible, Err(Error::Unavailable(_))),
            "{wire:?}: a visible batch waits for the listing: {visible:?}"
        );
    }
}
