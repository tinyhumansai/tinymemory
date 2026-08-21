//! Test helpers for the jobs runtime — not used in production code paths.

use anyhow::Result;

#[cfg(test)]
use tinymemory_api::host::test_support::TestHostConfig;

use crate::Config;

/// Deterministically run queued memory-tree jobs until no immediately
/// claimable work remains. Intended for tests that need the async pipeline
/// to settle without spawning background tasks.
pub async fn drain_until_idle(config: &Config) -> Result<()> {
    loop {
        if !super::worker::run_once(config).await? {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "testing_tests.rs"]
mod tests;
