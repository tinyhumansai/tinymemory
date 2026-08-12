//! Native HTTP adapters for self-hosted memory engines.
//!
//! The adapters preserve TinyMemory's exact `(namespace, key)` upsert contract
//! in backend metadata while delegating semantic recall to each engine's native
//! search API. They advertise Core, Recall, and Portability through
//! [`tinymemory::mandatory::MemoryTraitProvider`].
//!
//! Credentials are accepted only at construction and are never exposed by
//! `Debug` implementations or error messages.

pub mod cognee;
mod common;
pub mod mem0;
pub mod supermemory;

pub use cognee::{CogneeMemory, COGNEE_DRIVER_ID};
pub use mem0::{Mem0Memory, MEM0_DRIVER_ID};
pub use supermemory::{SupermemoryMemory, SUPERMEMORY_DRIVER_ID};

use std::sync::Arc;

use tinymemory::mandatory::MemoryTraitProvider;

/// Wrap a Supermemory HTTP backend as a bound TinyMemory provider.
#[must_use]
pub fn supermemory_provider(memory: SupermemoryMemory) -> MemoryTraitProvider {
    MemoryTraitProvider::new(Arc::new(memory), SUPERMEMORY_DRIVER_ID)
}

/// Wrap a Mem0 HTTP backend as a bound TinyMemory provider.
#[must_use]
pub fn mem0_provider(memory: Mem0Memory) -> MemoryTraitProvider {
    MemoryTraitProvider::new(Arc::new(memory), MEM0_DRIVER_ID)
}

/// Wrap a Cognee HTTP backend as a bound TinyMemory provider.
#[must_use]
pub fn cognee_provider(memory: CogneeMemory) -> MemoryTraitProvider {
    MemoryTraitProvider::new(Arc::new(memory), COGNEE_DRIVER_ID)
}
