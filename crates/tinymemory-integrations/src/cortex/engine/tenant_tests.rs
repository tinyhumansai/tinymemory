//! Tests for naming the proxied wire's tenant from a call's namespaces.

use super::*;
use crate::cortex::CortexCredential;

fn ns(path: &str) -> Namespace {
    path.parse().unwrap()
}

fn proxied() -> CortexEngine {
    CortexEngine::proxied(
        "https://memory.example.test",
        CortexCredential::api_key("service"),
    )
    .unwrap()
}

#[test]
fn the_tenant_is_the_leading_user_segment() {
    assert_eq!(tenant_of(&ns("user:p0123")), Some("p0123"));
    assert_eq!(tenant_of(&ns("user:alice/agent:a/ws:main")), Some("alice"));
    assert_eq!(tenant_of(&ns("team:acme/user:alice")), None);
    assert_eq!(tenant_of(&Namespace::ROOT), None);
}

#[test]
fn off_the_proxied_wire_a_call_is_unchanged() {
    let direct = CortexEngine::direct(
        "https://cortex.example.test",
        CortexCredential::api_key("key"),
    )
    .unwrap();
    assert!(matches!(direct.for_call([]), Ok(Cow::Borrowed(_))));
    assert!(matches!(
        direct.for_call([&Namespace::ROOT]),
        Ok(Cow::Borrowed(_))
    ));
}

#[test]
fn a_proxied_call_names_its_one_tenant() {
    let engine = proxied();
    let a = ns("user:alice");
    let b = ns("user:alice/agent:helper");
    let scoped = engine.for_call([&a, &b]).unwrap();
    assert!(matches!(scoped, Cow::Owned(_)));
}

#[test]
fn a_proxied_call_without_a_tenant_is_a_config_error() {
    let engine = proxied();
    let cases: [Vec<Namespace>; 4] = [
        vec![],
        vec![Namespace::ROOT],
        vec![ns("ws:main/user:alice")],
        vec![ns("user:alice"), ns("user:bob")],
    ];
    for namespaces in cases {
        let error = engine.for_call(namespaces.iter()).unwrap_err();
        assert!(
            matches!(error, Error::Config(_)),
            "{namespaces:?}: {error:?}"
        );
    }
}

#[test]
fn a_tenant_id_over_96_characters_is_a_config_error() {
    let engine = proxied();
    let fits = ns(&format!("user:{}", "a".repeat(96)));
    assert!(engine.for_call([&fits]).is_ok());
    let long = ns(&format!("user:{}", "a".repeat(97)));
    let error = engine.for_call([&long]).unwrap_err();
    assert!(matches!(error, Error::Config(_)), "{error:?}");
}
