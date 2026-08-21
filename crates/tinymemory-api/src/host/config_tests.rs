//! Tests for the surrounding module.

use super::*;

#[test]
fn composio_direct_mode_is_ascii_case_insensitive() {
    assert!(ComposioMode {
        mode: "DIRECT".into(),
        ..Default::default()
    }
    .is_direct());
    assert!(!ComposioMode {
        mode: COMPOSIO_MODE_BACKEND.into(),
        ..Default::default()
    }
    .is_direct());
}

#[test]
fn composio_debug_output_redacts_the_api_key() {
    let mode = ComposioMode {
        api_key: Some("composio-secret".into()),
        ..Default::default()
    };
    let debug = format!("{mode:?}");
    assert!(!debug.contains("composio-secret"));
    assert!(debug.contains("<redacted>"));
}
