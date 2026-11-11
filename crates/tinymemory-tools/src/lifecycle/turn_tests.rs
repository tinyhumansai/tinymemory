//! Public contract tests for the runtime-neutral turn lifecycle hooks.

use serde_json::json;
use tinymemory_api::{TimeHint, ToolCallRef, chrono::NaiveDate};

use super::date_hint::parse_range;
use super::turn::{LoggedToolCall, MAX_TOOL_LINE_CHARS, logged_reply, user_turn_index};
use super::window::InPromptFrom;
use super::{PostTurn, PreTurn, RecallPolicy};

#[test]
fn resumed_defaults_false_for_existing_serialized_turns() {
    let turn: PreTurn = serde_json::from_value(json!({
        "thread_id": "thread",
        "turn_index": 4,
        "user_text": "hello"
    }))
    .unwrap();
    assert!(!turn.resumed);

    let resumed: PreTurn = serde_json::from_value(json!({
        "thread_id": "thread",
        "turn_index": 4,
        "user_text": "hello",
        "resumed": true
    }))
    .unwrap();
    assert!(resumed.resumed);
}

#[test]
fn channel_defaults_preserve_existing_turn_requests() {
    assert_eq!(PreTurn::new("thread", 0, "hello").channel, None);
    assert_eq!(PostTurn::new("thread", 1, "hello").channel, None);

    let old_pre: PreTurn = serde_json::from_value(json!({
        "thread_id": "thread",
        "turn_index": 0,
        "user_text": "hello"
    }))
    .unwrap();
    let old_post: PostTurn = serde_json::from_value(json!({
        "thread_id": "thread",
        "turn_index": 1,
        "assistant_text": "hello"
    }))
    .unwrap();
    assert_eq!(old_pre.channel, None);
    assert_eq!(old_post.channel, None);
}

#[test]
fn log_only_policy_keeps_logging_possible_but_disables_every_recall_section() {
    let policy = RecallPolicy::default().log_only();
    assert_eq!(policy.learnings_limit, 0);
    assert_eq!(policy.brain_limit, 0);
    assert_eq!(policy.history_limit, 0);
    assert_eq!(policy.team_limit, 0);
    assert_eq!(policy.budget_tokens, RecallPolicy::default().budget_tokens);
    assert_eq!(
        policy.build_beliefs_every,
        RecallPolicy::default().build_beliefs_every
    );
}

#[test]
fn turn_indices_saturate_without_wrapping() {
    assert_eq!(user_turn_index(0), 0);
    assert_eq!(user_turn_index(3), 6);
    assert_eq!(user_turn_index(usize::MAX), u32::MAX - 1);
}

#[test]
fn logged_reply_keeps_one_bounded_line_per_tool_result() {
    let calls = vec![
        LoggedToolCall {
            call: ToolCallRef {
                name: "web_search".into(),
                id: Some("call-1".into()),
            },
            result: Some("  Rust 1.90\n released   today ".into()),
        },
        LoggedToolCall {
            call: ToolCallRef {
                name: "noop".into(),
                id: None,
            },
            result: None,
        },
        LoggedToolCall {
            call: ToolCallRef {
                name: "huge".into(),
                id: None,
            },
            result: Some("🙂".repeat(5_000)),
        },
        LoggedToolCall {
            call: ToolCallRef {
                name: "empty".into(),
                id: None,
            },
            result: Some(" \n\t ".into()),
        },
    ];

    let text = logged_reply(" Done. ", &calls);
    assert!(text.starts_with("Done.\n\nTools:\n- web_search → Rust 1.90 released today"));
    assert!(!text.contains("noop"));
    let huge = text.lines().last().unwrap();
    assert!(huge.chars().count() <= MAX_TOOL_LINE_CHARS + "- huge → ".chars().count());
    assert!(huge.ends_with(&"🙂".repeat(MAX_TOOL_LINE_CHARS)));
    assert!(!text.contains("empty"));
    assert_eq!(logged_reply("plain", &[]), "plain");
    assert_eq!(
        logged_reply(
            "",
            &[LoggedToolCall {
                call: ToolCallRef {
                    name: "only_tool".into(),
                    id: None,
                },
                result: Some("result".into()),
            }]
        ),
        "\n\nTools:\n- only_tool → result"
    );
}

#[test]
fn the_live_window_starts_at_its_initial_value_and_is_clamped_to_the_turn() {
    assert_eq!(InPromptFrom::new(6, 20).get(), 6);
    assert_eq!(InPromptFrom::new(30, 20).get(), 20);
}

#[test]
fn a_fold_moves_two_positions_per_user_message() {
    let window = InPromptFrom::new(6, 20);
    assert_eq!(window.advance_past(["first", "second"]), 10);
    assert_eq!(window.get(), 10);
    assert_eq!(window.advance_past(["third"]), 12);
    assert_eq!(window.advance_past(Vec::<String>::new()), 12);
}

#[test]
fn identical_messages_in_one_fold_are_separate_turns() {
    let window = InPromptFrom::new(0, 40);
    assert_eq!(window.advance_past(["yes", "yes"]), 4);
    assert_eq!(window.advance_past(["ok", "yes"]), 8);
}

#[test]
fn replaying_the_same_fold_does_not_advance_again() {
    let window = InPromptFrom::new(0, 40);
    assert_eq!(window.advance_past(["a", "b"]), 4);
    assert_eq!(window.advance_past(["c"]), 6);
    assert_eq!(window.advance_past(["c"]), 6);
}

#[test]
fn a_fold_from_the_turns_original_head_is_counted_from_there() {
    let window = InPromptFrom::new(2, 40);
    assert_eq!(window.advance_past(["a", "b"]), 6);
    assert_eq!(window.advance_past(["a", "b", "c"]), 8);
}

#[test]
fn a_fold_never_moves_past_the_turn_being_answered() {
    let window = InPromptFrom::new(16, 20);
    assert_eq!(window.advance_past(["a", "b", "c", "d", "e"]), 20);
}

#[test]
fn date_hint_range_parser_handles_prose_fences_single_days_and_bad_ranges() {
    let day = |day| NaiveDate::from_ymd_opt(2026, 10, day).unwrap();
    let parsed = parse_range(
        "```json\n{\"from\":\"2026-10-06\",\"to\":\"2026-10-08\"}\n```",
        Some("Asia/Kolkata"),
    )
    .unwrap();
    assert_eq!(
        parsed,
        TimeHint::new(day(6), day(8), Some("Asia/Kolkata".into())).unwrap()
    );
    let single = parse_range("{\"from\":\"2026-10-03\"}", Some("UTC")).unwrap();
    assert_eq!((single.from, single.to), (day(3), day(3)));
    for answer in [
        "{\"from\":null,\"to\":null}",
        "no date here",
        "{\"from\":\"last Saturday\"}",
        "{\"from\":\"2026-10-08\",\"to\":\"2026-10-06\"}",
        "} {",
    ] {
        assert_eq!(parse_range(answer, Some("UTC")), None, "{answer}");
    }
    assert_eq!(
        parse_range("{\"from\":\"2026-10-03\"}", Some("not/a-zone")),
        None
    );
}
