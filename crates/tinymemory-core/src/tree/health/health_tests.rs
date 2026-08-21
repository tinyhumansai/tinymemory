//! Tests for the surrounding module.

use super::*;

#[test]
fn storage_unavailable_discriminant_round_trips() {
    assert_eq!(
        u8_to_code(code_to_u8(FailureCode::StorageUnavailable)),
        Some(FailureCode::StorageUnavailable)
    );
}
