//! The migration's error: an engine failure, or something the host reported.

use tinymemory_api::Error as EngineError;

/// Why a migration step failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// The engine refused or failed.
    #[error(transparent)]
    Engine(#[from] EngineError),
    /// Memory is off: no usable engine, or nobody signed in to move for.
    #[error("memory is off: {0}")]
    Off(String),
    /// The request was malformed or refused (for example a run is going).
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// The saved state could not be read or written.
    #[error("engine error: {0}")]
    State(String),
}

/// Result alias for migration steps.
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// Shorthand for [`Self::InvalidRequest`].
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::InvalidRequest(message.into())
    }

    /// The stable code of this failure (never content), as the host's wire
    /// taxonomy names it.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Off(_) => "MEMORY_OFF",
            Self::InvalidRequest(_) => "INVALID_REQUEST",
            Self::State(_) => "ENGINE",
            Self::Engine(error) => error.code(),
        }
    }

    /// Whether this is known to refuse every request on the account, not just
    /// this one: memory off, a rejected credential, no credits, or a plan
    /// that excludes the feature. An
    /// [`EngineError::Unavailable`] may be request-scoped, so it does not stop
    /// a loop over items from trying the remaining items.
    #[must_use]
    pub fn is_account_wide(&self) -> bool {
        match self {
            Self::Off(_) => true,
            Self::Engine(error) => error.is_account_wide(),
            Self::InvalidRequest(_) | Self::State(_) => false,
        }
    }
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod tests;
