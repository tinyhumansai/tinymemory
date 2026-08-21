//! Tests for the surrounding module.

use super::*;
use std::time::{SystemTime, UNIX_EPOCH};

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn make_candidate(value: &str) -> LearningCandidate {
    LearningCandidate {
        class: FacetClass::Style,
        key: "verbosity".into(),
        value: value.into(),
        cue_family: CueFamily::Explicit,
        evidence: EvidenceRef::Episodic { episodic_id: 1 },
        initial_confidence: 0.8,
        observed_at: now_secs(),
    }
}

#[test]
fn push_then_drain_preserves_fifo_order() {
    let buf = Buffer::new(10);
    buf.push(make_candidate("a"));
    buf.push(make_candidate("b"));
    buf.push(make_candidate("c"));

    let drained = buf.drain();
    assert_eq!(drained.len(), 3);
    assert_eq!(drained[0].value, "a");
    assert_eq!(drained[1].value, "b");
    assert_eq!(drained[2].value, "c");
}

#[test]
fn drain_empties_the_buffer() {
    let buf = Buffer::new(10);
    buf.push(make_candidate("x"));
    buf.push(make_candidate("y"));
    assert_eq!(buf.len(), 2);

    let _ = buf.drain();
    assert_eq!(buf.len(), 0);
    assert!(buf.is_empty());
}

#[test]
fn bounded_capacity_evicts_oldest() {
    let buf = Buffer::new(3);
    buf.push(make_candidate("first"));
    buf.push(make_candidate("second"));
    buf.push(make_candidate("third"));
    // Buffer is full — next push evicts "first"
    buf.push(make_candidate("fourth"));

    assert_eq!(buf.len(), 3);
    let items = buf.drain();
    assert_eq!(items[0].value, "second");
    assert_eq!(items[1].value, "third");
    assert_eq!(items[2].value, "fourth");
}

#[test]
fn peek_does_not_remove() {
    let buf = Buffer::new(10);
    buf.push(make_candidate("p"));
    buf.push(make_candidate("q"));

    let peeked = buf.peek();
    assert_eq!(peeked.len(), 2);
    // Buffer still holds the items
    assert_eq!(buf.len(), 2);

    let drained = buf.drain();
    assert_eq!(drained[0].value, "p");
    assert_eq!(drained[1].value, "q");
}

#[test]
fn cue_family_weight_values() {
    assert_eq!(CueFamily::Explicit.weight(), 1.0);
    assert_eq!(CueFamily::Structural.weight(), 0.9);
    assert_eq!(CueFamily::Behavioral.weight(), 0.7);
    assert_eq!(CueFamily::Recurrence.weight(), 0.6);
}

#[test]
fn roundtrip_serde_evidence_ref() {
    let cases: Vec<EvidenceRef> = vec![
        EvidenceRef::Episodic { episodic_id: 42 },
        EvidenceRef::EpisodicWindow {
            from_id: 10,
            to_id: 20,
        },
        EvidenceRef::SourceSummary {
            summary_id: "sum-abc".into(),
        },
        EvidenceRef::TreeTopic {
            topic_id: "topic-xyz".into(),
        },
        EvidenceRef::DocumentChunk {
            source_id: "notion:page1".into(),
            chunk_id: "chunk-001".into(),
        },
        EvidenceRef::EmailMessage {
            source_id: "gmail:user@example.com".into(),
            message_id: "<abc123@mail.gmail.com>".into(),
        },
        EvidenceRef::Provider {
            toolkit: "gmail".into(),
            connection_id: "conn-1".into(),
            field: "display_name".into(),
        },
        EvidenceRef::ToolCall {
            tool_name: "write_file".into(),
            episodic_id: 99,
        },
        EvidenceRef::TreeSourceWeight {
            window_label: "2026-W18".into(),
        },
    ];

    for ev in &cases {
        let json = serde_json::to_string(ev).expect("serialize failed");
        let back: EvidenceRef = serde_json::from_str(&json).expect("deserialize failed");
        assert_eq!(ev, &back, "round-trip failed for variant: {json}");
    }
}

#[test]
fn global_returns_same_instance_across_calls() {
    let a = global() as *const Buffer;
    let b = global() as *const Buffer;
    assert_eq!(a, b, "global() must return the same static instance");
}
