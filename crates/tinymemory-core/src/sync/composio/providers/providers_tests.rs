//! Tests for the surrounding module.

use super::*;
use serde_json::json;

#[test]
fn pick_str_finds_first_non_empty_match() {
    let v = json!({
        "data": { "user": { "email": "  user@example.com  ", "name": "" } },
        "fallback": "fallback@example.com"
    });
    // first path empty -> falls through
    assert_eq!(
        pick_str(&v, &["data.user.name", "data.user.email"]),
        Some("user@example.com".to_string())
    );
    // missing path -> falls through to fallback
    assert_eq!(
        pick_str(&v, &["data.missing", "fallback"]),
        Some("fallback@example.com".to_string())
    );
    // nothing matches
    assert_eq!(pick_str(&v, &["nope.nope"]), None);
}

#[test]
fn sync_outcome_elapsed_ms_is_safe_when_finish_lt_start() {
    let mut o = SyncOutcome {
        started_at_ms: 100,
        finished_at_ms: 50,
        ..Default::default()
    };
    assert_eq!(o.elapsed_ms(), 0);
    o.finished_at_ms = 250;
    assert_eq!(o.elapsed_ms(), 150);
}

#[test]
fn pick_str_returns_none_for_non_string_values() {
    let v = json!({ "count": 42, "flag": true, "empty": "", "whitespace": "   " });
    assert_eq!(pick_str(&v, &["count"]), None);
    assert_eq!(pick_str(&v, &["flag"]), None);
    assert_eq!(pick_str(&v, &["empty"]), None);
    assert_eq!(pick_str(&v, &["whitespace"]), None);
}

#[test]
fn pick_str_respects_path_order() {
    let v = json!({ "a": "first", "b": "second" });
    assert_eq!(pick_str(&v, &["a", "b"]), Some("first".into()));
    assert_eq!(pick_str(&v, &["b", "a"]), Some("second".into()));
}

#[test]
fn sync_reason_as_str_matches_enum_variant() {
    assert_eq!(SyncReason::ConnectionCreated.as_str(), "connection_created");
    assert_eq!(SyncReason::Periodic.as_str(), "periodic");
    assert_eq!(SyncReason::Manual.as_str(), "manual");
}

#[test]
fn sync_reason_serde_is_snake_case() {
    let s = serde_json::to_string(&SyncReason::ConnectionCreated).unwrap();
    assert_eq!(s, "\"connection_created\"");
    let back: SyncReason = serde_json::from_str(&s).unwrap();
    assert_eq!(back, SyncReason::ConnectionCreated);
}

// Note: `toolkit_has_scope` tests now live in `scope_lookup.rs`
// alongside the implementation.

#[test]
fn catalog_for_toolkit_resolves_new_microsoft_and_todoist_slugs() {
    // Newly added catalogs (#2283): OneDrive, Excel, Todoist must be
    // discoverable both by their canonical UI slug AND by the
    // prefix that `toolkit_from_slug` extracts from action slugs.
    assert!(catalog_for_toolkit("one_drive").is_some());
    assert!(catalog_for_toolkit("onedrive").is_some());
    // ONE_DRIVE_GET_FILE → toolkit_from_slug() → "one"
    assert!(catalog_for_toolkit("one").is_some());
    assert!(catalog_for_toolkit("excel").is_some());
    assert!(catalog_for_toolkit("todoist").is_some());
}

#[test]
fn agent_ready_toolkits_includes_new_catalogs_and_is_sorted() {
    let slugs = agent_ready_toolkits();
    assert!(slugs.contains(&"one_drive"));
    assert!(slugs.contains(&"excel"));
    assert!(slugs.contains(&"todoist"));
    // Spot-check legacy entries still present.
    assert!(slugs.contains(&"gmail"));
    assert!(slugs.contains(&"slack"));
    // Uncurated toolkit must NOT appear — guarantees the UI badge
    // logic can rely on this set to flag "preview" toolkits.
    assert!(!slugs.contains(&"sharepoint"));
    assert!(!slugs.contains(&"clickup"));
    // Stable order across builds — the RPC consumer caches it.
    let mut expected = slugs.clone();
    expected.sort_unstable();
    assert_eq!(slugs, expected);
}

#[test]
fn capability_matrix_includes_new_catalog_only_toolkits() {
    let matrix = capability_matrix();
    for slug in ["one_drive", "excel", "todoist"] {
        let row = matrix
            .iter()
            .find(|entry| entry.toolkit == slug)
            .unwrap_or_else(|| panic!("{slug} capability row missing"));
        assert!(!row.native_provider, "{slug} should not be native");
        assert!(row.curated_tools, "{slug} should be catalogued");
        assert!(
            row.curated_tool_count > 0,
            "{slug} catalog should be non-empty"
        );
        assert!(
            row.tool_execution,
            "{slug} tool execution should be enabled"
        );
        // No profile/sync/memory ingest — catalog-only.
        assert!(!row.user_profile);
        assert!(!row.initial_sync);
        assert!(!row.periodic_sync);
        assert!(!row.memory_ingest);
    }
}

#[test]
fn capability_matrix_distinguishes_native_from_catalog_only_toolkits() {
    let matrix = capability_matrix();

    let gmail = matrix
        .iter()
        .find(|entry| entry.toolkit == "gmail")
        .expect("gmail capability row");
    assert!(gmail.native_provider);
    assert!(gmail.curated_tools);
    assert!(gmail.curated_tool_count > 0);
    assert!(gmail.user_profile);
    assert!(gmail.initial_sync);
    assert!(gmail.periodic_sync);
    assert_eq!(gmail.sync_interval_secs, Some(15 * 60));
    assert!(gmail.trigger_webhooks);
    assert!(gmail.memory_ingest);

    let google_calendar = matrix
        .iter()
        .find(|entry| entry.toolkit == "googlecalendar")
        .expect("googlecalendar capability row");
    assert!(!google_calendar.native_provider);
    assert!(google_calendar.curated_tools);
    assert!(google_calendar.curated_tool_count > 0);
    assert!(google_calendar.tool_execution);
    assert!(!google_calendar.user_profile);
    assert!(!google_calendar.initial_sync);
    assert!(!google_calendar.periodic_sync);
    assert_eq!(google_calendar.sync_interval_secs, None);
    assert!(!google_calendar.memory_ingest);
}

#[test]
fn capability_matrix_includes_clickup_as_native_memory_provider() {
    // Locks in the per-issue #2288 registration: a ClickUp row must
    // appear in the capability matrix with the same native-provider
    // flags Gmail/Notion/Slack already carry (`memory_ingest`,
    // `periodic_sync`, non-zero `sync_interval_secs`). If a future
    // change drops one of the four registration touchpoints
    // (CAPABILITY_TOOLKITS, has_native_provider,
    // native_provider_sync_interval, catalog_for_toolkit) this test
    // fails loud rather than silently degrading the provider to
    // catalog-only status.
    let matrix = capability_matrix();
    let clickup = matrix
        .iter()
        .find(|entry| entry.toolkit == "clickup")
        .expect("clickup capability row");
    assert!(clickup.native_provider, "clickup must be native");
    assert!(clickup.curated_tools, "clickup must have a curated catalog");
    assert!(
        clickup.curated_tool_count > 0,
        "clickup catalog must be non-empty"
    );
    assert!(clickup.user_profile);
    assert!(clickup.initial_sync);
    assert!(clickup.periodic_sync);
    assert_eq!(clickup.sync_interval_secs, Some(30 * 60));
    assert!(clickup.memory_ingest);
}

#[test]
fn capability_matrix_includes_linear_as_native_memory_provider() {
    // Per-issue #2400 registration: a Linear row must appear in
    // the capability matrix as a native memory-ingest provider,
    // matching gmail / notion / slack / clickup. If a future
    // change drops one of the five registration touchpoints
    // (CAPABILITY_TOOLKITS, has_native_provider,
    // native_provider_sync_interval, catalog_for_toolkit,
    // toolkit_description) this test fails loud rather than
    // silently degrading the provider to catalog-only status.
    let matrix = capability_matrix();
    let linear = matrix
        .iter()
        .find(|entry| entry.toolkit == "linear")
        .expect("linear capability row");
    assert!(linear.native_provider, "linear must be native");
    assert!(linear.curated_tools, "linear must have a curated catalog");
    assert!(
        linear.curated_tool_count > 0,
        "linear catalog must be non-empty"
    );
    assert!(linear.user_profile);
    assert!(linear.initial_sync);
    assert!(linear.periodic_sync);
    assert_eq!(linear.sync_interval_secs, Some(30 * 60));
    assert!(linear.memory_ingest);
}

#[test]
fn capability_matrix_includes_github_as_native_memory_provider() {
    let matrix = capability_matrix();
    let github = matrix
        .iter()
        .find(|entry| entry.toolkit == "github")
        .expect("github capability row");
    assert!(github.native_provider, "github must be native");
    assert!(github.curated_tools, "github must have a curated catalog");
    assert!(
        github.curated_tool_count > 0,
        "github catalog must be non-empty"
    );
    assert!(github.user_profile);
    assert!(github.initial_sync);
    assert!(github.periodic_sync);
    assert_eq!(github.sync_interval_secs, Some(30 * 60));
    assert!(github.memory_ingest);
}

#[test]
fn toolkit_description_known_slugs_are_distinct_and_non_empty() {
    let known = [
        "gmail",
        "notion",
        "github",
        "slack",
        "discord",
        "google_calendar",
        "google_drive",
        "google_docs",
        "google_sheets",
        "outlook",
        "microsoft_teams",
        "linear",
        "jira",
        "trello",
        "asana",
        "dropbox",
        "twitter",
        "spotify",
        "telegram",
        "whatsapp",
        "twilio",
        "shopify",
        "stripe",
        "hubspot",
        "salesforce",
        "airtable",
        "figma",
        "youtube",
        "calendar",
    ];
    let fallback = toolkit_description("__definitely_unknown_slug__");
    for slug in known {
        let desc = toolkit_description(slug);
        assert!(!desc.is_empty(), "{slug} description must not be empty");
        assert_ne!(
            desc, fallback,
            "known slug `{slug}` must not map to the generic fallback"
        );
    }
}

#[test]
fn toolkit_description_unknown_slug_uses_generic_fallback() {
    assert_eq!(
        toolkit_description("not_a_real_toolkit_123"),
        "Interact with this connected service via its available actions"
    );
    assert_eq!(
        toolkit_description(""),
        "Interact with this connected service via its available actions"
    );
}

#[test]
fn toolkit_description_is_case_sensitive() {
    // The match is lowercase-only by convention; an uppercase slug
    // should fall through to the generic description. Explicitly
    // documenting this guards against accidental case-insensitive
    // matching sneaking in later.
    let fallback = toolkit_description("__fallback__");
    assert_eq!(toolkit_description("GMAIL"), fallback);
    assert_eq!(toolkit_description("Notion"), fallback);
}

#[test]
fn provider_user_profile_default_is_empty() {
    let p = ProviderUserProfile::default();
    assert!(p.toolkit.is_empty());
    assert!(p.connection_id.is_none());
    assert!(p.display_name.is_none());
    assert!(p.email.is_none());
    assert!(p.username.is_none());
    assert!(p.avatar_url.is_none());
    assert!(p.profile_url.is_none());
    assert!(p.extras.is_null());
}
