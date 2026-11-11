//! Test-only knobs for [`CortexEngine`].

use std::time::Duration;

use super::CortexEngine;

impl CortexEngine {
    /// Shortens every wait and backoff, so a test reaches timeouts fast.
    pub(crate) fn with_test_timing(mut self, visibility: Duration) -> Self {
        self.log.timing = crate::cortex::log::Timing {
            visibility,
            settle: visibility,
            poll: Duration::from_millis(5),
            erasure: Duration::from_millis(300),
            evidence: visibility,
        };
        self.log.client.set_read_backoff(Duration::from_millis(5));
        self
    }
}
