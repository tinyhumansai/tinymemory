//! Where an item from the legacy tree goes in the per-user tree.
//!
//! The rule: an item goes where today's write path would put it now, so a
//! later sync of the same item replays instead of storing a duplicate.
//!
//! - **Brain documents** at `source:<id>` go to W4's connector node:
//!   [`legacy_brain_node`], then [`brain_node_with`] and [`file_into`]: the
//!   same placement new syncs use (`pdf`, `markdown` and the other
//!   per-format nodes become `files`; GitHub splits by repository when the
//!   setting is on).
//! - **Conversations** are pooled at the chat node; the agent id stays in
//!   the item's metadata.
//! - **Workflow items** (keyed memory and run digests, tagged `flow:<id>`)
//!   stay with the root's learnings and documents for now, as they are read
//!   today. [`FlowPlacement::InNode`] is the one switch that moves them into
//!   each flow's own node, once flow memory is read from there.
//! - Everything else keeps its namespace; only the wire root changes.

use tinymemory_api::{ItemKind, Namespace, SegmentKind, StoreItem};

use super::error::Result;
use crate::layout::MemoryLayout;
pub use crate::layout::placement::{
    brain_node_with, file_into, files_source, github_collection, legacy_brain_node,
};

/// The tag every workflow item carries. Legacy items carry it whatever
/// features the host build has, so placing them must not depend on them.
pub const FLOWS_TAG: &str = "flows";

/// Where workflow items go.
#[derive(Clone, Copy)]
pub enum FlowPlacement {
    /// With the root's learnings and documents, as today.
    WithRoot,
    /// In each flow's own node: the flow id to its node.
    InNode(fn(&str) -> Result<Namespace>),
}

/// Where items go in the per-user tree.
#[derive(Debug)]
pub struct Placement {
    /// The layout new items are written with.
    pub layout: MemoryLayout,
    /// Where conversations are pooled.
    pub chat_node: Namespace,
    /// Where workflow items go.
    pub flows: FlowPlacement,
    /// `[memory] split_github_by_repo`: GitHub documents go one collection
    /// per repository, as a sync files them now.
    pub split_github_by_repo: bool,
}

impl std::fmt::Debug for FlowPlacement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WithRoot => f.write_str("WithRoot"),
            Self::InNode(_) => f.write_str("InNode(..)"),
        }
    }
}

/// The flow a workflow item belongs to: its `flow:<id>` tag, when it also
/// carries the `flows` tag (keyed memory and run digests both do).
fn flow_of(item: &StoreItem) -> Option<&str> {
    let tags = &item.meta().tags;
    if !tags.iter().any(|tag| tag == FLOWS_TAG) {
        return None;
    }
    tags.iter()
        .filter_map(|tag| tag.strip_prefix("flow:"))
        .find(|id| !id.contains(':'))
}

/// The id of the `source:` segment directly below `root`, if `namespace` is
/// a brain node of the legacy layout.
fn legacy_source<'a>(root: &Namespace, namespace: &'a Namespace) -> Option<&'a str> {
    let (root, segments) = (root.segments(), namespace.segments());
    let below = segments.get(root.len())?;
    (segments.starts_with(root) && below.kind() == SegmentKind::Source).then(|| below.id())
}

impl Placement {
    /// `item` as it is stored in the per-user tree.
    ///
    /// # Errors
    ///
    /// When a node cannot be built (an id the namespace grammar refuses).
    pub fn place(&self, mut item: StoreItem) -> Result<StoreItem> {
        if let Some(flow) = flow_of(&item) {
            if let FlowPlacement::InNode(node) = self.flows {
                item.meta_mut().namespace = node(flow)?;
            }
            return Ok(item);
        }
        if item.kind() == ItemKind::Conversation {
            item.meta_mut().namespace = self.chat_node.clone();
            return Ok(item);
        }
        if item.kind() == ItemKind::Document {
            let namespace = item.meta().namespace.clone();
            if let Some(id) = legacy_source(self.layout.root(), &namespace) {
                let source = legacy_brain_node(id, &item);
                let node =
                    brain_node_with(self.split_github_by_repo, &self.layout, &source, &item)?;
                return Ok(file_into(node, item));
            }
        }
        Ok(item)
    }
}

#[cfg(test)]
#[path = "map_tests.rs"]
mod tests;
