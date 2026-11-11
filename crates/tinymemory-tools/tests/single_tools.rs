//! The single-name memory facade: action dispatch, scope, pages, prompt
//! windows, bounded evidence, and writes through the durable queue.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use async_trait::async_trait;
use serde_json::json;
use tinymemory_api::conformance::ReferenceEngine;
use tinymemory_api::{
    EngineDescriptor, Error, FetchMode, FetchRequest, LearningKind, MemoryEngine, MemoryMeta,
    Namespace, Reach, RecallRequest, SourceKind, StoreItem, ThreadWindow, ToolCallRef, Turn,
    TurnRange,
};
use tinymemory_tools::list_store::{ListStore, MemoryListStore, StoreError};
use tinymemory_tools::pending::{
    Ack, Binding, Entry, Limits, PendingWrites, WriteExecutor, WriteGate, WriteOperation,
};
use tinymemory_tools::tools::single::{
    CallContext, ConcurrencyClass, PendingWriteSink, PermissionClass, SingleTool, WriteSink,
};
use tinymemory_tools::tools::{MemoryTools, ToolScope};

fn poll_once<F: Future>(mut future: Pin<&mut F>) -> Poll<F::Output> {
    let mut context = Context::from_waker(Waker::noop());
    future.as_mut().poll(&mut context)
}

fn scope() -> ToolScope {
    let root = Namespace::agent("writer");
    ToolScope {
        place: root.clone(),
        reach: Some(Reach::subtree(root)),
        writes: true,
        zone: None,
        workspace: Some("/workspace/project".into()),
        thread_id: Some("thread-current".into()),
        agent_id: Some("agent-writer".into()),
        tool_call: Some(ToolCallRef {
            name: "memory".into(),
            id: Some("call-1".into()),
        }),
    }
}

type Queue = Arc<PendingWrites<MemoryListStore<Entry>>>;

fn queue(max_entries: usize) -> Queue {
    Arc::new(PendingWrites::new(
        MemoryListStore::default(),
        Limits {
            max_entries,
            max_bytes: 2 * 1024 * 1024,
        },
    ))
}

fn queued_tool<S>(
    engine: Arc<ReferenceEngine>,
    scope: ToolScope,
    queue: Arc<PendingWrites<S>>,
    gate: Arc<WriteGate>,
) -> SingleTool
where
    S: ListStore<Entry> + 'static,
{
    let sink = Arc::new(PendingWriteSink::new(
        queue,
        gate,
        Binding("workspace-owner-v1".into()),
    ));
    SingleTool::new("memory", MemoryTools::with_scope(engine, scope)).with_write_sink(sink)
}

struct PolicySink {
    pending: PendingWriteSink<MemoryListStore<Entry>>,
    preflight_error: Option<Error>,
    enqueue_error: Option<Error>,
    scrub_learnings: bool,
    preflight_calls: AtomicUsize,
}

impl PolicySink {
    fn new(
        queue: Queue,
        preflight_error: Option<Error>,
        enqueue_error: Option<Error>,
        scrub_learnings: bool,
    ) -> Self {
        Self {
            pending: PendingWriteSink::new(
                queue,
                Arc::new(WriteGate::new("process-policy")),
                Binding("workspace-owner-v1".into()),
            ),
            preflight_error,
            enqueue_error,
            scrub_learnings,
            preflight_calls: AtomicUsize::new(0),
        }
    }
}

impl WriteSink for PolicySink {
    fn preflight(&self, _: &ToolScope) -> tinymemory_api::Result<()> {
        self.preflight_calls.fetch_add(1, Ordering::SeqCst);
        match &self.preflight_error {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }

    fn enqueue(
        &self,
        mut operation: WriteOperation,
        scope: &ToolScope,
    ) -> tinymemory_api::Result<Ack> {
        if let Some(error) = &self.enqueue_error {
            return Err(error.clone());
        }
        if self.scrub_learnings
            && let WriteOperation::Learn {
                item: StoreItem::Learning { text, .. },
            } = &mut operation
        {
            *text = text.replace("secret", "redacted");
        }
        self.pending.enqueue(operation, scope)
    }
}

fn tool(engine: Arc<ReferenceEngine>, scope: ToolScope) -> SingleTool {
    queued_tool(
        engine,
        scope,
        queue(256),
        Arc::new(WriteGate::new("process-1")),
    )
}

#[tokio::test]
async fn one_configurable_tool_dispatches_actions_and_classifies_each_action() {
    let engine = Arc::new(ReferenceEngine::new());
    engine
        .store(StoreItem::learning(
            "prefers green tea",
            LearningKind::Preference,
            0.9,
            MemoryMeta {
                namespace: scope().place,
                ..MemoryMeta::default()
            },
        ))
        .await
        .expect("seed memory");
    let queue = queue(8);
    let tool = SingleTool::new("community_memory", MemoryTools::with_scope(engine, scope()))
        .with_write_sink(Arc::new(PendingWriteSink::new(
            queue.clone(),
            Arc::new(WriteGate::new("process-1")),
            Binding("workspace-owner-v1".into()),
        )));

    let spec = tool.spec();
    assert_eq!(spec.name, "community_memory");
    assert_eq!(
        spec.parameters["properties"]["action"]["enum"],
        json!(["recall", "fetch", "learn", "forget"])
    );
    assert!(spec.parameters["properties"].get("limit").is_none());
    assert!(spec.parameters["properties"].get("instructions").is_none());
    let recall_schema = &spec.parameters["oneOf"][0]["properties"];
    assert!(recall_schema.get("limit").is_none());
    assert!(recall_schema.get("instructions").is_none());
    let fetch_schema = &spec.parameters["oneOf"][1]["properties"];
    assert!(fetch_schema.get("limit").is_none());
    assert_eq!(fetch_schema["mode"]["default"], "keyword");
    assert!(
        spec.parameters["oneOf"][3]["properties"]["ids"]
            .get("maxItems")
            .is_none()
    );
    for (action, permission, concurrency) in [
        ("recall", PermissionClass::ReadOnly, ConcurrencyClass::Safe),
        ("fetch", PermissionClass::ReadOnly, ConcurrencyClass::Safe),
        ("learn", PermissionClass::Write, ConcurrencyClass::Exclusive),
        (
            "forget",
            PermissionClass::Write,
            ConcurrencyClass::Exclusive,
        ),
    ] {
        let args = json!({ "action": action });
        assert_eq!(tool.permission_for(&args), permission, "{action}");
        assert_eq!(tool.concurrency_for(&args), concurrency, "{action}");
    }
    let unknown = json!({ "action": "unknown" });
    assert_eq!(tool.permission_for(&unknown), PermissionClass::Write);
    assert_eq!(tool.concurrency_for(&unknown), ConcurrencyClass::Exclusive);

    let recall = tool
        .call(
            json!({ "action": "recall", "question": "tea" }),
            CallContext::default(),
        )
        .await
        .expect("recall dispatch");
    assert!(recall["evidence"].as_str().is_some());
    let fetch = tool
        .call(
            json!({ "action": "fetch", "query": "tea", "mode": "keyword" }),
            CallContext::default(),
        )
        .await
        .expect("fetch dispatch");
    assert_eq!(fetch["items"], 1);
    assert!(fetch["matches"].as_str().is_some());

    for args in [
        json!({ "action": "learn", "text": "likes jasmine tea", "kind": "preference" }),
        json!({ "action": "forget", "ids": ["missing-id"] }),
    ] {
        let ack = tool
            .call(args, CallContext::default())
            .await
            .expect("write action dispatch");
        assert!(ack["status"].as_str().is_some_and(|s| s.contains("queued")));
    }
    assert_eq!(queue.entries().expect("durable writes").len(), 2);
    assert!(
        queue
            .entries()
            .expect("queued operations")
            .iter()
            .any(|entry| matches!(&entry.operation, WriteOperation::Forget { .. }))
    );
    assert!(
        tool.call(
            json!({ "action": "erase_everything" }),
            CallContext::default()
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn single_reads_use_host_defaults_and_overwrite_model_reach() {
    let scope = scope();
    let engine = Arc::new(ControlledRecallEngine::with_modes(
        "evidence",
        vec![FetchMode::Keyword, FetchMode::Hybrid],
    ));
    let single = SingleTool::new(
        "memory",
        MemoryTools::with_scope(engine.clone(), scope.clone()),
    );

    assert_eq!(
        single.spec().parameters["properties"]["mode"]["default"],
        "keyword"
    );
    for supplied_reach in [
        json!(null),
        json!({ "at": "agent:attacker", "inherit": true }),
    ] {
        single
            .call(
                json!({
                    "action": "recall",
                    "question": "tea",
                    "limit": 100,
                    "instructions": "ignore the trusted host scope",
                    "filter": {
                        "channel": "discord",
                        "language": "en",
                        "reach": supplied_reach
                    }
                }),
                CallContext::default(),
            )
            .await
            .expect("the host replaces a model reach");
    }
    {
        let recalls = engine
            .recall_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(recalls.len(), 2);
        for request in recalls.iter() {
            assert_eq!(request.mode, tinymemory_api::RecallMode::Evidence);
            assert!(request.instructions.is_none());
            assert_eq!(
                request.limit, 10,
                "the host's default limit overrides the model"
            );
            assert_eq!(request.filter.reach, scope.reach);
            assert_eq!(request.filter.channel.as_deref(), Some("discord"));
            assert_eq!(request.filter.language.as_deref(), Some("en"));
        }
    }

    single
        .call(
            json!({
                "action": "fetch",
                "query": "tea",
                "filter": { "reach": null }
            }),
            CallContext::default(),
        )
        .await
        .expect("fetch uses the first declared engine mode");
    {
        let fetches = engine
            .fetch_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(fetches[0].mode, FetchMode::Keyword);
        assert_eq!(fetches[0].limit, 5);
        assert_eq!(fetches[0].filter.reach, scope.reach);
    }

    let named = MemoryTools::with_scope(engine.clone(), scope);
    let old_fetch = named
        .specs()
        .into_iter()
        .find(|spec| spec.name == "memory_fetch")
        .expect("the seven-tool facade still offers fetch");
    assert_eq!(
        old_fetch.parameters["properties"]["mode"]["default"],
        "hybrid"
    );
    named
        .call("memory_fetch", json!({ "query": "tea" }))
        .await
        .expect("the seven-tool fetch path retains its hybrid default");
    assert!(
        named
            .call(
                "memory_fetch",
                json!({ "query": "tea", "filter": { "channel": "discord" } }),
            )
            .await
            .is_err(),
        "the seven-tool strict filter schema is unchanged"
    );
    assert_eq!(
        engine.fetch_requests.lock().expect("recorded fetches")[1].mode,
        FetchMode::Hybrid
    );
}

#[tokio::test]
async fn learn_stamps_trusted_scope_and_acknowledges_only_durable_queueing() {
    let engine = Arc::new(ReferenceEngine::new());
    let queue = queue(4);
    let gate = Arc::new(WriteGate::new("process-2"));
    let tool = SingleTool::new(
        "trusted_memory",
        MemoryTools::with_scope(engine.clone(), scope()),
    )
    .with_write_sink(Arc::new(PendingWriteSink::new(
        queue.clone(),
        gate,
        Binding("workspace-owner-v1".into()),
    )));
    let ack = tool
        .call(
            json!({
                "action": "learn",
                "text": "prefers short reports",
                "kind": "preference",
                "meta": {
                    "namespace": "agent:attacker",
                    "workspace": "/attacker",
                    "thread_id": "other-thread",
                    "agent_id": "other-agent",
                    "source": { "kind": "import" }
                }
            }),
            CallContext::default(),
        )
        .await
        .expect("durable queue acknowledgement");

    assert!(ack["status"].as_str().is_some_and(|s| s.contains("queued")));
    assert!(engine.is_empty(), "the facade must not write to the engine");
    let entries = queue.entries().expect("persisted queue");
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(entry.binding, Binding("workspace-owner-v1".into()));
    assert_eq!(entry.scope_root, Namespace::agent("writer"));
    assert_eq!(entry.epoch.as_ref().map(|epoch| epoch.generation), Some(0));
    let WriteOperation::Learn {
        item: StoreItem::Learning { meta, .. },
    } = &entry.operation
    else {
        panic!("learn action must queue a learning operation")
    };
    assert_eq!(meta.namespace, scope().place);
    assert_eq!(meta.workspace.as_deref(), Some("/workspace/project"));
    assert_eq!(meta.thread_id.as_deref(), Some("thread-current"));
    assert_eq!(meta.agent_id.as_deref(), Some("agent-writer"));
    assert_eq!(
        meta.tool_call.as_ref().map(|call| call.name.as_str()),
        Some("memory")
    );
    assert_eq!(
        meta.tool_call.as_ref().and_then(|call| call.id.as_deref()),
        Some("call-1")
    );
    assert_eq!(meta.source.kind, SourceKind::Agent);
}

#[tokio::test]
async fn learn_uses_the_configured_name_only_when_scope_has_no_call_reference() {
    let engine = Arc::new(ReferenceEngine::new());
    let queue = queue(4);
    let mut tool_scope = scope();
    tool_scope.tool_call = None;
    let tool = SingleTool::new(
        "fallback_memory",
        MemoryTools::with_scope(engine, tool_scope),
    )
    .with_write_sink(Arc::new(PendingWriteSink::new(
        queue.clone(),
        Arc::new(WriteGate::new("process-fallback")),
        Binding("workspace-owner-v1".into()),
    )));

    tool.call(
        json!({ "action": "learn", "text": "fallback metadata" }),
        CallContext::default(),
    )
    .await
    .expect("queued learn");
    let entries = queue.entries().expect("queued write");
    let WriteOperation::Learn {
        item: StoreItem::Learning { meta, .. },
    } = &entries[0].operation
    else {
        panic!("learn action must queue a learning operation")
    };
    let call = meta.tool_call.as_ref().expect("tool call fallback");
    assert_eq!(call.name, "fallback_memory");
    assert_eq!(call.id, None);
}

#[tokio::test]
async fn forget_trims_drops_blank_sorts_and_deduplicates_unbounded_ids() {
    let engine = Arc::new(ReferenceEngine::new());
    let queue = queue(4);
    let tool = queued_tool(
        engine,
        scope(),
        queue.clone(),
        Arc::new(WriteGate::new("process-forget-normalization")),
    );

    tool.call(
        json!({
            "action": "forget",
            "ids": [" z ", "", "alpha", "z", "  alpha  ", "   "]
        }),
        CallContext::default(),
    )
    .await
    .expect("mixed blank and duplicate ids are normalized");
    let entries = queue.entries().expect("queued normalized forget");
    let WriteOperation::Forget { ids, .. } = &entries[0].operation else {
        panic!("forget action must queue a forget operation")
    };
    assert_eq!(ids, &["alpha", "z"]);

    let before = entries.len();
    assert!(
        tool.call(
            json!({ "action": "forget", "ids": [" ", "\t"] }),
            CallContext::default(),
        )
        .await
        .is_err()
    );
    assert_eq!(queue.entries().expect("unchanged queue").len(), before);

    let ids: Vec<String> = (0..=tinymemory_api::explore::MAX_GET_IDS)
        .map(|index| format!("id-{index:03}"))
        .collect();
    tool.call(
        json!({ "action": "forget", "ids": ids }),
        CallContext::default(),
    )
    .await
    .expect("forget is not constrained by the read/get batch cap");
    let entries = queue.entries().expect("large forget queued");
    let WriteOperation::Forget { ids, .. } = &entries[1].operation else {
        panic!("forget action must queue a forget operation")
    };
    assert_eq!(ids.len(), tinymemory_api::explore::MAX_GET_IDS + 1);
}

#[tokio::test]
async fn readonly_scope_and_missing_sink_never_fall_back_to_engine_writes() {
    let engine = Arc::new(ReferenceEngine::new());
    let mut read_scope = scope();
    read_scope.writes = false;
    let queue = queue(4);
    let read_only = queued_tool(
        engine.clone(),
        read_scope,
        queue.clone(),
        Arc::new(WriteGate::new("process-3")),
    );
    assert!(
        read_only
            .call(
                json!({ "action": "learn", "text": "should be refused" }),
                CallContext::default(),
            )
            .await
            .is_err()
    );
    assert!(queue.entries().expect("queue state").is_empty());
    assert!(engine.is_empty());

    let missing_sink = SingleTool::new("memory", MemoryTools::with_scope(engine.clone(), scope()));
    assert!(
        missing_sink
            .call(
                json!({ "action": "learn", "text": "must not go direct" }),
                CallContext::default(),
            )
            .await
            .is_err()
    );
    assert!(engine.is_empty());
}

#[tokio::test]
async fn write_sink_preflight_and_policy_errors_preserve_host_error_types() {
    let off_queue = queue(8);
    let sink = Arc::new(PolicySink::new(
        off_queue.clone(),
        Some(Error::Off("memory is disabled".into())),
        None,
        false,
    ));
    let tool = SingleTool::new(
        "memory",
        MemoryTools::with_scope(Arc::new(ReferenceEngine::new()), scope()),
    )
    .with_write_sink(sink.clone());

    tool.call(
        json!({ "action": "recall", "question": "tea" }),
        CallContext::default(),
    )
    .await
    .expect("read calls do not run write preflight");
    assert_eq!(sink.preflight_calls.load(Ordering::SeqCst), 0);

    for malformed_write in [
        json!({ "action": "learn" }),
        json!({ "action": "forget", "ids": [] }),
    ] {
        assert_eq!(
            tool.call(malformed_write, CallContext::default()).await,
            Err(Error::Off("memory is disabled".into())),
            "preflight keeps the host's error precedence for known writes"
        );
    }
    assert_eq!(sink.preflight_calls.load(Ordering::SeqCst), 2);
    assert!(
        off_queue
            .entries()
            .expect("disabled queue stays empty")
            .is_empty()
    );

    let policy_queue = queue(8);
    let policy_sink = Arc::new(PolicySink::new(
        policy_queue.clone(),
        None,
        Some(Error::InvalidRequest("host policy rejected write".into())),
        false,
    ));
    let policy_tool = SingleTool::new(
        "memory",
        MemoryTools::with_scope(Arc::new(ReferenceEngine::new()), scope()),
    )
    .with_write_sink(policy_sink.clone());
    assert_eq!(
        policy_tool
            .call(
                json!({ "action": "learn", "text": "a valid learning" }),
                CallContext::default(),
            )
            .await,
        Err(Error::InvalidRequest("host policy rejected write".into()))
    );
    assert_eq!(policy_sink.preflight_calls.load(Ordering::SeqCst), 1);
    assert!(
        policy_queue
            .entries()
            .expect("policy rejection happens before queueing")
            .is_empty()
    );

    let scrubbed_queue = queue(8);
    let scrubber = Arc::new(PolicySink::new(scrubbed_queue.clone(), None, None, true));
    let scrubbed_tool = SingleTool::new(
        "memory",
        MemoryTools::with_scope(Arc::new(ReferenceEngine::new()), scope()),
    )
    .with_write_sink(scrubber);
    let ack = scrubbed_tool
        .call(
            json!({ "action": "learn", "text": "secret token" }),
            CallContext::default(),
        )
        .await
        .expect("the policy wrapper delegates the scrubbed operation");
    let entries = scrubbed_queue.entries().expect("scrubbed write queued");
    let WriteOperation::Learn { item } = &entries[0].operation else {
        panic!("learn action must queue a learning operation")
    };
    let StoreItem::Learning { text, .. } = item else {
        panic!("learn operation must hold a learning item")
    };
    assert_eq!(text, "redacted token");
    assert_eq!(ack["id"], item.fingerprint());
}

#[tokio::test]
async fn fetch_is_five_items_with_a_cursor_and_continues_the_same_search() {
    let engine = Arc::new(ReferenceEngine::new());
    let scope = scope();
    for index in 0..6 {
        engine
            .store(StoreItem::learning(
                format!("tea note {index}"),
                LearningKind::Fact,
                0.9,
                MemoryMeta {
                    namespace: scope.place.clone(),
                    ..MemoryMeta::default()
                },
            ))
            .await
            .expect("seed item");
    }
    let tool = tool(engine, scope);
    let first = tool
        .call(
            json!({ "action": "fetch", "query": "tea", "mode": "keyword", "limit": 50 }),
            CallContext::default(),
        )
        .await
        .expect("first page");
    assert_eq!(first["items"], 5);
    let cursor = first["next_cursor"].as_str().expect("next cursor");
    let second = tool
        .call(
            json!({ "action": "fetch", "query": "tea", "mode": "keyword", "cursor": cursor }),
            CallContext::default(),
        )
        .await
        .expect("continued page");
    assert_eq!(second["items"], 1);
    assert!(second.get("next_cursor").is_none());
}

fn conversation(scope: &ToolScope, thread_id: &str, turn: u32, text: &str) -> StoreItem {
    StoreItem::Conversation {
        turns: vec![Turn::new(tinymemory_api::Role::User, text)],
        meta: MemoryMeta {
            namespace: scope.place.clone(),
            thread_id: Some(thread_id.to_string()),
            turns: Some(TurnRange {
                first: turn,
                last: turn,
            }),
            ..MemoryMeta::default()
        },
    }
}

#[tokio::test]
async fn recall_and_fetch_exclude_the_live_window_but_keep_compacted_turns() {
    let engine = Arc::new(ReferenceEngine::new());
    let scope = scope();
    engine
        .store(conversation(
            &scope,
            "thread-current",
            2,
            "oolong tea was from Taiwan",
        ))
        .await
        .expect("store compacted turn");
    engine
        .store(conversation(
            &scope,
            "thread-current",
            7,
            "oolong tea is in this prompt",
        ))
        .await
        .expect("store prompt turn");
    let mut no_thread_scope = scope.clone();
    no_thread_scope.thread_id = None;
    let no_thread_tool = tool(engine.clone(), no_thread_scope);
    let tool = tool(engine, scope);
    let context = CallContext {
        thread_window: Some(ThreadWindow {
            thread_id: "thread-current".into(),
            from_turn: 6,
        }),
    };

    for args in [
        json!({ "action": "recall", "question": "oolong tea" }),
        json!({ "action": "fetch", "query": "oolong", "mode": "keyword" }),
    ] {
        let result = tool.call(args, context.clone()).await.expect("scoped read");
        let rendered = result.to_string();
        assert!(rendered.contains("from Taiwan"), "{rendered}");
        assert!(!rendered.contains("in this prompt"), "{rendered}");
    }

    let mismatch = tool
        .call(
            json!({ "action": "fetch", "query": "oolong", "mode": "keyword" }),
            CallContext {
                thread_window: Some(ThreadWindow::whole("thread-elsewhere")),
            },
        )
        .await
        .expect("mismatched window falls back to current thread");
    assert_eq!(mismatch["items"], 0);

    let without_current_thread = no_thread_tool
        .call(
            json!({ "action": "fetch", "query": "oolong", "mode": "keyword" }),
            CallContext {
                thread_window: Some(ThreadWindow {
                    thread_id: "thread-current".into(),
                    from_turn: 6,
                }),
            },
        )
        .await
        .expect("a prompt window without a current thread is ignored");
    assert_eq!(without_current_thread["items"], 2);
}

#[tokio::test]
async fn recall_caps_utf8_evidence_without_splitting_a_line() {
    let evidence = format!(
        "entry zero {}\nentry one {}\nentry two {}",
        "x".repeat(8_890),
        "界".repeat(500),
        "x".repeat(2_000)
    );
    let engine = Arc::new(ControlledRecallEngine::new(evidence));
    let result = SingleTool::new("memory", MemoryTools::with_scope(engine, scope()))
        .call(
            json!({ "action": "recall", "question": "entry" }),
            CallContext::default(),
        )
        .await
        .expect("evidence result");
    let evidence = result["evidence"].as_str().expect("evidence text");
    assert!(evidence.contains("entry zero"));
    assert!(
        !evidence.contains("entry one"),
        "byte counting excludes this multibyte line even though its scalar count fits"
    );
    assert!(!evidence.contains("entry two"));
    assert!(evidence.contains("[…]"));
}

#[tokio::test]
async fn recall_keeps_ranked_entries_then_whole_lines_and_reports_the_omitted_count() {
    let evidence = evidence_of(30, 4, 200);
    let engine = Arc::new(ControlledRecallEngine::new(evidence.clone()));
    let result = SingleTool::new("memory", MemoryTools::with_scope(engine, scope()))
        .call(
            json!({ "action": "recall", "question": "entry" }),
            CallContext::default(),
        )
        .await
        .expect("evidence result");
    let capped = result["evidence"].as_str().expect("evidence text");

    assert_whole_evidence_lines(&evidence, capped);
    assert!(capped.contains("entry 11 line 3"));
    assert!(capped.contains("entry 12 line 0"));
    assert!(!capped.contains("entry 12 line 1"));
    assert!(capped.find("entry 11 line 3").unwrap() < capped.find("entry 12 line 0").unwrap());
    assert!(capped.contains("\n[…]"));
    assert!(capped.contains("(17 more memory entries left out, the last one shortened,"));
}

#[tokio::test]
async fn an_oversized_first_evidence_line_is_kept_whole() {
    let first_line = format!("entry zero {}", "x".repeat(10_500));
    let engine = Arc::new(ControlledRecallEngine::new(format!(
        "{first_line}\nentry one"
    )));
    let result = SingleTool::new("memory", MemoryTools::with_scope(engine, scope()))
        .call(
            json!({ "action": "recall", "question": "entry" }),
            CallContext::default(),
        )
        .await
        .expect("evidence result");
    let evidence = result["evidence"].as_str().expect("evidence text");
    assert!(evidence.starts_with(&first_line));
    assert!(!evidence.contains("entry one"));
    assert!(evidence.contains("[…]"));
}

fn evidence_of(entries: usize, lines: usize, width: usize) -> String {
    (0..entries)
        .map(|entry| {
            (0..lines)
                .map(|line| {
                    let head = format!("[2023-05-21 01:33 UTC] [user] entry {entry} line {line} ");
                    format!("{head}{}", "x".repeat(width.saturating_sub(head.len())))
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n\n---\n\n")
}

fn assert_whole_evidence_lines(evidence: &str, capped: &str) {
    let source: std::collections::HashSet<&str> = evidence.lines().collect();
    for line in capped.lines() {
        assert!(
            source.contains(line) || line == "[…]" || line.starts_with('('),
            "a line was cut: {line:?}"
        );
    }
}

#[tokio::test]
async fn the_partly_searched_closing_note_survives_the_evidence_cap() {
    let engine = Arc::new(ControlledRecallEngine::new(format!(
        "entry {}\n\n(Memory was only partly searched: one scope did not answer in time.)",
        "x".repeat(10_500)
    )));
    let tool = SingleTool::new("memory", MemoryTools::with_scope(engine, scope()));
    let result = tool
        .call(
            json!({ "action": "recall", "question": "entry" }),
            CallContext::default(),
        )
        .await
        .expect("evidence result");
    assert!(
        result["evidence"]
            .as_str()
            .expect("evidence text")
            .ends_with("(Memory was only partly searched: one scope did not answer in time.)")
    );
}

struct ControlledRecallEngine {
    inner: ReferenceEngine,
    descriptor: EngineDescriptor,
    recall_answer: String,
    recall_citations: Vec<tinymemory_api::Citation>,
    recall_requests: Mutex<Vec<RecallRequest>>,
    fetch_requests: Mutex<Vec<FetchRequest>>,
}

impl ControlledRecallEngine {
    fn new(recall_answer: impl Into<String>) -> Self {
        Self::with_modes(
            recall_answer,
            ReferenceEngine::new().descriptor().fetch_modes.clone(),
        )
    }

    fn with_modes(recall_answer: impl Into<String>, fetch_modes: Vec<FetchMode>) -> Self {
        let inner = ReferenceEngine::new();
        let mut descriptor = inner.descriptor().clone();
        descriptor.fetch_modes = fetch_modes;
        Self {
            inner,
            descriptor,
            recall_answer: recall_answer.into(),
            recall_citations: Vec::new(),
            recall_requests: Mutex::new(Vec::new()),
            fetch_requests: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl MemoryEngine for ControlledRecallEngine {
    fn descriptor(&self) -> &tinymemory_api::EngineDescriptor {
        &self.descriptor
    }

    async fn health(&self) -> tinymemory_api::EngineHealth {
        self.inner.health().await
    }

    async fn recall(
        &self,
        request: tinymemory_api::RecallRequest,
    ) -> tinymemory_api::Result<tinymemory_api::RecallAnswer> {
        self.recall_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request);
        Ok(tinymemory_api::RecallAnswer {
            answer: self.recall_answer.clone(),
            citations: self.recall_citations.clone(),
            model: None,
        })
    }

    async fn fetch(
        &self,
        request: tinymemory_api::FetchRequest,
    ) -> tinymemory_api::Result<tinymemory_api::FetchPage> {
        self.fetch_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request.clone());
        self.inner.fetch(request).await
    }

    async fn store(&self, item: StoreItem) -> tinymemory_api::Result<tinymemory_api::StoreReceipt> {
        self.inner.store(item).await
    }

    async fn forget(
        &self,
        target: tinymemory_api::ForgetTarget,
    ) -> tinymemory_api::Result<tinymemory_api::ForgetReport> {
        self.inner.forget(target).await
    }

    async fn list(
        &self,
        request: tinymemory_api::ListRequest,
    ) -> tinymemory_api::Result<tinymemory_api::ListPage> {
        self.inner.list(request).await
    }
}

#[derive(Default)]
struct FailingStore;

impl ListStore<Entry> for FailingStore {
    fn load(&self) -> std::result::Result<Vec<Entry>, StoreError> {
        Err(StoreError("read failed".into()))
    }

    fn save(&self, _: &[Entry]) -> std::result::Result<(), StoreError> {
        Err(StoreError("write failed".into()))
    }
}

#[tokio::test]
async fn full_and_unavailable_queues_never_acknowledge_a_write() {
    let engine = Arc::new(ReferenceEngine::new());
    let full_queue = queue(1);
    let tool = queued_tool(
        engine.clone(),
        scope(),
        full_queue.clone(),
        Arc::new(WriteGate::new("process-4")),
    );
    tool.call(
        json!({ "action": "learn", "text": "one fact" }),
        CallContext::default(),
    )
    .await
    .expect("first item fits");
    let rejected = tool
        .call(
            json!({ "action": "learn", "text": "another different fact" }),
            CallContext::default(),
        )
        .await;
    assert!(matches!(rejected, Err(Error::Unavailable(_))));
    assert_eq!(full_queue.entries().expect("full queue").len(), 1);
    assert!(engine.is_empty());

    let unavailable = Arc::new(PendingWrites::new(FailingStore, Limits::default()));
    let broken = queued_tool(
        engine.clone(),
        scope(),
        unavailable,
        Arc::new(WriteGate::new("process-5")),
    );
    assert!(matches!(
        broken
            .call(
                json!({ "action": "learn", "text": "unavailable" }),
                CallContext::default(),
            )
            .await,
        Err(Error::Unavailable(_))
    ));
    assert!(engine.is_empty());
}

#[tokio::test]
async fn pending_adapter_coalesces_fences_and_notifies_after_successful_drain() {
    #[derive(Default)]
    struct Recorder(Mutex<Vec<WriteOperation>>);

    #[async_trait]
    impl WriteExecutor for Recorder {
        async fn run(&self, operation: &WriteOperation) -> std::result::Result<(), String> {
            self.0
                .lock()
                .expect("recorder lock")
                .push(operation.clone());
            Ok(())
        }
    }

    let engine = Arc::new(ReferenceEngine::new());
    let queue = queue(8);
    let gate = Arc::new(WriteGate::new("process-6"));
    let tool = queued_tool(engine.clone(), scope(), queue.clone(), gate.clone());
    let learn = json!({ "action": "learn", "text": "likes jasmine tea", "kind": "preference" });
    tool.call(learn.clone(), CallContext::default())
        .await
        .expect("first queued learn");
    tool.call(learn.clone(), CallContext::default())
        .await
        .expect("adjacent retry is acknowledged");
    assert_eq!(queue.entries().expect("coalesced").len(), 1);

    let fence = gate.begin_fence();
    tool.call(learn, CallContext::default())
        .await
        .expect("new work is queued during a fence");
    let entries = queue.entries().expect("fenced queue");
    assert_eq!(entries.len(), 2, "fenced writes do not coalesce backwards");
    assert_eq!(
        entries[1].epoch.as_ref().map(|epoch| epoch.generation),
        Some(1)
    );
    drop(fence);
    assert_eq!(
        queue
            .clear_stale(gate.process(), 1)
            .expect("host clears pre-fence work"),
        1
    );
    assert_eq!(queue.entries().expect("post-fence queue").len(), 1);

    let recorder = Recorder::default();
    let settled = Mutex::new(0);
    let count = queue
        .drain(
            &recorder,
            &gate,
            gate.generation(),
            |_| true,
            |_| *settled.lock().expect("settlement lock") += 1,
        )
        .await;
    assert_eq!(count, 1);
    assert_eq!(*settled.lock().expect("settlement count"), 1);
    assert_eq!(recorder.0.lock().expect("recorded operations").len(), 1);
    assert!(queue.entries().expect("drained queue").is_empty());
    assert!(
        engine.is_empty(),
        "the adapter never performs engine writes"
    );
}

#[tokio::test]
async fn a_failed_drain_retains_the_head_and_skips_settled_notification() {
    #[derive(Default)]
    struct FailOnce(Mutex<bool>);

    #[async_trait]
    impl WriteExecutor for FailOnce {
        async fn run(&self, _: &WriteOperation) -> std::result::Result<(), String> {
            let mut failed = self.0.lock().expect("failure lock");
            if !*failed {
                *failed = true;
                Err("UNAVAILABLE".into())
            } else {
                Ok(())
            }
        }
    }

    let queue = queue(8);
    let gate = Arc::new(WriteGate::new("process-7"));
    let tool = queued_tool(
        Arc::new(ReferenceEngine::new()),
        scope(),
        queue.clone(),
        gate.clone(),
    );
    for text in ["first", "second"] {
        tool.call(
            json!({ "action": "learn", "text": text }),
            CallContext::default(),
        )
        .await
        .expect("durable append");
    }
    let notifications = Mutex::new(Vec::new());
    let settled = queue
        .drain(
            &FailOnce::default(),
            &gate,
            gate.generation(),
            |_| true,
            |entry| {
                notifications
                    .lock()
                    .expect("notification lock")
                    .push(entry.ticket.clone())
            },
        )
        .await;
    assert_eq!(settled, 0);
    assert!(notifications.lock().expect("notifications").is_empty());
    let entries = queue.entries().expect("failed head remains");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].status.as_deref(), Some("UNAVAILABLE"));
}

struct PendingExecutor(Mutex<bool>);

#[async_trait]
impl WriteExecutor for PendingExecutor {
    async fn run(&self, _: &WriteOperation) -> std::result::Result<(), String> {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        futures::future::pending().await
    }
}

#[derive(Default)]
struct RetryExecutor(Mutex<usize>);

#[async_trait]
impl WriteExecutor for RetryExecutor {
    async fn run(&self, _: &WriteOperation) -> std::result::Result<(), String> {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
        Ok(())
    }
}

#[tokio::test]
async fn cancelling_an_in_flight_drain_leaves_the_unsettled_entry_durable() {
    let queue = queue(4);
    let gate = Arc::new(WriteGate::new("process-8"));
    let tool = queued_tool(
        Arc::new(ReferenceEngine::new()),
        scope(),
        queue.clone(),
        gate.clone(),
    );
    tool.call(
        json!({ "action": "learn", "text": "retry after cancellation" }),
        CallContext::default(),
    )
    .await
    .expect("queue before drain");

    let settled = Mutex::new(false);
    let executor = PendingExecutor(Mutex::new(false));
    let mut drain = Box::pin(queue.drain(
        &executor,
        &gate,
        gate.generation(),
        |_| true,
        |_| *settled.lock().expect("settlement state") = true,
    ));
    assert!(poll_once(drain.as_mut()).is_pending());
    assert!(*executor.0.lock().expect("executor was polled"));
    drop(drain);
    assert!(!*settled.lock().expect("settlement state"));
    assert_eq!(
        queue.entries().expect("durable after cancellation").len(),
        1
    );

    let retry = RetryExecutor::default();
    let retried = Mutex::new(0);
    let count = queue
        .drain(
            &retry,
            &gate,
            gate.generation(),
            |_| true,
            |_| *retried.lock().expect("retry settlement count") += 1,
        )
        .await;
    assert_eq!(count, 1, "dropping the first drain releases its queue lock");
    assert_eq!(*retry.0.lock().expect("retry applied once"), 1);
    assert_eq!(*retried.lock().expect("retry settled once"), 1);
    assert!(queue.entries().expect("retry clears the entry").is_empty());
}

#[path = "single_tools/padded_action_tests.rs"]
mod padded_action_tests;

#[path = "single_tools/detailed_call_tests.rs"]
mod detailed_call_tests;

#[path = "single_tools/queued_tests.rs"]
mod queued_tests;
