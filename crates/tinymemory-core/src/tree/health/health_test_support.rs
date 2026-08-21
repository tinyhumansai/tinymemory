#![cfg(any(test, feature = "test-support"))]
//! Test-only serialization and reset for global degraded-health state.

use super::*;

pub fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    let guard = LOCK
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    SEMANTIC_RECALL_DEGRADED.store(false, Ordering::Relaxed);
    LOCAL_MODEL_USER_ERROR_SURFACED.store(false, Ordering::Relaxed);
    STRUCTURE_DEGRADED.store(false, Ordering::Relaxed);
    STORAGE_DEGRADED.store(false, Ordering::Relaxed);
    SEMANTIC_RECALL_CAUSE.store(0, Ordering::Relaxed);
    STRUCTURE_CAUSE.store(0, Ordering::Relaxed);
    STORAGE_CAUSE.store(0, Ordering::Relaxed);
    guard
}
