//! Tests for the surrounding module.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;

/// Every action slug these notes tell the model to call must be one the
/// toolkit actually exposes. A note naming a slug that was renamed or
/// dropped from the curated list is worse than no note: it sends the model
/// after a tool that is not in its list.
#[test]
fn result_notes_only_name_curated_action_slugs() {
    // Each toolkit is checked against its OWN catalogue. Pooling them would
    // let a Gmail note name a Slack-only action and still pass, which is the
    // mistake most likely to be made when editing prose that mentions both.
    let gmail: Vec<&str> = crate::composio::catalogs::gmail::GMAIL_CURATED
        .iter()
        .map(|tool| tool.slug)
        .collect();
    let slack: Vec<&str> = crate::composio::catalogs::messaging::SLACK_CURATED
        .iter()
        .map(|tool| tool.slug)
        .collect();

    for (slug, curated) in [("gmail", &gmail), ("slack", &slack)] {
        let notes = toolkit_result_notes(slug).expect("both toolkits have notes");
        for word in notes.split(|c: char| !(c.is_ascii_uppercase() || c == '_')) {
            // An all-caps underscored token in this prose is an action slug.
            if word.len() > 6 && word.contains('_') {
                assert!(
                    curated.contains(&word),
                    "{slug} notes name `{word}`, which is not one of {slug}'s curated actions"
                );
            }
        }
    }
}

/// A toolkit nobody has established a result shape for gets no entry — a
/// guess about a response is worse here than silence.
#[test]
fn result_notes_absent_for_unestablished_toolkits() {
    assert!(toolkit_result_notes("notion").is_none());
    assert!(toolkit_result_notes("definitely_not_a_toolkit").is_none());
}

/// The failure this entry exists for: a sub-agent searched threads, got
/// snippets rather than bodies, and reported that mail which does exist
/// could not be found. The note has to name both halves — that a thread
/// listing has no body, and which action produces one.
#[test]
fn gmail_notes_separate_finding_a_thread_from_reading_it() {
    let notes = toolkit_result_notes("gmail").expect("gmail has notes");
    assert!(
        notes.contains("GMAIL_LIST_THREADS") && notes.contains("never a message body"),
        "must say a thread listing carries no body: {notes}"
    );
    assert!(
        notes.contains("GMAIL_FETCH_MESSAGE_BY_THREAD_ID"),
        "must name the action that reads the thread: {notes}"
    );
}
