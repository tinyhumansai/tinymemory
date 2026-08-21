//! PII redaction helpers for log output.
//!
//! Per project rule (CLAUDE.md): "Never log secrets or full PII."
//! After the participant-bucketing change introduced in the MD-content PR,
//! source_ids and content_paths can embed full email addresses, so any log
//! line that prints them needs to redact.

use sha2::{Digest, Sha256};

/// Redact a string by hashing it to 8 hex chars. Stable across runs for the
/// same input — safe to grep for in logs when debugging with the raw value
/// available externally.
///
/// Use for source_ids, entity_ids, content_paths and similar PII-bearing
/// strings in log output.
pub fn redact(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    let d = h.finalize();
    format!("{:08x}", u32::from_be_bytes([d[0], d[1], d[2], d[3]]))
}

/// Redact a URL/endpoint by stripping path, query, fragment and credentials,
/// keeping only the host (and port if present).
///
/// Examples:
/// - `"http://localhost:11434/api/chat"` → `"localhost:11434"`
/// - `"https://user:pass@example.com/foo?q=1"` → `"example.com"`
/// - `"ollama://host:1234"` → `"host:1234"`
///
/// Does not pull in a URL-parsing crate; uses cheap string splitting which is
/// sufficient for the endpoint-config strings this codebase passes around.
pub fn redact_endpoint(url: &str) -> String {
    // Strip scheme (everything before "://").
    let after_scheme = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    // Take only the authority (everything up to the first '/', '?', or '#') so
    // any '@' in the path / query (e.g. `?email=foo@bar`) doesn't get treated
    // as a userinfo separator.
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    // Within the authority, the LAST '@' separates userinfo from host:port.
    // (RFC 3986: userinfo may itself contain '@' — split-on-first would
    // truncate the host. Use rsplit so `user:p@ss@example.com` extracts
    // `example.com` correctly.)
    let host_port = authority
        .rsplit_once('@')
        .map(|(_, r)| r)
        .unwrap_or(authority);
    host_port.to_string()
}

#[cfg(test)]
#[path = "redact_tests.rs"]
mod tests;
