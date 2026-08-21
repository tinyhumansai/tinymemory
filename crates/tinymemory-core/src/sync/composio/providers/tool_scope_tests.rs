//! Tests for the surrounding module.

use super::*;

#[test]
fn classify_unknown_picks_admin_for_destructive_verbs() {
    assert_eq!(classify_unknown("GMAIL_DELETE_EMAIL"), ToolScope::Admin);
    assert_eq!(classify_unknown("GMAIL_TRASH_EMAIL"), ToolScope::Admin);
    assert_eq!(classify_unknown("GMAIL_MODIFY_LABELS"), ToolScope::Admin);
}

#[test]
fn classify_unknown_picks_write_for_mutating_verbs() {
    assert_eq!(classify_unknown("GMAIL_SEND_EMAIL"), ToolScope::Write);
    assert_eq!(classify_unknown("NOTION_CREATE_PAGE"), ToolScope::Write);
    assert_eq!(classify_unknown("NOTION_UPDATE_PAGE"), ToolScope::Write);
}

#[test]
fn classify_unknown_defaults_to_read() {
    assert_eq!(classify_unknown("GMAIL_FETCH_EMAILS"), ToolScope::Read);
    assert_eq!(classify_unknown("NOTION_SEARCH"), ToolScope::Read);
    assert_eq!(classify_unknown("GMAIL_GET_PROFILE"), ToolScope::Read);
}

#[test]
fn classify_unknown_admin_takes_precedence_over_write() {
    // MODIFY_LABELS contains no write verb but DELETE_DRAFT does — make
    // sure the admin check wins.
    assert_eq!(classify_unknown("GMAIL_DELETE_DRAFT"), ToolScope::Admin);
}

#[test]
fn toolkit_from_slug_extracts_lowercase_prefix() {
    assert_eq!(
        toolkit_from_slug("GMAIL_SEND_EMAIL"),
        Some("gmail".to_string())
    );
    assert_eq!(
        toolkit_from_slug("NOTION_FETCH_DATA"),
        Some("notion".to_string())
    );
    assert_eq!(toolkit_from_slug(""), None);
    assert_eq!(
        toolkit_from_slug("noUnderscore"),
        Some("nounderscore".into())
    );
}

#[test]
fn toolkit_from_slug_handles_known_multi_segment_toolkits() {
    assert_eq!(
        toolkit_from_slug("ZOHO_MAIL_SEND_EMAIL"),
        Some("zoho_mail".to_string())
    );
    assert_eq!(
        toolkit_from_slug("ONE_DRIVE_GET_FILE"),
        Some("one_drive".to_string())
    );
    assert_eq!(
        toolkit_from_slug("MICROSOFT_TEAMS_SEND_MESSAGE"),
        Some("microsoft_teams".to_string())
    );
}

#[test]
fn find_curated_is_case_insensitive() {
    let catalog = &[CuratedTool {
        slug: "GMAIL_SEND_EMAIL",
        scope: ToolScope::Write,
    }];
    assert!(find_curated(catalog, "gmail_send_email").is_some());
    assert!(find_curated(catalog, "GMAIL_SEND_EMAIL").is_some());
    assert!(find_curated(catalog, "GMAIL_DELETE_EMAIL").is_none());
}

#[test]
fn tool_scope_serializes_lowercase() {
    assert_eq!(serde_json::to_string(&ToolScope::Read).unwrap(), "\"read\"");
    assert_eq!(
        serde_json::to_string(&ToolScope::Admin).unwrap(),
        "\"admin\""
    );
}
