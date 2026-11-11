//! Converting raw files into brain documents.

use super::*;
use crate::documents::ConverterChain;

#[test]
fn every_format_is_a_file() {
    for format in [
        DocumentFormat::Pdf,
        DocumentFormat::PlainText,
        DocumentFormat::Markdown,
        DocumentFormat::Html,
        DocumentFormat::Xlsx,
        DocumentFormat::Code,
        DocumentFormat::Unknown,
    ] {
        assert_eq!(source_for(format), BrainSource::Files, "{format:?}");
    }
}

#[test]
fn source_kinds_map_to_their_brain_source() {
    use tinymemory_api::SourceKind;

    for (kind, expected) in [
        (SourceKind::Github, BrainSource::Github),
        (SourceKind::Link, BrainSource::Web),
        (SourceKind::Rss, BrainSource::Web),
        (SourceKind::Folder, BrainSource::Files),
        (SourceKind::File, BrainSource::Files),
    ] {
        assert_eq!(brain_source(kind), expected, "{kind:?}");
    }
}

#[test]
fn parses_brain_sources_and_checks_ingest_size_at_the_boundary() {
    assert_eq!(parse_source(" notion ").unwrap(), BrainSource::Notion);
    assert!(parse_source(" ").is_err());
    assert!(check_ingest_size(MAX_INGEST_BYTES).is_ok());
    assert!(check_ingest_size(MAX_INGEST_BYTES + 1).is_err());
}

#[tokio::test]
async fn html_is_a_file_unless_the_caller_says_otherwise() {
    let page = RawDocument::new(
        "<html><head><title>Pricing</title></head><body><p>Pro is $20.</p></body></html>",
    )
    .with_filename("pricing.html");
    let chain = ConverterChain::default();
    let document = brain_document(&chain, &page, None, MemoryMeta::default())
        .await
        .unwrap();
    assert_eq!(document.source, BrainSource::Files);
    assert_eq!(document.mime.as_deref(), Some("text/html"));
    assert!(document.text.contains("Pro is $20."));

    let notion = brain_document(
        &chain,
        &page,
        Some(BrainSource::Notion),
        MemoryMeta::default(),
    )
    .await
    .unwrap();
    assert_eq!(notion.source, BrainSource::Notion);
}

#[tokio::test]
async fn the_caller_s_metadata_is_kept_and_language_filled() {
    let file = RawDocument::new("fn main() {}\n").with_filename("src/main.rs");
    let meta = MemoryMeta {
        repo: Some("acme/app".into()),
        ..MemoryMeta::default()
    };
    let document = brain_document(&ConverterChain::default(), &file, None, meta)
        .await
        .unwrap();
    assert_eq!(document.source, BrainSource::Files);
    assert_eq!(document.meta.repo.as_deref(), Some("acme/app"));
    assert_eq!(document.meta.language.as_deref(), Some("rust"));
}

#[tokio::test]
async fn an_empty_file_is_refused() {
    let error = brain_document(
        &ConverterChain::default(),
        &RawDocument::new("").with_filename("empty.md"),
        None,
        MemoryMeta::default(),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, Error::Invalid(_)), "{error:?}");
}
