//! Tests for the persisted [`super::EvidenceRef`] representation.

use super::EvidenceRef;

#[test]
fn every_evidence_variant_round_trips_with_its_stable_discriminator(
) -> Result<(), serde_json::Error> {
    let cases = [
        (EvidenceRef::Episodic { episodic_id: 42 }, "episodic"),
        (
            EvidenceRef::EpisodicWindow {
                from_id: 1,
                to_id: 9,
            },
            "episodic_window",
        ),
        (
            EvidenceRef::SourceSummary {
                summary_id: "sum-1".into(),
            },
            "source_summary",
        ),
        (
            EvidenceRef::TreeTopic {
                topic_id: "topic-1".into(),
            },
            "tree_topic",
        ),
        (
            EvidenceRef::DocumentChunk {
                source_id: "source-1".into(),
                chunk_id: "chunk-1".into(),
            },
            "document_chunk",
        ),
        (
            EvidenceRef::EmailMessage {
                source_id: "mailbox-1".into(),
                message_id: "message-1".into(),
            },
            "email_message",
        ),
        (
            EvidenceRef::Provider {
                toolkit: "github".into(),
                connection_id: "conn-1".into(),
                field: "login".into(),
            },
            "provider",
        ),
        (
            EvidenceRef::ToolCall {
                tool_name: "search".into(),
                episodic_id: 7,
            },
            "tool_call",
        ),
        (
            EvidenceRef::TreeSourceWeight {
                window_label: "recent".into(),
            },
            "tree_source_weight",
        ),
    ];

    for (evidence, discriminator) in cases {
        let value = serde_json::to_value(&evidence)?;
        assert_eq!(value["type"], discriminator);
        assert_eq!(serde_json::from_value::<EvidenceRef>(value)?, evidence);
    }
    Ok(())
}

#[test]
fn provider_evidence_pins_every_persisted_json_field() -> Result<(), serde_json::Error> {
    let evidence = EvidenceRef::Provider {
        toolkit: "github".into(),
        connection_id: "conn-7".into(),
        field: "login".into(),
    };
    let literal = serde_json::json!({
        "type": "provider",
        "toolkit": "github",
        "connection_id": "conn-7",
        "field": "login"
    });

    assert_eq!(serde_json::to_value(&evidence)?, literal);
    assert_eq!(serde_json::from_value::<EvidenceRef>(literal)?, evidence);
    Ok(())
}

#[test]
fn unknown_evidence_discriminators_fail_closed() {
    let error = serde_json::from_value::<EvidenceRef>(serde_json::json!({
        "type": "future_untrusted_kind",
        "content": "must not be guessed"
    }));
    assert!(error.is_err());
}
