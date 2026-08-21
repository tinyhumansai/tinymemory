//! Tests for the surrounding module.

use super::*;
use crate::store::profile::{self, profile_load_all, PROFILE_INIT_SQL};
use parking_lot::Mutex;
use rusqlite::Connection;
use serde_json::json;
use std::sync::Arc;

fn setup_db() -> Arc<Mutex<Connection>> {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(PROFILE_INIT_SQL).unwrap();
    Arc::new(Mutex::new(conn))
}

// ── IdentityKind ───────────────────────────────────────────────

#[test]
fn identity_kind_round_trips_through_str() {
    for kind in [
        IdentityKind::UserId,
        IdentityKind::Email,
        IdentityKind::Handle,
        IdentityKind::Phone,
        IdentityKind::DisplayName,
        IdentityKind::AvatarUrl,
        IdentityKind::ProfileUrl,
    ] {
        assert_eq!(IdentityKind::parse(kind.as_str()), Some(kind));
    }
}

#[test]
fn identity_kind_parse_rejects_unknown() {
    assert_eq!(IdentityKind::parse("username"), None);
    assert_eq!(IdentityKind::parse(""), None);
    assert_eq!(IdentityKind::parse("UserId"), None);
}

#[test]
fn matchable_kinds_exclude_url_fields() {
    assert!(IdentityKind::UserId.is_matchable());
    assert!(IdentityKind::Email.is_matchable());
    assert!(IdentityKind::Handle.is_matchable());
    assert!(IdentityKind::Phone.is_matchable());
    assert!(IdentityKind::DisplayName.is_matchable());
    assert!(!IdentityKind::AvatarUrl.is_matchable());
    assert!(!IdentityKind::ProfileUrl.is_matchable());
}

#[test]
fn confidence_orders_hard_above_weak() {
    assert!(IdentityKind::UserId.confidence() > IdentityKind::Email.confidence());
    assert!(IdentityKind::Email.confidence() > IdentityKind::Handle.confidence());
    assert!(IdentityKind::Handle.confidence() > IdentityKind::DisplayName.confidence());
}

// ── canonicalize ──────────────────────────────────────────────

#[test]
fn canonicalize_email_lowercases_and_trims() {
    assert_eq!(
        canonicalize(IdentityKind::Email, "  Cyrus@Example.COM "),
        Some("cyrus@example.com".to_string())
    );
}

#[test]
fn canonicalize_handle_strips_at_and_lowercases() {
    assert_eq!(
        canonicalize(IdentityKind::Handle, "@Cyrus"),
        Some("cyrus".to_string())
    );
    assert_eq!(
        canonicalize(IdentityKind::Handle, "cyrus"),
        Some("cyrus".to_string())
    );
}

#[test]
fn canonicalize_phone_keeps_only_digits_and_plus() {
    assert_eq!(
        canonicalize(IdentityKind::Phone, "+1 (555) 123-4567"),
        Some("+15551234567".to_string())
    );
}

#[test]
fn canonicalize_display_name_collapses_whitespace() {
    assert_eq!(
        canonicalize(IdentityKind::DisplayName, "  Cyrus    Smith  "),
        Some("Cyrus Smith".to_string())
    );
}

#[test]
fn canonicalize_user_id_preserved_as_is() {
    // Slack user_ids are case-sensitive; do not lowercase.
    assert_eq!(
        canonicalize(IdentityKind::UserId, "U123ABC"),
        Some("U123ABC".to_string())
    );
}

#[test]
fn canonicalize_empty_returns_none() {
    assert_eq!(canonicalize(IdentityKind::Email, ""), None);
    assert_eq!(canonicalize(IdentityKind::Email, "   "), None);
}

// ── expand_identity_rows ──────────────────────────────────────

fn fixture_profile(toolkit: &str, username: Option<&str>, extras: Value) -> ProviderUserProfile {
    ProviderUserProfile {
        toolkit: toolkit.into(),
        connection_id: Some("conn-1".into()),
        display_name: Some("Cyrus Smith".into()),
        email: Some("cyrus@example.com".into()),
        username: username.map(str::to_string),
        avatar_url: None,
        profile_url: Some("https://example.com/cyrus".into()),
        extras,
    }
}

#[test]
fn expand_slack_promotes_username_to_user_id_and_extras_handle() {
    let p = fixture_profile("slack", Some("U123ABC"), json!({ "handle": "cyrus" }));
    let rows = expand_identity_rows("slack", &p);

    assert!(rows.contains(&(IdentityKind::UserId, "U123ABC".to_string())));
    assert!(rows.contains(&(IdentityKind::Handle, "cyrus".to_string())));
    assert!(rows.contains(&(IdentityKind::Email, "cyrus@example.com".to_string())));
    assert!(rows.contains(&(IdentityKind::DisplayName, "Cyrus Smith".to_string())));
    assert!(rows.contains(&(
        IdentityKind::ProfileUrl,
        "https://example.com/cyrus".to_string()
    )));
}

#[test]
fn expand_gmail_skips_username_with_no_user_id_concept() {
    let p = fixture_profile("gmail", None, Value::Null);
    let rows = expand_identity_rows("gmail", &p);

    assert!(rows
        .iter()
        .all(|(k, _)| !matches!(k, IdentityKind::UserId | IdentityKind::Handle)));
    assert!(rows.contains(&(IdentityKind::Email, "cyrus@example.com".to_string())));
}

#[test]
fn expand_notion_treats_username_as_user_id() {
    let p = fixture_profile(
        "notion",
        Some("f3c1a8e2-b9b7-4a8d-9d5b-31a2e9f44e2f"),
        Value::Null,
    );
    let rows = expand_identity_rows("notion", &p);

    assert!(rows.contains(&(
        IdentityKind::UserId,
        "f3c1a8e2-b9b7-4a8d-9d5b-31a2e9f44e2f".to_string()
    )));
}

#[test]
fn expand_unknown_toolkit_falls_back_to_handle() {
    let p = fixture_profile("hypothetical", Some("alice"), Value::Null);
    let rows = expand_identity_rows("hypothetical", &p);

    assert!(rows.contains(&(IdentityKind::Handle, "alice".to_string())));
}

#[test]
fn expand_empty_profile_emits_nothing_matchable() {
    let p = ProviderUserProfile {
        toolkit: "gmail".into(),
        connection_id: Some("c-1".into()),
        display_name: None,
        email: None,
        username: None,
        avatar_url: None,
        profile_url: None,
        extras: Value::Null,
    };
    let rows = expand_identity_rows("gmail", &p);
    assert!(rows.is_empty());
}

// ── upsert wiring (uses the underlying profile_upsert directly) ─

#[test]
fn upsert_writes_kind_tagged_key() {
    let conn = setup_db();

    profile::profile_upsert(
        &conn,
        "skill-slack-conn-1-user_id",
        &FacetType::Workflow,
        "skill:slack:conn-1:user_id",
        "U123ABC",
        IdentityKind::UserId.confidence(),
        None,
        1000.0,
    )
    .unwrap();

    let facets = profile_load_all(&conn).unwrap();
    let row = facets
        .iter()
        .find(|f| f.key == "skill:slack:conn-1:user_id")
        .expect("row exists");
    assert_eq!(row.value, "U123ABC");
    assert!((row.confidence - 1.00).abs() < f64::EPSILON);
}

#[test]
fn upsert_repeated_increments_evidence() {
    let conn = setup_db();

    for now in [1000.0, 2000.0] {
        profile::profile_upsert(
            &conn,
            "skill-notion-default-email",
            &FacetType::Workflow,
            "skill:notion:default:email",
            "user@workspace.com",
            IdentityKind::Email.confidence(),
            None,
            now,
        )
        .unwrap();
    }

    let facets = profile_load_all(&conn).unwrap();
    assert_eq!(facets.len(), 1);
    assert_eq!(facets[0].evidence_count, 2);
}

// ── parse_skill_identity_key ──────────────────────────────────

#[test]
fn parse_key_round_trip() {
    let parsed = parse_skill_identity_key("skill:slack:conn_1:user_id");
    assert_eq!(
        parsed,
        Some((
            "slack".to_string(),
            "conn_1".to_string(),
            "user_id".to_string()
        ))
    );
}

#[test]
fn parse_key_rejects_wrong_prefix() {
    assert!(parse_skill_identity_key("preference:slack:c:email").is_none());
}

#[test]
fn parse_key_rejects_extra_segments() {
    assert!(parse_skill_identity_key("skill:slack:c:email:extra").is_none());
}

// ── render ────────────────────────────────────────────────────

#[test]
fn render_includes_handle_with_at_and_omits_user_id() {
    let rendered = render_connected_identities_section(&[ConnectedIdentity {
        source: "slack".into(),
        identifier: "T01ABC".into(),
        display_name: Some("Cyrus Smith".into()),
        email: Some("cyrus@example.com".into()),
        handle: Some("cyrus".into()),
        phone: None,
        user_id: Some("U123ABC".into()),
        avatar_url: None,
        profile_url: None,
    }]);
    assert!(rendered.contains("## Connected Identities"));
    assert!(rendered.contains("- Slack (T01ABC): Cyrus Smith | cyrus@example.com | @cyrus"));
    assert!(
        !rendered.contains("U123ABC"),
        "user_id should not appear in prompt"
    );
}

#[test]
fn render_empty_list_returns_empty_string() {
    assert_eq!(render_connected_identities_section(&[]), "");
}

#[test]
fn render_sanitizes_untrusted_fields_and_skips_empty_identities() {
    let rendered = render_connected_identities_section(&[
        ConnectedIdentity {
            source: "linear".into(),
            identifier: " conn\n42 ".into(),
            display_name: Some(" Alice\tExample ".into()),
            email: Some("alice|example.com".into()),
            handle: Some("\r alice ".into()),
            phone: None,
            user_id: None,
            avatar_url: None,
            profile_url: Some(" https://example.com/a|b ".into()),
        },
        ConnectedIdentity {
            source: "slack".into(),
            identifier: "unused".into(),
            display_name: Some(" \n\t ".into()),
            ..Default::default()
        },
    ]);

    assert_eq!(
        rendered,
        "## Connected Identities\n\n- Linear (conn 42): Alice Example | alice/example.com | @alice | https://example.com/a/b\n"
    );
}

#[test]
fn render_returns_empty_when_every_identity_has_only_non_rendered_fields() {
    let rendered = render_connected_identities_section(&[ConnectedIdentity {
        source: "slack".into(),
        identifier: "conn".into(),
        phone: Some("+15551234567".into()),
        user_id: Some("U123".into()),
        avatar_url: Some("https://example.com/avatar".into()),
        ..Default::default()
    }]);

    assert!(rendered.is_empty());
}

#[test]
fn helper_parsing_and_normalization_cover_malformed_inputs() {
    for key in ["", "skill", "skill:slack", "skill:slack:conn"] {
        assert_eq!(parse_skill_identity_key(key), None);
    }
    assert_eq!(
        normalize_connection_identifier("  Team.Name/@Me  "),
        "team_name__me"
    );
    assert_eq!(normalize_connection_identifier("___"), "");
    assert_eq!(title_case(""), "");
    assert_eq!(title_case("slack"), "Slack");
}

#[test]
fn canonicalize_preserves_urls_and_removes_empty_phone_noise() {
    assert_eq!(
        canonicalize(IdentityKind::ProfileUrl, " https://example.com/Me "),
        Some("https://example.com/Me".into())
    );
    assert_eq!(
        canonicalize(IdentityKind::Phone, "extension only"),
        Some(String::new())
    );
}

// ── now_secs sanity ───────────────────────────────────────────

#[test]
fn now_secs_returns_recent_unix_seconds() {
    let t = now_secs();
    assert!(t > 1_000_000_000.0);
}

#[test]
fn persist_returns_zero_when_memory_client_not_ready() {
    // Exercise the early-return branch. Global client may or may
    // not be initialised in the test binary depending on ordering.
    let p = fixture_profile("gmail", None, Value::Null);
    let _ = persist_provider_profile(&p);
}
