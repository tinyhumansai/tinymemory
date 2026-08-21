//! Core traits and data structures for the OpenHuman memory system.
//!
//! This module defines the foundational `Memory` trait that all storage backends
//! must implement. The trait and the standard memory value types (`MemoryEntry`,
//! `MemoryCategory`, `MemoryTaint`, `RecallOpts`, `NamespaceSummary`) are
//! **re-exported from `tinymemory-api`**, the engine-neutral contract.
//!
//! They used to come from the `tinycortex` crate — from the *engine*, in other
//! words, which meant `tinymemory_core::MemoryEntry` was the engine's type and
//! not the contract's, and a second engine could not have been bound without
//! translating (issue #18 §A1/§A2). Naming the contract directly is what makes
//! this crate engine-neutral at the type level; the 30+ host consumers keep
//! their `memory::traits::…` import paths unchanged either way.
//!
//! `MemoryTaint` is security-critical provenance — it fails closed to
//! `ExternalSync` for unknown/corrupt values so the subconscious gate refuses
//! external-effect tools on chunks of unknown origin. Its semantics were proven
//! byte-identical to the former host definition before re-exporting; the tests
//! below are the host-side seam that pins that contract on the crate type.
//!
//! Backend-specific resources such as SQLite connections are carried explicitly
//! by factories instead of being exposed through the storage abstraction.

// ── The contract's trait and value types ─────────────────────────────────────
//
// Named directly rather than reached through the engine's re-export. Since §A1 the
// engine re-exports this same contract, so the two spellings resolve to one type
// either way — but going through the engine to reach an engine-neutral contract
// is what §1.1 of issue #18 calls out, and it is what would have to be undone
// before a second engine could be bound.
pub use tinymemory_api::recall::RecallOpts;
pub use tinymemory_api::traits::Memory;
pub use tinymemory_api::types::{MemoryCategory, MemoryEntry, MemoryTaint, NamespaceSummary};

#[cfg(test)]
#[path = "traits_tests.rs"]
mod tests;
