//! Tests for hosted backend error codes.

use super::*;

#[test]
fn a_hosted_code_is_read_back_from_the_prefix() {
    let error = Error::Unauthorized("[UNAUTHORIZED] memory API memory/events — expired".into());
    assert_eq!(error_code(&error), Some("UNAUTHORIZED"));
}

#[test]
fn an_unprefixed_message_has_no_code() {
    assert_eq!(error_code(&Error::Engine("HTTP 500".into())), None);
    assert_eq!(error_code(&Error::Engine("[lowercase] nope".into())), None);
    assert_eq!(error_code(&Error::Engine("[] empty".into())), None);
}

#[test]
fn hosted_credit_errors_are_typed_and_keep_the_backend_code() {
    let credits = Error::InsufficientCredits(format!("[{INSUFFICIENT_CREDITS_CODE}] top up"));
    assert_eq!(error_code(&credits), Some(INSUFFICIENT_CREDITS_CODE));
}

#[test]
fn plan_required_needs_the_engine_variant_and_a_plan_code() {
    assert!(is_plan_required(&Error::Engine(
        "[MEMORY_PLAN_REQUIRED] not in the plan".into()
    )));
    assert!(!is_plan_required(&Error::Unauthorized(
        "[MEMORY_PLAN_REQUIRED] not in the plan".into()
    )));
    assert!(!is_plan_required(&Error::Engine(
        "[USER_INSUFFICIENT_CREDITS] top up".into()
    )));
    assert!(!is_plan_required(&Error::Engine(
        "MEMORY_PLAN_REQUIRED without the prefix".into()
    )));
}
