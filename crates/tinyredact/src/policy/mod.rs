//! The scrubber's one tunable and the types every scrubbing pass returns.
//!
//! [`Policy`] carries the single knob the historical copies of this scrubber
//! disagreed on — [`BareCardGate`] — and defaults to the strictest setting.
//! [`Sanitized`] pairs a cleaned value with the [`SanitizationReport`] that
//! counts what was changed to produce it.

/// How a bare (no separators) Luhn-valid 13-19 digit run is judged as a credit
/// card by the content scrubber. Separated runs (`4111 1111 1111 1111`) are
/// always Luhn-gated only. A separated run that fails Luhn as a whole is
/// retried on windows of its digit groups, and a card found that way must be
/// corroborated under either setting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum BareCardGate {
    /// Redact every Luhn-valid run. The strictest behaviour and the default.
    #[default]
    LuhnOnly,
    /// Also require a plausible network IIN at an issued length, or a card
    /// keyword within 64 bytes, so machine identifiers such as 13-digit
    /// epoch-millisecond timestamps are left alone.
    Corroborated,
}

/// Tunables for content scrubbing. The default never redacts less than
/// [`BareCardGate::LuhnOnly`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Policy {
    /// Gate applied to bare credit-card-shaped digit runs.
    pub bare_card: BareCardGate,
}

impl Policy {
    /// The policy the TinyCortex engine has always applied: bare card runs need
    /// corroboration beyond their checksum.
    pub const fn corroborated() -> Self {
        Self {
            bare_card: BareCardGate::Corroborated,
        }
    }
}

/// Tally of what a sanitization pass changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SanitizationReport {
    /// Count of secret/token pattern matches rewritten in string text by the
    /// text-pattern redaction pass.
    pub text_redactions: usize,
    /// Count of JSON object entries dropped wholesale because their key was
    /// classified as sensitive by the key classifier.
    pub key_redactions: usize,
    /// Count of full private-key blocks replaced; these are
    /// the most severe hits since the entire block is removed.
    pub blocked_secret_hits: usize,
    /// Count of nodes collapsed because JSON nesting reached
    /// the JSON traversal depth cap; the subtree is replaced rather than walked.
    pub depth_redactions: usize,
    /// Count of personal-identifier matches replaced by the
    /// lightweight PII screen.
    pub pii_redactions: usize,
}

impl SanitizationReport {
    /// True when any field recorded a redaction.
    pub fn changed(&self) -> bool {
        self.text_redactions > 0
            || self.key_redactions > 0
            || self.blocked_secret_hits > 0
            || self.depth_redactions > 0
            || self.pii_redactions > 0
    }

    /// Sum two reports field-wise.
    pub fn merge(self, rhs: Self) -> Self {
        Self {
            text_redactions: self.text_redactions + rhs.text_redactions,
            key_redactions: self.key_redactions + rhs.key_redactions,
            blocked_secret_hits: self.blocked_secret_hits + rhs.blocked_secret_hits,
            depth_redactions: self.depth_redactions + rhs.depth_redactions,
            pii_redactions: self.pii_redactions + rhs.pii_redactions,
        }
    }
}

/// A sanitized value plus the [`SanitizationReport`] describing the changes.
#[derive(Debug, Clone)]
pub struct Sanitized<T> {
    /// The cleaned value with secrets and PII removed.
    pub value: T,
    /// Tally of what the sanitization pass changed to produce `value`.
    pub report: SanitizationReport,
}
