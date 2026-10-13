//! The proxied wire against a cortexdb-saas proxy double: each call names
//! its tenant from its namespace, a call without one sends nothing, erasing
//! a tenant purges it, and only the proxy's allowlisted routes are used.

use super::*;
use crate::cortex::testing::{ProxyDouble, proxied_double, proxied_engine, sample_items};
use tinymemory_api::{FetchMode, ItemKind, MetaFilter, Namespace};

/// `item`, placed at `namespace`.
fn at(mut item: StoreItem, namespace: &str) -> StoreItem {
    item.meta_mut().namespace = namespace.parse().unwrap();
    item
}

/// A filter reaching `namespace` and everything below it.
fn below(namespace: &str) -> MetaFilter {
    MetaFilter {
        reach: Some(Reach::subtree(namespace.parse().unwrap())),
        ..MetaFilter::default()
    }
}

fn learning(namespace: &str) -> StoreItem {
    at(sample_items().remove(2), namespace)
}

/// Every request either named `tenant` or was the given one.
fn all_named(proxy: &ProxyDouble, tenant: &str) {
    for (request, named) in proxy.requests() {
        assert_eq!(named.as_deref(), Some(tenant), "{request}");
    }
}

#[tokio::test]
async fn every_request_names_the_tenant_of_its_namespace() {
    let (endpoint, proxy) = proxied_double().await;
    let engine = proxied_engine(&endpoint);
    engine
        .store(learning("user:alice/agent:helper"))
        .await
        .unwrap();
    assert!(!proxy.requests().is_empty());
    all_named(&proxy, "alice");
    let alice = proxy.tenant("alice").unwrap();
    let scopes: Vec<String> = alice
        .log
        .lock()
        .unwrap()
        .events
        .iter()
        .map(|e| e["scope"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        scopes,
        vec!["user:alice/agent:helper/app:learnings"],
        "relative to the tenant root the proxy pins: no root is sent"
    );
}

#[tokio::test]
async fn two_namespaces_on_one_engine_are_two_tenants() {
    let (endpoint, proxy) = proxied_double().await;
    let engine = proxied_engine(&endpoint);
    engine.store(learning("user:alice")).await.unwrap();
    engine.store(learning("user:bob/ws:main")).await.unwrap();
    assert_eq!(
        proxy.tenants_named().into_iter().collect::<Vec<_>>(),
        vec!["alice".to_string(), "bob".to_string()]
    );
    assert_eq!(proxy.tenant("alice").unwrap().event_count(), 1);
    assert_eq!(proxy.tenant("bob").unwrap().event_count(), 1);

    let alice = engine
        .list(ListRequest::new(below("user:alice"), 10))
        .await
        .unwrap();
    assert_eq!(alice.items.len(), 1);
    let last = proxy.requests().last().cloned().unwrap();
    assert_eq!(last.1.as_deref(), Some("alice"), "{last:?}");

    let mut fetch = FetchRequest::new("helix", FetchMode::Hybrid, 5);
    fetch.filter = below("user:bob");
    engine.fetch(fetch).await.unwrap();
    let last = proxy.requests().last().cloned().unwrap();
    assert_eq!(last.1.as_deref(), Some("bob"), "{last:?}");
}

#[tokio::test]
async fn a_call_without_a_leading_user_segment_sends_nothing() {
    let (endpoint, proxy) = proxied_double().await;
    let engine = proxied_engine(&endpoint);
    for namespace in ["", "ws:main", "team:acme/user:alice"] {
        let error = engine.store(learning(namespace)).await.unwrap_err();
        assert!(matches!(error, Error::Config(_)), "{namespace}: {error:?}");
    }
    let error = engine
        .list(ListRequest::new(MetaFilter::default(), 10))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Config(_)), "no reach: {error:?}");
    assert!(error.to_string().contains("names none"), "{error}");
    let error = engine
        .forget(ForgetTarget::Ids(vec!["0".repeat(40).into()]))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Config(_)), "bare ids: {error:?}");
    assert!(proxy.requests().is_empty(), "{:?}", proxy.requests());
}

#[tokio::test]
async fn a_batch_naming_two_tenants_is_refused() {
    let (endpoint, proxy) = proxied_double().await;
    let error = proxied_engine(&endpoint)
        .store_many(vec![learning("user:alice"), learning("user:bob")])
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Config(_)), "{error:?}");
    assert!(error.to_string().contains("two memory tenants"), "{error}");
    assert!(proxy.requests().is_empty());
}

#[tokio::test]
async fn a_tenant_id_longer_than_the_proxy_takes_is_refused() {
    let (endpoint, proxy) = proxied_double().await;
    let long = "a".repeat(97);
    let error = proxied_engine(&endpoint)
        .store(learning(&format!("user:{long}")))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Config(_)), "{error:?}");
    assert!(proxy.requests().is_empty());
}

#[tokio::test]
async fn erasing_a_tenants_whole_subtree_purges_the_tenant() {
    let (endpoint, proxy) = proxied_double().await;
    let engine = proxied_engine(&endpoint);
    engine
        .store(learning("user:alice/agent:helper"))
        .await
        .unwrap();
    engine.store(learning("user:bob")).await.unwrap();
    let report = engine
        .erase(EraseRequest::new(Reach::subtree(
            "user:alice".parse().unwrap(),
        )))
        .await
        .unwrap();
    assert_eq!(report.erased_scopes, 1);
    assert_eq!(*proxy.purged.lock().unwrap(), vec!["alice".to_string()]);
    let purge = proxy.requests().last().cloned().unwrap();
    assert_eq!(
        purge,
        (
            "POST /v1/tenant/purge".to_string(),
            Some("alice".to_string())
        )
    );
    assert!(proxy.tenant("alice").is_none());
    assert_eq!(proxy.tenant("bob").unwrap().event_count(), 1, "bob is kept");
}

#[tokio::test]
async fn a_narrower_erasure_forgets_each_scopes_events() {
    let (endpoint, proxy) = proxied_double().await;
    let engine = proxied_engine(&endpoint);
    for item in sample_items() {
        engine.store(at(item, "user:alice")).await.unwrap();
    }
    let alice = proxy.tenant("alice").unwrap();
    assert_eq!(alice.event_count(), 5);
    let mut request = EraseRequest::new(Reach::subtree("user:alice".parse().unwrap()));
    request.kinds = vec![ItemKind::Conversation];
    let report = engine.erase(request).await.unwrap();
    assert_eq!(report.erased_scopes, 1);
    assert!(report.receipts.is_empty());
    assert_eq!(alice.event_count(), 2, "the three turns are gone");
    let requests = proxy.requests();
    assert!(
        requests.iter().any(|(r, _)| r == "POST /v1/forget"),
        "{requests:?}"
    );
    assert!(proxy.purged.lock().unwrap().is_empty());
    all_named(&proxy, "alice");
}

#[tokio::test]
async fn erasing_the_whole_tree_names_no_tenant_and_is_refused() {
    let (endpoint, proxy) = proxied_double().await;
    let mut request = EraseRequest::new(Reach::subtree(Namespace::ROOT));
    request.whole_tree = true;
    let error = proxied_engine(&endpoint)
        .erase(request)
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Config(_)), "{error:?}");
    assert!(proxy.requests().is_empty());
}

#[tokio::test]
async fn only_allowlisted_routes_are_used() {
    let (endpoint, proxy) = proxied_double().await;
    let engine = proxied_engine(&endpoint);
    for item in sample_items() {
        engine.store(at(item, "user:alice")).await.unwrap();
    }
    let mut recall = RecallRequest::new("which editor", 5);
    recall.filter = below("user:alice");
    engine.recall(recall).await.unwrap();
    engine
        .consolidate(ConsolidateRequest::new(Reach::subtree(
            "user:alice".parse().unwrap(),
        )))
        .await
        .unwrap();
    for (request, _) in proxy.requests() {
        for refused in [
            "/v1/erasures",
            "/v1/scopes?",
            "/v1/scopes/members",
            "/v1/admin/version",
            "/v1/auth/whoami",
            "/v1/export",
        ] {
            assert!(!request.contains(refused), "{request}");
        }
    }
    all_named(&proxy, "alice");
}

#[tokio::test]
async fn health_probes_as_a_tenant_nothing_is_written_under() {
    let (endpoint, proxy) = proxied_double().await;
    assert_eq!(proxied_engine(&endpoint).health().await, EngineHealth::Ok);
    assert_eq!(
        proxy.requests(),
        vec![(
            "GET /v1/admin/health".to_string(),
            Some("tinymemory-health".to_string())
        )]
    );
}

#[tokio::test]
async fn the_proxied_wire_is_in_the_tenant_layout_and_refuses_a_scope_root() {
    let engine = CortexEngine::proxied(
        "https://memory.example.test",
        CortexCredential::api_key("service"),
    )
    .unwrap();
    assert_eq!(engine.wire(), CortexWire::Proxied);
    assert_eq!(engine.descriptor().id, "cortexdb-proxied");
    assert_eq!(engine.layout, ScopeLayout::tenant());
    let error = engine
        .clone()
        .with_scope_root("org:acme", None)
        .unwrap_err();
    assert!(matches!(error, Error::Config(_)), "{error:?}");
    let engine = engine.with_tenant_root().unwrap();
    assert_eq!(engine.layout, ScopeLayout::tenant());
    assert!(
        engine
            .clone()
            .with_consolidation(Consolidation::Automatic)
            .is_ok()
    );
    assert!(engine.with_consolidation(Consolidation::Scheduled).is_err());
}

#[tokio::test]
async fn the_tenant_header_cannot_be_fixed_as_a_default_header() {
    let error = CortexEngine::proxied(
        "https://memory.example.test",
        CortexCredential::api_key("service"),
    )
    .unwrap()
    .with_default_headers([("X-Memory-Tenant", "everyone")])
    .unwrap_err();
    assert!(matches!(error, Error::Config(_)), "{error:?}");
}

#[tokio::test]
async fn a_cluster_service_endpoint_needs_the_cleartext_opt_in() {
    let endpoint = "http://memory-api.memory.svc.cluster.local";
    let engine =
        CortexEngine::proxied(endpoint, CortexCredential::api_key("service")).unwrap();
    let error = engine.store(learning("user:alice")).await.unwrap_err();
    assert!(matches!(error, Error::Config(_)), "{error:?}");
    assert!(
        error.to_string().contains("with_private_network_cleartext"),
        "{error}"
    );
    for public in ["http://memory.example.com", "http://10.0.0.7"] {
        let error = CortexEngine::proxied(public, CortexCredential::api_key("service"))
            .unwrap_err();
        assert!(matches!(error, Error::Config(_)), "{public}: {error:?}");
    }
}
