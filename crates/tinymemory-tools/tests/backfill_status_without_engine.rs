//! Public API tests for backfill status while no memory engine is bound.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tinymemory_tools::backfill::{
    BackfillFile, BackfillPhase, BackfillState, Backfiller, CounterStore, ThreadMessage,
    ThreadSource,
};

#[derive(Clone, Default)]
struct Source {
    threads: Vec<String>,
    messages: BTreeMap<String, Vec<ThreadMessage>>,
    unreadable: Vec<String>,
    fail_listing: bool,
}

#[async_trait]
impl ThreadSource for Source {
    async fn list_threads(&self) -> Result<Vec<String>, String> {
        if self.fail_listing {
            Err("thread listing failed".into())
        } else {
            Ok(self.threads.clone())
        }
    }

    async fn messages(&self, thread_id: &str) -> Result<Vec<ThreadMessage>, String> {
        if self.unreadable.iter().any(|id| id == thread_id) {
            return Err("thread messages failed".into());
        }
        Ok(self.messages.get(thread_id).cloned().unwrap_or_default())
    }
}

struct Store(Mutex<BackfillFile>);

impl CounterStore for Store {
    fn load(&self) -> BackfillFile {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn save(&self, file: &BackfillFile) -> Result<(), String> {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = file.clone();
        Ok(())
    }
}

fn messages(turns: &[(&str, &str)]) -> Vec<ThreadMessage> {
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

#[tokio::test]
async fn status_without_engine_uses_saved_counters_and_only_marks_stale_runs() {
    let mut file = BackfillFile {
        state: BackfillState {
            phase: BackfillPhase::Running,
            threads_total: 4,
            threads_done: 2,
            turns_stored: 3,
            items_stored: 6,
            ..BackfillState::default()
        },
        stored: BTreeMap::from([("first".into(), 1), ("second".into(), 2)]),
    };
    let store = Arc::new(Store(Mutex::new(file.clone())));
    let source = Source {
        threads: vec!["first".into(), "second".into()],
        messages: BTreeMap::from([
            (
                "first".into(),
                messages(&[("q0", "a0"), ("q1", "a1"), ("q2", "a2")]),
            ),
            ("second".into(), messages(&[("q0", "a0"), ("q1", "a1")])),
        ]),
        ..Source::default()
    };

    let stale = Backfiller::status_without_engine(&source, store.as_ref(), false)
        .await
        .expect("saved counters suffice for offline status");
    assert_eq!(stale.state.phase, BackfillPhase::Error);
    assert_eq!(
        stale.state.error.as_deref(),
        Some("the sync was interrupted; start it again to resume")
    );
    assert_eq!(stale.state.threads_total, 4);
    assert_eq!(stale.state.threads_done, 2);
    assert_eq!(stale.state.turns_stored, 3);
    assert_eq!(stale.state.items_stored, 6);
    assert_eq!(stale.pending_threads, 1);
    assert_eq!(stale.pending_turns, 2);

    let active = Backfiller::status_without_engine(&source, store.as_ref(), true)
        .await
        .expect("active run status");
    assert_eq!(active.state.phase, BackfillPhase::Running);
    assert_eq!(active.state.error, None);
    assert_eq!(active.pending_threads, 1);
    assert_eq!(active.pending_turns, 2);

    file.state.phase = BackfillPhase::Running;
    assert_eq!(store.load(), file, "status must not persist derived state");
}

#[tokio::test]
async fn status_without_engine_propagates_listing_failure_and_skips_unreadable_threads() {
    let store = Store(Mutex::new(BackfillFile::default()));
    let listing_failure = Source {
        fail_listing: true,
        ..Source::default()
    };
    assert!(
        Backfiller::status_without_engine(&listing_failure, &store, false)
            .await
            .is_err()
    );

    let mut file = BackfillFile::default();
    file.stored.insert("readable".into(), 1);
    let store = Store(Mutex::new(file));
    let source = Source {
        threads: vec!["unreadable".into(), "readable".into()],
        messages: BTreeMap::from([("readable".into(), messages(&[("q0", "a0"), ("q1", "a1")]))]),
        unreadable: vec!["unreadable".into()],
        ..Source::default()
    };
    let view = Backfiller::status_without_engine(&source, &store, false)
        .await
        .expect("unreadable thread is skipped");
    assert_eq!(view.pending_threads, 1);
    assert_eq!(view.pending_turns, 1);
}
