//! Tests for the surrounding module.

use super::*;
use tempfile::TempDir;
use tinymemory_api::host::test_support::TestHostConfig;

#[test]
fn preview_keeps_short_text() {
    assert_eq!(utf8_prefix("hello", 2048), "hello");
}

#[test]
fn preview_respects_utf8_byte_boundary() {
    assert_eq!(utf8_prefix("aéb", 2), "a");
    assert_eq!(utf8_prefix("éb", 2), "é");
}

#[test]
fn suffix_preview_preserves_trailing_utf8() {
    assert_eq!(utf8_suffix("aéb", 2), "b");
    assert_eq!(utf8_suffix("aéb", 3), "éb");
}

#[tokio::test]
async fn empty_inputs_are_noop_ingests_across_every_product_funnel() {
    crate::test_seams::init();
    let temp = TempDir::new().unwrap();
    let mut config = TestHostConfig::default();
    config.workspace_dir = temp.path().join("workspace");

    let chat = ingest_chat(
        &config,
        "empty-chat",
        "owner",
        vec!["test".into()],
        ChatBatch {
            platform: "slack".into(),
            channel_label: "empty".into(),
            messages: Vec::new(),
        },
    )
    .await
    .unwrap();
    assert_eq!(chat.chunks_written, 0);

    let thread = EmailThread {
        provider: "gmail".into(),
        thread_subject: "empty".into(),
        messages: Vec::new(),
    };
    let email = ingest_email(&config, "empty-email", "owner", Vec::new(), thread.clone())
        .await
        .unwrap();
    assert_eq!(email.chunks_written, 0);
    let email_with_refs = ingest_email_with_raw_refs(
        &config,
        "empty-email-refs",
        "owner",
        Vec::new(),
        thread,
        Vec::new(),
    )
    .await
    .unwrap();
    assert_eq!(email_with_refs.chunks_written, 0);

    let document = DocumentInput {
        provider: "notion".into(),
        title: String::new(),
        body: String::new(),
        modified_at: chrono::Utc::now(),
        source_ref: None,
    };
    let plain = ingest_document(
        &config,
        "empty-document",
        "owner",
        Vec::new(),
        document.clone(),
    )
    .await
    .unwrap();
    assert_eq!(plain.chunks_written, 0);
    let versioned = ingest_document_versioned(
        &config,
        "empty-document-versioned",
        "owner",
        Vec::new(),
        document,
        Some("docs".into()),
        Some(1_700_000_000_000),
    )
    .await
    .unwrap();
    assert_eq!(versioned.chunks_written, 0);
}
