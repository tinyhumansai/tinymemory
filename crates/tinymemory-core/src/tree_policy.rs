//! Tree policy layer.
//!
//! `tree` itself stays generic: summaries, buffers, sealing, and storage.
//! Flavor-specific tuning (global cadence, topic hotness thresholds, source
//! label policy) is centralized here so per-flavor modules don't each own
//! their own scattered constants and arithmetic.

use crate::store::trees::types::{
    EntityIndexStats, TOPIC_ARCHIVE_THRESHOLD, TOPIC_CREATION_THRESHOLD, TOPIC_RECHECK_EVERY,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreePolicy {
    Source,
    Topic,
    Global,
}

impl TreePolicy {
    pub fn global() -> Self {
        Self::Global
    }

    pub fn topic() -> Self {
        Self::Topic
    }

    pub fn source() -> Self {
        Self::Source
    }

    pub fn topic_creation_threshold(self) -> f32 {
        let _ = self;
        TOPIC_CREATION_THRESHOLD
    }

    pub fn topic_archive_threshold(self) -> f32 {
        let _ = self;
        TOPIC_ARCHIVE_THRESHOLD
    }

    pub fn topic_recheck_every(self) -> u32 {
        let _ = self;
        TOPIC_RECHECK_EVERY
    }

    pub fn topic_hotness(self, entity_id: &str, idx: &EntityIndexStats, now_ms: i64) -> f32 {
        let _ = self;
        let mention_weight = ((idx.mention_count_30d as f32) + 1.0).ln();
        let source_weight = (idx.distinct_sources as f32) * 0.5;
        let recency_weight = self.topic_recency_decay(idx.last_seen_ms, now_ms);
        let centrality = idx.graph_centrality.unwrap_or(0.0);
        let query_weight = (idx.query_hits_30d as f32) * 2.0;

        let total = mention_weight + source_weight + recency_weight + centrality + query_weight;
        log::debug!(
            "[tree_topic::hotness] id={} mentions={} sources={} recency={:.3} centrality={:.3} \
             queries={} total={:.3}",
            crate::util::redact::redact(entity_id),
            idx.mention_count_30d,
            idx.distinct_sources,
            recency_weight,
            centrality,
            idx.query_hits_30d,
            total
        );
        total
    }

    pub fn topic_recency_decay(self, last_seen_ms: Option<i64>, now_ms: i64) -> f32 {
        let _ = self;
        let Some(last_seen) = last_seen_ms else {
            return 0.0;
        };
        let age_ms = (now_ms - last_seen).max(0);
        const DAY_MS: i64 = 24 * 60 * 60 * 1_000;
        let age_days = (age_ms as f32) / (DAY_MS as f32);

        if age_days <= 1.0 {
            1.0
        } else if age_days <= 7.0 {
            let frac = (age_days - 1.0) / 6.0;
            1.0 - 0.5 * frac
        } else if age_days <= 30.0 {
            let frac = (age_days - 7.0) / 23.0;
            0.5 - 0.5 * frac
        } else {
            0.0
        }
    }
}

#[cfg(test)]
#[path = "tree_policy_tests.rs"]
mod tests;
