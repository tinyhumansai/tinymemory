//! Secret and PII scrubbing for anything a host persists or hands on.
//!
//! Conservative by design — it prefers false positives over leaking
//! credentials into long-lived stores. One copy of this policy is shared by the
//! memory engines and the OpenHuman host; it used to exist three times. The
//! design, what is blocked versus redacted, and the known limits are in this
//! module's `README.md`.
//!
//! This crate owns the scrubbers (`sanitize_text_with`, `sanitize_json_with`,
//! [`Policy`]) for every caller: approvals, artifact offload and agent
//! middleware use it without any memory dependency. Scrubbing a
//! `StoreItem` (`scrub_item`) stays in `tinymemory-integrations`, which depends
//! on this crate.
//!
//! The exhaustive multilingual national-ID PII module ([`pii`]) runs as part
//! of [`sanitize_text`]. The write-rejection boundary ([`has_likely_pii`])
//! stays stricter than content scrubbing: formatted national IDs are rejected,
//! while phone-like text is scrubbed from content without rejecting every
//! write that mentions it. Email addresses are only detected
//! ([`has_likely_email`]), never redacted from content.
//!
//! Before the shape regexes, [`sanitize_text`] redacts the value after a
//! credential *marker* — a one-time-secret URL's `/secret/<key>` and a `Bearer`
//! value too short for the regexes — keeping the marker and the prose around
//! it. [`redact_credential_markers`] runs just those rules, for a host that
//! scrubs plain text without the PII pass.
//!
//! # The one policy knob
//!
//! The previous copies differed in exactly one behaviour: how a *bare*
//! (separator-less) Luhn-valid 13-19 digit run is treated as a credit card.
//! The OpenHuman host redacted every such run; TinyCortex additionally demanded
//! corroboration (a real network IIN at an issued length, or a card keyword
//! nearby) so 13-digit epoch-millisecond timestamps in stored JSON envelopes
//! stopped being corrupted (opencompany#1201). [`BareCardGate`] names both and
//! the plain functions default to the stricter [`BareCardGate::LuhnOnly`], so no
//! caller that does not opt in redacts less than before. Callers that want the
//! corroborated behaviour use the `*_with` variants and [`Policy::corroborated`].

mod markers;
mod paths;
mod pattern;
/// Exhaustive checksum-gated multilingual national-ID PII module. Content
/// scrubbing runs from [`sanitize_text`]; the boundary check is re-exported as
/// [`has_likely_pii`].
pub mod pii;
mod policy;
mod sanitize;

pub use markers::redact_credential_markers;
pub use paths::redact_home_paths;
pub use pii::{has_likely_email, has_likely_pii};
pub use policy::{BareCardGate, Policy, SanitizationReport, Sanitized};
pub use sanitize::{
    has_likely_secret, sanitize_json, sanitize_json_with, sanitize_text, sanitize_text_with,
};
