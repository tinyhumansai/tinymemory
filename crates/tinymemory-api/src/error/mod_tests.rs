//! Error rendering and classification.

use super::*;

#[test]
fn every_variant_renders_its_class_and_message() {
    let cases = [
        (Error::Unsupported("x".into()), "unsupported: x"),
        (Error::Off("disabled".into()), "memory is off: disabled"),
        (
            Error::InsufficientCredits("top up".into()),
            "insufficient credits: top up",
        ),
        (Error::InvalidRequest("x".into()), "invalid request: x"),
        (Error::Unauthorized("x".into()), "unauthorized: x"),
        (Error::NotFound("x".into()), "not found: x"),
        (Error::Conflict("x".into()), "conflict: x"),
        (Error::Unavailable("x".into()), "unavailable: x"),
        (Error::Engine("x".into()), "engine error: x"),
        (Error::Config("x".into()), "configuration error: x"),
    ];
    for (error, rendered) in cases {
        assert_eq!(error.to_string(), rendered);
    }
}

#[test]
fn only_unavailable_is_transient() {
    assert!(Error::Unavailable("busy".into()).is_transient());
    assert!(!Error::Engine("broken".into()).is_transient());
    assert!(!Error::Unauthorized("expired".into()).is_transient());
}

#[test]
fn errors_have_stable_host_codes_and_account_wide_classification() {
    let cases = [
        (Error::Off("disabled".into()), "MEMORY_OFF", true),
        (Error::Unsupported("mode".into()), "UNSUPPORTED", false),
        (
            Error::InvalidRequest("bad".into()),
            "INVALID_REQUEST",
            false,
        ),
        (Error::NotFound("missing".into()), "INVALID_REQUEST", false),
        (Error::Config("key".into()), "INVALID_REQUEST", false),
        (Error::Unauthorized("expired".into()), "UNAUTHORIZED", true),
        (
            Error::InsufficientCredits("top up".into()),
            "INSUFFICIENT_CREDITS",
            true,
        ),
        (Error::Unavailable("offline".into()), "UNAVAILABLE", false),
        (Error::Engine("broken".into()), "ENGINE", false),
        (Error::Conflict("changed".into()), "ENGINE", false),
    ];

    for (error, code, account_wide) in cases {
        assert_eq!(error.code(), code);
        assert_eq!(error.is_account_wide(), account_wide, "{error:?}");
    }
}

#[test]
fn request_timeouts_and_rate_limits_are_transient_not_account_wide() {
    for message in ["request timed out", "rate limited"] {
        let error = Error::Unavailable(message.into());
        assert!(error.is_transient(), "{error:?}");
        assert!(!error.is_account_wide(), "{error:?}");
    }
}

#[test]
fn a_plan_refusal_is_account_wide_only_with_a_well_formed_code() {
    let plan = Error::Engine("[MEMORY_PLAN_REQUIRED] memory API recall: not in the plan".into());
    assert!(plan.is_plan_required());
    assert!(plan.is_account_wide());
    for other in [
        Error::Engine("saw [MEMORY_PLAN_REQUIRED] mid-message".into()),
        Error::Engine("[not a code_PLAN_REQUIRED] memory API x".into()),
        Error::Engine("[USER_INSUFFICIENT_CREDITS] top up".into()),
        Error::Unavailable("[MEMORY_PLAN_REQUIRED] x".into()),
    ] {
        assert!(!other.is_plan_required(), "{other:?}");
        assert!(!other.is_account_wide(), "{other:?}");
    }
}
