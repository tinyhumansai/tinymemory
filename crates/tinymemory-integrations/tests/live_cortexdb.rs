//! The `cortexdb` engine against a real CortexDB server.
//!
//! Skipped unless `TINYMEMORY_LIVE_CORTEXDB_URL` names one; the harness in
//! `integration/cortexdb/` boots a pinned server for it, and
//! `scripts/cortexdb-live.sh` runs this whole file against that harness. The
//! key defaults to the harness's (`TINYMEMORY_TEST_CORTEX_KEY`).
//!
//! Two passes: the shared conformance suite, then the three stores the host
//! uses (a document, a conversation with a tool call, a learning) read back
//! through `list`, `fetch` and `recall`, compiled into `context.md`, and
//! forgotten.

// The helpers outside `#[test]` fns fail the test by panicking, like the tests.
#![allow(clippy::expect_used)]

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tinymemory_api::{
    FetchMode, FetchRequest, ForgetTarget, ItemKind, LearningKind, ListRequest, MemoryEngine,
    MemoryMeta, MetaFilter, RecallRequest, Role, SourceKind, SourceRef, StoreItem, ToolCallRef,
    Turn,
};
use tinymemory_integrations::cortex::{CortexCredential, CortexEngine};
use tinymemory_tools::context::{ContextSpec, compile};

const DEFAULT_KEY: &str = "tinymemory-cortex-test";

/// How long a stored item may take to become readable. CortexDB indexes
/// asynchronously, so a write is not visible to the very next read.
const VISIBILITY: Duration = Duration::from_secs(60);

fn live_engine() -> Option<CortexEngine> {
    let url = std::env::var("TINYMEMORY_LIVE_CORTEXDB_URL").ok()?;
    let key = std::env::var("TINYMEMORY_TEST_CORTEX_KEY").unwrap_or_else(|_| DEFAULT_KEY.into());
    Some(CortexEngine::direct(&url, CortexCredential::api_key(key)).expect("a valid live endpoint"))
}

fn run_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    format!("live-{nanos}")
}

fn meta(workspace: &str, source: SourceKind) -> MemoryMeta {
    MemoryMeta {
        workspace: Some(workspace.to_string()),
        source: SourceRef {
            kind: source,
            id: Some(format!("{workspace}-{}", source.as_str())),
        },
        ..MemoryMeta::default()
    }
}

/// Lists `filter` until it holds `want` items or [`VISIBILITY`] runs out.
async fn list_until(engine: &CortexEngine, filter: &MetaFilter, want: usize) -> Vec<String> {
    let deadline = Instant::now() + VISIBILITY;
    loop {
        let page = engine
            .list(ListRequest::new(filter.clone(), 50))
            .await
            .expect("list");
        let texts: Vec<String> = page.items.into_iter().map(|hit| hit.text).collect();
        if texts.len() >= want || Instant::now() >= deadline {
            return texts;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[tokio::test]
async fn the_live_server_upholds_the_contract() {
    let Some(engine) = live_engine() else {
        eprintln!("TINYMEMORY_LIVE_CORTEXDB_URL unset; skipping");
        return;
    };
    tinymemory_api::conformance::run(&engine)
        .await
        .expect("the live CortexDB conforms");
}

#[tokio::test]
async fn documents_conversations_and_learnings_round_trip_into_context() {
    let Some(engine) = live_engine() else {
        eprintln!("TINYMEMORY_LIVE_CORTEXDB_URL unset; skipping");
        return;
    };
    assert!(engine.health().await.is_serving(), "the server is serving");
    let workspace = run_id();

    // Document: a file read from a folder source.
    let mut doc_meta = meta(&workspace, SourceKind::Folder);
    doc_meta.folder = Some("/notes".into());
    doc_meta.file_path = Some("/notes/aurora.md".into());
    doc_meta.language = Some("en".into());
    let document = engine
        .store(StoreItem::Document {
            title: Some("Aurora plan".into()),
            body: tinymemory_api::DocumentBody::Text(
                "Project Aurora launches on Thursday from the Lisbon office.".into(),
            ),
            mime: Some("text/markdown".into()),
            meta: doc_meta,
        })
        .await
        .expect("store document");

    // Conversation: two turns, one of which called a tool.
    let mut conv_meta = meta(&workspace, SourceKind::Conversation);
    conv_meta.thread_id = Some(format!("{workspace}-thread"));
    conv_meta.agent_id = Some("orchestrator".into());
    let mut answer = Turn::new(Role::Assistant, "Booked the Lisbon venue for Thursday.");
    answer.tool_calls.push(ToolCallRef {
        name: "calendar_create".into(),
        id: Some("call-1".into()),
    });
    let conversation = engine
        .store(StoreItem::Conversation {
            turns: vec![
                Turn::new(Role::User, "Book a venue for the Aurora launch."),
                answer,
            ],
            meta: conv_meta,
        })
        .await
        .expect("store conversation");

    // Learning: explicit, from the agent, attributed to its tool call.
    let mut learn_meta = meta(&workspace, SourceKind::Agent);
    learn_meta.tool_call = Some(ToolCallRef {
        name: "memory".into(),
        id: Some("call-2".into()),
    });
    let learning = engine
        .store(StoreItem::learning(
            "The user prefers launch events in Lisbon.",
            LearningKind::Preference,
            0.9,
            learn_meta,
        ))
        .await
        .expect("store learning");

    let by_kind = |kind: ItemKind| MetaFilter {
        workspace: Some(workspace.clone()),
        kinds: vec![kind],
        ..MetaFilter::default()
    };
    let docs = list_until(&engine, &by_kind(ItemKind::Document), 1).await;
    assert_eq!(docs.len(), 1, "the document lists back: {docs:?}");
    assert!(docs[0].contains("Project Aurora"));
    let convs = list_until(&engine, &by_kind(ItemKind::Conversation), 1).await;
    assert_eq!(convs.len(), 1, "the conversation lists back: {convs:?}");
    assert!(
        convs[0].contains("calendar_create (call-1)"),
        "the tool call stays visible: {}",
        convs[0]
    );
    let learns = list_until(&engine, &by_kind(ItemKind::Learning), 1).await;
    assert_eq!(learns.len(), 1, "the learning lists back: {learns:?}");

    // Metadata filters narrow server-side and client-side alike.
    let by_file = MetaFilter {
        workspace: Some(workspace.clone()),
        file_path: Some("/notes/aurora.md".into()),
        ..MetaFilter::default()
    };
    assert_eq!(list_until(&engine, &by_file, 1).await.len(), 1);
    let by_tool = MetaFilter {
        workspace: Some(workspace.clone()),
        tool_call: Some("memory".into()),
        ..MetaFilter::default()
    };
    assert_eq!(list_until(&engine, &by_tool, 1).await.len(), 1);

    // Fetch: hybrid search over the run's items finds the document.
    let mut fetch = FetchRequest::new("When does Project Aurora launch?", FetchMode::Hybrid, 10);
    fetch.filter = MetaFilter {
        workspace: Some(workspace.clone()),
        ..MetaFilter::default()
    };
    let page = engine.fetch(fetch).await.expect("fetch");
    assert!(
        page.hits.iter().any(|hit| hit.id == document.id),
        "fetch finds the document: {:?}",
        page.hits.iter().map(|hit| &hit.text).collect::<Vec<_>>()
    );

    // Recall: CortexDB's ask route answers and cites what it used.
    let mut recall = RecallRequest::new("When does Project Aurora launch?", 10);
    recall.filter = MetaFilter {
        workspace: Some(workspace.clone()),
        ..MetaFilter::default()
    };
    let answer = engine.recall(recall).await.expect("recall");
    assert!(!answer.answer.trim().is_empty(), "recall answers");
    assert!(!answer.citations.is_empty(), "recall cites its evidence");

    // context.md: compiled from the same engine, it lists the learning.
    let context = compile(&engine, &ContextSpec::default())
        .await
        .expect("compile context");
    assert_eq!(context.engine, "cortexdb");
    assert!(
        context
            .markdown
            .contains("The user prefers launch events in Lisbon."),
        "context.md carries the learning:\n{}",
        context.markdown
    );
    assert!(context.tokens <= ContextSpec::default().budget_tokens);

    // Forget the run.
    let report = engine
        .forget(ForgetTarget::Filter(MetaFilter {
            workspace: Some(workspace.clone()),
            ..MetaFilter::default()
        }))
        .await
        .expect("forget");
    assert_eq!(report.forgotten, 3, "all three items are forgotten");
    let _ = (conversation, learning);
}
