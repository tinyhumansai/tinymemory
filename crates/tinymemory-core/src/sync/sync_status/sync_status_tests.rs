//! Tests for the surrounding module.

use super::*;

#[test]
fn freshness_thresholds_match_the_engine() {
    let now = 10_000_000;
    assert_eq!(FreshnessLabel::from_age_ms(None, now), FreshnessLabel::Idle);
    assert_eq!(
        FreshnessLabel::from_age_ms(Some(now - 30_000), now),
        FreshnessLabel::Active
    );
    assert_eq!(
        FreshnessLabel::from_age_ms(Some(now - 30_001), now),
        FreshnessLabel::Recent
    );
    assert_eq!(
        FreshnessLabel::from_age_ms(Some(now - 300_001), now),
        FreshnessLabel::Idle
    );
}
