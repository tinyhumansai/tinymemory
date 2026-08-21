//! Generic tree registry — get-or-create for any tree kind (#709).
//!
//! All three tree flavors (Source, Global, Topic) share `UNIQUE(kind, scope)`
//! and the same race-recovery dance — there is no reason for three copies.
//! Source-specific side-effects (writing the `_source.md` mirror) live in
//! the `sources::registry` wrapper rather than here.

use anyhow::Result;
use chrono::Utc;
use uuid::Uuid;

#[cfg(test)]
use tinymemory_api::host::test_support::TestHostConfig;

use crate::store::trees::types::{Tree, TreeKind, TreeStatus};
use crate::tree::tree::store;
use crate::Config;

/// Generic get-or-create. All three tree flavors (Source, Global, Topic)
/// share UNIQUE(kind, scope) and the same race-recovery dance — there's
/// no reason for three copies.
///
/// Source-specific side-effects (writing the `_source.md` on-disk mirror)
/// are NOT performed here; callers that need them should go through
/// [`crate::tree_source::registry::get_or_create_source_tree`].
pub fn get_or_create_tree(config: &Config, kind: TreeKind, scope: &str) -> Result<Tree> {
    if let Some(existing) = store::get_tree_by_scope(config, kind, scope)? {
        log::debug!(
            "[tree::registry] found tree id={} kind={} scope={}",
            existing.id,
            kind.as_str(),
            scope
        );
        return Ok(existing);
    }

    let tree = Tree {
        id: new_tree_id(kind),
        kind,
        scope: scope.to_string(),
        ask: None,
        root_id: None,
        max_level: 0,
        status: TreeStatus::Active,
        created_at: Utc::now(),
        last_sealed_at: None,
    };
    match store::insert_tree(config, &tree) {
        Ok(()) => {
            log::info!(
                "[tree::registry] created tree id={} kind={} scope={}",
                tree.id,
                kind.as_str(),
                scope
            );
            Ok(tree)
        }
        Err(err) if is_unique_violation(&err) => {
            // Race: another caller created a tree for the same (kind, scope)
            // between our initial lookup and this insert. UNIQUE(kind, scope)
            // rejected our row; re-query and return the winner.
            log::debug!(
                "[tree::registry] UNIQUE race for kind={} scope={} — re-querying",
                kind.as_str(),
                scope
            );
            store::get_tree_by_scope(config, kind, scope)?.ok_or_else(|| {
                anyhow::anyhow!(
                    "UNIQUE violation on insert but no row found on re-query for kind={} scope={}",
                    kind.as_str(),
                    scope
                )
            })
        }
        Err(err) => Err(err),
    }
}

/// Return true if `err` represents a SQLite UNIQUE constraint violation.
/// Matches both the anyhow-wrapped rusqlite error text and the raw SQLite
/// error codes in case the wrapping chain is shorter.
pub fn is_unique_violation(err: &anyhow::Error) -> bool {
    if let Some(rusqlite::Error::SqliteFailure(sqlite_err, _)) =
        err.downcast_ref::<rusqlite::Error>()
    {
        return sqlite_err.code == rusqlite::ErrorCode::ConstraintViolation;
    }
    // Fallback for chained/wrapped errors: scan the rendered message.
    let msg = format!("{err:#}");
    msg.contains("UNIQUE constraint failed")
}

/// Generate a stable id for a new tree row, prefixed with the kind discriminator.
pub fn new_tree_id(kind: TreeKind) -> String {
    format!("{}:{}", kind.as_str(), Uuid::new_v4())
}

/// Public id generator for summary nodes — exported so `bucket_seal` can
/// share the same format. The Unix-ms timestamp is the leading sort
/// key so `ORDER BY id` is globally chronological across all levels
/// (a level-first layout grouped L1, L2, … together, breaking that).
/// `:013` zero-pads the millisecond field to 13 digits so the
/// lexicographic order matches numeric order through year 2286 — well
/// outside any reasonable retention window. Level is suffixed for
/// filter-by-level queries (`LIKE '%:L1-%'`). 8-hex of `u32` entropy
/// shrinks same-millisecond collision probability to ~2⁻³² per pair,
/// sized for uniqueness across the file-system and Obsidian wikilink
/// namespaces.
pub fn new_summary_id(level: u32) -> String {
    let ms = chrono::Utc::now().timestamp_millis() as u64;
    let rand_tail: u32 = rand::random();
    format!("summary:{:013}:L{}-{:08x}", ms, level, rand_tail)
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
