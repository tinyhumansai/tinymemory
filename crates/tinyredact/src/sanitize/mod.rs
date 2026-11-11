//! Scrubbing free text and JSON values of secrets and PII.
//!
//! [`sanitize_text_with`] runs, in order: private-key blocks
//! ([`patterns::BLOCK_PATTERNS`], replaced in full), credential markers
//! ([`crate::redact_credential_markers`]), credential shapes
//! ([`patterns::REDACTION_PATTERNS`]) and finally the PII pass
//! ([`crate::pii`]). [`sanitize_json_with`] walks a JSON value,
//! replacing the value under a sensitive-looking key wholesale and running
//! every other string through [`sanitize_text_with`].

use serde_json::Value;

use crate::policy::{Policy, SanitizationReport, Sanitized};
use crate::{markers, pii};

mod patterns;

use patterns::{BLOCK_PATTERNS, REDACTION_PATTERNS};

/// Replacement for a JSON value under a sensitive key, or a subtree beyond
/// [`MAX_JSON_SANITIZE_DEPTH`].
pub(crate) const REDACTED_SECRET: &str = "[REDACTED_SECRET]";
/// Replacement for a whole private-key block.
pub(crate) const REDACTED_PRIVATE_KEY: &str = "[REDACTED_PRIVATE_KEY]";
/// Nesting depth at which [`sanitize_json_with`] stops walking and replaces
/// the subtree with [`REDACTED_SECRET`].
pub(crate) const MAX_JSON_SANITIZE_DEPTH: usize = 128;

/// True when `value` looks like it contains a credential.
pub fn has_likely_secret(value: &str) -> bool {
    BLOCK_PATTERNS.iter().any(|p| p.is_match(value))
        || REDACTION_PATTERNS.iter().any(|(p, _)| p.is_match(value))
}

/// Scrub secrets and PII from free text, returning the cleaned text plus a
/// [`SanitizationReport`].
pub fn sanitize_text(value: &str) -> Sanitized<String> {
    sanitize_text_with(value, Policy::default())
}

/// [`sanitize_text`] under an explicit [`Policy`].
pub fn sanitize_text_with(value: &str, policy: Policy) -> Sanitized<String> {
    let mut out = value.to_string();
    let mut report = SanitizationReport::default();

    for pattern in BLOCK_PATTERNS.iter() {
        let hits = pattern.find_iter(&out).count();
        if hits > 0 {
            report.blocked_secret_hits += hits;
            out = pattern.replace_all(&out, REDACTED_PRIVATE_KEY).into_owned();
        }
    }

    // Values after a credential marker (`/secret/<key>`, `Bearer <value>`),
    // before the shape regexes: it catches what they cannot — a one-time key,
    // a short bearer value — and its `[REDACTED]` is not token-shaped, so no
    // regex below fires on it again. Only ever replaces, so the pass makes the
    // scrubber strictly stricter.
    let (marked, hits) = markers::redact_counted(&out);
    if hits > 0 {
        report.text_redactions += hits;
        out = marked.into_owned();
    }

    for (pattern, replacement) in REDACTION_PATTERNS.iter() {
        let hits = pattern.find_iter(&out).count();
        if hits > 0 {
            report.text_redactions += hits;
            out = pattern.replace_all(&out, *replacement).into_owned();
        }
    }

    // Full multilingual national-ID PII scrub (checksum-gated, normalization
    // pre-pass) — runs after secret redaction so every call site that scrubs
    // secrets also scrubs PII.
    let pii = pii::redact_pii_with(&out, policy);
    report = report.merge(pii.report);
    out = pii.value;

    Sanitized { value: out, report }
}

/// Recursively scrub a JSON value: sensitive keys are replaced wholesale and
/// every string value runs through `sanitize_text`.
pub fn sanitize_json(value: &Value) -> Sanitized<Value> {
    sanitize_json_with(value, Policy::default())
}

/// [`sanitize_json`] under an explicit [`Policy`].
pub fn sanitize_json_with(value: &Value, policy: Policy) -> Sanitized<Value> {
    sanitize_json_inner(value, 0, policy)
}

/// Recursive worker behind [`sanitize_json`].
///
/// `depth` counts nesting from the call in `sanitize_json` (which starts at
/// `0`); once it reaches [`MAX_JSON_SANITIZE_DEPTH`] the whole subtree at that
/// point is replaced by a single redaction marker rather than walked further,
/// bounding recursion against pathologically deep or adversarial JSON.
fn sanitize_json_inner(value: &Value, depth: usize, policy: Policy) -> Sanitized<Value> {
    if depth >= MAX_JSON_SANITIZE_DEPTH {
        return Sanitized {
            value: Value::String(REDACTED_SECRET.to_string()),
            report: SanitizationReport {
                depth_redactions: 1,
                ..SanitizationReport::default()
            },
        };
    }

    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            let mut report = SanitizationReport::default();
            for (key, value) in map {
                if is_sensitive_key(key) {
                    report.key_redactions += 1;
                    out.insert(key.clone(), Value::String(REDACTED_SECRET.to_string()));
                    continue;
                }
                let sanitized = sanitize_json_inner(value, depth + 1, policy);
                report = report.merge(sanitized.report);
                out.insert(key.clone(), sanitized.value);
            }
            Sanitized {
                value: Value::Object(out),
                report,
            }
        }
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            let mut report = SanitizationReport::default();
            for item in items {
                let sanitized = sanitize_json_inner(item, depth + 1, policy);
                report = report.merge(sanitized.report);
                out.push(sanitized.value);
            }
            Sanitized {
                value: Value::Array(out),
                report,
            }
        }
        Value::String(value) => {
            let sanitized = sanitize_text_with(value, policy);
            Sanitized {
                value: Value::String(sanitized.value),
                report: sanitized.report,
            }
        }
        _ => Sanitized {
            value: value.clone(),
            report: SanitizationReport::default(),
        },
    }
}

/// True when a JSON object key's name itself suggests it holds a secret
/// (`api_key`, `token`, `password`, …), independent of the value's contents.
///
/// Matching keys are redacted wholesale in [`sanitize_json_inner`] — the
/// value is replaced rather than scanned, since a key named e.g. `password`
/// is assumed sensitive even if its value doesn't match any
/// [`REDACTION_PATTERNS`] regex. Matching is on the key with all
/// non-alphanumeric characters stripped and lowercased, so `API-Key`,
/// `api_key`, and `apiKey` are all treated identically.
fn is_sensitive_key(key: &str) -> bool {
    let normalized: String = key
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();

    matches!(
        normalized.as_str(),
        "apikey"
            | "token"
            | "accesstoken"
            | "refreshtoken"
            | "authorization"
            | "password"
            | "secret"
            | "clientsecret"
    ) || normalized.ends_with("token")
        || normalized.ends_with("apikey")
        || normalized.ends_with("clientsecret")
        || normalized.contains("password")
        || normalized.contains("secret")
        || normalized.ends_with("key")
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "mod_default_policy_tests.rs"]
mod default_policy_tests;
