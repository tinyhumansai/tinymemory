//! Tests for the surrounding module.

use super::*;

/// A recall with no minimum score must omit `threshold`, not send null.
/// The hosted platform types it as a number in 0..=1 and answers 400 to
/// an explicit null — store and list succeeded while recall failed.
#[test]
fn an_unset_min_score_omits_the_threshold_field() {
    let body = Mem0Dialect::search_body("q", 10, json!({"user_id": "ns"}), None);
    assert!(
        body.get("threshold").is_none(),
        "threshold must be absent, not null: {body}"
    );
    assert_eq!(body["top_k"], 10);
    assert_eq!(body["query"], "q");
}

#[test]
fn a_set_min_score_is_sent() {
    let body = Mem0Dialect::search_body("q", 10, json!({"user_id": "ns"}), Some(0.25));
    assert_eq!(body["threshold"], 0.25);
}

/// `top_k` outside the documented 1..=1000 is a validation error, so a
/// caller's limit is clamped rather than forwarded into a 400.
#[test]
fn top_k_is_clamped_to_the_documented_range() {
    assert_eq!(
        Mem0Dialect::search_body("q", 0, json!({}), None)["top_k"],
        1
    );
    assert_eq!(
        Mem0Dialect::search_body("q", 5000, json!({}), None)["top_k"],
        1000
    );
}
