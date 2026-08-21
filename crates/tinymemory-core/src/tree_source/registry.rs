//! Source-tree registry — thin wrapper around the generic
//! [`crate::tree::tree::registry::get_or_create_tree`]
//! that adds the source-specific `_source.md` on-disk mirror write after
//! every get-or-create call.

use anyhow::Result;

#[cfg(test)]
use tinymemory_api::host::test_support::TestHostConfig;

use super::file;
use crate::store::trees::types::Tree;
use crate::tree::tree::TreeFactory;
use crate::Config;

/// Look up the source tree for `scope`, or create a new one.
///
/// Scope format convention (Phase 3a): use the ingested chunk's
/// `metadata.source_id` verbatim, so re-ingesting the same Slack channel
/// or Gmail account keeps appending to the same tree.
///
/// After every successful get-or-create the `_source.md` on-disk mirror
/// for this source is (re)written. The write is best-effort — a failure
/// is logged but does not abort the call.
pub fn get_or_create_source_tree(config: &Config, scope: &str) -> Result<Tree> {
    log::debug!(
        "[sources::registry] get_or_create_source_tree scope={}",
        crate::util::redact::redact(scope)
    );
    let tree = TreeFactory::source(scope).get_or_create(config)?;
    if let Err(e) = file::write_source_file(config, &tree) {
        log::warn!(
            "[tree_source::registry] write_source_file failed scope={} err={e:#}",
            crate::util::redact::redact(scope)
        );
    }
    Ok(tree)
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
