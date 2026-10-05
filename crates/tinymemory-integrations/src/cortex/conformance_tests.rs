//! The shared conformance suite, run against both wires through the doubles.

use crate::cortex::testing::{direct_double, direct_engine, hosted_double, hosted_engine};

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
