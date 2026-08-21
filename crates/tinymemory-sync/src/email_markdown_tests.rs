//! Tests for the surrounding module.

use super::*;

/// The engine's canonicaliser emits this exact shape from its own copy of
/// this assembly, and the chunker splits on `---\nFrom:`. A failure here
/// is a coordinated format change, never a local edit.
#[test]
fn thread_markdown_format_is_pinned() {
    let thread = EmailThread {
        provider: "gmail".into(),
        thread_subject: "Hello".into(),
        messages: vec![EmailMessage {
            from: "a@example.com".into(),
            to: vec!["b@example.com".into()],
            cc: Vec::new(),
            subject: "Hello".into(),
            sent_at: DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
                .unwrap()
                .with_timezone(&Utc),
            body: "Hi there".into(),
            source_ref: Some("gmail:m1".into()),
            list_unsubscribe: None,
        }],
    };
    assert_eq!(
        thread_markdown(thread).unwrap(),
        "---\nFrom: a@example.com\nTo: b@example.com\nSubject: Hello\nDate: 2026-01-02T03:04:05+00:00\n\nHi there\n\n"
    );
}

#[test]
fn empty_thread_is_none_and_body_separators_are_escaped() {
    assert!(thread_markdown(EmailThread {
        provider: "gmail".into(),
        thread_subject: String::new(),
        messages: Vec::new(),
    })
    .is_none());

    let thread = EmailThread {
        provider: "gmail".into(),
        thread_subject: "s".into(),
        messages: vec![EmailMessage {
            from: "a".into(),
            to: Vec::new(),
            cc: Vec::new(),
            subject: "s".into(),
            sent_at: DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
                .unwrap()
                .with_timezone(&Utc),
            body: "x\n---\ny".into(),
            source_ref: None,
            list_unsubscribe: None,
        }],
    };
    let md = thread_markdown(thread).unwrap();
    assert!(
        md.contains("\\---"),
        "chunk separator must be escaped: {md}"
    );
}

fn message_json(sent_at: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "from": "sender",
        "subject": "subject",
        "sent_at": sent_at,
        "body": "body"
    })
}

#[test]
fn flexible_timestamp_accepts_rfc3339_and_numeric_or_string_milliseconds() {
    for value in [
        serde_json::json!("2026-01-02T03:04:05Z"),
        serde_json::json!(1_767_323_045_000_i64),
        serde_json::json!("1767323045000"),
    ] {
        let message: EmailMessage = serde_json::from_value(message_json(value)).unwrap();
        assert_eq!(message.sent_at.timestamp_millis(), 1_767_323_045_000);
    }
}

#[test]
fn flexible_timestamp_rejects_seconds_and_malformed_text() {
    for value in [
        serde_json::json!(1_767_322_245_i64),
        serde_json::json!("1767322245"),
        serde_json::json!("last Tuesday"),
    ] {
        let error = serde_json::from_value::<EmailMessage>(message_json(value)).unwrap_err();
        assert!(
            error.to_string().contains("milliseconds")
                || error.to_string().contains("cannot parse"),
            "unexpected error: {error}"
        );
    }
}

#[test]
fn rendering_sorts_oldest_first_and_escapes_header_markdown() {
    let at = |timestamp: &str| {
        DateTime::parse_from_rfc3339(timestamp)
            .unwrap()
            .with_timezone(&Utc)
    };
    let thread = EmailThread {
        provider: "gmail".into(),
        thread_subject: "thread".into(),
        messages: vec![
            EmailMessage {
                from: "*new*".into(),
                to: Vec::new(),
                cc: Vec::new(),
                subject: "[later]".into(),
                sent_at: at("2026-02-01T00:00:00Z"),
                body: "new".into(),
                source_ref: None,
                list_unsubscribe: None,
            },
            EmailMessage {
                from: "_old_".into(),
                to: vec!["a|b".into()],
                cc: vec!["c`d".into()],
                subject: "# first".into(),
                sent_at: at("2026-01-01T00:00:00Z"),
                body: "old".into(),
                source_ref: None,
                list_unsubscribe: Some("<https://example.com/unsub?a=1&b=2>".into()),
            },
        ],
    };

    let markdown = thread_markdown(thread).unwrap();
    assert!(markdown.find("old").unwrap() < markdown.find("new").unwrap());
    assert!(markdown.contains("From: \\_old\\_"));
    assert!(markdown.contains("To: a\\|b"));
    assert!(markdown.contains("Cc: c\\`d"));
    assert!(markdown.contains("Subject: # first"));
    assert!(markdown.contains("List-Unsubscribe: <https://example.com/unsub?a=1&b=2>"));
}

#[test]
fn message_serde_defaults_optional_fields_and_uses_epoch_milliseconds() {
    let before = Utc::now().timestamp_millis();
    let message: EmailMessage = serde_json::from_value(serde_json::json!({
        "from": "sender",
        "subject": "subject",
        "sent_at": null,
        "body": "body"
    }))
    .unwrap();
    let after = Utc::now().timestamp_millis();
    assert!(message.sent_at.timestamp_millis() >= before);
    assert!(message.sent_at.timestamp_millis() <= after);
    assert!(message.to.is_empty());
    assert!(message.cc.is_empty());
    assert!(message.source_ref.is_none());
    assert!(message.list_unsubscribe.is_none());

    let serialized = serde_json::to_value(&message).unwrap();
    assert_eq!(serialized["sent_at"], message.sent_at.timestamp_millis());
}

#[test]
fn empty_cleaned_bodies_still_preserve_message_boundaries() {
    let timestamp = DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
        .unwrap()
        .with_timezone(&Utc);
    let markdown = thread_markdown(EmailThread {
        provider: "gmail".into(),
        thread_subject: "empty body".into(),
        messages: vec![EmailMessage {
            from: "sender".into(),
            to: Vec::new(),
            cc: Vec::new(),
            subject: "empty".into(),
            sent_at: timestamp,
            body: "   \n\t".into(),
            source_ref: None,
            list_unsubscribe: None,
        }],
    })
    .unwrap();
    assert!(markdown.ends_with("\n\n\n"), "{markdown:?}");
}

#[test]
fn flexible_timestamp_rejects_out_of_range_milliseconds() {
    let error = serde_json::from_value::<EmailMessage>(message_json(serde_json::json!(i64::MAX)))
        .unwrap_err();
    assert!(error.to_string().contains("invalid epoch-ms"), "{error}");
}

#[test]
fn thread_shape_round_trips_without_losing_message_metadata() {
    let raw = serde_json::json!({
        "provider": "imap",
        "thread_subject": "subject",
        "messages": [{
            "from": "a@example.com",
            "to": ["b@example.com"],
            "cc": ["c@example.com"],
            "subject": "subject",
            "sent_at": "2026-01-02T03:04:05Z",
            "body": "body",
            "source_ref": "imap:1",
            "list_unsubscribe": "mailto:unsubscribe@example.com"
        }]
    });
    let thread: EmailThread = serde_json::from_value(raw).unwrap();
    let encoded = serde_json::to_value(&thread).unwrap();
    assert_eq!(encoded["provider"], "imap");
    assert_eq!(encoded["messages"][0]["source_ref"], "imap:1");
    assert_eq!(
        encoded["messages"][0]["list_unsubscribe"],
        "mailto:unsubscribe@example.com"
    );
}
