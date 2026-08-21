//! Secret-detection and redaction for memory writes — thin host shim over
//! `crate::engine::backend::store::safety` (W3).
//!
//! The conservative secret + PII scrubbers (`has_likely_secret`,
//! `has_likely_pii`, `sanitize_text`, `sanitize_json`) + the
//! `SanitizationReport`/`Sanitized<T>` types are the crate's — now including the
//! full multilingual national-ID PII module (ported into the crate so the crate
//! `sanitize_text` matches this host's byte-for-byte). The host keeps only
//! [`sanitize_document_input`], which scrubs the host-specific
//! [`NamespaceDocumentInput`] shape by delegating each field to the crate
//! scrubbers. The retained test suite doubles as a byte-parity guard: it asserts
//! the crate scrubber still redacts every secret/PII pattern the host relied on.

pub mod pii;

use crate::store::types::NamespaceDocumentInput;

pub use crate::engine::backend::store::safety::{
    has_likely_pii, has_likely_secret, sanitize_json, sanitize_text, SanitizationReport, Sanitized,
};

/// Canonical storage form of a caller-supplied memory **identifier** — a
/// namespace, a document key, or a KV key.
///
/// An identifier is an address, not content: whatever this returns is what the
/// row is stored under, so every read / update / delete that addresses a row by
/// identifier has to canonicalize through this same function, or it looks up a
/// row the write never created (#5164).
///
/// Two properties make that safe, and both follow the split the crate's PII
/// module documents between its **strict boundary predicate** and its **lenient
/// content scrubber**:
///
/// * **Strict gating.** Only identifiers that trip [`has_likely_pii`] —
///   formatted / keyword-gated national IDs (`ssn-123-45-6789`,
///   `cliente-RFC-VECJ880326XK4`, `cuit-20-11111111-2`) — are rewritten.
///   `redact_pii` on its own also rewrites bare digit-run shapes, and the
///   scanners legitimately build identifiers out of those: WhatsApp JIDs
///   (`12025551234-1543890267@g.us`), iMessage `+1…` chat ids, millisecond
///   timestamps, padded counters. Rewriting those maps two distinct contacts
///   onto one `(namespace, key)`, where the upsert's `ON CONFLICT … DO UPDATE`
///   has one contact's document silently overwrite the other's.
/// * **Idempotence.** The `[REDACTED_PII_*]` placeholders carry no PII pattern
///   of their own, so canonicalizing an already-canonical identifier is a
///   no-op — which is what lets read paths canonicalize unconditionally.
pub fn canonical_identifier(value: &str) -> String {
    if !has_likely_pii(value) {
        return value.to_string();
    }
    pii::redact_pii(value).value
}

/// Canonical storage form of a document key: the exact transform
/// `upsert_document` / `upsert_document_metadata_only` apply before writing the
/// `memory_docs.key` column (trim, then [`canonical_identifier`]).
///
/// Single-sourced so the by-key read paths (`Memory::get`, `Memory::forget`)
/// cannot drift from the write path. Drift there is invisible — the lookup
/// simply misses, the caller treats the row as absent and writes again, which
/// is the unthrottled loop #5164 was reported for.
pub fn canonical_document_key(key: &str) -> String {
    canonical_identifier(key.trim())
}

/// Scrub a namespace-document input, field by field, via the crate scrubbers.
///
/// Sanitization is content-cleaning only; provenance `taint` survives untouched
/// so the write gate's taint check still sees the real source signal.
pub fn sanitize_document_input(input: NamespaceDocumentInput) -> Sanitized<NamespaceDocumentInput> {
    let mut report = SanitizationReport::default();

    let title = sanitize_text(&input.title);
    report = report.merge(title.report);
    let content = sanitize_text(&input.content);
    report = report.merge(content.report);

    let mut tags = Vec::with_capacity(input.tags.len());
    for tag in input.tags {
        let sanitized = sanitize_text(&tag);
        report = report.merge(sanitized.report);
        tags.push(sanitized.value);
    }

    let metadata = sanitize_json(&input.metadata);
    report = report.merge(metadata.report);

    Sanitized {
        value: NamespaceDocumentInput {
            namespace: input.namespace,
            key: input.key,
            title: title.value,
            content: content.value,
            source_type: input.source_type,
            priority: input.priority,
            tags,
            metadata: metadata.value,
            category: input.category,
            session_id: input.session_id,
            document_id: input.document_id,
            taint: input.taint,
        },
        report,
    }
}

#[cfg(test)]
#[path = "safety_tests.rs"]
mod tests;
