//! Whitespace-padded actions dispatch to recall, fetch, learn, and forget.

use super::*;

async fn seed_green_tea(engine: &ReferenceEngine, scope: &ToolScope) -> tinymemory_api::Result<()> {
    engine
        .store(StoreItem::learning(
            "prefers green tea",
            LearningKind::Preference,
            0.9,
            MemoryMeta {
                namespace: scope.place.clone(),
                ..MemoryMeta::default()
            },
        ))
        .await
        .map(|_| ())
}

#[tokio::test]
async fn single_call_accepts_padded_action_for_recall() {
    let engine = Arc::new(ReferenceEngine::new());
    let scope = scope();
    seed_green_tea(&engine, &scope)
        .await
        .expect("seed recall fixture");
    let tool = SingleTool::new("memory", MemoryTools::with_scope(engine, scope));

    let result = tool
        .call(
            json!({ "action": " recall ", "question": "tea" }),
            CallContext::default(),
        )
        .await
        .expect("padded recall action is accepted");

    assert_eq!(result["items"], 1);
}

#[tokio::test]
async fn single_call_accepts_padded_action_for_fetch() {
    let engine = Arc::new(ReferenceEngine::new());
    let scope = scope();
    seed_green_tea(&engine, &scope)
        .await
        .expect("seed fetch fixture");
    let tool = SingleTool::new("memory", MemoryTools::with_scope(engine, scope));

    let result = tool
        .call(
            json!({ "action": " fetch ", "query": "tea", "mode": "keyword" }),
            CallContext::default(),
        )
        .await
        .expect("padded fetch action is accepted");

    assert_eq!(result["items"], 1);
}

#[tokio::test]
async fn single_call_accepts_padded_action_for_learn() {
    let engine = Arc::new(ReferenceEngine::new());
    let scope = scope();
    let queue = queue(8);
    let tool = queued_tool(
        engine.clone(),
        scope.clone(),
        queue.clone(),
        Arc::new(WriteGate::new("padded-learn")),
    );

    let result = tool
        .call(
            json!({ "action": " learn ", "text": "likes jasmine tea" }),
            CallContext::default(),
        )
        .await
        .expect("padded learn action is accepted");

    assert_eq!(result["status"], "queued; not yet saved to memory");
    let entries = queue.entries().expect("queued learn");
    assert_eq!(entries.len(), 1);
    assert!(matches!(
        &entries[0].operation,
        WriteOperation::Learn {
            item: StoreItem::Learning { text, meta, .. }
        } if text == "likes jasmine tea" && meta.namespace == scope.place
    ));
    assert!(
        engine.is_empty(),
        "learn is queued rather than stored inline"
    );
}

#[tokio::test]
async fn single_call_accepts_padded_action_for_forget() {
    let engine = Arc::new(ReferenceEngine::new());
    let scope = scope();
    let receipt = engine
        .store(StoreItem::learning(
            "prefers green tea",
            LearningKind::Preference,
            0.9,
            MemoryMeta {
                namespace: scope.place.clone(),
                ..MemoryMeta::default()
            },
        ))
        .await
        .expect("seed memory to forget");
    let id = receipt.id.0;
    let expected_reach = scope.reach.clone().expect("scoped reach");
    let queue = queue(8);
    let tool = queued_tool(
        engine.clone(),
        scope,
        queue.clone(),
        Arc::new(WriteGate::new("padded-forget")),
    );

    let result = tool
        .call(
            json!({ "action": " forget ", "ids": [id.clone()] }),
            CallContext::default(),
        )
        .await
        .expect("padded forget action is accepted");

    assert_eq!(result["status"], "queued; not yet removed from memory");
    let entries = queue.entries().expect("queued forget");
    assert_eq!(entries.len(), 1);
    assert!(matches!(
        &entries[0].operation,
        WriteOperation::Forget { ids, reach } if ids == &vec![id] && reach == &expected_reach
    ));
    assert!(
        !engine.is_empty(),
        "forget is queued and must not remove memory inline"
    );
}
