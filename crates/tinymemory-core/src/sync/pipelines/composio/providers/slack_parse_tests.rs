//! Tests for tolerant Slack cursor, mention, and timestamp parsing.

use serde_json::json;

use super::*;

#[test]
fn mentions_resolve_known_users_and_keep_unknown_ids() {
    let users = serde_json::Map::from_iter([
        ("U123".to_string(), json!("Ada")),
        ("U999".to_string(), json!(17)),
    ]);
    assert_eq!(
        replace_mentions("hi <@U123>, ask <@U456> and <@U999>", Some(&users)),
        "hi @Ada, ask @U456 and @U999"
    );
    assert_eq!(replace_mentions("plain", None), "plain");
}

#[test]
fn cursor_parser_skips_blank_values_and_understands_nested_envelopes() {
    let data = json!({
        "data": {
            "response_metadata": { "next_cursor": "  " },
            "next_cursor": "  page-2  "
        }
    });
    assert_eq!(next_cursor(&data).as_deref(), Some("page-2"));
    assert_eq!(next_cursor(&json!({"next_cursor": 2})), None);
}

#[test]
fn malformed_cursor_json_restarts_with_an_empty_map() {
    assert!(decode_cursors(None).is_empty());
    assert!(decode_cursors(Some("not json")).is_empty());
    assert!(decode_cursors(Some("[1,2]")).is_empty());
    assert_eq!(
        decode_cursors(Some(r#"{"C1":"100.2","C2":"200.0"}"#))
            .get("C2")
            .map(String::as_str),
        Some("200.0")
    );
}

#[test]
fn timestamp_parser_rejects_malformed_numeric_components() {
    assert_eq!(parse_ts("1714003200.000100"), Some((1_714_003_200, 100)));
    assert_eq!(parse_ts("1714003200"), Some((1_714_003_200, 0)));
    for malformed in ["", "abc.1", "12.abc", "12.", ".1"] {
        assert_eq!(parse_ts(malformed), None, "accepted {malformed:?}");
    }
}

#[test]
fn search_helpers_default_safely_for_malformed_payloads() {
    assert!(search_matches(&json!(null)).is_empty());
    assert!(search_matches(&json!({"messages": "wrong"})).is_empty());
    assert_eq!(search_total_pages(&json!({"pages": "many"})), 1);
    assert_eq!(
        search_total_pages(&json!({"data":{"data":{"messages":{"paging":{"pages":7}}}}})),
        7
    );
}
