//! Tests for the surrounding module.

use super::*;

#[test]
fn toolkit_has_scope_distinguishes_gated_from_ungated_scopes() {
    // gmail catalog includes destructive verbs (delete / trash /
    // batch_delete), so admin-gating actually unlocks something.
    assert!(toolkit_has_scope("gmail", ToolScope::Admin));
    assert!(toolkit_has_scope("gmail", ToolScope::Read));
    assert!(toolkit_has_scope("gmail", ToolScope::Write));
    // Case-insensitive toolkit slug → still routes to the catalog.
    assert!(toolkit_has_scope("GMAIL", ToolScope::Admin));
    // Unknown toolkit → no catalog → no scope is "gating" anything.
    assert!(!toolkit_has_scope("nonexistent-toolkit", ToolScope::Admin));
}
