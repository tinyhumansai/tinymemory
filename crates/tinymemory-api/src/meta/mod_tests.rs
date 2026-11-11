//! Metadata serialisation and defaults.

use chrono::{TimeZone, Utc};

use super::*;

#[test]
fn a_default_meta_names_the_agent_as_its_source() {
    let meta = MemoryMeta::default();
    assert_eq!(meta.source.kind, SourceKind::Agent);
    assert_eq!(meta.source.id, None);
}

#[test]
fn unset_fields_are_omitted_and_round_trip() {
    let mut meta = MemoryMeta::from_source(SourceKind::Github, Some("src_1".into()));
    meta.repo = Some("owner/name".into());
    meta.tool_call = Some(ToolCallRef {
        name: "grep".into(),
        id: None,
    });
    meta.channel = Some("telegram".into());
    let json = serde_json::to_value(&meta).expect("serialise");
    assert_eq!(
        json,
        serde_json::json!({
            "repo": "owner/name",
            "channel": "telegram",
            "tool_call": { "name": "grep" },
            "source": { "kind": "github", "id": "src_1" }
        })
    );
    let back: MemoryMeta = serde_json::from_value(json).expect("deserialise");
    assert_eq!(back, meta);
}

#[test]
fn host_metadata_overlays_set_fields_and_preserves_unset_fields() {
    let caller = MemoryMeta {
        repo: Some("caller/repo".into()),
        channel: Some("caller-channel".into()),
        tags: vec!["caller-tag".into()],
        ..MemoryMeta::default()
    };
    let host = MemoryMeta {
        repo: Some("host/repo".into()),
        observed_actor: Some(ObservedActor {
            id: "user:+15551234567".into(),
            name: Some("Mum".into()),
        }),
        ..MemoryMeta::default()
    };

    let overlaid = caller.overlay(host);
    assert_eq!(overlaid.repo.as_deref(), Some("host/repo"));
    assert_eq!(overlaid.channel.as_deref(), Some("caller-channel"));
    assert_eq!(overlaid.tags, ["caller-tag"]);
    assert_eq!(
        overlaid.observed_actor,
        Some(ObservedActor {
            id: "user:+15551234567".into(),
            name: Some("Mum".into()),
        })
    );
}

#[test]
fn host_values_replace_namespace_and_source_and_append_tags_when_present() {
    let caller = MemoryMeta {
        namespace: "agent:caller".parse().unwrap(),
        source: SourceRef {
            kind: SourceKind::File,
            id: Some("caller".into()),
        },
        tags: vec!["caller".into()],
        ..MemoryMeta::default()
    };
    let host = MemoryMeta {
        namespace: "agent:host".parse().unwrap(),
        source: SourceRef {
            kind: SourceKind::Conversation,
            id: Some("host".into()),
        },
        tags: vec!["host".into()],
        ..MemoryMeta::default()
    };

    let overlaid = caller.overlay(host);
    assert_eq!(overlaid.namespace, "agent:host".parse().unwrap());
    assert_eq!(overlaid.source.kind, SourceKind::Conversation);
    assert_eq!(overlaid.source.id.as_deref(), Some("host"));
    assert_eq!(overlaid.tags, ["caller", "host"]);
}

#[test]
fn host_root_namespace_and_default_source_are_authoritative() {
    let caller = MemoryMeta {
        namespace: "agent:caller".parse().unwrap(),
        source: SourceRef {
            kind: SourceKind::File,
            id: Some("caller".into()),
        },
        ..MemoryMeta::default()
    };
    let host = MemoryMeta::default();

    let overlaid = caller.overlay(host);
    assert_eq!(overlaid.namespace, Namespace::ROOT);
    assert_eq!(overlaid.source, SourceRef::default());
}

#[test]
fn host_tags_append_once_after_caller_tags() {
    let caller = MemoryMeta {
        tags: vec!["caller".into(), "shared".into()],
        ..MemoryMeta::default()
    };
    let host = MemoryMeta {
        tags: vec!["host".into(), "shared".into()],
        ..MemoryMeta::default()
    };

    let overlaid = caller.overlay(host);
    assert_eq!(overlaid.tags, ["caller", "shared", "host"]);
}

#[test]
fn host_overlay_covers_every_metadata_field() {
    let host = MemoryMeta {
        namespace: "agent:host".parse().unwrap(),
        workspace: Some("host-workspace".into()),
        folder: Some("host-folder".into()),
        file_path: Some("host-file".into()),
        language: Some("host-language".into()),
        repo: Some("host-repo".into()),
        commit: Some("host-commit".into()),
        url: Some("host-url".into()),
        thread_id: Some("host-thread".into()),
        channel: Some("host-channel".into()),
        turns: Some(TurnRange { first: 2, last: 4 }),
        agent_id: Some("host-agent".into()),
        tool_call: Some(ToolCallRef {
            name: "host-tool".into(),
            id: Some("host-call".into()),
        }),
        source: SourceRef {
            kind: SourceKind::Conversation,
            id: Some("host-source".into()),
        },
        tags: vec!["host-tag".into()],
        observed_at: Some(Utc.with_ymd_and_hms(2026, 10, 17, 0, 0, 0).unwrap()),
        derive: Some(false),
        observed_actor: Some(ObservedActor {
            id: "telegram:123456789".into(),
            name: Some("Host sender".into()),
        }),
    };

    assert_eq!(MemoryMeta::default().overlay(host.clone()), host);
}

#[test]
fn channel_sender_actor_ids_keep_host_normalization_rules() {
    assert_eq!(
        ObservedActor::for_channel_sender("whatsapp", "+15551234567", Some(" Mum ")),
        Some(ObservedActor {
            id: "user:+15551234567".into(),
            name: Some("Mum".into()),
        })
    );
    assert_eq!(
        ObservedActor::for_channel_sender("signal", "+1 (555) 123-4567", None)
            .map(|actor| actor.id)
            .as_deref(),
        Some("user:+15551234567")
    );
    assert_eq!(
        ObservedActor::for_channel_sender("SMS", "5551234567", None)
            .map(|actor| actor.id)
            .as_deref(),
        Some("user:5551234567")
    );
    for channel in ["imessage", "linq"] {
        assert_eq!(
            ObservedActor::for_channel_sender(channel, "+1 (555) 123-4567", None)
                .map(|actor| actor.id)
                .as_deref(),
            Some("user:+15551234567"),
            "{channel} identifies senders by phone number"
        );
    }
    assert_eq!(
        ObservedActor::for_channel_sender("imessage", "Priya@Acme.com", None)
            .map(|actor| actor.id)
            .as_deref(),
        Some("user:priya@acme.com")
    );
    assert_eq!(
        ObservedActor::for_channel_sender("email", "priya@acme.com", None)
            .map(|actor| actor.id)
            .as_deref(),
        Some("user:priya@acme.com")
    );
    assert_eq!(
        ObservedActor::for_channel_sender("telegram", "123456789", None)
            .map(|actor| actor.id)
            .as_deref(),
        Some("telegram:123456789")
    );
    assert_eq!(
        ObservedActor::for_channel_sender("whatsapp", "98765432101234@lid", None)
            .map(|actor| actor.id)
            .as_deref(),
        Some("whatsapp:98765432101234@lid")
    );
    assert_eq!(
        ObservedActor::for_channel_sender("slack", "two words", None),
        None
    );
    assert_eq!(
        ObservedActor::for_channel_sender(" ", "+15551234567", None),
        None
    );
    assert_eq!(
        ObservedActor::for_channel_sender("whatsapp", "  ", Some("Mum")),
        None
    );
}

#[test]
fn an_empty_object_deserialises_to_the_default() {
    let meta: MemoryMeta = serde_json::from_str("{}").expect("deserialise");
    assert_eq!(meta, MemoryMeta::default());
}

#[test]
fn metadata_without_a_channel_remains_readable() {
    let meta: MemoryMeta = serde_json::from_value(serde_json::json!({
        "thread_id": "legacy-thread",
        "source": { "kind": "conversation" }
    }))
    .expect("legacy metadata deserialises");
    assert_eq!(meta.thread_id.as_deref(), Some("legacy-thread"));
    assert_eq!(meta.channel, None);
}

#[test]
fn source_kind_wire_strings_match_serde() {
    for kind in SourceKind::ALL {
        let json = serde_json::to_value(kind).expect("serialise");
        assert_eq!(json, serde_json::json!(kind.as_str()));
    }
}

#[test]
fn the_retired_composio_source_kind_decodes_as_import() {
    let kind: SourceKind =
        serde_json::from_value(serde_json::json!("composio")).expect("legacy kind decodes");
    assert_eq!(kind, SourceKind::Import);
    assert_eq!(
        serde_json::to_value(SourceKind::Import).expect("serialise"),
        serde_json::json!("import")
    );
}
