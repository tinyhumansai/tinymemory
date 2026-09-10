//! Native HTTP adapters for self-hosted memory engines.
//!
//! The adapters preserve TinyMemory's exact `(namespace, key)` upsert contract
//! in backend metadata while delegating semantic recall to each engine's native
//! search API. They advertise Core, Recall, and Portability through
//! [`tinymemory_api::mandatory::MemoryTraitProvider`].
//!
//! Credentials are accepted only at construction and are never exposed by
//! `Debug` implementations or error messages.

pub mod cognee;
mod cognee_graph;
mod common;
pub mod cortex;
mod cortex_provider;
mod graph_provider;
pub mod livingbrain;
pub mod mem0;
mod mem0_graph;
mod mem0_provider;
pub mod supermemory;

pub use agentmemory::{AgentMemoryMemory, AGENTMEMORY_API_ENDPOINT, AGENTMEMORY_DRIVER_ID};
pub use cognee::{CogneeMemory, COGNEE_DRIVER_ID};
pub use cognee_graph::CogneeGraph;
pub use cortex::{CortexMemory, CORTEX_API_ENDPOINT, CORTEX_DRIVER_ID};
pub use cortex_provider::CortexProvider;
pub use graph_provider::GraphMemoryProvider;
pub use livingbrain::{
    Capture, CaptureBatchReceipt, CaptureKind, CaptureReceipt, CaptureSource, ChatSender, ChatTurn,
    ChatTurnReceipt, LivingBrain, LivingBrainExport, LivingBrainSearchResult,
    LIVINGBRAIN_API_ENDPOINT,
};
pub use mem0::{Mem0Memory, MEM0_API_ENDPOINT, MEM0_DRIVER_ID};
pub use mem0_graph::Mem0Graph;
pub use mem0_provider::Mem0Provider;
pub use supermemory::{SupermemoryMemory, SUPERMEMORY_API_ENDPOINT, SUPERMEMORY_DRIVER_ID};

use std::sync::Arc;

use tinymemory_api::mandatory::MemoryTraitProvider;

/// Wrap a Supermemory HTTP backend as a bound TinyMemory provider.
#[must_use]
pub fn supermemory_provider(memory: SupermemoryMemory) -> MemoryTraitProvider {
    MemoryTraitProvider::new(Arc::new(memory), SUPERMEMORY_DRIVER_ID)
}

/// Wrap a Mem0 HTTP backend as a bound TinyMemory provider.
#[must_use]
pub fn mem0_provider(memory: Mem0Memory) -> Mem0Provider {
    Mem0Provider::new(memory)
}

/// Wrap a Cognee HTTP backend as a bound TinyMemory provider.
#[must_use]
pub fn cognee_provider(memory: CogneeMemory) -> MemoryTraitProvider {
    MemoryTraitProvider::new(Arc::new(memory), COGNEE_DRIVER_ID)
}

/// Wrap a CortexDB HTTP backend as a bound TinyMemory provider.
#[must_use]
pub fn cortex_provider(memory: CortexMemory) -> CortexProvider {
    let client = memory.operation_client();
    CortexProvider::new(memory, client)
}

/// Wrap an AgentMemory HTTP backend as a bound TinyMemory provider.
#[must_use]
pub fn agentmemory_provider(memory: AgentMemoryMemory) -> MemoryTraitProvider {
    MemoryTraitProvider::new(Arc::new(memory), AGENTMEMORY_DRIVER_ID)
}

/// Wrap a Cognee HTTP backend as a bound TinyMemory provider that also
/// advertises Graph, backed by [`CogneeGraph`] — see its docs for exactly
/// which `MemoryGraph` methods have a real Cognee counterpart.
///
/// # Errors
///
/// Returns an error when `endpoint` is not an HTTP(S) URL.
pub fn cognee_graph_provider(
    memory: CogneeMemory,
    endpoint: &str,
    access_token: Option<&str>,
) -> anyhow::Result<GraphMemoryProvider> {
    let graph = CogneeGraph::new(endpoint, access_token)?;
    Ok(GraphMemoryProvider::new(
        cognee_provider(memory),
        Arc::new(graph),
    ))
}

/// Wrap a Cognee Cloud backend as a bound TinyMemory provider that also
/// advertises Graph, using the same `X-Api-Key` authentication for memory and
/// graph requests.
///
/// # Errors
///
/// Returns an error when `endpoint` is invalid or `api_key` is blank.
pub fn cognee_api_graph_provider(
    memory: CogneeMemory,
    endpoint: &str,
    api_key: &str,
) -> anyhow::Result<GraphMemoryProvider> {
    let graph = CogneeGraph::api(endpoint, api_key)?;
    Ok(GraphMemoryProvider::new(
        cognee_provider(memory),
        Arc::new(graph),
    ))
}

/// Wrap a Mem0 HTTP backend as a bound TinyMemory provider that also
/// advertises Graph, backed by [`Mem0Graph`] — a client-side heuristic over
/// the same stored entries, not Mem0's native (platform-only) Graph Memory.
/// See [`Mem0Graph`]'s docs for exactly what that means and why.
#[must_use]
pub fn mem0_graph_provider(memory: Mem0Memory) -> GraphMemoryProvider {
    let memory: Arc<dyn tinymemory_api::traits::Memory> = Arc::new(memory);
    let provider = Mem0Provider::from_memory(Arc::clone(&memory));
    GraphMemoryProvider::new(provider, Arc::new(Mem0Graph::new(memory)))
}

#[cfg(test)]
mod failure_test;

pub mod agentmemory;
#[cfg(test)]
mod conformance_test;
