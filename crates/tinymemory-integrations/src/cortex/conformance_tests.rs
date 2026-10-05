//! The shared conformance suite, run against both wires through the doubles,
//! and the isolation check between tenants sharing one direct key.

use crate::cortex::CortexTenancy;
use crate::cortex::testing::{
    direct_double, direct_engine, hosted_double, hosted_engine, tenant_engine,
};

#[tokio::test]
async fn the_direct_wire_upholds_the_contract() {
    let (endpoint, _state) = direct_double().await;
    tinymemory_api::conformance::run(&direct_engine(&endpoint))
        .await
        .unwrap();
}

#[tokio::test]
async fn the_tinyhumans_wire_upholds_the_contract() {
    let (endpoint, _state) = hosted_double().await;
    tinymemory_api::conformance::run(&hosted_engine(&endpoint))
        .await
        .unwrap();
}

#[tokio::test]
async fn two_tenants_pinned_on_one_key_are_isolated() {
    let (endpoint, _state) = direct_double().await;
    let alice = tenant_engine(
        &endpoint,
        CortexTenancy::pinned("org:acme/user:alice").unwrap(),
    );
    let bob = tenant_engine(
        &endpoint,
        CortexTenancy::pinned("org:acme/user:bob").unwrap(),
    );
    tinymemory_api::conformance::run_isolation(&alice, &bob)
        .await
        .unwrap();
    tinymemory_api::conformance::run_isolation(&bob, &alice)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_tenant_pinned_under_another_is_isolated_from_it() {
    let (endpoint, _state) = direct_double().await;
    let org = tenant_engine(&endpoint, CortexTenancy::pinned("org:acme").unwrap());
    let alice = tenant_engine(
        &endpoint,
        CortexTenancy::pinned("org:acme/user:alice").unwrap(),
    );
    tinymemory_api::conformance::run_isolation(&org, &alice)
        .await
        .unwrap();
    tinymemory_api::conformance::run_isolation(&alice, &org)
        .await
        .unwrap();
}

#[tokio::test]
async fn two_single_user_engines_on_one_key_share_everything() {
    // The configuration the tenancy declaration exists to make deliberate:
    // `single_user` asserts the key is one person's, so two people on it
    // share one tree, and the check must say so.
    let (endpoint, _state) = direct_double().await;
    let error = tinymemory_api::conformance::run_isolation(
        &direct_engine(&endpoint),
        &direct_engine(&endpoint),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            &error,
            tinymemory_api::conformance::Error::Check {
                check: "isolation",
                ..
            }
        ),
        "{error:?}"
    );
}
