//! Product adapters for tinycortex-owned stale-buffer flushing.

use anyhow::Result;
use chrono::{DateTime, Duration, Utc};

use crate::store::trees::types::DEFAULT_FLUSH_AGE_SECS;
use crate::tree::tree::bucket_seal::{cascade_all_from, LabelStrategy};
use crate::Config;

pub async fn flush_stale_buffers(
    config: &Config,
    max_age: Duration,
    strategy: &LabelStrategy,
) -> Result<usize> {
    crate::engine::flush_stale_tree_buffers(config, max_age, strategy).await
}

pub async fn flush_stale_buffers_default(
    config: &Config,
    strategy: &LabelStrategy,
) -> Result<usize> {
    flush_stale_buffers(config, Duration::seconds(DEFAULT_FLUSH_AGE_SECS), strategy).await
}

pub async fn force_flush_tree(
    config: &Config,
    tree_id: &str,
    now: Option<DateTime<Utc>>,
    strategy: &LabelStrategy,
) -> Result<Vec<String>> {
    let tree = crate::store::trees::store::get_tree(config, tree_id)?
        .ok_or_else(|| anyhow::anyhow!("no tree with id {tree_id}"))?;
    cascade_all_from(config, &tree, 0, now.or_else(|| Some(Utc::now())), strategy).await
}

#[cfg(test)]
#[path = "flush_tests.rs"]
mod tests;
