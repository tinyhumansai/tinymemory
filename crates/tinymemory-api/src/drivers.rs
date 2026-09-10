//! The reserved driver ids.
//!
//! These name the engines this workspace ships. They live in the contract crate
//! rather than in the facade's registry for the same reason
//! [`crate::null::NULL_DRIVER_ID`] already did: an adapter has to spell the id
//! it binds under, and reaching into the facade for it made every adapter
//! depend on the facade — which in turn made the facade unable to depend on the
//! adapters, a package cycle cargo forbids. That cycle is what blocked #18
//! §D1's per-engine features.
//!
//! Reserving them here does **not** mean the contract knows about these
//! engines. It knows their *names*, so that admission can refuse something else
//! binding under one — a host that compiles an adapter out must still reject an
//! impostor claiming its id. The class each id is admitted under stays in the
//! facade's registry, where the trust decision belongs.
//!
//! `tinymemory::registry` re-exports all of these, so existing paths resolve
//! unchanged.

/// The driver id of the bundled TinyCortex embedded engine.
pub const TINYCORTEX_DRIVER_ID: &str = "tinycortex";

/// The driver id of `tinymemory-core`'s own in-process store.
///
/// Distinct from [`TINYCORTEX_DRIVER_ID`], and the distinction is the point:
/// that one names the bundled TinyCortex engine, this one names
/// `tinymemory_core::store::UnifiedMemory` — a separate SQLite store this
/// workspace implements itself. Both are `Embedded`; they are not the same
/// engine, and a host that binds one has not bound the other.
///
/// Named for what `create_memory` has always called this backend
/// (`effective_memory_backend_name` returns `"namespace"`), so the id an
/// operator sees in status matches the name already in the logs rather than
/// introducing a third vocabulary for one store.
pub const NAMESPACE_DRIVER_ID: &str = "namespace";

/// Driver id of the native Supermemory HTTP adapter.
pub const SUPERMEMORY_DRIVER_ID: &str = "supermemory";

/// Driver id of the native Mem0 HTTP adapter.
pub const MEM0_DRIVER_ID: &str = "mem0";

/// Driver id of the native Cognee HTTP adapter.
pub const COGNEE_DRIVER_ID: &str = "cognee";

/// Driver id of the native CortexDB HTTP adapter.
pub const CORTEX_DRIVER_ID: &str = "cortex";

/// Driver id of the local AgentMemory HTTP adapter.
pub const AGENTMEMORY_DRIVER_ID: &str = "agentmemory";
