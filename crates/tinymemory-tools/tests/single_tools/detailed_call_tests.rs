//! Detailed-call parity, recall details, and pure action classifiers.

use super::*;
use tinymemory_tools::{RecallDetails, SingleToolOutcome};

fn citation(id: &str, snippet: &str, scope: &ToolScope) -> tinymemory_api::Citation {
    tinymemory_api::Citation {
        id: tinymemory_api::ItemId(id.to_string()),
        kind: tinymemory_api::ItemKind::Learning,
        snippet: snippet.to_string(),
        meta: MemoryMeta {
            namespace: scope.place.clone(),
            agent_id: Some("writer".into()),
            ..MemoryMeta::default()
        },
        score: Some(0.9),
    }
}

struct StableAckSink;

impl WriteSink for StableAckSink {
    fn enqueue(
        &self,
        operation: WriteOperation,
        _scope: &ToolScope,
    ) -> tinymemory_api::Result<Ack> {
        Ok(match operation {
            WriteOperation::Learn { .. } => Ack::Learned {
                id: "stable-learning-id".into(),
            },
            WriteOperation::Forget { ids, .. } => Ack::Forgotten { queued: ids.len() },
        })
    }
}

#[tokio::test]
async fn single_call_with_details_returns_raw_question_full_citations_and_same_value() {
    let scope = scope();
    let citations = vec![
        citation("memory-one", "green tea", &scope),
        citation("memory-two", "jasmine tea", &scope),
    ];
    let mut engine = ControlledRecallEngine::new("green tea\n\njasmine tea");
    engine.recall_citations = citations.clone();
    let engine = Arc::new(engine);
    let tool = SingleTool::new("memory", MemoryTools::with_scope(engine.clone(), scope));
    let question = "  which tea do I prefer?  ";
    let args = json!({ "action": "recall", "question": question });

    let legacy_value = tool
        .call(args.clone(), CallContext::default())
        .await
        .expect("legacy call");
    let requests_after_legacy = engine
        .recall_requests
        .lock()
        .expect("recall requests")
        .clone();
    assert_eq!(requests_after_legacy.len(), 1);
    assert_eq!(requests_after_legacy[0].question, question);

    let outcome: SingleToolOutcome = tool
        .call_with_details(args, CallContext::default())
        .await
        .expect("call with details");
    let requests_after_details = engine
        .recall_requests
        .lock()
        .expect("recall requests")
        .clone();
    assert_eq!(
        requests_after_details.len(),
        requests_after_legacy.len() + 1,
        "the detailed call performs one native recall"
    );
    assert_eq!(requests_after_details[1].question, question);
    assert_eq!(outcome.value, legacy_value);
    assert_eq!(outcome.value["items"], 2);

    let details = outcome.recall.expect("recall details");
    assert_eq!(details.question, question);
    assert_eq!(details.citations, citations);
    assert_eq!(
        details
            .citations
            .iter()
            .map(|citation| citation.id.0.clone())
            .collect::<Vec<_>>(),
        vec!["memory-one".to_string(), "memory-two".to_string()]
    );
}

#[tokio::test]
async fn call_with_details_marks_empty_recall_but_not_fetch_or_writes() {
    let engine = Arc::new(ControlledRecallEngine::new("  "));
    let tool = SingleTool::new("memory", MemoryTools::with_scope(engine, scope()))
        .with_write_sink(Arc::new(StableAckSink));

    let recall_args = json!({ "action": "recall", "question": "nothing relevant" });
    let legacy_recall = tool
        .call(recall_args.clone(), CallContext::default())
        .await
        .expect("legacy empty recall");
    let empty_recall = tool
        .call_with_details(recall_args, CallContext::default())
        .await
        .expect("empty recall succeeds");
    assert_eq!(empty_recall.value, legacy_recall);
    assert_eq!(
        empty_recall.value,
        json!({
            "evidence": "",
            "note": "Nothing relevant is stored in memory."
        })
    );
    assert_eq!(
        empty_recall.recall,
        Some(RecallDetails {
            question: "nothing relevant".into(),
            citations: Vec::new(),
        })
    );

    let fetch_args = json!({ "action": "fetch", "query": "nothing", "mode": "keyword" });
    let legacy_fetch = tool
        .call(fetch_args.clone(), CallContext::default())
        .await
        .expect("legacy fetch");
    let fetched = tool
        .call_with_details(fetch_args, CallContext::default())
        .await
        .expect("fetch succeeds");
    assert_eq!(fetched.value, legacy_fetch);
    assert!(fetched.recall.is_none());
    assert_eq!(fetched.value["items"], 0);

    let learn_args = json!({ "action": "learn", "text": "likes mint tea" });
    let legacy_learn = tool
        .call(learn_args.clone(), CallContext::default())
        .await
        .expect("legacy learn");
    let learned = tool
        .call_with_details(learn_args, CallContext::default())
        .await
        .expect("learn succeeds");
    assert_eq!(learned.value, legacy_learn);
    assert!(learned.recall.is_none());

    let forget_args = json!({ "action": "forget", "ids": ["missing-id"] });
    let legacy_forget = tool
        .call(forget_args.clone(), CallContext::default())
        .await
        .expect("legacy forget");
    let forgotten = tool
        .call_with_details(forget_args, CallContext::default())
        .await
        .expect("forget succeeds");
    assert_eq!(forgotten.value, legacy_forget);
    assert!(forgotten.recall.is_none());
}

#[tokio::test]
async fn call_with_details_preserves_action_errors() {
    let writable = SingleTool::new(
        "memory",
        MemoryTools::with_scope(Arc::new(ReferenceEngine::new()), scope()),
    );
    let mut read_only_scope = scope();
    read_only_scope.writes = false;
    let read_only = SingleTool::new(
        "memory",
        MemoryTools::with_scope(Arc::new(ReferenceEngine::new()), read_only_scope),
    );
    for (tool, args) in [
        (&writable, json!({})),
        (&writable, json!({ "action": "unknown" })),
        (&writable, json!({ "action": "recall", "question": " " })),
        (&writable, json!({ "action": "recall" })),
        (&writable, json!({ "action": "learn", "text": "a fact" })),
        (&writable, json!({ "action": " unknown " })),
        (&read_only, json!({ "action": "learn", "text": "a fact" })),
    ] {
        let legacy = tool.call(args.clone(), CallContext::default()).await;
        let detailed = tool.call_with_details(args, CallContext::default()).await;
        match (legacy, detailed) {
            (Err(legacy), Err(detailed)) => {
                assert_eq!(legacy.code(), detailed.code());
                assert_eq!(legacy.to_string(), detailed.to_string());
            }
            (Ok(_), _) => panic!("legacy call unexpectedly accepted invalid input"),
            (_, Ok(_)) => panic!("detailed call unexpectedly accepted invalid input"),
        }
    }

    let padded_unknown = writable
        .call_with_details(json!({ "action": " unknown " }), CallContext::default())
        .await
        .expect_err("padded unknown action is still rejected");
    assert!(
        padded_unknown
            .to_string()
            .contains("unknown action  unknown ;")
    );
}

#[test]
fn single_tool_pure_action_classifiers_match_instance_methods() {
    let tool = SingleTool::new(
        "memory",
        MemoryTools::with_scope(Arc::new(ReferenceEngine::new()), scope()),
    );
    let cases = [
        (
            json!({ "action": "recall" }),
            PermissionClass::ReadOnly,
            ConcurrencyClass::Safe,
            false,
        ),
        (
            json!({ "action": "fetch" }),
            PermissionClass::ReadOnly,
            ConcurrencyClass::Safe,
            false,
        ),
        (
            json!({ "action": "learn" }),
            PermissionClass::Write,
            ConcurrencyClass::Exclusive,
            true,
        ),
        (
            json!({ "action": "forget" }),
            PermissionClass::Write,
            ConcurrencyClass::Exclusive,
            true,
        ),
        (
            json!({ "action": " recall " }),
            PermissionClass::Write,
            ConcurrencyClass::Exclusive,
            false,
        ),
        (
            json!({ "action": " fetch " }),
            PermissionClass::Write,
            ConcurrencyClass::Exclusive,
            false,
        ),
        (
            json!({ "action": " learn " }),
            PermissionClass::Write,
            ConcurrencyClass::Exclusive,
            true,
        ),
        (
            json!({ "action": " forget " }),
            PermissionClass::Write,
            ConcurrencyClass::Exclusive,
            true,
        ),
        (
            json!({ "action": "unknown" }),
            PermissionClass::Write,
            ConcurrencyClass::Exclusive,
            false,
        ),
        (
            json!({ "action": 4 }),
            PermissionClass::Write,
            ConcurrencyClass::Exclusive,
            false,
        ),
        (
            json!({ "action": null }),
            PermissionClass::Write,
            ConcurrencyClass::Exclusive,
            false,
        ),
        (
            json!({}),
            PermissionClass::Write,
            ConcurrencyClass::Exclusive,
            false,
        ),
    ];
    for (args, permission, concurrency, is_write) in cases {
        assert_eq!(
            tinymemory_tools::tools::single::permission_for(&args),
            permission,
            "{args}"
        );
        assert_eq!(
            tinymemory_tools::tools::single::concurrency_for(&args),
            concurrency,
            "{args}"
        );
        assert_eq!(
            tinymemory_tools::tools::single::is_write_action(&args),
            is_write,
            "{args}"
        );
        assert_eq!(tool.permission_for(&args), permission, "{args}");
        assert_eq!(tool.concurrency_for(&args), concurrency, "{args}");
    }
}
