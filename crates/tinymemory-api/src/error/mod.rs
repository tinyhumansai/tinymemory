//! The one error every engine returns.
//!
//! Variants classify a failure by what a host can do about it, not by where it
//! happened. Messages are lowercase, carry no trailing punctuation, and never
//! carry a credential: an engine sanitises its own failure before it becomes
//! [`Error::Engine`] or one of the typed account-wide variants. A hosted plan
//! refusal remains [`Error::Unauthorized`] for source compatibility with
//! exhaustive matches; [`Error::is_plan_required`] distinguishes it from a
//! credential rejection.

use serde::Serialize;

/// Every way a memory operation can fail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum Error {
    /// Memory is disabled or no usable engine is configured.
    #[error("memory is off: {0}")]
    Off(String),
    /// The engine does not offer the requested operation or mode; a host
    /// reads [`crate::EngineDescriptor`] and should never ask.
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// The request itself is malformed (an empty filter for a forget, a zero
    /// limit, an unresolved document URI).
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// The credential was missing, expired or rejected.
    #[error("unauthorized: {0}")]
    Unauthorized(String),
    /// The hosted account cannot make this request until it has more credits.
    #[error("insufficient credits: {0}")]
    InsufficientCredits(String),
    /// The addressed item or route does not exist.
    #[error("not found: {0}")]
    NotFound(String),
    /// The write conflicts with what the engine already holds.
    #[error("conflict: {0}")]
    Conflict(String),
    /// A transient failure (timeout, rate limit, unavailable upstream); the
    /// same call may succeed later.
    #[error("unavailable: {0}")]
    Unavailable(String),
    /// The engine's own failure, already sanitised.
    #[error("engine error: {0}")]
    Engine(String),
    /// The engine was configured incorrectly (unknown id, missing endpoint or
    /// key, a credentialed cleartext endpoint).
    #[error("configuration error: {0}")]
    Config(String),
}

impl Error {
    /// The stable host-facing code for this failure.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Off(_) => "MEMORY_OFF",
            Self::Unsupported(_) => "UNSUPPORTED",
            Self::InvalidRequest(_) | Self::NotFound(_) | Self::Config(_) => "INVALID_REQUEST",
            Self::Unauthorized(_) if self.is_plan_required() => "PLAN_REQUIRED",
            Self::Unauthorized(_) => "UNAUTHORIZED",
            Self::InsufficientCredits(_) => "INSUFFICIENT_CREDITS",
            Self::Unavailable(_) => "UNAVAILABLE",
            Self::Conflict(_) | Self::Engine(_) => "ENGINE",
        }
    }

    /// Whether the failure is known to affect every request on the account.
    /// `Unavailable` is deliberately excluded: it describes a transient
    /// request failure and carries no typed proof of an account-wide outage.
    #[must_use]
    pub fn is_account_wide(&self) -> bool {
        matches!(
            self,
            Self::Off(_) | Self::Unauthorized(_) | Self::InsufficientCredits(_)
        )
    }

    /// Whether a hosted authorization response refused a feature outside the
    /// account's plan. The error remains [`Self::Unauthorized`] so this
    /// classification does not break downstream exhaustive matches.
    #[must_use]
    pub fn is_plan_required(&self) -> bool {
        matches!(self, Self::Unauthorized(message) if hosted_error_code(message).is_some_and(|code| code.ends_with("_PLAN_REQUIRED")))
    }

    /// Whether retrying the same call later may succeed.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Unavailable(_))
    }
}

fn hosted_error_code(message: &str) -> Option<&str> {
    let rest = message.strip_prefix('[')?;
    let (code, _) = rest.split_once("] ")?;
    (!code.is_empty()
        && code
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
    .then_some(code)
}

/// The crate-wide result alias.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
