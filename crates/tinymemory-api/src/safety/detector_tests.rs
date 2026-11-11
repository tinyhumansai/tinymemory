//! Classification entry-point and risk-threshold regressions.

use super::*;

#[test]
fn empty_input_is_none() {
    let r = scan("");
    assert_eq!(r.level, RiskLevel::None);
    assert_eq!(r.score, 0);
    assert!(r.categories.is_empty());
    assert!(!r.is_sensitive());
}

#[test]
fn level_thresholds_are_monotonic() {
    assert_eq!(level_from_score(0), RiskLevel::None);
    assert_eq!(level_from_score(1), RiskLevel::Low);
    assert_eq!(level_from_score(19), RiskLevel::Low);
    assert_eq!(level_from_score(20), RiskLevel::Medium);
    assert_eq!(level_from_score(44), RiskLevel::Medium);
    assert_eq!(level_from_score(45), RiskLevel::High);
    assert_eq!(level_from_score(1000), RiskLevel::High);
}

#[test]
fn distinct_spans_folds_overlaps_and_keeps_separate_values() {
    assert_eq!(distinct_spans(vec![]), 0);
    assert_eq!(distinct_spans(vec![(5, 20)]), 1);
    // Two rules over one value, one of them a sub-span of the other.
    assert_eq!(distinct_spans(vec![(6, 20), (5, 20)]), 1);
    // Partial overlap is still one value.
    assert_eq!(distinct_spans(vec![(0, 10), (8, 15)]), 1);
    // Touching and separate ranges are distinct values.
    assert_eq!(distinct_spans(vec![(0, 10), (10, 20)]), 2);
    assert_eq!(distinct_spans(vec![(30, 40), (0, 10), (12, 20)]), 3);
}
