//! On-disk format parity — Layer-1 regression pins (migration W3 gate, spec §0.3).
//!
//! Existing user workspaces must open unchanged after the store cutover. These
//! are the cheap, fixture-free asserters from the parity checklist: they pin the
//! crate's deterministic **on-disk contracts** to the exact byte forms that
//! historical OpenHuman workspaces were written with, so any future crate change
//! that would silently reshape chunk IDs, vector encoding, or vault paths fails
//! here instead of corrupting a real workspace.
//!
//! The golden constants were computed from the format spec (SHA-256 first-32-hex
//! chunk IDs; little-endian packed f32 vectors) and cross-checked against the
//! crate at the W3 baseline. The Layer-2 golden-workspace differential harness
//! (a real `chunks.db` + vault opened and compared) is the merge gate for the
//! actual store flips; this layer runs on every PR.
//!
//! Test-only module — no runtime code.

#[cfg(test)]
#[path = "parity_tests.rs"]
mod tests;
