//! Tests for the surrounding module.

use super::*;

fn entry() -> SyncAuditEntry {
    SyncAuditEntry {
        timestamp: DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
            .unwrap()
            .with_timezone(&Utc),
        source_id: "composio:gmail:conn-1".into(),
        source_kind: "composio".into(),
        scope: "user".into(),
        items_fetched: 7,
        batches: 2,
        input_tokens: 100,
        output_tokens: 40,
        estimated_cost_usd: 0.5,
        composio_actions_called: 3,
        composio_cost_usd: 0.1,
        actual_charged_usd: None,
        duration_ms: 1234,
        success: true,
        error: None,
    }
}

/// The engine appends to the same file with its own copy of this type.
/// This pins the exact serialised line so the two writers cannot drift
/// apart silently — a failure here means a coordinated format change,
/// never a local edit.
#[test]
fn audit_line_format_is_pinned() {
    let line = serde_json::to_string(&entry()).unwrap();
    assert_eq!(
        line,
        "{\"timestamp\":\"2026-01-02T03:04:05Z\",\
         \"source_id\":\"composio:gmail:conn-1\",\
         \"source_kind\":\"composio\",\
         \"scope\":\"user\",\
         \"items_fetched\":7,\
         \"batches\":2,\
         \"input_tokens\":100,\
         \"output_tokens\":40,\
         \"estimated_cost_usd\":0.5,\
         \"composio_actions_called\":3,\
         \"composio_cost_usd\":0.1,\
         \"actual_charged_usd\":null,\
         \"duration_ms\":1234,\
         \"success\":true}"
    );
}

#[test]
fn append_then_read_round_trips_newest_first() {
    let tmp = tempfile::tempdir().unwrap();
    let mut first = entry();
    first.source_id = "first".into();
    let mut second = entry();
    second.source_id = "second".into();
    append_audit_entry(tmp.path(), &first).unwrap();
    append_audit_entry(tmp.path(), &second).unwrap();

    let entries = read_audit_log(tmp.path()).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].source_id, "second");
    assert_eq!(entries[1].source_id, "first");
}

/// An unreadable log must surface as an error, never as an empty log —
/// budget accounting fails closed on it.
#[test]
fn io_failure_is_distinguishable_from_an_empty_log() {
    let tmp = tempfile::tempdir().unwrap();
    // A directory where the file should be makes the read fail.
    std::fs::create_dir_all(tmp.path().join(AUDIT_DIR).join(AUDIT_FILENAME)).unwrap();
    let error = read_audit_log(tmp.path()).expect_err("directory read must fail");
    assert!(
        error.downcast_ref::<std::io::Error>().is_some(),
        "expected the audit I/O error to remain distinguishable: {error:#}"
    );
}

#[test]
fn missing_file_reads_as_empty_and_torn_lines_are_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(read_audit_log(tmp.path()).unwrap().is_empty());

    append_audit_entry(tmp.path(), &entry()).unwrap();
    let path = tmp.path().join(AUDIT_DIR).join(AUDIT_FILENAME);
    let mut content = std::fs::read_to_string(&path).unwrap();
    content.push_str("{\"torn\":");
    std::fs::write(&path, content).unwrap();

    assert_eq!(read_audit_log(tmp.path()).unwrap().len(), 1);
}
