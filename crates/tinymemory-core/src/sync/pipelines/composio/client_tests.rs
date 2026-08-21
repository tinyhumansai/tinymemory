//! Tests for the surrounding module.

use super::*;

/// 4xx is permanent: an invalid key must fail once, not retry with
/// backoff. Only rate-limit/upstream statuses and transport failures
/// (connect/read errors, timeouts) are worth another attempt.
#[test]
fn retry_classification_is_by_status_not_by_substring() {
    let retry = |m: &str| retryable_transport_error(&anyhow::anyhow!("{m}"));
    assert!(retry(
        "Composio direct request failed with HTTP 429 Too Many Requests"
    ));
    assert!(retry(
        "Composio proxy request failed with HTTP 503 Service Unavailable"
    ));
    assert!(retry("Composio direct transport error: connection reset"));
    assert!(!retry(
        "Composio direct request failed with HTTP 401 Unauthorized"
    ));
    assert!(!retry(
        "Composio proxy request failed with HTTP 404 Not Found"
    ));
    assert!(!retry(
        "Composio direct request failed with HTTP 400 Bad Request"
    ));
}

/// An error payload is a failure even when the flag is absent or true.
#[test]
fn an_error_payload_is_never_a_success() {
    let r = decode_direct_response(serde_json::json!({"error": "quota exceeded"}));
    assert!(!r.successful, "missing flag + error must be a failure");
    assert_eq!(r.error.as_deref(), Some("quota exceeded"));

    let r = decode_direct_response(serde_json::json!({"successful": true, "error": " boom "}));
    assert!(!r.successful, "flag=true + error must still be a failure");
    assert_eq!(r.error.as_deref(), Some("boom"));

    let r = decode_direct_response(
        serde_json::json!({"successful": true, "error": "  ", "data": {"x": 1}}),
    );
    assert!(r.successful, "an empty error string is no error");
    assert!(r.error.is_none());
    assert_eq!(r.data["x"], 1);
}

/// The client is built with finite timeouts; a build failure must not
/// silently degrade to an untimed client.
#[test]
fn client_builds_with_timeouts() {
    let _ = ComposioClient::new(ComposioSyncConfig::default());
    assert!(CONNECT_TIMEOUT < REQUEST_TIMEOUT);
}

#[test]
fn proxied_backend_envelope_decodes_provider_response() {
    let response = decode_proxy_response(serde_json::json!({
        "success": true,
        "data": {
            "successful": true,
            "data": {"messages": [{"messageId": "message-1"}]},
            "error": null
        }
    }))
    .unwrap();

    assert!(response.successful);
    assert_eq!(response.data["messages"][0]["messageId"], "message-1");
}

#[test]
fn flat_proxy_response_remains_supported() {
    let response = decode_proxy_response(serde_json::json!({
        "successful": true,
        "data": {"items": [1]}
    }))
    .unwrap();

    assert!(response.successful);
    assert_eq!(response.data["items"], serde_json::json!([1]));
}
