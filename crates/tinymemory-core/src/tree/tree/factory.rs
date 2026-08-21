//! Kind/profile factory for memory-tree instances.
//!
//! Centralizes the flavor-specific bits so callers get a uniform API:
//! - underlying [`TreeKind`]
//! - canonical scope
//! - summary-file kind
//! - scope-slug rules
//! - default seal-time label strategy

use std::borrow::Cow;

use anyhow::Result;

use crate::store::content::paths::slugify_source_id;
use crate::store::content::SummaryTreeKind;
use crate::store::trees::archive_tree;
use crate::store::trees::types::{Tree, TreeKind};
use crate::tree::score::extract::build_summary_extractor;
use crate::tree::tree::bucket_seal::{append_leaf, LabelStrategy, LeafRef};
use crate::tree::tree::flush::force_flush_tree;
use crate::tree::tree::registry::get_or_create_tree;
use crate::Config;

pub use crate::engine::backend::tree::{TreeProfile, GLOBAL_SCOPE};

/// Factory/config object for one tree instance.
#[derive(Debug, Clone)]
pub struct TreeFactory<'a> {
    inner: crate::engine::backend::tree::TreeFactory<'a>,
}

impl<'a> TreeFactory<'a> {
    pub fn source(scope: impl Into<Cow<'a, str>>) -> Self {
        Self {
            inner: crate::engine::backend::tree::TreeFactory::source(scope),
        }
    }

    pub fn topic(scope: impl Into<Cow<'a, str>>) -> Self {
        Self {
            inner: crate::engine::backend::tree::TreeFactory::topic(scope),
        }
    }

    pub fn global() -> Self {
        Self {
            inner: crate::engine::backend::tree::TreeFactory::global(),
        }
    }

    pub fn from_tree(tree: &'a Tree) -> Self {
        Self {
            inner: crate::engine::backend::tree::TreeFactory::from_tree(tree),
        }
    }

    pub fn profile(&self) -> TreeProfile {
        self.inner.profile()
    }

    pub fn kind(&self) -> TreeKind {
        self.inner.kind()
    }

    pub fn scope(&self) -> &str {
        self.inner.scope()
    }

    pub fn summary_tree_kind(&self) -> SummaryTreeKind {
        match self.kind() {
            TreeKind::Source => SummaryTreeKind::Source,
            TreeKind::Topic => SummaryTreeKind::Topic,
            TreeKind::Global => SummaryTreeKind::Global,
            _ => SummaryTreeKind::Source,
        }
    }

    pub fn scope_slug(&self) -> String {
        let scope = self.scope();
        match self.kind() {
            TreeKind::Topic | TreeKind::Global => slugify_source_id(scope),
            TreeKind::Source => {
                if let Some(gmail_scope) = scope.strip_prefix("gmail:") {
                    slugify_source_id(gmail_scope)
                } else {
                    slugify_source_id(scope)
                }
            }
            _ => slugify_source_id(scope),
        }
    }

    pub fn label_strategy(&self, config: &Config) -> LabelStrategy {
        match self.kind() {
            TreeKind::Source => LabelStrategy::ExtractFromContent(build_summary_extractor(config)),
            TreeKind::Topic | TreeKind::Global => LabelStrategy::Empty,
            _ => LabelStrategy::ExtractFromContent(build_summary_extractor(config)),
        }
    }

    /// Look up or create the tree row in the database. Instance-specific
    /// side-effects (e.g. `_source.md` mirror) are handled by the
    /// per-instance registry wrappers in `memory::tree_source` etc.
    pub fn get_or_create(&self, config: &Config) -> Result<Tree> {
        get_or_create_tree(config, self.kind(), self.scope())
    }

    /// Append one leaf to this tree profile using its default labeling policy.
    pub async fn insert_leaf(&self, config: &Config, leaf: &LeafRef) -> Result<Vec<String>> {
        let tree = self.get_or_create(config)?;
        let strategy = self.label_strategy(config);
        append_leaf(config, &tree, leaf, &strategy).await
    }

    /// Force-flush/seal this tree profile's currently loaded tree.
    pub async fn seal_now(&self, config: &Config) -> Result<Vec<String>> {
        let tree = self.get_or_create(config)?;
        let strategy = self.label_strategy(config);
        force_flush_tree(config, &tree.id, None, &strategy).await
    }

    /// Archive this tree profile's current tree.
    pub fn archive(&self, config: &Config) -> Result<()> {
        let tree = self.get_or_create(config)?;
        archive_tree(config, &tree.id)
    }
}

#[cfg(test)]
#[path = "factory_tests.rs"]
mod tests;
