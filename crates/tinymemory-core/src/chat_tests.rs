//! Tests for the surrounding module.

use super::*;

#[tokio::test]
async fn static_chat_provider_returns_response_and_counts() {
    let p = StaticChatProvider::new("hello");
    let prompt = ChatPrompt {
        system: "sys".into(),
        user: "u".into(),
        temperature: 0.0,
        kind: "test",
        max_tokens: None,
    };
    assert_eq!(p.chat_for_json(&prompt).await.unwrap(), "hello");
    assert_eq!(p.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn chat_for_text_with_usage_default_impl_reports_no_usage() {
    // A provider that doesn't override `chat_for_text_with_usage`
    // (here the `chat_for_json`-only `StaticChatProvider`) must still
    // return its text, with `None` usage — so summarise() falls back
    // to the estimate rather than reporting a bogus zero charge.
    let p = StaticChatProvider::new("summary text");
    let prompt = ChatPrompt {
        system: "sys".into(),
        user: "u".into(),
        temperature: 0.0,
        kind: "test",
        max_tokens: None,
    };
    let (text, usage) = p.chat_for_text_with_usage(&prompt).await.unwrap();
    assert_eq!(text, "summary text");
    assert!(usage.is_none());
}
