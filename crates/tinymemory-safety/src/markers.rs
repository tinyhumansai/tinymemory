//! Credential-marker rules: values that follow an unambiguous marker.
//!
//! The regex set in [`crate::sanitize_text`] recognises credentials by their
//! own shape — a vendor prefix, a JWT's three segments, eight or more token
//! characters after `Bearer`. Two leaks get past shape alone, both measured in
//! a live OpenCompany deployment where every operator message was remembered
//! verbatim and recalled into later turns:
//!
//! - **One-time-secret URLs** (`https://ots.example/secret/<key>`). The key is
//!   the credential, and nothing about it looks like one; the token regexes key
//!   on `secret` followed by `=`, `:` or a space, never a `/`.
//! - **Short `Bearer` values.** Any non-empty bearer value is a valid
//!   credential (`Bearer s3cret`), but the regex floor is eight characters.
//!
//! Both are found by their *marker* instead, and only the value after it is
//! replaced: memory should still record that a link or a token was shared,
//! since that is the context an agent needs. A generic "anything that looks
//! like a token" rule would mangle prose, so the `Bearer` rule is tuned against
//! the English word — "ring bearer", "bearer bond", "Bearer or not" survive.
//!
//! Ported from OpenCompany's `redact_secrets`, which is what this replaces.

use std::borrow::Cow;

/// The replacement for a credential value.
const REDACTED: &str = "[REDACTED]";
/// The one-time-secret URL path. Exact match: those URLs are lowercase.
const SECRET_URL_MARKER: &str = "/secret/";
/// The HTTP `Authorization` scheme, matched ASCII-case-insensitively (RFC
/// 9110's auth-scheme ABNF is case-insensitive).
const BEARER_MARKER: &str = "bearer ";

/// Redacts the value after every one-time-secret URL path (`/secret/<key>`)
/// and every `Bearer` scheme in `text`, keeping the marker and the prose.
///
/// The entry point for a host that scrubs plain text on its way into memory
/// and wants exactly these two rules — without the PII pass and the broader
/// token regexes of [`crate::sanitize_text`], which applies these rules too.
/// Text with neither marker is returned borrowed, without allocating.
///
/// ```
/// use tinymemory_safety::redact_credential_markers;
///
/// assert_eq!(
///     redact_credential_markers("open https://ots.example/secret/AbC123 now"),
///     "open https://ots.example/secret/[REDACTED] now"
/// );
/// assert_eq!(
///     redact_credential_markers("auth with Bearer s3cret please"),
///     "auth with Bearer [REDACTED] please"
/// );
/// assert_eq!(
///     redact_credential_markers("the ring bearer walked down the aisle"),
///     "the ring bearer walked down the aisle"
/// );
/// ```
pub fn redact_credential_markers(text: &str) -> Cow<'_, str> {
    redact_counted(text).0
}

/// [`redact_credential_markers`] plus the number of values it replaced, for
/// the [`crate::SanitizationReport`].
pub(crate) fn redact_counted(text: &str) -> (Cow<'_, str>, usize) {
    if !text.contains(SECRET_URL_MARKER) && find_bearer_marker(text).is_none() {
        return (Cow::Borrowed(text), 0);
    }

    let mut out = String::with_capacity(text.len());
    let mut hits = 0;
    let mut rest = text;
    while let Some((pos, marker)) = next_marker(rest) {
        out.push_str(&rest[..pos + marker.len()]);
        // The capital "Bearer " is the unambiguous auth-header spelling, so a
        // plain digit-free word after it like `secret` is still a credential.
        // The lower-case form is also ordinary English ("ring bearer"), so a
        // plain word after it is only redacted once prose is implausible.
        let aggressive = marker == SECRET_URL_MARKER || rest.as_bytes()[pos].is_ascii_uppercase();
        let tail = &rest[pos + marker.len()..];
        // Extra whitespace after the scheme is legal (`Bearer   sk-...`): keep
        // it, but skip it, or the scan stops at the first space and stores the
        // credential verbatim.
        let value_start = tail.len() - tail.trim_start().len();
        out.push_str(&tail[..value_start]);
        let mut value = &tail[value_start..];
        // Formatted chat wraps a credential in a backtick or quote; skip one
        // leading wrapper so the scan reaches the credential. The wrapper is
        // kept, and its closing mate stays in `rest`.
        let wrapper_len = usize::from(matches!(
            value.as_bytes().first(),
            Some(b'`' | b'\'' | b'"')
        ));
        out.push_str(&value[..wrapper_len]);
        value = &value[wrapper_len..];
        let end = value
            .find(|c: char| !is_token_char(c))
            .unwrap_or(value.len());
        let mut consumed = end;
        if end >= first_value_floor(&value[..end], aggressive) {
            out.push_str(REDACTED);
            hits += 1;
            // A bearer value can be several space-separated fragments
            // (`Bearer firstpart secondpart`). Once the first looked like a
            // credential, keep redacting fragments that clear a higher floor,
            // so trailing prose ("please", "the token was rotated") survives.
            // Only a space stop continues the run: a wrapper or punctuation
            // stop means the credential was a single token.
            if value.as_bytes().get(end) == Some(&b' ') {
                let mut cursor = end;
                while let Some(rel) = value[cursor..].find(' ') {
                    let token_start = cursor + rel + 1;
                    let fragment_end = value[token_start..]
                        .find(|c: char| !is_token_char(c))
                        .unwrap_or(value.len() - token_start);
                    if fragment_end == 0 {
                        cursor = token_start; // repeated spaces; keep probing
                        continue;
                    }
                    let fragment = &value[token_start..token_start + fragment_end];
                    if fragment.chars().count() < continuation_floor(fragment) {
                        break; // prose: the space before it survives into `rest`
                    }
                    out.push(' ');
                    out.push_str(REDACTED);
                    hits += 1;
                    cursor = token_start + fragment_end;
                }
                consumed = cursor;
            }
        } else {
            // Too short to be a secret ("Bearer or not").
            out.push_str(&value[..end]);
        }
        rest = &tail[value_start + wrapper_len + consumed..];
    }
    out.push_str(rest);
    (Cow::Owned(out), hits)
}

/// The length from which the first value after a marker is a credential.
///
/// A digit makes even a short value token-shaped (`s3cret`), since prose after
/// a marker carries none. A digit-free value counts from six characters after
/// the unambiguous markers, or when it is not a plain word (`sk-longsecret`);
/// a plain word after a lower-case "bearer " is prose until twelve.
fn first_value_floor(value: &str, aggressive: bool) -> usize {
    let has_digit = value.chars().any(|c| c.is_ascii_digit());
    let plain_word = value.chars().all(|c| c.is_ascii_alphanumeric());
    if has_digit {
        4
    } else if aggressive || !plain_word {
        6
    } else {
        12
    }
}

/// The length from which a later space-separated fragment continues a
/// credential run: higher than the first value's, so a short prose word
/// cannot enter the run.
fn continuation_floor(fragment: &str) -> usize {
    if fragment.chars().any(|c| c.is_ascii_digit()) {
        4
    } else {
        8
    }
}

/// Byte offset of the next [`BEARER_MARKER`], case-insensitively. The marker
/// is pure ASCII, so a byte scan is safe and allocation-free.
fn find_bearer_marker(text: &str) -> Option<usize> {
    text.as_bytes()
        .windows(BEARER_MARKER.len())
        .position(|window| window.eq_ignore_ascii_case(BEARER_MARKER.as_bytes()))
}

/// The earliest marker in `rest`, as `(byte offset, marker)`. The marker is
/// used only for its length; the output keeps the caller's casing.
fn next_marker(rest: &str) -> Option<(usize, &'static str)> {
    rest.find(SECRET_URL_MARKER)
        .map(|pos| (pos, SECRET_URL_MARKER))
        .into_iter()
        .chain(find_bearer_marker(rest).map(|pos| (pos, BEARER_MARKER)))
        .min_by_key(|(pos, _)| *pos)
}

/// A character that can appear inside a credential: base64url's `-` and `_`,
/// the `.` joining a JWT's segments, and base64's `+`, `/`, `~`, `=`. Stopping
/// at any of them would leak the rest of the credential.
fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+' | '/' | '~' | '=')
}

#[cfg(test)]
#[path = "markers_tests.rs"]
mod tests;
