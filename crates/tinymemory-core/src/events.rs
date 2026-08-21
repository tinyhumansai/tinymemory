//! The process-global [`MemoryEventSink`], and the `publish` the extracted code
//! calls in place of the host's bus.
//!
//! # Why a global
//!
//! The publish sites are scattered across ingestion, the summary tree, the sync
//! pipelines and the store — deep inside call stacks that already thread a
//! config, a store handle and a cancellation token. Threading a fourth
//! parameter through all of them to reach a sink would be a large, mechanical,
//! reviewer-hostile diff for no gain, and it is exactly the shape the host's own
//! `BUS` static had before the extraction. This mirrors that shape rather than
//! inventing a new one.
//!
//! # Default is silence, not a panic
//!
//! Before a host installs a sink — in unit tests, in the standalone engine
//! build, during early startup — [`publish`] drops the event. An event bus that
//! panicked or errored when unwired would turn every emit site into an error
//! path, and none of the call sites have anything useful to do with that error:
//! the work they are reporting on has already happened.

use std::sync::Arc;

use parking_lot::RwLock;

pub use tinymemory_api::host::{
    EmbeddingHealthReason, MemoryEvent, MemoryEventSink, NoopEventSink,
};

static SINK: RwLock<Option<Arc<dyn MemoryEventSink>>> = RwLock::new(None);

/// Install the host's event sink. Called once during startup wiring, before any
/// memory work begins. Calling it again replaces the sink, which is what test
/// harnesses want between cases.
pub fn set_event_sink(sink: Arc<dyn MemoryEventSink>) {
    *SINK.write() = Some(sink);
}

/// Remove any installed sink, returning to silent-drop behaviour. For tests.
pub fn clear_event_sink() {
    *SINK.write() = None;
}

/// The installed sink, or `None` when no host has wired one up.
#[must_use]
pub fn event_sink() -> Option<Arc<dyn MemoryEventSink>> {
    SINK.read().clone()
}

/// Announce a memory-domain event to the host. A no-op when no sink is
/// installed.
pub fn publish(event: MemoryEvent) {
    let sink = SINK.read().clone();
    if let Some(sink) = sink {
        sink.publish(event);
    } else {
        log::trace!("[memory:events] dropped event with no sink installed: {event:?}");
    }
}

#[cfg(test)]
#[path = "events_test_support.rs"]
mod test_support;
#[cfg(test)]
pub(crate) use test_support::RecordingSink;
