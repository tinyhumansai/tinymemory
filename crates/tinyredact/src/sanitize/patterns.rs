//! The credential shape tables the text scrubber runs.
//!
//! [`BLOCK_PATTERNS`] match whole private-key blocks, which are replaced
//! wholesale. [`REDACTION_PATTERNS`] match a credential's shape — a provider
//! token prefix, a `key=value` assignment, a JWT — and rewrite only the
//! matched span, keeping a captured prefix where the replacement names one.

use std::sync::LazyLock;

use regex::Regex;

use crate::pattern::literal;

/// Private-key blocks (PEM, OpenSSH, PGP), replaced in full.
pub(super) static BLOCK_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        literal(
            r"(?is)-----BEGIN(?: [A-Z]+)? PRIVATE KEY-----.*?-----END(?: [A-Z]+)? PRIVATE KEY-----",
        ),
        literal(r"(?is)-----BEGIN OPENSSH PRIVATE KEY-----.*?-----END OPENSSH PRIVATE KEY-----"),
        literal(
            r"(?is)-----BEGIN PGP PRIVATE KEY BLOCK-----.*?-----END PGP PRIVATE KEY BLOCK-----",
        ),
    ]
});

/// Credential shapes paired with the replacement each match is rewritten to.
pub(super) static REDACTION_PATTERNS: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    vec![
        (
            literal(r"(?i)(bearer\s+)[A-Za-z0-9._~+/=-]{8,}"),
            "${1}[REDACTED]",
        ),
        (
            literal(r#"(?i)(api[_-]?key["']?\s*[=:\s]\s*["']?)[^\s"']+"#),
            "${1}[REDACTED]",
        ),
        (
            literal(
                r#"(?i)(?:\b|_)(token|access[_-]?token|refresh[_-]?token|client[_-]?secret|password|secret)\b["']?\s*[=:\s]\s*["']?[^\s"'&]+"#,
            ),
            "[REDACTED]",
        ),
        (literal(r"\bsk-[A-Za-z0-9]{20,}\b"), "[REDACTED]"),
        (literal(r"\bgh[pousr]_[A-Za-z0-9_]{20,}\b"), "[REDACTED]"),
        (literal(r"\bAKIA[0-9A-Z]{16}\b"), "[REDACTED]"),
        (literal(r"\bASIA[0-9A-Z]{16}\b"), "[REDACTED]"),
        (
            literal(r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9._-]{8,}\.[A-Za-z0-9._-]{8,}\b"),
            "[REDACTED]",
        ),
        (
            literal(
                r#"(?i)\b(access_token|refresh_token|id_token|authorization_code|code_verifier|code_challenge)\b["']?\s*[=:\s]\s*["']?[^\s"'&]+"#,
            ),
            "[REDACTED]",
        ),
        (literal(r"\bAIza[0-9A-Za-z\-_]{35}\b"), "[REDACTED]"),
        (literal(r"\bsk-ant-[A-Za-z0-9\-_]{16,}\b"), "[REDACTED]"),
        (
            literal(r"\bsk-(?:proj|org)-[A-Za-z0-9\-_]{12,}\b"),
            "[REDACTED]",
        ),
        (
            literal(r"\b(?:sk|rk)_(?:live|test)_[A-Za-z0-9]{16,}\b"),
            "[REDACTED]",
        ),
        (
            literal(r"\bxox(?:a|b|p|s|r)-[A-Za-z0-9-]{10,}\b"),
            "[REDACTED]",
        ),
        (literal(r"\bgithub_pat_[A-Za-z0-9_]{20,}\b"), "[REDACTED]"),
        (literal(r"\bglpat-[A-Za-z0-9\-_]{16,}\b"), "[REDACTED]"),
        (literal(r"\bnpm_[A-Za-z0-9]{20,}\b"), "[REDACTED]"),
        (
            literal(r"\bSG\.[A-Za-z0-9_\-]{16,}\.[A-Za-z0-9_\-]{16,}\b"),
            "[REDACTED]",
        ),
    ]
});
