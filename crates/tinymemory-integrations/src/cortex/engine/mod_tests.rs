//! Round trips of every operation on both wires, through the doubles.

use super::*;
use crate::cortex::testing::{both, direct_engine, sample_items as items};
use std::sync::atomic::Ordering;
use tinymemory_api::{FetchMode, ItemKind, MemoryMeta, MetaFilter};

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
async fn recall_answers_once_from_one_pack_with_filtered_citations() {
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
        assert_eq!(seen.recalls.len(), 1, "one pack");
        assert_eq!(seen.recalls[0]["scope"], "app:tinymemory");
        assert_eq!(seen.recalls[0]["view"], "descend");
        assert_eq!(seen.answers.len(), 1, "one answer");
        assert_eq!(seen.answers[0]["use_pack_id"], "pack_test");
    }
}

#[tokio::test]
async fn recall_over_one_kind_uses_that_kind_scope() {
    for (engine, state) in both().await {
        let mut req = RecallRequest::new("anything", 3);
        req.filter = MetaFilter::kinds([ItemKind::Document]);
        let answer = engine.recall(req).await.unwrap();
        assert!(
            answer.citations.is_empty(),
            "no decodable events, still an answer"
        );
        assert!(!answer.answer.is_empty());
        let seen = state.seen.lock().unwrap();
        assert_eq!(seen.recalls[0]["scope"], "app:tinymemory/app:documents");
        assert!(seen.recalls[0].get("view").is_none());
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
    let error = direct_engine(&endpoint)
        .recall(RecallRequest::new("q", 1))
        .await
        .unwrap_err();
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
