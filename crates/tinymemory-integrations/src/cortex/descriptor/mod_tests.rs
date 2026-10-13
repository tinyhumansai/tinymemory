//! Tests for the descriptors and route table.

use super::*;

#[test]
fn the_direct_descriptor_needs_a_key_and_defaults_its_endpoint() {
    let d = cortexdb_descriptor();
    assert_eq!(d.id, "cortexdb");
    assert!(!d.hosted);
    assert!(!d.needs_endpoint);
    assert!(d.needs_key);
    assert_eq!(d.default_endpoint, Some("https://api-v1.cortexdb.ai"));
    assert_eq!(d.fetch_modes, vec![FetchMode::Hybrid]);
}

#[test]
fn the_hosted_descriptor_is_hosted_and_hybrid_only() {
    let d = tinyhumans_descriptor();
    assert_eq!(d.id, "tinyhumans");
    assert!(d.hosted);
    assert!(!d.needs_endpoint);
    assert!(d.needs_key);
    assert_eq!(d.default_endpoint, Some("https://api.tinyhumans.ai"));
    assert!(d.supports(FetchMode::Hybrid));
    assert!(!d.supports(FetchMode::Keyword));
    assert!(!d.supports(FetchMode::Vector));
}

#[test]
fn every_hosted_route_is_under_memory_and_every_direct_one_under_v1() {
    let routes = [
        Route::Experience,
        Route::Bulk,
        Route::Events,
        Route::Recall,
        Route::Forget,
        Route::Answer,
        Route::Health,
        Route::Scopes,
    ];
    for route in routes {
        assert!(CortexWire::Direct.path(route).starts_with("v1/"));
        assert!(CortexWire::TinyHumans.path(route).starts_with("memory/"));
    }
    assert_eq!(CortexWire::TinyHumans.descriptor().id, TINYHUMANS_ENGINE_ID);
    assert_eq!(CortexWire::Direct.descriptor().id, CORTEXDB_ENGINE_ID);
}

#[test]
fn only_the_managed_api_consolidates_on_its_own_by_default() {
    assert_eq!(
        cortexdb_descriptor().consolidation,
        Consolidation::Automatic,
        "the direct descriptor describes its default endpoint, the managed API"
    );
    assert_eq!(
        tinyhumans_descriptor().consolidation,
        Consolidation::Scheduled
    );
    assert_eq!(
        direct_consolidation("https://api-v1.cortexdb.ai"),
        Consolidation::Automatic
    );
    for self_hosted in [
        "http://127.0.0.1:3141",
        "https://cortex.example.test",
        "https://api-v1.cortexdb.ai:8443",
        "http://api-v1.cortexdb.ai",
    ] {
        assert_eq!(
            direct_consolidation(self_hosted),
            Consolidation::OnDemand,
            "{self_hosted}"
        );
    }
}

#[test]
fn the_proxied_descriptor_needs_an_endpoint_and_a_key() {
    let d = proxied_descriptor();
    assert_eq!(d.id, PROXIED_ENGINE_ID);
    assert_eq!(d.id, "cortexdb-proxied");
    assert!(!d.hosted);
    assert!(d.needs_endpoint);
    assert!(d.needs_key);
    assert_eq!(d.default_endpoint, None);
    assert_eq!(d.fetch_modes, vec![FetchMode::Hybrid]);
    assert_eq!(d.consolidation, Consolidation::OnDemand);
    assert_eq!(CortexWire::Proxied.descriptor().id, PROXIED_ENGINE_ID);
}

#[test]
fn the_proxied_route_table_lists_only_its_routes() {
    let expected = [
        (Route::Experience, "v1/experience"),
        (Route::Bulk, "v1/experience/bulk"),
        (Route::Events, "v1/events"),
        (Route::Recall, "v1/recall"),
        (Route::Forget, "v1/forget"),
        (Route::Answer, "v1/answer"),
        (Route::Health, "v1/admin/health"),
        (Route::Scopes, "v1/scopes/list"),
        (Route::BuildBeliefs, "v1/beliefs/build"),
        (Route::Beliefs, "v1/beliefs"),
        // A scope is erased by forgetting its events.
        (Route::Erasures, "v1/forget"),
        (Route::EraseAll, "v1/tenant/purge"),
    ];
    for (route, path) in expected {
        assert_eq!(CortexWire::Proxied.path(route), path, "{route:?}");
    }
    assert_ne!(CortexWire::Proxied.path(Route::Erasures), "v1/erasures");
}
