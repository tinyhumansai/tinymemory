//! Tests for the surrounding module.

use super::*;
use serde_json::json;

#[test]
fn connection_is_active_matches_ui_status_normalization() {
    for status in ["ACTIVE", "CONNECTED", "active", "connected", " connected "] {
        let conn = ComposioConnection {
            id: "c1".into(),
            toolkit: "slack".into(),
            status: status.into(),
            created_at: None,
            account_email: None,
            workspace: None,
            username: None,
        };
        assert!(conn.is_active(), "status {status:?} should be active");
    }

    for status in ["PENDING", "INITIATED", "FAILED", ""] {
        let conn = ComposioConnection {
            id: "c1".into(),
            toolkit: "slack".into(),
            status: status.into(),
            created_at: None,
            account_email: None,
            workspace: None,
            username: None,
        };
        assert!(!conn.is_active(), "status {status:?} should not be active");
    }
}

#[test]
fn connection_normalizes_toolkit_for_runtime_matching() {
    let conn = ComposioConnection {
        id: "c1".into(),
        toolkit: " Slack ".into(),
        status: "ACTIVE".into(),
        created_at: None,
        account_email: None,
        workspace: None,
        username: None,
    };
    assert_eq!(conn.normalized_toolkit(), "slack");
}

#[test]
fn toolkits_response_defaults_to_empty() {
    let resp: ComposioToolkitsResponse = serde_json::from_str("{}").unwrap();
    assert!(resp.toolkits.is_empty());
}

#[test]
fn toolkits_response_roundtrips() {
    let resp = ComposioToolkitsResponse {
        toolkits: vec!["gmail".into(), "notion".into()],
        ..Default::default()
    };
    let value = serde_json::to_value(&resp).unwrap();
    // Empty catalog is skipped on the wire — back-compat with old cores.
    assert_eq!(value, json!({ "toolkits": ["gmail", "notion"] }));
    let back: ComposioToolkitsResponse = serde_json::from_value(value).unwrap();
    assert_eq!(back.toolkits, vec!["gmail", "notion"]);
    assert!(back.catalog.is_empty());
}

#[test]
fn toolkits_response_forwards_catalog() {
    // A backend that sends the dynamic catalog must deserialize and
    // re-serialize verbatim so the field reaches the desktop UI.
    let raw = json!({
        "toolkits": ["gmail"],
        "catalog": [
            {
                "slug": "gmail",
                "name": "Gmail",
                "logo": "https://logos.composio.dev/api/gmail",
                "description": "Send and read email",
                "categories": ["productivity"],
                "enabled": true
            }
        ]
    });
    let resp: ComposioToolkitsResponse = serde_json::from_value(raw).unwrap();
    assert_eq!(resp.catalog.len(), 1);
    let entry = &resp.catalog[0];
    assert_eq!(entry.slug, "gmail");
    assert_eq!(entry.name, "Gmail");
    assert_eq!(entry.enabled, Some(true));
    assert_eq!(entry.categories, vec!["productivity".to_string()]);

    // Round-trips back out with the catalog intact.
    let value = serde_json::to_value(&resp).unwrap();
    assert_eq!(value["catalog"][0]["slug"], "gmail");
    assert_eq!(value["catalog"][0]["enabled"], true);
}

#[test]
fn connection_parses_and_serializes_camelcase_created_at() {
    let raw = json!({
        "id": "conn_1",
        "toolkit": "gmail",
        "status": "ACTIVE",
        "createdAt": "2026-02-01T00:00:00Z"
    });
    let conn: ComposioConnection = serde_json::from_value(raw.clone()).unwrap();
    assert_eq!(conn.id, "conn_1");
    assert_eq!(conn.toolkit, "gmail");
    assert_eq!(conn.status, "ACTIVE");
    assert_eq!(conn.created_at.as_deref(), Some("2026-02-01T00:00:00Z"));

    // Round-trip must use camelCase too.
    let serialized = serde_json::to_value(&conn).unwrap();
    assert!(serialized.get("createdAt").is_some());
}

#[test]
fn connection_without_created_at_omits_field_when_serialized() {
    let conn = ComposioConnection {
        id: "x".into(),
        toolkit: "notion".into(),
        status: "PENDING".into(),
        created_at: None,
        account_email: None,
        workspace: None,
        username: None,
    };
    let s = serde_json::to_value(&conn).unwrap();
    assert!(
        s.get("createdAt").is_none(),
        "createdAt must be skipped when None"
    );
}

#[test]
fn authorize_response_uses_camelcase_keys() {
    let raw = json!({
        "connectUrl": "https://composio.dev/oauth/abc",
        "connectionId": "conn_2"
    });
    let resp: ComposioAuthorizeResponse = serde_json::from_value(raw).unwrap();
    assert_eq!(resp.connect_url, "https://composio.dev/oauth/abc");
    assert_eq!(resp.connection_id, "conn_2");

    let s = serde_json::to_value(&resp).unwrap();
    assert!(s.get("connectUrl").is_some());
    assert!(s.get("connectionId").is_some());
}

#[test]
fn tool_schema_defaults_type_field_to_function() {
    let raw = json!({
        "function": {
            "name": "GMAIL_SEND_EMAIL",
            "description": "Send an email",
            "parameters": { "type": "object" }
        }
    });
    let tool: ComposioToolSchema = serde_json::from_value(raw).unwrap();
    assert_eq!(tool.kind, "function");
    assert_eq!(tool.function.name, "GMAIL_SEND_EMAIL");
    assert_eq!(tool.function.description.as_deref(), Some("Send an email"));
    assert!(tool.function.parameters.is_some());
}

#[test]
fn tool_function_tolerates_missing_description_and_parameters() {
    let raw = json!({ "function": { "name": "SLUG_ONLY" } });
    let tool: ComposioToolSchema = serde_json::from_value(raw).unwrap();
    assert_eq!(tool.function.name, "SLUG_ONLY");
    assert!(tool.function.description.is_none());
    assert!(tool.function.parameters.is_none());
}

#[test]
fn execute_response_parses_cost_and_error() {
    let raw = json!({
        "data": { "messageId": "m-1" },
        "successful": true,
        "error": null,
        "costUsd": 0.0025
    });
    let resp: ComposioExecuteResponse = serde_json::from_value(raw).unwrap();
    assert!(resp.successful);
    assert!(resp.error.is_none());
    assert!((resp.cost_usd - 0.0025).abs() < f64::EPSILON);
}

#[test]
fn execute_response_defaults_when_fields_missing() {
    let resp: ComposioExecuteResponse = serde_json::from_str("{}").unwrap();
    assert!(!resp.successful);
    assert!(resp.error.is_none());
    assert_eq!(resp.cost_usd, 0.0);
    assert!(resp.data.is_null());
}

#[test]
fn available_trigger_deserializes_and_serializes_camelcase_fields() {
    let raw = json!({
        "slug": "GMAIL_NEW_GMAIL_MESSAGE",
        "scope": "static",
        "defaultConfig": { "labelIds": ["INBOX"] },
        "requiredConfigKeys": ["labelIds"],
        "repo": { "owner": "acme", "repo": "inbox" }
    });
    let trigger: ComposioAvailableTrigger = serde_json::from_value(raw).unwrap();
    assert_eq!(trigger.slug, "GMAIL_NEW_GMAIL_MESSAGE");
    assert_eq!(trigger.scope, "static");
    assert_eq!(
        trigger.default_config,
        Some(json!({ "labelIds": ["INBOX"] }))
    );
    assert_eq!(
        trigger.required_config_keys,
        Some(vec!["labelIds".to_string()])
    );
    let repo = trigger.repo.as_ref().expect("repo");
    assert_eq!(repo.owner, "acme");
    assert_eq!(repo.repo, "inbox");

    let value = serde_json::to_value(&trigger).unwrap();
    assert!(value.get("defaultConfig").is_some());
    assert!(value.get("requiredConfigKeys").is_some());
}

#[test]
fn active_trigger_parses_connection_id_and_optional_fields() {
    let raw = json!({
        "id": "ti_1",
        "slug": "GMAIL_NEW_GMAIL_MESSAGE",
        "toolkit": "gmail",
        "connectionId": "c-1",
        "triggerConfig": { "labelIds": "INBOX" },
        "state": "active"
    });
    let trigger: ComposioActiveTrigger = serde_json::from_value(raw).unwrap();
    assert_eq!(trigger.id, "ti_1");
    assert_eq!(trigger.slug, "GMAIL_NEW_GMAIL_MESSAGE");
    assert_eq!(trigger.connection_id, "c-1");
    assert_eq!(trigger.trigger_config, Some(json!({"labelIds":"INBOX"})));
    assert_eq!(trigger.state.as_deref(), Some("active"));

    let value = serde_json::to_value(&trigger).unwrap();
    assert!(value.get("connectionId").is_some());
    assert!(value.get("triggerConfig").is_some());
    assert!(value.get("state").is_some());
}

#[test]
fn trigger_enable_response_uses_camelcase_and_optional_defaults() {
    let raw = json!({
        "triggerId": "ti_9",
        "slug": "GMAIL_NEW_GMAIL_MESSAGE",
        "connectionId": "c-9"
    });
    let resp: ComposioEnableTriggerResponse = serde_json::from_value(raw).unwrap();
    assert_eq!(resp.trigger_id, "ti_9");
    assert_eq!(resp.slug, "GMAIL_NEW_GMAIL_MESSAGE");
    assert_eq!(resp.connection_id, "c-9");

    let serialized = serde_json::to_value(&resp).unwrap();
    assert_eq!(serialized.get("triggerId").unwrap(), "ti_9");
    assert_eq!(serialized.get("connectionId").unwrap(), "c-9");
}

#[test]
fn delete_trigger_response_defaults_deleted_to_false() {
    let raw = json!({});
    let resp: ComposioDisableTriggerResponse = serde_json::from_value(raw).unwrap();
    assert!(!resp.deleted);
}

#[test]
fn trigger_event_defaults_empty_fields_to_empty_strings() {
    let ev: ComposioTriggerEvent = serde_json::from_str("{}").unwrap();
    assert_eq!(ev.toolkit, "");
    assert_eq!(ev.trigger, "");
    assert_eq!(ev.metadata.id, "");
    assert_eq!(ev.metadata.uuid, "");
    assert!(ev.payload.is_null());
}

#[test]
fn trigger_event_parses_full_payload() {
    let raw = json!({
        "toolkit": "gmail",
        "trigger": "GMAIL_NEW_GMAIL_MESSAGE",
        "payload": { "subject": "hi" },
        "metadata": { "id": "evt-1", "uuid": "uuid-1" }
    });
    let ev: ComposioTriggerEvent = serde_json::from_value(raw).unwrap();
    assert_eq!(ev.toolkit, "gmail");
    assert_eq!(ev.trigger, "GMAIL_NEW_GMAIL_MESSAGE");
    assert_eq!(ev.metadata.id, "evt-1");
    assert_eq!(ev.metadata.uuid, "uuid-1");
    assert_eq!(ev.payload["subject"], "hi");
}

#[test]
fn active_trigger_accepts_string_fields() {
    let v = json!({
        "id": "t1",
        "slug": "GMAIL_NEW_MAIL",
        "toolkit": "gmail",
        "connectionId": "c1",
        "state": "ACTIVE",
    });
    let trig: ComposioActiveTrigger = serde_json::from_value(v).unwrap();
    assert_eq!(trig.id, "t1");
    assert_eq!(trig.slug, "GMAIL_NEW_MAIL");
    assert_eq!(trig.toolkit, "gmail");
    assert_eq!(trig.connection_id, "c1");
    assert_eq!(trig.state.as_deref(), Some("ACTIVE"));
}

#[test]
fn active_trigger_accepts_object_fields() {
    // Mirrors upstream API drift where these fields arrive as objects
    // rather than plain strings.
    let v = json!({
        "id": {"id": "t1"},
        "slug": {"slug": "GMAIL_NEW_MAIL"},
        "toolkit": {"slug": "gmail", "logo": "https://…"},
        "connectionId": {"id": "c1"},
        "state": {"state": "ACTIVE", "slug": "should-be-ignored"},
    });
    let trig: ComposioActiveTrigger = serde_json::from_value(v).unwrap();
    assert_eq!(trig.id, "t1");
    assert_eq!(trig.slug, "GMAIL_NEW_MAIL");
    assert_eq!(trig.toolkit, "gmail");
    assert_eq!(trig.connection_id, "c1");
    // `state` priority must prefer the literal `state` key over metadata.
    assert_eq!(trig.state.as_deref(), Some("ACTIVE"));
}

#[test]
fn active_trigger_state_falls_back_to_value() {
    let v = json!({
        "id": "t1",
        "slug": "X",
        "toolkit": "gmail",
        "connectionId": "c1",
        "state": {"value": "PENDING"},
    });
    let trig: ComposioActiveTrigger = serde_json::from_value(v).unwrap();
    assert_eq!(trig.state.as_deref(), Some("PENDING"));
}

#[test]
fn active_trigger_state_missing_or_unknown_returns_none() {
    let v = json!({
        "id": "t1",
        "slug": "X",
        "toolkit": "gmail",
        "connectionId": "c1",
    });
    let trig: ComposioActiveTrigger = serde_json::from_value(v).unwrap();
    assert!(trig.state.is_none());

    let v = json!({
        "id": "t1",
        "slug": "X",
        "toolkit": "gmail",
        "connectionId": "c1",
        "state": {"unrelated": 42},
    });
    let trig: ComposioActiveTrigger = serde_json::from_value(v).unwrap();
    assert!(trig.state.is_none());
}

#[test]
fn active_trigger_required_field_rejects_unsupported_object() {
    // Object without any of slug/id/name/key must fail loudly so we
    // notice further upstream shape drift instead of silently dropping
    // the trigger.
    let v = json!({
        "id": {"unrelated": 42},
        "slug": "X",
        "toolkit": "gmail",
        "connectionId": "c1",
    });
    let err = serde_json::from_value::<ComposioActiveTrigger>(v).unwrap_err();
    assert!(err.to_string().contains("expected string or object"));
}
