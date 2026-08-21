//! Production runtime-override policy: no test provider is installed.

#[cfg(not(any(test, feature = "test-support")))]
use super::*;

#[cfg(any(test, feature = "test-support"))]
pub(super) use super::test_support::current_runtime;

#[cfg(not(any(test, feature = "test-support")))]
pub(super) fn current_runtime() -> Option<(Arc<dyn ChatProvider>, String)> {
    None
}
