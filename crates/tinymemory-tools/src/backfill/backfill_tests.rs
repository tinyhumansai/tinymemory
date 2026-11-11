use super::*;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tinymemory_api::conformance::ReferenceEngine;

fn message(sender: &str, content: &str, at: &str) -> ThreadMessage {
    ThreadMessage::new(sender, content, at)
}

fn turn(user: &str, assistant: &str) -> PastTurn {
    PastTurn {
        user: user.into(),
        assistant: assistant.into(),
        at: DateTime::parse_from_rfc3339("2026-09-01T10:01:00Z")
            .unwrap()
            .with_timezone(&Utc),
    }
}

#[test]
fn backfill_phase_keeps_the_existing_state_file_wire_values() {
    for (phase, expected) in [
        (BackfillPhase::Idle, "idle"),
        (BackfillPhase::Running, "running"),
        (BackfillPhase::Done, "done"),
        (BackfillPhase::Error, "error"),
    ] {
        let state = BackfillState {
            phase,
            ..BackfillState::default()
        };
        assert_eq!(serde_json::to_value(state).unwrap()["phase"], expected);
    }
}

#[test]
fn messages_become_turns_like_the_chat_shows_them() {
    let messages = [
        message("agent", "Welcome!", "2026-09-01T09:59:00Z"),
        message("user", "Plan a trip", "2026-09-01T10:00:00Z"),
        message("agent", "Where to?", "2026-09-01T10:01:00Z"),
        message("agent", "Also, when?", "2026-09-01T10:02:00Z"),
        message("user", "   ", "2026-09-01T10:03:00Z"),
        message("User", "Bali in October", "not a time"),
    ];
    let turns = turns_of(&messages);
    assert_eq!(turns.len(), 3);
    assert_eq!(turns[0].user, "");
    assert_eq!(turns[0].assistant, "Welcome!");
    assert_eq!(turns[1].user, "Plan a trip");
    assert_eq!(turns[1].assistant, "Where to?\n\nAlso, when?");
    assert_eq!(turns[1].at.to_rfc3339(), "2026-09-01T10:02:00+00:00");
    assert_eq!(turns[2].user, "Bali in October");
    assert!(turns[2].assistant.is_empty());
}

#[test]
fn the_range_stops_where_live_logging_began() {
    assert_eq!(pending_range(10, None, 0), 0..10);
    assert_eq!(pending_range(10, Some(7), 0), 0..7, "turns 7.. are live's");
    assert_eq!(pending_range(10, Some(7), 4), 4..7);
    assert_eq!(pending_range(10, Some(7), 9), 7..7, "nothing left");
    assert_eq!(pending_range(2, Some(5), 0), 0..2);
}

#[test]
fn groups_hold_whole_turns() {
    let groups = turn_groups(3..3 + TURNS_PER_GROUP * 2 + 1);
    assert_eq!(groups.len(), 3);
    assert_eq!(groups[0].len(), TURNS_PER_GROUP);
    assert_eq!(groups[0][0], 3);
    assert_eq!(groups[2], vec![3 + TURNS_PER_GROUP * 2]);
    assert!(turn_groups(4..4).is_empty());
}

#[test]
fn a_turn_is_one_item_per_message_at_the_agents_node() {
    let layout = MemoryLayout::default();
    let past = turn("q", "a");
    let items = turn_items(&layout, MAIN_AGENT, "t", 3, &past);
    assert_eq!(items.len(), 2);
    let meta = items[1].meta();
    assert_eq!(meta.turns.map(|range| range.first), Some(7));
    assert_eq!(meta.namespace.to_string(), "agent:orchestrator");
    assert_eq!(meta.agent_id.as_deref(), Some(MAIN_AGENT));
    assert!(meta.tags.iter().any(|tag| tag == BACKFILL_TAG));
    let reply_only = PastTurn {
        user: String::new(),
        ..past
    };
    assert_eq!(
        turn_items(&layout, MAIN_AGENT, "t", 0, &reply_only).len(),
        1
    );
}

#[test]
fn pooled_conversations_land_at_the_pooled_node() {
    let layout = MemoryLayout::default()
        .with_pooled_conversations(&"ws:main".parse().unwrap())
        .unwrap();
    let items = turn_items(&layout, MAIN_AGENT, "t", 0, &turn("q", "a"));
    assert_eq!(items[0].meta().namespace.to_string(), "ws:main");
}

#[test]
fn a_group_flattens_its_turns_in_order_and_skips_unknown_indices() {
    let layout = MemoryLayout::default();
    let turns = [turn("q0", "a0"), turn("q1", "a1")];
    let items = group_items(&layout, MAIN_AGENT, "t", &turns, &[0, 1, 9]);
    let firsts: Vec<_> = items
        .iter()
        .map(|item| item.meta().turns.unwrap().first)
        .collect();
    assert_eq!(firsts, [0, 1, 2, 3]);
}

#[tokio::test]
async fn live_logging_is_found_by_the_first_untagged_turn() {
    let engine = ReferenceEngine::new();
    let layout = MemoryLayout::default();
    // Turn 0 was backfilled; turn 2 and 3 were logged live (no tag).
    for item in turn_items(&layout, MAIN_AGENT, "t", 0, &turn("q", "a")) {
        engine.store(item).await.unwrap();
    }
    assert_eq!(first_live_turn(&engine, "t").await, None);
    for item in turn_items(&layout, MAIN_AGENT, "t", 2, &turn("q", "a")) {
        let mut item = item;
        item.meta_mut().tags.clear();
        engine.store(item).await.unwrap();
    }
    assert_eq!(first_live_turn(&engine, "t").await, Some(2));
    assert_eq!(first_live_turn(&engine, "other").await, None);
}

#[derive(Clone, Default)]
struct TestThreadSource {
    threads: Vec<String>,
    messages: BTreeMap<String, Vec<ThreadMessage>>,
    unreadable: Vec<String>,
    fail_listing: bool,
}

#[async_trait::async_trait]
impl ThreadSource for TestThreadSource {
    async fn list_threads(&self) -> std::result::Result<Vec<String>, String> {
        if self.fail_listing {
            Err("thread listing failed".to_string())
        } else {
            Ok(self.threads.clone())
        }
    }

    async fn messages(&self, thread_id: &str) -> std::result::Result<Vec<ThreadMessage>, String> {
        if self
            .unreadable
            .iter()
            .any(|unreadable| unreadable == thread_id)
        {
            return Err("thread messages failed".to_string());
        }
        Ok(self.messages.get(thread_id).cloned().unwrap_or_default())
    }
}

#[derive(Default)]
struct TestCounterStore(Mutex<BackfillFile>);

impl CounterStore for TestCounterStore {
    fn load(&self) -> BackfillFile {
        self.0.lock().unwrap().clone()
    }

    fn save(&self, file: &BackfillFile) -> std::result::Result<(), String> {
        *self.0.lock().unwrap() = file.clone();
        Ok(())
    }
}

#[derive(Default)]
struct RejectingCounterStore {
    file: Mutex<BackfillFile>,
    save_attempts: AtomicUsize,
}

impl CounterStore for RejectingCounterStore {
    fn load(&self) -> BackfillFile {
        self.file.lock().unwrap().clone()
    }

    fn save(&self, _file: &BackfillFile) -> std::result::Result<(), String> {
        self.save_attempts.fetch_add(1, Ordering::SeqCst);
        Err("counter file is read-only".to_string())
    }
}

struct TestListingEngine {
    inner: ReferenceEngine,
    fail: Option<tinymemory_api::Error>,
    waits: Arc<Mutex<Vec<tinymemory_api::WaitFor>>>,
    fail_store_on: Option<usize>,
    store_calls: AtomicUsize,
    list_calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl MemoryEngine for TestListingEngine {
    fn descriptor(&self) -> &tinymemory_api::EngineDescriptor {
        self.inner.descriptor()
    }

    async fn health(&self) -> tinymemory_api::EngineHealth {
        self.inner.health().await
    }

    async fn recall(
        &self,
        request: tinymemory_api::RecallRequest,
    ) -> tinymemory_api::Result<tinymemory_api::RecallAnswer> {
        self.inner.recall(request).await
    }

    async fn fetch(
        &self,
        request: tinymemory_api::FetchRequest,
    ) -> tinymemory_api::Result<tinymemory_api::FetchPage> {
        self.inner.fetch(request).await
    }

    async fn store(
        &self,
        item: tinymemory_api::StoreItem,
    ) -> tinymemory_api::Result<tinymemory_api::StoreReceipt> {
        self.inner.store(item).await
    }

    async fn store_many_with(
        &self,
        items: Vec<tinymemory_api::StoreItem>,
        options: tinymemory_api::WriteOptions,
    ) -> tinymemory_api::Result<Vec<tinymemory_api::StoreReceipt>> {
        self.waits.lock().unwrap().push(options.wait);
        let call = self.store_calls.fetch_add(1, Ordering::SeqCst) + 1;
        if self.fail_store_on == Some(call) {
            return Err(tinymemory_api::Error::Unavailable(
                "scripted backfill store failure".to_string(),
            ));
        }
        self.inner.store_many_with(items, options).await
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
        self.list_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(error) = &self.fail {
            return Err(error.clone());
        }
        // Deliberately ignore the filter to prove the cursor validates the
        // returned item's kind, namespace, agent, thread and tag itself.
        let mut unfiltered =
            tinymemory_api::ListRequest::new(tinymemory_api::MetaFilter::default(), request.limit);
        unfiltered.cursor = request.cursor;
        let mut page = self.inner.list(unfiltered).await?;
        if let Some(first) = page.items.first().cloned() {
            page.items.push(first);
        }
        Ok(page)
    }
}

fn backfiller(
    engine: Arc<dyn MemoryEngine>,
    source: TestThreadSource,
    store: Arc<dyn CounterStore>,
) -> Backfiller {
    Backfiller::new(
        engine,
        MemoryLayout::default(),
        MAIN_AGENT,
        Arc::new(source),
        store,
    )
}

fn source_messages(turns: &[(&str, &str)]) -> Vec<ThreadMessage> {
    turns
        .iter()
        .flat_map(|(user, assistant)| {
            [
                ThreadMessage::new("user", *user, "2026-09-01T10:00:00Z"),
                ThreadMessage::new("assistant", *assistant, "2026-09-01T10:01:00Z"),
            ]
        })
        .collect()
}

fn indexed_source_messages(count: usize) -> Vec<ThreadMessage> {
    (0..count)
        .flat_map(|index| {
            [
                ThreadMessage::new("user", format!("user {index}"), "2026-09-01T10:00:00Z"),
                ThreadMessage::new(
                    "assistant",
                    format!("answer {index}"),
                    "2026-09-01T10:01:00Z",
                ),
            ]
        })
        .collect()
}

#[tokio::test]
async fn cursor_counts_only_contiguous_turns_with_every_expected_role() {
    let engine = Arc::new(TestListingEngine {
        inner: ReferenceEngine::new(),
        fail: None,
        waits: Arc::default(),
        fail_store_on: None,
        store_calls: AtomicUsize::new(0),
        list_calls: Arc::new(AtomicUsize::new(0)),
    });
    let layout = MemoryLayout::default();
    // Turn zero is assistant-only, so its valid item has the odd turn index.
    for item in turn_items(
        &layout,
        MAIN_AGENT,
        "thread",
        0,
        &PastTurn {
            user: String::new(),
            assistant: "welcome".to_string(),
            at: DateTime::parse_from_rfc3339("2026-09-01T10:01:00Z")
                .unwrap()
                .with_timezone(&Utc),
        },
    ) {
        engine.store(item).await.unwrap();
    }
    // Turn one is only partly visible: a user item alone must not make the
    // cursor skip the missing assistant item. Listing duplicates are also
    // injected, and must not masquerade as the missing role.
    for item in turn_items(
        &layout,
        MAIN_AGENT,
        "thread",
        1,
        &turn("question", "answer"),
    ) {
        if item.meta().turns.unwrap().first == 2 {
            engine.store(item).await.unwrap();
        }
    }
    let source = TestThreadSource {
        threads: vec!["thread".to_string()],
        messages: BTreeMap::from([(
            "thread".to_string(),
            vec![
                ThreadMessage::new("user", "", "2026-09-01T10:00:00Z"),
                ThreadMessage::new("assistant", "welcome", "2026-09-01T10:01:00Z"),
                ThreadMessage::new("user", "question", "2026-09-01T10:01:00Z"),
                ThreadMessage::new("assistant", "answer", "2026-09-01T10:01:00Z"),
            ],
        )]),
        ..Default::default()
    };
    let backfiller = backfiller(engine, source, Arc::new(TestCounterStore::default()));

    let view = backfiller.status(false).await.unwrap();

    assert_eq!(view.pending_threads, 1);
    assert_eq!(view.pending_turns, 1);
}

#[tokio::test]
async fn status_and_begin_list_each_thread_once_for_live_and_backfilled_turns() {
    let inner = ReferenceEngine::new();
    let layout = MemoryLayout::default();
    for item in turn_items(&layout, MAIN_AGENT, "thread", 0, &turn("old", "answer")) {
        inner.store(item).await.unwrap();
    }
    for mut item in turn_items(&layout, MAIN_AGENT, "thread", 2, &turn("live", "answer")) {
        item.meta_mut().tags.clear();
        inner.store(item).await.unwrap();
    }
    let list_calls = Arc::new(AtomicUsize::new(0));
    let engine = Arc::new(TestListingEngine {
        inner,
        fail: None,
        waits: Arc::default(),
        fail_store_on: None,
        store_calls: AtomicUsize::new(0),
        list_calls: Arc::clone(&list_calls),
    });
    let source = TestThreadSource {
        threads: vec!["thread".to_string()],
        messages: BTreeMap::from([(
            "thread".to_string(),
            source_messages(&[("old", "answer"), ("missing", "answer"), ("live", "answer")]),
        )]),
        ..Default::default()
    };
    let backfiller = backfiller(engine, source, Arc::new(TestCounterStore::default()));

    let view = backfiller.status(false).await.unwrap();
    assert_eq!(view.pending_threads, 1);
    assert_eq!(view.pending_turns, 1);
    assert_eq!(list_calls.load(Ordering::SeqCst), 1);

    let _run = backfiller.begin().await.unwrap();
    assert_eq!(list_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_user_only_turn_is_complete_when_its_expected_user_item_is_visible() {
    let engine = Arc::new(ReferenceEngine::new());
    let layout = MemoryLayout::default();
    let user_only = PastTurn {
        user: "question".to_string(),
        assistant: String::new(),
        at: DateTime::parse_from_rfc3339("2026-09-01T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc),
    };
    for item in turn_items(&layout, MAIN_AGENT, "thread", 0, &user_only) {
        engine.store(item).await.unwrap();
    }
    let source = TestThreadSource {
        threads: vec!["thread".to_string()],
        messages: BTreeMap::from([(
            "thread".to_string(),
            vec![
                ThreadMessage::new("user", "question", "2026-09-01T10:00:00Z"),
                ThreadMessage::new("assistant", "", "2026-09-01T10:01:00Z"),
            ],
        )]),
        ..Default::default()
    };
    let view = backfiller(engine, source, Arc::new(TestCounterStore::default()))
        .status(false)
        .await
        .unwrap();
    assert_eq!(view.pending_turns, 0);
}

#[tokio::test]
async fn stale_running_backfill_reports_interrupted_without_a_live_slot() {
    let engine: Arc<dyn MemoryEngine> = Arc::new(ReferenceEngine::new());
    let mut file = BackfillFile::default();
    file.state.phase = BackfillPhase::Running;
    let store = Arc::new(TestCounterStore(Mutex::new(file)));
    let backfiller = backfiller(engine.clone(), TestThreadSource::default(), store.clone());

    let stopped = backfiller.status(false).await.unwrap();
    assert_eq!(stopped.state.phase, BackfillPhase::Error);
    assert!(
        stopped
            .state
            .error
            .as_deref()
            .unwrap()
            .contains("interrupted")
    );

    let live = backfiller.status(true).await.unwrap();
    assert_eq!(live.state.phase, BackfillPhase::Running);
}

#[tokio::test]
async fn cursor_stops_at_a_gap_and_paginates_all_backfill_items() {
    let engine = Arc::new(ReferenceEngine::new());
    let layout = MemoryLayout::default();
    let turns: Vec<_> = (0..240)
        .map(|index| turn(&format!("user {index}"), &format!("answer {index}")))
        .collect();
    // A gap on a later page means subsequent items cannot move the contiguous
    // cursor past it; earlier pages must still be read to locate the gap.
    for index in (0..240).filter(|index| *index != 220) {
        for item in group_items(&layout, MAIN_AGENT, "thread", &turns, &[index]) {
            engine.store(item).await.unwrap();
        }
    }
    let source = TestThreadSource {
        threads: vec!["thread".to_string()],
        messages: BTreeMap::from([("thread".to_string(), indexed_source_messages(240))]),
        ..Default::default()
    };
    let backfiller = backfiller(engine, source, Arc::new(TestCounterStore::default()));

    let view = backfiller.status(false).await.unwrap();

    assert_eq!(view.pending_turns, 20, "the gap keeps later turns pending");
}

#[tokio::test]
async fn cursor_reads_all_pages_when_every_turn_is_already_stored() {
    let engine = Arc::new(ReferenceEngine::new());
    let layout = MemoryLayout::default();
    let turns: Vec<_> = (0..240)
        .map(|index| turn(&format!("user {index}"), &format!("answer {index}")))
        .collect();
    for index in 0..turns.len() {
        for item in group_items(&layout, MAIN_AGENT, "thread", &turns, &[index]) {
            engine.store(item).await.unwrap();
        }
    }
    let source = TestThreadSource {
        threads: vec!["thread".to_string()],
        messages: BTreeMap::from([("thread".to_string(), indexed_source_messages(240))]),
        ..Default::default()
    };
    let backfiller = backfiller(engine, source, Arc::new(TestCounterStore::default()));

    let view = backfiller.status(false).await.unwrap();

    assert_eq!(view.pending_threads, 0);
    assert_eq!(view.pending_turns, 0);
}

#[tokio::test]
async fn listing_failure_is_fatal_but_an_unreadable_thread_is_skipped() {
    let engine: Arc<dyn MemoryEngine> = Arc::new(ReferenceEngine::new());
    let failing_list = TestThreadSource {
        fail_listing: true,
        ..Default::default()
    };
    assert!(
        backfiller(
            engine.clone(),
            failing_list,
            Arc::new(TestCounterStore::default())
        )
        .status(false)
        .await
        .is_err()
    );

    let source = TestThreadSource {
        threads: vec!["unreadable".to_string(), "readable".to_string()],
        messages: BTreeMap::from([(
            "readable".to_string(),
            source_messages(&[("question", "answer")]),
        )]),
        unreadable: vec!["unreadable".to_string()],
        ..Default::default()
    };
    let view = backfiller(engine, source, Arc::new(TestCounterStore::default()))
        .status(false)
        .await
        .unwrap();
    assert_eq!(view.pending_threads, 1);
    assert_eq!(view.pending_turns, 1);
}

#[tokio::test]
async fn persisted_turn_counts_fallback_only_for_unsupported_listing() {
    let source = TestThreadSource {
        threads: vec!["thread".to_string()],
        messages: BTreeMap::from([(
            "thread".to_string(),
            source_messages(&[("first", "answer"), ("second", "answer")]),
        )]),
        ..Default::default()
    };
    let mut saved = BackfillFile::default();
    saved.stored.insert("thread".to_string(), 1);
    let store = Arc::new(TestCounterStore(Mutex::new(saved)));
    let unsupported = Arc::new(TestListingEngine {
        inner: ReferenceEngine::new(),
        fail: Some(tinymemory_api::Error::Unsupported(
            "listing is not supported".to_string(),
        )),
        waits: Arc::default(),
        fail_store_on: None,
        store_calls: AtomicUsize::new(0),
        list_calls: Arc::new(AtomicUsize::new(0)),
    });
    let view = backfiller(unsupported, source.clone(), store.clone())
        .status(false)
        .await
        .unwrap();
    assert_eq!(
        view.pending_turns, 1,
        "the stored count is used as fallback"
    );

    for error in [
        tinymemory_api::Error::Unauthorized("bad credential".to_string()),
        tinymemory_api::Error::Unavailable("engine down".to_string()),
    ] {
        let engine = Arc::new(TestListingEngine {
            inner: ReferenceEngine::new(),
            fail: Some(error),
            waits: Arc::default(),
            fail_store_on: None,
            store_calls: AtomicUsize::new(0),
            list_calls: Arc::new(AtomicUsize::new(0)),
        });
        assert!(
            backfiller(engine, source.clone(), store.clone())
                .status(false)
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn backfiller_waits_for_accepted_batches_then_the_last_batch_to_be_visible() {
    let engine = Arc::new(TestListingEngine {
        inner: ReferenceEngine::new(),
        fail: None,
        waits: Arc::default(),
        fail_store_on: None,
        store_calls: AtomicUsize::new(0),
        list_calls: Arc::new(AtomicUsize::new(0)),
    });
    let source = TestThreadSource {
        threads: vec!["thread".to_string()],
        messages: BTreeMap::from([("thread".to_string(), indexed_source_messages(26))]),
        ..Default::default()
    };
    let store = Arc::new(TestCounterStore::default());
    let backfiller = backfiller(engine.clone(), source, store.clone());
    let run = backfiller.begin().await.unwrap();

    let state = backfiller.run(run).await;

    assert_eq!(state.phase, BackfillPhase::Done);
    assert_eq!(state.turns_stored, 26);
    assert_eq!(state.items_stored, 52);
    assert_eq!(
        *engine.waits.lock().unwrap(),
        [
            tinymemory_api::WaitFor::Accepted,
            tinymemory_api::WaitFor::Visible
        ]
    );
    assert_eq!(store.load().state, state);
}

#[tokio::test]
async fn failed_visible_batch_keeps_only_completed_counters_and_replays_the_missing_turn() {
    let engine = Arc::new(TestListingEngine {
        inner: ReferenceEngine::new(),
        fail: None,
        waits: Arc::default(),
        fail_store_on: Some(2),
        store_calls: AtomicUsize::new(0),
        list_calls: Arc::new(AtomicUsize::new(0)),
    });
    let source = TestThreadSource {
        threads: vec!["thread".to_string()],
        messages: BTreeMap::from([("thread".to_string(), indexed_source_messages(26))]),
        ..Default::default()
    };
    let store = Arc::new(TestCounterStore::default());
    let backfiller = backfiller(engine.clone(), source, store.clone());

    let failed = backfiller.run(backfiller.begin().await.unwrap()).await;

    assert_eq!(failed.phase, BackfillPhase::Error);
    assert!(
        failed
            .error
            .as_deref()
            .unwrap()
            .contains("scripted backfill store failure")
    );
    assert_eq!(failed.turns_stored, 25);
    assert_eq!(failed.items_stored, 50);
    assert_eq!(store.load().stored.get("thread"), Some(&25));
    assert_eq!(
        *engine.waits.lock().unwrap(),
        [
            tinymemory_api::WaitFor::Accepted,
            tinymemory_api::WaitFor::Visible
        ]
    );

    let resumed = backfiller.run(backfiller.begin().await.unwrap()).await;

    assert_eq!(resumed.phase, BackfillPhase::Done);
    assert_eq!(resumed.turns_stored, 1);
    assert_eq!(resumed.items_stored, 2);
    assert_eq!(store.load().stored.get("thread"), Some(&26));
    assert_eq!(
        *engine.waits.lock().unwrap(),
        [
            tinymemory_api::WaitFor::Accepted,
            tinymemory_api::WaitFor::Visible,
            tinymemory_api::WaitFor::Visible,
        ]
    );
    let listed = engine
        .inner
        .list(tinymemory_api::ListRequest::new(
            tinymemory_api::MetaFilter {
                kinds: vec![tinymemory_api::ItemKind::Conversation],
                tags_any: vec![BACKFILL_TAG.to_string()],
                thread_id: Some("thread".to_string()),
                ..Default::default()
            },
            100,
        ))
        .await
        .unwrap();
    assert_eq!(listed.items.len(), 52, "the failed turn was replayed once");
}

#[tokio::test]
async fn counter_store_save_failures_do_not_stop_the_backfill() {
    let engine = Arc::new(TestListingEngine {
        inner: ReferenceEngine::new(),
        fail: None,
        waits: Arc::default(),
        fail_store_on: None,
        store_calls: AtomicUsize::new(0),
        list_calls: Arc::new(AtomicUsize::new(0)),
    });
    let source = TestThreadSource {
        threads: vec!["thread".to_string()],
        messages: BTreeMap::from([("thread".to_string(), source_messages(&[("q", "a")]))]),
        ..Default::default()
    };
    let store = Arc::new(RejectingCounterStore::default());
    let backfiller = backfiller(engine.clone(), source, store.clone());

    let state = backfiller.run(backfiller.begin().await.unwrap()).await;

    assert_eq!(state.phase, BackfillPhase::Done);
    assert!(store.save_attempts.load(Ordering::SeqCst) >= 3);
    let listed = engine
        .inner
        .list(tinymemory_api::ListRequest::new(
            tinymemory_api::MetaFilter {
                kinds: vec![tinymemory_api::ItemKind::Conversation],
                tags_any: vec![BACKFILL_TAG.to_string()],
                thread_id: Some("thread".to_string()),
                ..Default::default()
            },
            10,
        ))
        .await
        .unwrap();
    assert_eq!(listed.items.len(), 2, "the driver continues storing items");
    assert_eq!(store.load().state.phase, BackfillPhase::Idle);
}

#[tokio::test]
async fn cursor_ignores_foreign_hits_but_honors_an_owned_live_turn() {
    let engine = Arc::new(TestListingEngine {
        inner: ReferenceEngine::new(),
        fail: None,
        waits: Arc::default(),
        fail_store_on: None,
        store_calls: AtomicUsize::new(0),
        list_calls: Arc::new(AtomicUsize::new(0)),
    });
    let layout = MemoryLayout::default();
    let source_turn = turn("question", "answer");
    let assistant_index_zero = |thread_id: &str| {
        turn_items(&layout, MAIN_AGENT, thread_id, 0, &source_turn)
            .into_iter()
            .filter(|item| item.meta().turns.unwrap().first == 1)
            .collect::<Vec<_>>()
    };

    // The actual backfill has only stored the user half of turn zero.
    let backfill_user = turn_items(&layout, MAIN_AGENT, "thread", 0, &source_turn)
        .into_iter()
        .find(|item| item.meta().turns.unwrap().first == 0)
        .unwrap();
    engine.store(backfill_user).await.unwrap();

    // These hits independently violate namespace, agent, kind or thread.
    // Include tagged and untagged conversation hits where relevant: none may
    // complete the backfill pair or falsely move the live cutoff to turn zero.
    let mut wrong_namespace_tagged = assistant_index_zero("thread");
    let mut wrong_namespace_live = assistant_index_zero("thread");
    for item in wrong_namespace_tagged
        .iter_mut()
        .chain(&mut wrong_namespace_live)
    {
        item.meta_mut().namespace = "agent:other".parse().unwrap();
    }
    for item in &mut wrong_namespace_live {
        item.meta_mut().tags.clear();
    }
    let mut wrong_agent_tagged = assistant_index_zero("thread");
    let mut wrong_agent_live = assistant_index_zero("thread");
    for item in wrong_agent_tagged.iter_mut().chain(&mut wrong_agent_live) {
        item.meta_mut().agent_id = Some("other".to_string());
    }
    for item in &mut wrong_agent_live {
        item.meta_mut().tags.clear();
    }
    let mut wrong_thread = assistant_index_zero("elsewhere");
    let wrong_kind = vec![StoreItem::learning(
        "not a conversation",
        tinymemory_api::LearningKind::Fact,
        0.5,
        MemoryMeta {
            namespace: "agent:orchestrator".parse().unwrap(),
            thread_id: Some("thread".to_string()),
            agent_id: Some(MAIN_AGENT.to_string()),
            tags: vec![BACKFILL_TAG.to_string()],
            ..MemoryMeta::default()
        },
    )];
    // This is a valid, owned live turn. It defines the cutoff at turn one;
    // pending older turn zero must still be reported.
    let mut owned_live = turn_items(&layout, MAIN_AGENT, "thread", 1, &turn("later", "live"));
    for item in &mut owned_live {
        item.meta_mut().tags.clear();
    }
    for item in wrong_namespace_tagged
        .drain(..)
        .chain(wrong_namespace_live.drain(..))
        .chain(wrong_agent_tagged.drain(..))
        .chain(wrong_agent_live.drain(..))
        .chain(wrong_thread.drain(..))
        .chain(wrong_kind)
        .chain(owned_live)
    {
        engine.store(item).await.unwrap();
    }

    let source = TestThreadSource {
        threads: vec!["thread".to_string()],
        messages: BTreeMap::from([(
            "thread".to_string(),
            source_messages(&[("question", "answer"), ("later", "live")]),
        )]),
        ..Default::default()
    };
    let backfiller = backfiller(engine, source, Arc::new(TestCounterStore::default()));

    let view = backfiller.status(false).await.unwrap();

    assert_eq!(view.pending_turns, 1);
}
