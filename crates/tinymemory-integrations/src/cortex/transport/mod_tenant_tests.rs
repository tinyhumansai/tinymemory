//! Tests for the proxied wire's tenant header and the private-network
//! cleartext opt-in.

use super::*;

fn proxied(endpoint: &str) -> Result<HttpClient> {
    HttpClient::new(
        CortexWire::Proxied,
        endpoint,
        CortexCredential::api_key("service"),
    )
}

#[test]
fn tenant_ids_are_short_url_safe_words() {
    for valid in ["p0123456789abcdef", "a", "user_1-x", &"z".repeat(96)] {
        assert!(valid_tenant_id(valid), "{valid}");
    }
    for invalid in ["", "a/b", "a:b", "a b", "é", &"z".repeat(97)] {
        assert!(!valid_tenant_id(invalid), "{invalid}");
    }
    let mut client = proxied("https://memory.example.test").unwrap();
    let error = client.set_tenant("a/b").unwrap_err();
    assert!(matches!(error, Error::Config(_)), "{error:?}");
}

#[tokio::test]
async fn a_proxied_request_without_a_tenant_is_never_built() {
    let client = proxied("https://memory.example.test").unwrap();
    let error = client.request(Method::GET, "v1/events").await.unwrap_err();
    assert!(matches!(error, Error::Config(_)), "{error:?}");
}

#[tokio::test]
async fn a_proxied_request_carries_its_tenant_and_no_other_wire_does() {
    let mut client = proxied("https://memory.example.test").unwrap();
    client.set_tenant("p42").unwrap();
    let request = client
        .request(Method::GET, "v1/events")
        .await
        .unwrap()
        .build()
        .unwrap();
    assert_eq!(request.headers()[TENANT_HEADER], "p42");

    let mut direct = HttpClient::new(
        CortexWire::Direct,
        "https://cortex.example.test",
        CortexCredential::api_key("key"),
    )
    .unwrap();
    direct.set_tenant("p42").unwrap();
    let request = direct
        .request(Method::GET, "v1/events")
        .await
        .unwrap()
        .build()
        .unwrap();
    assert!(request.headers().get(TENANT_HEADER).is_none());
}

#[test]
fn cluster_service_hosts_are_recognised() {
    for host in [
        "http://memory-api.memory.svc.cluster.local",
        "http://memory-api.memory.svc.cluster.local.:80",
        "http://memory-api.memory.svc",
        "http://Memory-API.Memory.SVC.Cluster.Local/base",
    ] {
        assert!(is_cluster_service(&Url::parse(host).unwrap()), "{host}");
    }
    for host in [
        "http://svc",
        "http://.svc",
        "http://memory.svc.example.com",
        "http://svc.cluster.local",
        "http://memory.example.com",
        "http://10.0.0.7",
    ] {
        assert!(!is_cluster_service(&Url::parse(host).unwrap()), "{host}");
    }
}

#[tokio::test]
async fn cluster_service_cleartext_is_sent_only_after_the_opt_in() {
    let mut client = proxied("http://memory-api.memory.svc.cluster.local").unwrap();
    client.set_tenant("p42").unwrap();
    let error = client.request(Method::GET, "v1/events").await.unwrap_err();
    assert!(matches!(error, Error::Config(_)), "{error:?}");
    client.allow_private_network_cleartext();
    let request = client
        .request(Method::GET, "v1/events")
        .await
        .unwrap()
        .build()
        .unwrap();
    assert_eq!(
        request.url().as_str(),
        "http://memory-api.memory.svc.cluster.local/v1/events"
    );
}

#[test]
fn public_cleartext_is_still_refused_when_built() {
    for refused in ["http://memory.example.com", "http://memory.svc.example.com"] {
        assert!(
            matches!(proxied(refused), Err(Error::Config(_))),
            "{refused}"
        );
    }
}

#[test]
fn the_tenant_header_is_reserved() {
    let error = default_headers([("x-memory-tenant", "everyone")]).unwrap_err();
    assert!(matches!(error, Error::Config(_)), "{error:?}");
}
