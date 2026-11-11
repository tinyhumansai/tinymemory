//! Secret and PII scrubbing for items and engine writes.
//!
//! The scrubbers themselves (`sanitize_text_with`, `sanitize_json_with`,
//! [`Policy`] and the PII module) live in the standalone `tinyredact` crate,
//! which has no memory dependency; this module re-exports them so a memory host
//! has one import, and adds [`scrub_item`], the only part that needs the
//! `StoreItem` type. [`ScrubbingEngine`] applies that policy to every write;
//! the Cortex registry's [`crate::registry::binder::EngineBinder`] always uses
//! it, including for host-installed and transient engines. [`TimedEngine`]
//! adds one content-free timing line per operation.

mod item;
mod scrubbing;
mod timing;

pub use item::{scrub_item, scrub_item_with};
pub use scrubbing::ScrubbingEngine;
pub use timing::TimedEngine;
pub use tinyredact::{
    BareCardGate, Policy, SanitizationReport, Sanitized, has_likely_email, has_likely_pii,
    has_likely_secret, pii, redact_credential_markers, sanitize_json, sanitize_json_with,
    sanitize_text, sanitize_text_with,
};

#[cfg(test)]
#[path = "decorator_tests.rs"]
mod decorator_tests;
