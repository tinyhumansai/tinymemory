//! Test-only state reset behavior.

use super::*;

impl IngestionState {
    pub fn reset_for_test(&self) {
        self.inner.queue_depth.store(0, Ordering::SeqCst);
        let mut snap = self.inner.snapshot.write();
        snap.running = false;
        snap.current_document_id = None;
        snap.current_title = None;
        snap.current_namespace = None;
    }
}
