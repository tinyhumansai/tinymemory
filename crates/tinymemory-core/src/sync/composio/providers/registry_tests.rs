//! Tests for the surrounding module.

use super::*;
use crate::sync::composio::providers::{ProviderContext, ProviderUserProfile};
use async_trait::async_trait;

struct DummyProvider {
    slug: &'static str,
}

#[async_trait]
impl ComposioProvider for DummyProvider {
    fn toolkit_slug(&self) -> &'static str {
        self.slug
    }
    async fn fetch_user_profile(
        &self,
        _ctx: &ProviderContext,
    ) -> Result<ProviderUserProfile, String> {
        Ok(ProviderUserProfile::default())
    }
}

#[test]
fn register_and_lookup_roundtrip() {
    register_provider(Arc::new(DummyProvider {
        slug: "test_dummy_a",
    }));
    let p = get_provider("test_dummy_a").expect("provider should be registered");
    assert_eq!(p.toolkit_slug(), "test_dummy_a");
}

#[test]
fn lookup_unknown_returns_none() {
    assert!(get_provider("__definitely_not_a_real_toolkit__").is_none());
}

#[test]
fn register_replaces_existing() {
    register_provider(Arc::new(DummyProvider {
        slug: "test_dummy_b",
    }));
    register_provider(Arc::new(DummyProvider {
        slug: "test_dummy_b",
    }));
    // Still exactly one entry under that slug.
    let count_with_b = all_providers()
        .iter()
        .filter(|p| p.toolkit_slug() == "test_dummy_b")
        .count();
    assert_eq!(count_with_b, 1);
}

#[test]
fn empty_slug_is_rejected() {
    register_provider(Arc::new(DummyProvider { slug: "" }));
    assert!(get_provider("").is_none());
}
