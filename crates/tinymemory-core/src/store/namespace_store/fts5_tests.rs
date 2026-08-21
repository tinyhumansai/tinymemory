//! Tests for the surrounding module.

use super::*;

fn setup_db() -> Arc<Mutex<Connection>> {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(EPISODIC_INIT_SQL).unwrap();
    Arc::new(Mutex::new(conn))
}

#[test]
fn insert_and_search() {
    let conn = setup_db();
    let entry = EpisodicEntry {
        id: None,
        session_id: "s1".into(),
        timestamp: 1000.0,
        role: "user".into(),
        content: "How do I deploy to production?".into(),
        lesson: Some("User frequently asks about deployment".into()),
        tool_calls_json: None,
        cost_microdollars: 100,
    };
    episodic_insert(&conn, &entry).unwrap();

    let results = episodic_search(&conn, "deploy production", 10).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].session_id, "s1");
    assert!(results[0].content.contains("deploy"));
}

#[test]
fn session_entries() {
    let conn = setup_db();
    for i in 0..3 {
        episodic_insert(
            &conn,
            &EpisodicEntry {
                id: None,
                session_id: "s2".into(),
                timestamp: 1000.0 + i as f64,
                role: if i % 2 == 0 { "user" } else { "assistant" }.into(),
                content: format!("Turn {i} content"),
                lesson: None,
                tool_calls_json: None,
                cost_microdollars: 0,
            },
        )
        .unwrap();
    }

    let entries = episodic_session_entries(&conn, "s2").unwrap();
    assert_eq!(entries.len(), 3);
    assert!(entries[0].timestamp < entries[2].timestamp);
}

#[test]
fn empty_search_returns_empty() {
    let conn = setup_db();
    let results = episodic_search(&conn, "nonexistent query", 10).unwrap();
    assert!(results.is_empty());
}

#[test]
fn insert_redacts_secret_like_content() {
    let conn = setup_db();
    episodic_insert(
        &conn,
        &EpisodicEntry {
            id: None,
            session_id: "s1".into(),
            timestamp: 1000.0,
            role: "user".into(),
            content: "Bearer abcdefghijklmnop".into(),
            lesson: Some("token=abc123".into()),
            tool_calls_json: Some("{\"api_key\":\"sk-1234567890123456789012345\"}".into()),
            cost_microdollars: 0,
        },
    )
    .unwrap();

    let rows = episodic_session_entries(&conn, "s1").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].content, "Bearer [REDACTED]");
    assert_eq!(rows[0].lesson.as_deref(), Some("[REDACTED]"));
    assert_eq!(
        rows[0].tool_calls_json.as_deref(),
        Some("{\"api_key\":\"[REDACTED_SECRET]\"}")
    );
}

#[test]
fn insert_rejects_secret_like_session_id() {
    let conn = setup_db();
    let err = episodic_insert(
        &conn,
        &EpisodicEntry {
            id: None,
            session_id: "Bearer abcdefghijklmnop".into(),
            timestamp: 1000.0,
            role: "user".into(),
            content: "hello".into(),
            lesson: None,
            tool_calls_json: None,
            cost_microdollars: 0,
        },
    )
    .expect_err("secret-like session_id should be rejected");
    assert!(err.to_string().contains("cannot contain secrets"));
}

// ── Cross-session search (#1505) ─────────────────────────────────────

fn insert_turn(conn: &Arc<Mutex<Connection>>, session_id: &str, ts: f64, content: &str) {
    episodic_insert(
        conn,
        &EpisodicEntry {
            id: None,
            session_id: session_id.into(),
            timestamp: ts,
            role: "user".into(),
            content: content.into(),
            lesson: None,
            tool_calls_json: None,
            cost_microdollars: 0,
        },
    )
    .unwrap();
}

#[test]
fn cross_session_search_surfaces_other_sessions_excluding_current() {
    let conn = setup_db();
    // Chat A — user shared the durable fact
    insert_turn(
        &conn,
        "session-a",
        1000.0,
        "I prefer Postgres for new services",
    );
    // Chat B — current chat, where the question is being asked
    insert_turn(
        &conn,
        "session-b",
        2000.0,
        "What database should I use today?",
    );
    // Chat C — yet another chat with a related fact
    insert_turn(&conn, "session-c", 1500.0, "Postgres timezone is UTC");

    // Asking from chat B: should see session-a + session-c (not session-b)
    let hits = episodic_cross_session_search(&conn, "Postgres", 10, Some("session-b")).unwrap();
    assert!(
        !hits.is_empty(),
        "cross-session search must surface hits from other sessions"
    );
    for hit in &hits {
        assert_ne!(
            hit.session_id, "session-b",
            "current session must be excluded from cross-session sweep, got {}",
            hit.session_id
        );
    }
    let session_ids: std::collections::HashSet<&str> =
        hits.iter().map(|h| h.session_id.as_str()).collect();
    assert!(session_ids.contains("session-a"));
    assert!(session_ids.contains("session-c"));
}

#[test]
fn cross_session_search_returns_empty_for_unknown_query() {
    let conn = setup_db();
    insert_turn(&conn, "session-a", 1000.0, "I prefer Postgres");
    let hits = episodic_cross_session_search(&conn, "kubernetes", 10, None).unwrap();
    assert!(
        hits.is_empty(),
        "no FTS match should produce zero hits, not all rows"
    );
}

#[test]
fn cross_session_search_handles_empty_query() {
    let conn = setup_db();
    insert_turn(&conn, "session-a", 1000.0, "anything");
    let hits = episodic_cross_session_search(&conn, "   ", 10, None).unwrap();
    assert!(hits.is_empty(), "empty query short-circuits to zero hits");
}

#[test]
fn cross_session_search_sanitises_punctuation_safely() {
    let conn = setup_db();
    insert_turn(&conn, "session-a", 1000.0, "Postgres deployment notes");
    // Query with FTS5-hostile punctuation — should not panic. Tokens
    // shared with the indexed row should still match (FTS5 phrase
    // ANDs every quoted token, so we use words that all appear in
    // the row to avoid AND-mismatch false negatives).
    let hits =
        episodic_cross_session_search(&conn, "\"Postgres\" (deployment)?", 10, None).unwrap();
    assert!(
        !hits.is_empty(),
        "punctuated query whose surviving tokens match the indexed row must still surface it"
    );
    assert!(hits[0].content.contains("Postgres"));
}

#[test]
fn episodic_search_sanitises_punctuation_safely() {
    let conn = setup_db();
    insert_turn(&conn, "session-a", 1000.0, "Postgres deployment notes");

    let hits = episodic_search(&conn, "\"Postgres\"，(deployment)?", 10)
        .expect("punctuated user query should not trip FTS5 syntax errors");

    assert!(
        !hits.is_empty(),
        "punctuated query whose surviving tokens match the indexed row must still surface it"
    );
    assert!(hits[0].content.contains("Postgres"));
}

#[test]
fn cross_session_search_does_not_panic_on_pure_punctuation() {
    let conn = setup_db();
    insert_turn(&conn, "session-a", 1000.0, "Postgres deployment notes");
    // All-punctuation query should normalise to empty and produce
    // zero hits without panicking.
    let hits = episodic_cross_session_search(&conn, "()*\":", 10, None).unwrap();
    assert!(
        hits.is_empty(),
        "punctuation-only query must produce zero hits"
    );
}

#[test]
fn cross_session_search_no_exclusion_includes_all_matches() {
    let conn = setup_db();
    insert_turn(&conn, "session-a", 1000.0, "Postgres preference");
    insert_turn(&conn, "session-b", 2000.0, "Postgres setup");

    let hits = episodic_cross_session_search(&conn, "Postgres", 10, None).unwrap();
    let session_ids: std::collections::HashSet<&str> =
        hits.iter().map(|h| h.session_id.as_str()).collect();
    assert!(session_ids.contains("session-a"));
    assert!(session_ids.contains("session-b"));
}
