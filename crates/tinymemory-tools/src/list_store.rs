//! The host port the durable queues ([`crate::pending`], [`crate::deletion`])
//! keep their entries in.

use std::sync::Mutex;

/// A durable store failed. The message is content-free; callers map it to
/// their own "queue unavailable" error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("queue store unavailable: {0}")]
pub struct StoreError(pub String);

/// Where the host keeps one queue's entries (a file, a document, a stream),
/// in order, oldest first.
///
/// Synchronous: the queues are small and read-modify-written under their own
/// lock. `load` returns an empty list when nothing is saved; an unreadable
/// store is `Err` so a queue never overwrites entries it could not read. A
/// host that sets a corrupt file aside returns empty after doing so.
pub trait ListStore<T>: Send + Sync {
    /// The saved entries.
    ///
    /// # Errors
    /// The store could not be read.
    fn load(&self) -> Result<Vec<T>, StoreError>;
    /// Replaces the saved entries. An empty list clears the store.
    ///
    /// # Errors
    /// The store could not be written; the caller treats the write as not made.
    fn save(&self, entries: &[T]) -> Result<(), StoreError>;
}

/// A [`ListStore`] that keeps entries in memory.
#[derive(Debug)]
pub struct MemoryListStore<T>(Mutex<Vec<T>>);

impl<T> Default for MemoryListStore<T> {
    fn default() -> Self {
        Self(Mutex::new(Vec::new()))
    }
}

impl<T: Clone + Send> ListStore<T> for MemoryListStore<T> {
    fn load(&self) -> Result<Vec<T>, StoreError> {
        Ok(self.0.lock().unwrap_or_else(|e| e.into_inner()).clone())
    }

    fn save(&self, entries: &[T]) -> Result<(), StoreError> {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = entries.to_vec();
        Ok(())
    }
}
