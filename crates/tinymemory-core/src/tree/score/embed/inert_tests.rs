//! Tests for the surrounding module.

use super::*;

#[tokio::test]
async fn returns_768_zero_vector() {
    let e = InertEmbedder::new();
    let v = e.embed("anything").await.unwrap();
    assert_eq!(v.len(), EMBEDDING_DIM);
    assert!(v.iter().all(|f| *f == 0.0));
}

#[tokio::test]
async fn name_is_inert() {
    assert_eq!(InertEmbedder::new().name(), "inert");
}

#[tokio::test]
async fn empty_input_still_returns_full_vector() {
    let v = InertEmbedder::new().embed("").await.unwrap();
    assert_eq!(v.len(), EMBEDDING_DIM);
}
