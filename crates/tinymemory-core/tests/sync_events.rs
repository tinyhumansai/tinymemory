//! Tests for sync lifecycle values and source-id decoding.

use std::sync::{Arc, Mutex as StdMutex};

use parking_lot::Mutex;
use tinymemory_core::events::{self, MemoryEvent, MemoryEventSink};
use tinymemory_core::sync_events::*;

static SINK_LOCK: StdMutex<()> = StdMutex::new(());

#[derive(Debug, Default)]
struct RecordingSink {
    events: Mutex<Vec<MemoryEvent>>,
}

impl RecordingSink {
    fn drain(&self) -> Vec<MemoryEvent> {
        std::mem::take(&mut *self.events.lock())
    }
}

impl MemoryEventSink for RecordingSink {
    fn publish(&self, event: MemoryEvent) {
        self.events.lock().push(event);
    }
}

/// Restores only if this test's sink still owns the global slot. A later host
/// install must never be overwritten by stale test cleanup.
struct SinkRestore {
    previous: Option<Arc<dyn MemoryEventSink>>,
    installed: Arc<dyn MemoryEventSink>,
}

impl Drop for SinkRestore {
    fn drop(&mut self) {
        let owns_slot = events::event_sink()
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &self.installed));
        if !owns_slot {
            return;
        }
        match self.previous.take() {
            Some(previous) => events::set_event_sink(previous),
            None => events::clear_event_sink(),
        }
    }
}

#[test]
fn source_id_decoder_preserves_colons_in_item_ids_and_rejects_malformed_values() {
    assert_eq!(
        extract_mem_src_id("mem_src:feed_7:https://example.com/posts/1"),
        Some("feed_7")
    );
    assert_eq!(
        extract_mem_src_id("mem_src:folder:notes/a.md"),
        Some("folder")
    );
    for malformed in [
        "slack:workspace-1",
        "mem_src:",
        "mem_src:source-only",
        "mem_src:source:",
    ] {
        assert_eq!(
            extract_mem_src_id(malformed),
            None,
            "accepted {malformed:?}"
        );
    }
}

#[test]
fn trigger_and_stage_strings_match_their_serde_wire_values() {
    for (trigger, expected) in [
        (MemorySyncTrigger::Manual, "manual"),
        (MemorySyncTrigger::Cron, "cron"),
    ] {
        assert_eq!(trigger.as_str(), expected);
        assert_eq!(serde_json::to_value(trigger).unwrap(), expected);
    }
    for (stage, expected) in [
        (MemorySyncStage::Requested, "requested"),
        (MemorySyncStage::Fetching, "fetching"),
        (MemorySyncStage::Stored, "stored"),
        (MemorySyncStage::Queued, "queued"),
        (MemorySyncStage::Ingesting, "ingesting"),
        (MemorySyncStage::Completed, "completed"),
        (MemorySyncStage::Failed, "failed"),
    ] {
        assert_eq!(stage.as_str(), expected);
        assert_eq!(serde_json::to_value(stage).unwrap(), expected);
    }
}

#[test]
fn emitting_a_sync_stage_preserves_all_optional_context() {
    let _guard = SINK_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let previous = events::event_sink();
    let sink = Arc::new(RecordingSink::default());
    let installed = Arc::clone(&sink) as Arc<dyn MemoryEventSink>;
    events::set_event_sink(Arc::clone(&installed));
    let _restore = SinkRestore {
        previous,
        installed,
    };
    emit_sync_stage(
        MemorySyncTrigger::Manual,
        MemorySyncStage::Failed,
        Some("rss"),
        Some("connection-4"),
        Some("bad feed".to_string()),
        Some("source-9"),
    );

    let events = sink.drain();
    assert_eq!(events.len(), 1);
    match &events[0] {
        MemoryEvent::SyncStageChanged {
            trigger,
            stage,
            provider,
            connection_id,
            detail,
            source_id,
        } => {
            assert_eq!(trigger, "manual");
            assert_eq!(stage, "failed");
            assert_eq!(provider.as_deref(), Some("rss"));
            assert_eq!(connection_id.as_deref(), Some("connection-4"));
            assert_eq!(detail.as_deref(), Some("bad feed"));
            assert_eq!(source_id.as_deref(), Some("source-9"));
        }
        event => panic!("unexpected event: {event:?}"),
    }
}

#[test]
fn stale_cleanup_does_not_overwrite_a_newer_sink_installation() {
    let _guard = SINK_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let first = Arc::new(RecordingSink::default());
    let first_dyn = Arc::clone(&first) as Arc<dyn MemoryEventSink>;
    events::set_event_sink(Arc::clone(&first_dyn));
    let restore = SinkRestore {
        previous: None,
        installed: first_dyn,
    };

    let newer = Arc::new(RecordingSink::default());
    let newer_dyn = Arc::clone(&newer) as Arc<dyn MemoryEventSink>;
    events::set_event_sink(Arc::clone(&newer_dyn));
    drop(restore);

    let current = events::event_sink().expect("newer sink must remain installed");
    assert!(Arc::ptr_eq(&current, &newer_dyn));
    events::clear_event_sink();
}
