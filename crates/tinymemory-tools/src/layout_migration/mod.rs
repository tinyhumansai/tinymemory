//! Moving memory from a legacy tree into the per-user tree of
//! [`crate::layout::MemoryLayout`].
//!
//! Memory written before the per-user layout lives under one legacy root;
//! the per-user layout keeps each person's memory below their own root.
//! Items are moved by being stored again ([`copy()`]) rather than by copying
//! events, because an item's namespace is part of its identity and its
//! placement changes ([`map`]). Progress is saved after every page
//! ([`state`]), so the move resumes where it stopped and repeats at most one
//! page.
//!
//! Everything is written against [`tinymemory_api::MemoryEngine::export`]
//! and two host ports, so the crate links no executor and does no I/O of its
//! own:
//!
//! - [`MigrationStateStore`] persists the progress.
//! - [`LayoutHost`] binds the two engines, says where items go, performs the
//!   switch to the per-user tree, answers whether moving is free now, and
//!   names the [`LegacyClaim`] on a shared legacy tree. The app-specific
//!   placement (flow nodes, the chat node) arrives as a [`Placement`].
//!
//! [`run`] is the whole job: detect, copy, switch, catch up, clean up.
//! [`scan`], [`status`] and [`retry`] are what a host's RPCs call.

pub mod cleanup;
pub mod copy;
pub mod error;
#[cfg(feature = "fs-claim")]
mod fs_claim;
pub mod job;
pub mod map;
pub mod service;
pub mod state;

#[cfg(test)]
pub(crate) mod test_host;

pub use cleanup::cleanup;
pub use copy::{Engines, PAGE, copy, legacy_present};
pub use error::{Error, Result};
#[cfg(feature = "fs-claim")]
pub use fs_claim::FsLegacyClaim;
pub use job::{LayoutHost, LegacyClaim, Outcome, Trigger, run};
pub use map::{
    FLOWS_TAG, FlowPlacement, Placement, brain_node_with, file_into, files_source,
    github_collection, legacy_brain_node,
};
pub use service::{
    MigrationService, MigrationSpawner, MigrationStatus, MigrationTask, ScanView, is_finished,
    record_failure, retry, scan, status,
};
pub use state::{Failure, MemoryStateStore, MigrationState, MigrationStateStore, Phase};
