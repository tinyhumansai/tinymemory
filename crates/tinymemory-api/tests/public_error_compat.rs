//! Downstream source-compatibility checks for the public API.

use tinymemory_api::Error;

fn exhaustive_host_match(error: Error) -> &'static str {
    match error {
        Error::Off(_) => "off",
        Error::Unsupported(_) => "unsupported",
        Error::InvalidRequest(_) => "invalid request",
        Error::Unauthorized(_) => "unauthorized",
        Error::InsufficientCredits(_) => "insufficient credits",
        Error::NotFound(_) => "not found",
        Error::Conflict(_) => "conflict",
        Error::Unavailable(_) => "unavailable",
        Error::Engine(_) => "engine",
        Error::Config(_) => "config",
    }
}

#[test]
fn downstream_exhaustive_error_matches_remain_source_compatible() {
    assert_eq!(
        exhaustive_host_match(Error::Unauthorized(
            "[MEMORY_PLAN_REQUIRED] not in plan".into()
        )),
        "unauthorized"
    );
}
