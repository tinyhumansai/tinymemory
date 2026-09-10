//! CortexDB full-provider response validation tests.

use serde_json::json;
use tinymemory_api::error::MemoryError;

use super::operations::{
    answer_text, cortex_role, event_identity, ingest_count, layer_limits, observed_at, receipt,
};

#[test]
fn receipt_requires_an_event_id_and_boolean_replay_flag() {
    assert!(matches!(
        receipt(&json!({"replayed_from_idempotency": false})),
        Err(MemoryError::Backend(_))
    ));
    assert!(matches!(
        receipt(&json!({"event_id": "evt-1"})),
        Err(MemoryError::Backend(_))
    ));
    assert!(matches!(
        receipt(&json!({"event_id": "evt-1", "replayed_from_idempotency": "false"})),
        Err(MemoryError::Backend(_))
    ));
    assert_eq!(
        receipt(&json!({"event_id": "evt-1", "replayed_from_idempotency": true})).ok(),
        Some(("evt-1".to_string(), true))
    );
}

#[test]
fn answer_layer_limits_never_exceed_the_contract_total() {
    for limit in 1..12 {
        let limits = layer_limits(limit);
        let total: u64 = limits
            .as_object()
            .into_iter()
            .flat_map(|values| values.values())
            .filter_map(serde_json::Value::as_u64)
            .sum();
        assert_eq!(total, limit as u64);
        assert_eq!(limits.as_object().map(serde_json::Map::len), Some(5));
    }
}

#[test]
fn learning_time_converts_to_rfc3339_and_rejects_invalid_values() {
    assert_eq!(
        observed_at(1_700_000_000.5).ok().as_deref(),
        Some("2023-11-14T22:13:20.500+00:00")
    );
    assert!(observed_at(f64::NAN).is_err());
    assert!(observed_at(f64::INFINITY).is_err());
}

#[test]
fn named_human_speakers_keep_the_user_message_class() {
    assert_eq!(cortex_role("alice"), "user");
    assert_eq!(cortex_role("assistant"), "assistant");
    assert_eq!(cortex_role("tool"), "tool");
    assert_eq!(cortex_role("system"), "system");
}

#[test]
fn ingest_counts_fail_instead_of_saturating() {
    assert_eq!(ingest_count(12).ok(), Some(12));
    if usize::BITS > u32::BITS {
        assert!(ingest_count(u32::MAX as usize + 1).is_err());
    }
}

#[test]
fn answer_text_rejects_missing_or_non_string_success_payloads() {
    assert!(answer_text(&json!({})).is_err());
    assert!(answer_text(&json!({"answer": null})).is_err());
    assert!(answer_text(&json!({"answer": 7})).is_err());
    assert_eq!(
        answer_text(&json!({"answer": "grounded"})).ok(),
        Some("grounded")
    );
}

#[test]
fn event_identity_cannot_collide_across_namespace_and_id_boundaries() {
    assert_ne!(event_identity("a:b", "c"), event_identity("a", "b:c"));
}
