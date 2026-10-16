//! Classifier entry point for the local PII detector.
//!
//! [`scan`] is a pure, synchronous function: it walks the compiled rule set
//! (see [`super::rules`]) over the input, tallies matches per category, and
//! folds the tally into a [`PiiScanResult`]. No async, no I/O, no network.

use std::collections::BTreeMap;

use super::rules::rules;
use super::types::{CategoryHit, PiiCategory, PiiScanResult, RiskLevel};

/// Extra points added per additional co-occurring category. Multiple distinct
/// identifier/context categories in one blob look more like a real record than
/// any single one alone, so co-occurrence nudges the score upward.
const CO_OCCURRENCE_BONUS: u32 = 5;

/// Map a raw score to a [`RiskLevel`]. Thresholds lean toward escalation
/// (recall over precision).
fn level_from_score(score: u32) -> RiskLevel {
    match score {
        0 => RiskLevel::None,
        1..=19 => RiskLevel::Low,
        20..=44 => RiskLevel::Medium,
        _ => RiskLevel::High,
    }
}

/// Scan `content` for identification risk.
///
/// Returns a [`PiiScanResult`] carrying the overall [`RiskLevel`], a numeric
/// score, and the distinct categories detected (with per-category counts).
///
/// Detection runs **fully locally** — pattern + keyword matching only. The
/// function is deterministic and side-effect-free, so it is safe to call from
/// any context (sync or async, on or off a Tokio runtime).
///
/// ## Scoring
/// * Each distinct category contributes its fixed category weight once.
/// * Each additional co-occurring category adds the fixed co-occurrence bonus.
/// * A "strong identifier" (SSN, card, passport, bank account) forces
///   [`RiskLevel::High`] regardless of the numeric score.
pub fn scan(content: &str) -> PiiScanResult {
    // BTreeMap keeps the output deterministic (categories emitted in enum
    // order) without a separate sort pass.
    //
    // Spans rather than a running count: two rules in one category can match
    // the same value (`+1 415-555-0100` is both a US-format and an
    // international phone number), and a count is of identifiers, not of
    // rules, so overlapping spans are folded into one before they are counted.
    let mut spans: BTreeMap<PiiCategory, Vec<(usize, usize)>> = BTreeMap::new();

    for rule in rules() {
        for m in rule.regex.find_iter(content) {
            if let Some(validator) = rule.validator
                && !validator(m.as_str())
            {
                continue;
            }
            spans
                .entry(rule.category)
                .or_default()
                .push((m.start(), m.end()));
        }
    }

    if spans.is_empty() {
        return PiiScanResult::default();
    }
    let counts: BTreeMap<PiiCategory, usize> = spans
        .into_iter()
        .map(|(category, spans)| (category, distinct_spans(spans)))
        .collect();

    let distinct = counts.len() as u32;
    let mut score: u32 = counts.keys().map(|c| c.weight()).sum();
    if distinct > 1 {
        score += CO_OCCURRENCE_BONUS * (distinct - 1);
    }

    let has_strong = counts.keys().any(|c| c.is_strong_identifier());
    let mut level = level_from_score(score);
    if has_strong && level < RiskLevel::High {
        level = RiskLevel::High;
    }

    let categories: Vec<PiiCategory> = counts.keys().copied().collect();
    let hits: Vec<CategoryHit> = counts
        .iter()
        .map(|(&category, &count)| CategoryHit { category, count })
        .collect();

    PiiScanResult {
        level,
        score,
        categories,
        hits,
    }
}

/// The number of distinct values among `spans`: byte ranges that overlap are
/// one value seen by more than one rule, ranges that merely touch or stand
/// apart are separate values. The same value written twice is two.
fn distinct_spans(mut spans: Vec<(usize, usize)>) -> usize {
    spans.sort_unstable();
    let mut count = 0;
    let mut covered_to = 0;
    for (start, end) in spans {
        if count == 0 || start >= covered_to {
            count += 1;
        }
        covered_to = covered_to.max(end);
    }
    count
}

/// Replacement for every value [`redact_identifiers`] removes.
pub const REDACTED_IDENTIFIER: &str = "[redacted]";

/// Replace every **structured identifier** in `content` (email, phone number,
/// national id, payment card, IBAN, IPv4 address, postal address) with
/// [`REDACTED_IDENTIFIER`].
///
/// Uses the same rules (and validators) as [`scan`], so what is redacted is
/// exactly what would be counted. Topical keyword categories (medical, legal,
/// ...) describe context rather than identify anyone and are left in place.
/// For text shown back to a user from a source that may quote someone's
/// details, such as a tool's error line.
pub fn redact_identifiers(content: &str) -> String {
    let mut out = content.to_string();
    for rule in rules() {
        if !rule.category.is_structured_identifier() {
            continue;
        }
        let redacted = rule.regex.replace_all(&out, |caps: &regex::Captures<'_>| {
            let matched = &caps[0];
            match rule.validator {
                Some(validator) if !validator(matched) => matched.to_string(),
                _ => REDACTED_IDENTIFIER.to_string(),
            }
        });
        if let std::borrow::Cow::Owned(changed) = redacted {
            out = changed;
        }
    }
    out
}

#[cfg(test)]
#[path = "detector_tests.rs"]
mod tests;
