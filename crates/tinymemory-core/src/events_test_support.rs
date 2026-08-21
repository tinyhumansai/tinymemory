//! Test-only event recorder and process-global installation guard.

use super::*;

#[derive(Debug, Default)]
pub(crate) struct RecordingSink {
    events: parking_lot::Mutex<Vec<MemoryEvent>>,
}

impl RecordingSink {
    pub(crate) fn install() -> RecordingSinkGuard {
        static TEST_SINK_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let lock = TEST_SINK_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = event_sink();
        let sink = Arc::new(Self::default());
        let installed = Arc::clone(&sink) as Arc<dyn MemoryEventSink>;
        set_event_sink(Arc::clone(&installed));
        RecordingSinkGuard {
            sink,
            installed,
            previous,
            _lock: lock,
        }
    }

    pub(crate) fn drain(&self) -> Vec<MemoryEvent> {
        std::mem::take(&mut *self.events.lock())
    }
}

pub(crate) struct RecordingSinkGuard {
    sink: Arc<RecordingSink>,
    installed: Arc<dyn MemoryEventSink>,
    previous: Option<Arc<dyn MemoryEventSink>>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl std::ops::Deref for RecordingSinkGuard {
    type Target = RecordingSink;
    fn deref(&self) -> &Self::Target {
        &self.sink
    }
}

impl Drop for RecordingSinkGuard {
    fn drop(&mut self) {
        let still_installed = event_sink()
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &self.installed));
        if still_installed {
            match self.previous.take() {
                Some(previous) => set_event_sink(previous),
                None => clear_event_sink(),
            }
        }
    }
}

impl MemoryEventSink for RecordingSink {
    fn publish(&self, event: MemoryEvent) {
        self.events.lock().push(event);
    }
}
