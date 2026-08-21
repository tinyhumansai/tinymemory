//! Query-side NLP for the deterministic (E2GraphRAG) retriever.
//!
//! [`extract_query_entities`] turns a natural-language query into a set of
//! canonical entity ids that key into `mem_tree_entity_index` and the
//! co-occurrence graph. It prefers the runtime Python server's spaCy backend
//! (named entities +
//! salient nouns) and falls back to the in-Rust regex extractor whenever
//! spaCy is disabled or unavailable — so retrieval always works offline, just
//! with lower person/org recall.
//!
//! Output is intentionally `Vec<CanonicalEntity>`: it reuses
//! `score::resolver::canonicalise` so query entity ids land in the exact
//! same `<kind>:<value>` namespace as the indexed chunk entities. No id
//! mismatch, no bespoke join.

// Provisioning (`ensure_spacy`, `spacy_provisioned`, the model id) stayed in
// the host — it downloads a Python toolchain and supervises a server. Only the
// extraction call and its wire types cross the seam.
pub use crate::nlp_host::{SpacyEntity, SpacyResponse};

#[cfg(test)]
use tinymemory_api::host::test_support::TestHostConfig;

use crate::tree::score::extract::{EntityKind, ExtractedEntities, ExtractedEntity, ExtractedTopic};
use crate::tree::score::resolver::{canonicalise, CanonicalEntity};
use crate::Config;

/// Map a spaCy entity label to our [`EntityKind`]. Unknown labels collapse to
/// [`EntityKind::Misc`] so they still participate as graph anchors.
fn map_spacy_label(label: &str) -> EntityKind {
    match label {
        "PERSON" => EntityKind::Person,
        "ORG" | "NORP" => EntityKind::Organization,
        "GPE" | "LOC" | "FAC" => EntityKind::Location,
        "PRODUCT" => EntityKind::Product,
        "EVENT" => EntityKind::Event,
        "DATE" | "TIME" => EntityKind::Datetime,
        "MONEY" | "QUANTITY" | "PERCENT" | "CARDINAL" | "ORDINAL" => EntityKind::Quantity,
        "LANGUAGE" => EntityKind::Technology,
        _ => EntityKind::Misc,
    }
}

/// Extract canonical query entities, preferring spaCy and falling back to the
/// in-Rust regex extractor. Never fails: an unavailable sidecar degrades to
/// the fallback rather than erroring, because an empty/partial entity set just
/// routes retrieval toward the global (dense) branch.
pub async fn extract_query_entities(config: &Config, query: &str) -> Vec<CanonicalEntity> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    if config.memory_tree().spacy_enabled {
        match crate::nlp_host::extract_spacy(config, trimmed).await {
            Ok(resp) => {
                let extracted = spacy_to_extracted(&resp);
                let canon = canonicalise(&extracted);
                log::debug!(
                    "[memory_tree::nlp] spaCy query extraction: entities={} nouns={} canonical={}",
                    resp.entities.len(),
                    resp.nouns.len(),
                    canon.len()
                );
                return canon;
            }
            Err(e) => {
                log::warn!("[memory_tree::nlp] spaCy extraction failed, falling back: {e:#}");
            }
        }
    } else {
        log::debug!("[memory_tree::nlp] spaCy disabled by config — using regex fallback");
    }

    fallback_extract(trimmed).await
}

/// Build [`ExtractedEntities`] from a spaCy response: named entities become
/// entity spans, salient nouns become topics. Topics are promoted to
/// `topic:<noun>` canonical ids by [`canonicalise`].
fn spacy_to_extracted(resp: &SpacyResponse) -> ExtractedEntities {
    let entities = resp
        .entities
        .iter()
        .map(|e| ExtractedEntity {
            kind: map_spacy_label(&e.label),
            text: e.text.clone(),
            span_start: e.start,
            span_end: e.end,
            score: 1.0,
        })
        .collect();
    let topics = resp
        .nouns
        .iter()
        .map(|n| ExtractedTopic {
            label: n.clone(),
            score: 1.0,
        })
        .collect();
    ExtractedEntities {
        entities,
        topics,
        llm_importance: None,
        llm_importance_reason: None,
    }
}

/// Regex-only fallback. Deterministic, no network, no LLM — catches
/// emails/urls/handles/hashtags in the query. Person/org recall is lost
/// (spaCy's job), which simply biases retrieval toward the global branch.
async fn fallback_extract(query: &str) -> Vec<CanonicalEntity> {
    use crate::tree::score::extract::{CompositeExtractor, EntityExtractor};
    let extractor = CompositeExtractor::regex_only();
    match extractor.extract(query).await {
        Ok(extracted) => {
            let canon = canonicalise(&extracted);
            log::debug!(
                "[memory_tree::nlp] regex fallback query extraction: canonical={}",
                canon.len()
            );
            canon
        }
        Err(e) => {
            log::warn!("[memory_tree::nlp] regex fallback failed: {e:#}");
            Vec::new()
        }
    }
}

#[cfg(test)]
#[path = "nlp_tests.rs"]
mod tests;
