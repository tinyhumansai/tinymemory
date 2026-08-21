//! Zero-cost production hooks for `OpenStore` lifecycle instrumentation.

/// Production implementation of the test-observation boundary.
pub(crate) struct OpenStoreInstrumentation {
    record_allocation: fn(),
    before_registration: fn() -> tinybus::Result<()>,
}

impl OpenStoreInstrumentation {
    /// Record that store allocation is about to begin.
    pub(crate) fn record_allocation(&self) {
        (self.record_allocation)();
    }

    /// Allow registration to proceed in production.
    pub(crate) fn before_registration(&self) -> tinybus::Result<()> {
        (self.before_registration)()
    }
}

impl Default for OpenStoreInstrumentation {
    fn default() -> Self {
        Self {
            record_allocation: || {},
            before_registration: || Ok(()),
        }
    }
}
