use super::*;

#[test]
fn account_wide_refusals_are_told_from_an_items_own_failure() {
    let credits = Error::Engine(EngineError::InsufficientCredits(
        "[USER_INSUFFICIENT_CREDITS] the account has insufficient credits (HTTP 402)".into(),
    ));
    assert!(credits.is_account_wide());
    assert_eq!(credits.code(), "INSUFFICIENT_CREDITS");
    assert!(Error::Off("signed out".into()).is_account_wide());
    assert!(Error::from(EngineError::Unauthorized("key".into())).is_account_wide());
    assert!(!Error::from(EngineError::Unavailable("request timed out".into())).is_account_wide());
    let plain = Error::from(EngineError::Engine("boom".into()));
    assert!(!plain.is_account_wide());
    assert_eq!(plain.code(), "ENGINE");
    assert!(!Error::from(EngineError::InvalidRequest("bad".into())).is_account_wide());
    assert!(!Error::State("torn".into()).is_account_wide());
}

#[test]
fn codes_match_the_wire_taxonomy() {
    assert_eq!(Error::Off(String::new()).code(), "MEMORY_OFF");
    assert_eq!(Error::invalid("x").code(), "INVALID_REQUEST");
    assert_eq!(
        Error::from(EngineError::NotFound("x".into())).code(),
        "INVALID_REQUEST"
    );
    assert_eq!(
        Error::from(EngineError::Unsupported("x".into())).code(),
        "UNSUPPORTED"
    );
    assert_eq!(
        Error::from(EngineError::Unauthorized("x".into())).code(),
        "UNAUTHORIZED"
    );
    assert_eq!(
        Error::from(EngineError::Unavailable("x".into())).code(),
        "UNAVAILABLE"
    );
}
