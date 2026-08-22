//! The contract suite against a real hosted service, not a double.
//!
//! Every other test in this crate runs an adapter over a double written from
//! the same documentation the adapter was. That agreement is worth having, but
//! it cannot catch a service that behaves differently from its documentation —
//! and when it does, the adapter and the double are wrong together and the
//! suite stays green. Issue #80 is exactly that: Supermemory strips two
//! characters from stored content server-side, and nothing here could see it
//! because nothing here ever spoke to Supermemory.
//!
//! So this target exists to be pointed at the real thing. It is skipped unless
//! the credentials are present, which keeps `cargo test` offline,
//! deterministic, and independent of a vendor's uptime by default.
//!
//! ```sh
//! TINYMEMORY_TEST_SUPERMEMORY_URL=https://api.supermemory.ai \
//! TINYMEMORY_TEST_SUPERMEMORY_KEY=sm_... \
//!   cargo test -p tinymemory-remote --test live_remote_engines
//! ```
//!
//! The suite writes and deletes records under its own namespaces in whatever
//! account the key belongs to. Point it at a scratch account rather than one
//! holding anything you would miss.

use std::sync::Arc;

use tinymemory_remote::{supermemory_provider, SupermemoryMemory};

/// Reads one engine's endpoint and key, or `None` when either is unset.
///
/// Both are required rather than defaulting the URL: a live test that invents
/// its own endpoint can end up silently exercising the wrong service.
fn credentials(engine: &str) -> Option<(String, String)> {
    let url = std::env::var(format!("TINYMEMORY_TEST_{engine}_URL")).ok()?;
    let key = std::env::var(format!("TINYMEMORY_TEST_{engine}_KEY")).ok()?;
    (!url.is_empty() && !key.is_empty()).then_some((url, key))
}

/// Runs the full provider contract against the live Supermemory API.
///
/// Skipped without `TINYMEMORY_TEST_SUPERMEMORY_URL` and `..._KEY`.
#[tokio::test]
async fn live_supermemory_upholds_the_provider_contract() -> anyhow::Result<()> {
    let Some((url, key)) = credentials("SUPERMEMORY") else {
        return Ok(());
    };
    let provider = supermemory_provider(SupermemoryMemory::api(&url, &key)?);
    tinymemory_conformance::assert_provider(Arc::new(provider)).await;
    Ok(())
}
