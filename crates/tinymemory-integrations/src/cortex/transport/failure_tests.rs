//! Tests for transport classification, status mapping and the hosted
//! envelope.

use super::*;
use crate::cortex::error::{error_code, is_plan_required};

#[test]
fn a_rustls_handshake_abort_is_named_tls_not_connect() {
    // reqwest sets is_connect for this, and the text never says "TLS".
    let class = classify_transport(
        false,
        true,
        "client error (Connect): received fatal alert: InternalError",
    );
    assert_eq!(class, TransportClass::Tls);
    assert!(class.describe().starts_with("TLS failed"));
}

#[test]
fn a_dns_failure_is_named_dns_not_connect() {
    let class = classify_transport(
        false,
        true,
        "client error (Connect): dns error: failed to lookup address information",
    );
    assert_eq!(class, TransportClass::Dns);
}

#[test]
fn a_refused_connection_is_connect_and_a_timeout_wins_over_every_hint() {
    assert_eq!(
        classify_transport(false, true, "tcp connect error: Connection refused"),
        TransportClass::Connect
    );
    assert_eq!(
        classify_transport(true, true, "dns error tls certificate"),
        TransportClass::Timeout
    );
    assert_eq!(
        classify_transport(false, false, "body error: incomplete message"),
        TransportClass::Other
    );
}

#[test]
fn direct_statuses_map_onto_the_contract() {
    let cases = [
        (401, "unauthorized"),
        (403, "unauthorized"),
        (404, "not found"),
        (400, "invalid request"),
        (413, "invalid request"),
        (422, "invalid request"),
        (409, "conflict"),
        (408, "unavailable"),
        (429, "unavailable"),
        (500, "unavailable"),
        (502, "unavailable"),
        (503, "unavailable"),
        (504, "unavailable"),
        (402, "engine error"),
        (418, "engine error"),
    ];
    for (code, prefix) in cases {
        let error = direct_status_error(
            "db.example",
            "v1/events",
            StatusCode::from_u16(code).unwrap(),
            "body",
        );
        assert!(error.to_string().starts_with(prefix), "{code}: {error}");
        assert_eq!(error_code(&error), None, "direct failures carry no code");
    }
}

#[test]
fn a_hosted_failure_carries_its_code_and_402_is_insufficient_credits() {
    let body = r#"{"success":false,"error":"top up","errorCode":"USER_INSUFFICIENT_CREDITS"}"#;
    let error = hosted_status_error(
        "api.example",
        "memory/experience",
        StatusCode::PAYMENT_REQUIRED,
        body,
    );
    assert!(matches!(error, Error::InsufficientCredits(_)), "{error:?}");
    assert_eq!(error_code(&error), Some("USER_INSUFFICIENT_CREDITS"));

    let unauthorized = hosted_status_error("h", "memory/events", StatusCode::UNAUTHORIZED, "{}");
    assert!(matches!(unauthorized, Error::Unauthorized(_)));
    assert_eq!(error_code(&unauthorized), Some("UNAUTHORIZED"));

    let conflict = hosted_status_error(
        "h",
        "memory/experience",
        StatusCode::CONFLICT,
        r#"{"errorCode":"CONFLICT","error":"claimed"}"#,
    );
    assert!(matches!(conflict, Error::Conflict(_)));
    assert_eq!(error_code(&conflict), Some("CONFLICT"));

    let limited = hosted_status_error(
        "h",
        "memory/events",
        StatusCode::TOO_MANY_REQUESTS,
        "not json",
    );
    assert!(matches!(limited, Error::Unavailable(_)));
    assert_eq!(error_code(&limited), Some("RATE_LIMITED"));
}

#[test]
fn an_uncoded_hosted_402_is_not_assumed_to_mean_credit_exhaustion() {
    for body in ["{}", r#"{"error":"payment required"}"#] {
        let error = hosted_status_error(
            "api.example",
            "memory/experience",
            StatusCode::PAYMENT_REQUIRED,
            body,
        );
        assert!(matches!(error, Error::Engine(_)), "{error:?}");
        assert_eq!(error_code(&error), Some("HTTP_402"));
        assert!(
            !error.to_string().contains("insufficient credits"),
            "an uncoded payment-required response must not claim credit exhaustion: {error}"
        );
    }
}

#[test]
fn a_hosted_400_with_the_conflict_code_is_a_conflict() {
    // The backend's own envelope for memory-api's 409 (`memoryUpstreamError`).
    let body =
        r#"{"success":false,"error":"idempotency key already claimed","errorCode":"CONFLICT"}"#;
    let error = hosted_status_error("h", "memory/experience", StatusCode::BAD_REQUEST, body);
    assert!(matches!(error, Error::Conflict(_)), "{error:?}");
    assert_eq!(error_code(&error), Some("CONFLICT"));

    let other = hosted_status_error(
        "h",
        "memory/experience",
        StatusCode::BAD_REQUEST,
        r#"{"success":false,"error":"bad scope","errorCode":"BAD_REQUEST"}"#,
    );
    assert!(matches!(other, Error::InvalidRequest(_)), "{other:?}");

    // Only the hosted classifier reads the code: CortexDB's own 400 is not a conflict.
    let direct = direct_status_error(
        "h",
        "v1/experience",
        StatusCode::BAD_REQUEST,
        r#"{"error_code":"CONFLICT"}"#,
    );
    assert!(matches!(direct, Error::InvalidRequest(_)), "{direct:?}");
}

#[test]
fn a_hosted_409_with_the_conflict_code_is_a_conflict() {
    // The backend after tinyhumansai/backend#1409 relays the refusal as 409.
    let body =
        r#"{"success":false,"error":"idempotency key already claimed","errorCode":"CONFLICT"}"#;
    let error = hosted_status_error("h", "memory/experience", StatusCode::CONFLICT, body);
    assert!(matches!(error, Error::Conflict(_)), "{error:?}");
    assert_eq!(error_code(&error), Some("CONFLICT"));
    // The same through the envelope unwrap a failing response takes.
    let unwrapped = unwrap_envelope(
        "h",
        "memory/experience",
        StatusCode::CONFLICT,
        body.as_bytes(),
    );
    assert!(
        matches!(unwrapped, Err(Error::Conflict(_))),
        "{unwrapped:?}"
    );
}

#[test]
fn the_delete_memory_envelope_is_unwrapped_to_its_data() {
    let body = br#"{"success":true,"data":{"erased":true,"scopes":4}}"#;
    let data = unwrap_envelope("h", "memory", StatusCode::OK, body).unwrap();
    assert_eq!(data["erased"], true);
    assert_eq!(data["scopes"], 4);
    let refused = hosted_status_error(
        "h",
        "memory",
        StatusCode::UNAUTHORIZED,
        r#"{"success":false,"error":"no key","errorCode":"UNAUTHORIZED"}"#,
    );
    assert_eq!(error_code(&refused), Some("UNAUTHORIZED"));
}

#[test]
fn a_hostile_error_code_cannot_break_the_prefix() {
    let error = hosted_status_error(
        "h",
        "memory/events",
        StatusCode::BAD_REQUEST,
        r#"{"errorCode":"bad] code\n","error":"x"}"#,
    );
    assert_eq!(error_code(&error), Some("BADCODE"));
}

#[test]
fn the_envelope_is_unwrapped_and_its_absence_is_an_engine_error() {
    let ok = unwrap_envelope(
        "h",
        "memory/events",
        StatusCode::OK,
        br#"{"success":true,"data":{"a":1}}"#,
    );
    assert_eq!(ok.unwrap(), serde_json::json!({ "a": 1 }));
    for body in [
        br#"{"success":true}"#.as_slice(),
        br#"{"items":[]}"#.as_slice(),
        b"not json".as_slice(),
    ] {
        assert!(matches!(
            unwrap_envelope("h", "memory/events", StatusCode::OK, body),
            Err(Error::Engine(_))
        ));
    }
    let refused = unwrap_envelope(
        "h",
        "memory/events",
        StatusCode::OK,
        br#"{"success":false,"error":"no","errorCode":"VALIDATION_ERROR"}"#,
    );
    assert_eq!(
        refused.as_ref().err().and_then(error_code),
        Some("VALIDATION_ERROR")
    );
}

#[test]
fn a_health_reason_withholds_the_backend_text() {
    let error = direct_status_error(
        "db.example",
        "v1/admin/health",
        StatusCode::UNAUTHORIZED,
        r#"{"detail":"bad key sk-SECRET123"}"#,
    );
    let reason = health_reason(&error);
    assert!(reason.contains("credential"), "{reason}");
    assert!(!reason.contains("sk-SECRET123"), "{reason}");
}

#[test]
fn a_long_backend_message_is_cut() {
    let error = direct_status_error("h", "v1/x", StatusCode::BAD_REQUEST, &"x".repeat(5000));
    assert!(error.to_string().len() < 600);
    assert!(error.to_string().ends_with('…'));
}

#[test]
fn a_write_the_indexer_has_not_reached_is_transient() {
    // CortexDB 0.10.4's answer to `v1/experience?wait=indexed` when the
    // indexer lags: the event is durable, and a resend replays.
    let body = r#"{"error_code":"WAIT_TIMEOUT","message":"wait=indexed timed out after 30s: the indexer has not reached this event yet. The event is captured and durable and processing continues; poll status_url."}"#;
    let error = direct_status_error(
        "db.example",
        "v1/experience",
        StatusCode::REQUEST_TIMEOUT,
        body,
    );
    assert!(matches!(error, Error::Unavailable(_)), "{error:?}");
    assert!(error.is_transient());

    let hosted = hosted_status_error("h", "memory/experience", StatusCode::REQUEST_TIMEOUT, body);
    assert!(matches!(hosted, Error::Unavailable(_)), "{hosted:?}");
}

#[test]
fn an_unusable_error_code_falls_back_to_the_other_key() {
    for body in [
        r#"{"errorCode":null,"error_code":"INVALID_BODY"}"#,
        r#"{"errorCode":7,"error_code":"INVALID_BODY"}"#,
        r#"{"errorCode":"-- ","error_code":"INVALID_BODY"}"#,
    ] {
        let error = hosted_status_error("h", "memory/x", StatusCode::BAD_REQUEST, body);
        assert_eq!(error_code(&error), Some("INVALID_BODY"), "{body}");
    }
    // The primary key still wins when it is usable.
    let error = hosted_status_error(
        "h",
        "memory/x",
        StatusCode::BAD_REQUEST,
        r#"{"errorCode":"FIRST","error_code":"SECOND"}"#,
    );
    assert_eq!(error_code(&error), Some("FIRST"));
}

/// tinyhumansai/openhuman#8081: a plan refusal and a plain 403 must not read
/// as an expired session, which a host signs the user out on.
#[test]
fn only_a_hosted_401_says_the_session_expired() {
    let body = r#"{"success":false,"error":"memory is not included in this account's plan","errorCode":"MEMORY_PLAN_REQUIRED"}"#;
    let plan = hosted_status_error("api.example", "memory/scopes", StatusCode::FORBIDDEN, body);
    assert!(matches!(plan, Error::Unauthorized(_)), "{plan:?}");
    assert!(is_plan_required(&plan));
    assert_eq!(error_code(&plan), Some("MEMORY_PLAN_REQUIRED"));
    assert!(
        plan.to_string()
            .contains("not included in this account's plan")
    );
    assert!(!plan.to_string().contains("session expired"), "{plan}");

    let refused = hosted_status_error("h", "memory/scopes", StatusCode::FORBIDDEN, "{}");
    assert!(matches!(refused, Error::Unauthorized(_)), "{refused:?}");
    assert!(!is_plan_required(&refused));
    assert!(refused.to_string().contains("refused"));
    assert!(
        !refused.to_string().contains("session expired"),
        "{refused}"
    );

    let expired = hosted_status_error("h", "memory/scopes", StatusCode::UNAUTHORIZED, "{}");
    assert!(matches!(expired, Error::Unauthorized(_)));
    assert!(expired.to_string().contains("session expired"), "{expired}");
}
