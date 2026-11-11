//! Compiling the scrubber's built-in regular expressions.
//!
//! Every credential and PII pattern in this module is a string literal
//! compiled once into a `LazyLock`. They are compiled through [`literal`] so
//! the one place a pattern could fail to compile is named, documented and
//! covered by tests: the scrubbing tests force every `LazyLock`, so a typo in
//! a pattern fails CI rather than a host.

use regex::Regex;

/// Compiles a built-in pattern.
///
/// # Panics
///
/// Panics if `pattern` is not a valid regular expression. Every caller passes
/// a literal that the safety tests compile, so this is unreachable in a
/// released build.
#[allow(
    clippy::expect_used,
    reason = "patterns are compile-time literals exercised by the safety tests"
)]
pub(super) fn literal(pattern: &str) -> Regex {
    Regex::new(pattern).expect("a built-in safety pattern is a valid regex")
}
