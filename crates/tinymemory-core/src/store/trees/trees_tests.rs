//! Tests for the surrounding module.

use super::*;

#[test]
fn tree_module_reexports_expected_constants() {
    assert_eq!(INPUT_TOKEN_BUDGET, 50_000);
    assert_eq!(OUTPUT_TOKEN_BUDGET, 5_000);
    assert_eq!(SUMMARY_FANOUT, 10);
    // Compile-time guardrails: both sides are constants, so evaluate the
    // invariant at build time rather than asserting a folded literal.
    const _: () = assert!(
        TOPIC_CREATION_THRESHOLD > TOPIC_ARCHIVE_THRESHOLD,
        "topics must be created before they can be archived"
    );
    const _: () = assert!(TOPIC_RECHECK_EVERY > 0);
}
