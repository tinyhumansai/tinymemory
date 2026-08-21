//! Test-only observation and failure injection for `OpenStore`.

use std::sync::atomic::{AtomicUsize, Ordering};

pub(crate) struct OpenStoreInstrumentation {
    allocation_attempts: AtomicUsize,
    registration_attempts: AtomicUsize,
    registration_failures: AtomicUsize,
}

impl OpenStoreInstrumentation {
    pub(crate) fn record_allocation(&self) {
        self.allocation_attempts.fetch_add(1, Ordering::SeqCst);
    }

    pub(crate) fn before_registration(&self) -> tinybus::Result<()> {
        self.registration_attempts.fetch_add(1, Ordering::SeqCst);
        if self
            .registration_failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            return Err(tinybus::Error::MethodFailed {
                name: "ai.tinyhumans.tinymemory.Error.Other".to_string(),
                message: "injected store registration failure".to_string(),
            });
        }
        Ok(())
    }

    pub(crate) fn fail_registrations(&self, count: usize) {
        self.registration_failures.store(count, Ordering::SeqCst);
    }

    pub(crate) fn allocation_attempts(&self) -> usize {
        self.allocation_attempts.load(Ordering::SeqCst)
    }

    pub(crate) fn registration_attempts(&self) -> usize {
        self.registration_attempts.load(Ordering::SeqCst)
    }
}

impl Default for OpenStoreInstrumentation {
    fn default() -> Self {
        Self {
            allocation_attempts: AtomicUsize::new(0),
            registration_attempts: AtomicUsize::new(0),
            registration_failures: AtomicUsize::new(0),
        }
    }
}
