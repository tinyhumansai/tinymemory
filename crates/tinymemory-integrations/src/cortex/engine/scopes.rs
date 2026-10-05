//! Which CortexDB scopes an operation reads.
//!
//! Every item lives in the scope of its kind under its namespace node (see
//! `envelope`). A [`MetaFilter`] names the kinds and the [`Reach`]; this
//! module turns them into the exact scopes to read, ordered by kind
//! ([`ItemKind::ALL`]) and then by namespace, so a cursor can resume by
//! position.
//!
//! - **A reach without descendants** reads `at` and, when it inherits, each
//!   ancestor: the nodes are known, so no request is needed. A node nothing
//!   was written to lists empty.
//! - **A subtree reach, or no reach at all,** needs the nodes below, which
//!   only the engine knows: they are discovered once per call from the
//!   registered scopes under the engine's TinyMemory root, and a reported
//!   scope outside that root (another tenant's) is never read. The root's own kind scopes
//!   are always read.
//!
//! Reads are always exact (`view=local`): server-side traversal is never
//! relied on, so one agent's read can never stray into a sibling's scope.

use std::collections::BTreeSet;

use tinymemory_api::{ItemKind, MetaFilter, Namespace, Reach};

use super::CortexEngine;
use super::items::admitted;
use crate::cortex::envelope::{parse_scope, scope_path};
use crate::cortex::error::Result;
use crate::cortex::tenancy::ScopeRoot;

/// One scope to read: a kind at a namespace node.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct KindScope {
    /// Position of the kind in [`ItemKind::ALL`]; the primary sort key.
    order: usize,
    /// The node.
    pub(crate) namespace: Namespace,
    /// The kind.
    pub(crate) kind: ItemKind,
    /// The scope path.
    pub(crate) path: String,
}

impl KindScope {
    /// The scope of `kind` at `namespace`, under `root`.
    pub(crate) fn new(root: &ScopeRoot, namespace: Namespace, kind: ItemKind) -> Self {
        Self {
            order: ItemKind::ALL
                .iter()
                .position(|k| *k == kind)
                .unwrap_or_default(),
            path: scope_path(root, &namespace, kind),
            namespace,
            kind,
        }
    }
}

/// The scopes `reach` reads exactly (no discovery) under `root`: its nodes
/// for each kind.
pub(crate) fn known(root: &ScopeRoot, reach: &Reach, kinds: &[ItemKind]) -> Vec<KindScope> {
    let mut scopes: Vec<KindScope> = reach
        .nodes()
        .into_iter()
        .flat_map(|node| {
            kinds
                .iter()
                .map(move |kind| KindScope::new(root, node.clone(), *kind))
        })
        .collect();
    scopes.sort();
    scopes
}

/// Whether reading `reach` needs the engine's list of nodes.
fn needs_discovery(reach: Option<&Reach>) -> bool {
    reach.is_none_or(|reach| reach.descendants)
}

impl CortexEngine {
    /// The scopes `filter` reads, kind first then namespace. See the module
    /// docs.
    pub(super) async fn scopes_for(&self, filter: &MetaFilter) -> Result<Vec<KindScope>> {
        let kinds = admitted(filter);
        if kinds.is_empty() {
            return Ok(Vec::new());
        }
        let reach = filter.reach.as_ref();
        let Some(base) = reach.filter(|_| !needs_discovery(reach)) else {
            return self.discovered(reach, &kinds).await;
        };
        Ok(known(&self.root, base, &kinds))
    }

    /// The scopes of `kinds` in `reach` that CortexDB has registered — only
    /// those with something written — kind first then namespace. Unlike a
    /// read, nothing is assumed to exist: a build of an empty scope would be
    /// wasted model time.
    pub(super) async fn held(&self, reach: &Reach, kinds: &[ItemKind]) -> Result<Vec<KindScope>> {
        let mut found = BTreeSet::new();
        for path in self.log.scopes(self.root.path()).await? {
            let Some((namespace, kind)) = parse_scope(&self.root, &path) else {
                continue;
            };
            if reach.admits(&namespace) && kinds.contains(&kind) {
                found.insert(KindScope::new(&self.root, namespace, kind));
            }
        }
        Ok(found.into_iter().collect())
    }

    /// Every scope of `kinds` the engine holds, in reach.
    async fn discovered(
        &self,
        reach: Option<&Reach>,
        kinds: &[ItemKind],
    ) -> Result<Vec<KindScope>> {
        let mut found: BTreeSet<KindScope> = match reach {
            Some(reach) => known(&self.root, reach, kinds).into_iter().collect(),
            None => known(&self.root, &Reach::exact(Namespace::ROOT), kinds)
                .into_iter()
                .collect(),
        };
        let prefix = match reach {
            Some(reach) if !reach.at.is_root() => {
                let mut path = scope_path(&self.root, &reach.at, ItemKind::Document);
                path.truncate(path.rfind('/').unwrap_or(path.len()));
                path
            }
            _ => self.root.path().to_string(),
        };
        for path in self.log.scopes(&prefix).await? {
            let Some((namespace, kind)) = parse_scope(&self.root, &path) else {
                continue;
            };
            let in_reach = reach.is_none_or(|reach| reach.admits(&namespace));
            if in_reach && kinds.contains(&kind) {
                found.insert(KindScope::new(&self.root, namespace, kind));
            }
        }
        Ok(found.into_iter().collect())
    }
}

#[cfg(test)]
#[path = "scopes_tests.rs"]
mod tests;
