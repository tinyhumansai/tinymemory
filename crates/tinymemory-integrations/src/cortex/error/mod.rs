//! The engine's errors are the contract's errors.
//!
//! This module does not define a parallel `Error`. Every public operation is a
//! [`tinymemory_api::MemoryEngine`] method, and those return
//! [`tinymemory_api::Error`]; a second enum would only be converted into it at
//! every boundary and would invite variants the host cannot act on. So the
//! contract's enum is re-exported here as [`Error`], and
//! construction and configuration failures use [`Error::Config`].
//!
//! # How a CortexDB failure is classified
//!
//! | HTTP | Variant |
//! | --- | --- |
//! | 401, 403 | [`Error::Unauthorized`] |
//! | 402 | [`Error::InsufficientCredits`] when the hosted code names exhausted credits |
//! | any, hosted, code `*_PLAN_REQUIRED` | [`Error::Unauthorized`] ([`is_plan_required`]) |
//! | 404 | [`Error::NotFound`] |
//! | 400, 413, 422 | [`Error::InvalidRequest`] |
//! | 409 | [`Error::Conflict`] |
//! | 408, 429, 500, 502, 503, 504 | [`Error::Unavailable`] (retried on reads) |
//! | anything else | [`Error::Engine`] |
//!
//! Transport faults (timeout, DNS, TLS, refused connection) are
//! [`Error::Unavailable`] too. A read retries an `Unavailable` three times,
//! and six (about 8 seconds) when CortexDB answers `503
//! AUTHORIZATION_STATE_CHANGED`, which a concurrent scope registration
//! causes and which settles on its own.
//!
//! **Why 402 has a dedicated variant.** An exhausted credit balance is neither transient
//! (`Unavailable` would invite a retry loop that cannot succeed until someone
//! tops up) nor a credential fault (`Unauthorized` would send the host to its
//! sign-in flow). It is the engine refusing to serve, which is what `Engine`
//! means; the hosted transport maps its structured backend code to
//! [`Error::InsufficientCredits`] so a host can show a top-up prompt.
//!
//! **Why a plan refusal remains `Unauthorized`.** The hosted backend answers `403` with
//! a `*_PLAN_REQUIRED` code (`MEMORY_PLAN_REQUIRED`) when the account's plan
//! does not include the feature. The credential is fine, so generic 403 text
//! would sign a valid user out (tinyhumansai/openhuman#8081). Retaining the
//! existing variant preserves exhaustive host matches; [`is_plan_required`]
//! lets a host show an upgrade path without retrying operations that cannot
//! succeed. A hosted `403` without such a code stays `Unauthorized`
//! but says the request was refused, not that the session expired, which
//! only a `401` says.
//!
//! # The `[CODE]` prefix
//!
//! The TinyHumans backend names every failure with an `errorCode`. The
//! contract's `Error` has no field for it, so a hosted failure's message
//! starts with `[CODE] ` and [`error_code`] reads it back. Direct CortexDB
//! failures carry no prefix.
//!
//! Messages never carry a credential: the request builder marks the
//! credential header sensitive, and no message is built from it.

pub use tinymemory_api::Error;

/// The result alias for this module's operations.
pub type Result<T> = std::result::Result<T, Error>;

/// The TinyHumans backend's code for an exhausted credit balance (HTTP 402).
pub(crate) const INSUFFICIENT_CREDITS_CODE: &str = "USER_INSUFFICIENT_CREDITS";

/// The suffix of the TinyHumans backend's codes for a feature the account's
/// plan does not include (`MEMORY_PLAN_REQUIRED`).
pub(crate) const PLAN_REQUIRED_SUFFIX: &str = "_PLAN_REQUIRED";

/// The message every variant carries.
fn message_of(error: &Error) -> &str {
    match error {
        Error::Unsupported(m)
        | Error::Off(m)
        | Error::InvalidRequest(m)
        | Error::Unauthorized(m)
        | Error::InsufficientCredits(m)
        | Error::NotFound(m)
        | Error::Conflict(m)
        | Error::Unavailable(m)
        | Error::Engine(m)
        | Error::Config(m) => m,
    }
}

/// The TinyHumans `errorCode` a hosted failure carried, when it has one.
///
/// Parses the `[CODE] ` prefix hosted failures put on their message. Returns
/// `None` for a direct CortexDB failure, a local refusal, or a message that no
/// longer starts with the prefix.
#[must_use]
pub fn error_code(error: &Error) -> Option<&str> {
    let rest = message_of(error).strip_prefix('[')?;
    let (code, _) = rest.split_once("] ")?;
    let well_formed = !code.is_empty()
        && code
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    well_formed.then_some(code)
}

/// Whether `error` is the hosted backend refusing a feature the account's
/// plan does not include. Not a credential fault: the host keeps the session
/// and shows an upgrade path.
#[must_use]
pub fn is_plan_required(error: &Error) -> bool {
    error.is_plan_required()
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
