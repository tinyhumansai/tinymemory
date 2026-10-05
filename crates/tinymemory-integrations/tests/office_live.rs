//! Exercises Office conversion through `documents` and into a live CortexDB.
#![cfg(all(feature = "documents-office", feature = "cortex"))]
#![allow(clippy::expect_used)]

use std::io::Write;
use std::time::{Duration, Instant};

use tinymemory_api::{
    ItemKind, ListRequest, MemoryEngine, MemoryMeta, MetaFilter, SourceKind, SourceRef,
};
use tinymemory_integrations::cortex::{CortexCredential, CortexEngine};
use tinymemory_integrations::documents::{
    ConverterChain, OfficeConverter, RawDocument, document_item,
};

const DEFAULT_KEY: &str = "tinymemory-cortex-test";

fn docx_fixture() -> Vec<u8> {
    let mut cursor = std::io::Cursor::new(Vec::new());
    let mut archive = zip::ZipWriter::new(&mut cursor);
    archive
        .start_file(
            "word/document.xml",
            zip::write::SimpleFileOptions::default(),
        )
        .expect("start document part");
    archive
        .write_all(
            br#"<w:document xmlns:w="urn:word"><w:body><w:p><w:r><w:t>Office pipeline marker 7391</w:t></w:r></w:p></w:body></w:document>"#,
        )
        .expect("write document part");
    archive.finish().expect("finish document archive");
    cursor.into_inner()
}

#[tokio::test]
async fn office_document_converts_and_round_trips_through_live_cortexdb() {
    let Ok(url) = std::env::var("TINYMEMORY_LIVE_CORTEXDB_URL") else {
        eprintln!("TINYMEMORY_LIVE_CORTEXDB_URL unset; skipping");
        return;
    };
    let key = std::env::var("TINYMEMORY_TEST_CORTEX_KEY").unwrap_or_else(|_| DEFAULT_KEY.into());
    let engine = CortexEngine::direct(&url, CortexCredential::api_key(key)).expect("live engine");
    assert!(engine.health().await.is_serving(), "CortexDB is serving");

    let workspace = format!("office-live-{}", std::process::id());
    let mut meta = MemoryMeta {
        workspace: Some(workspace.clone()),
        folder: Some("/office".into()),
        file_path: Some("/office/brief.docx".into()),
        source: SourceRef {
            kind: SourceKind::Folder,
            id: Some(format!("{workspace}-brief")),
        },
        ..MemoryMeta::default()
    };
    meta.language = Some("en".into());
    let converter = ConverterChain::default().prepend(Box::new(OfficeConverter));
    let document = RawDocument::new(docx_fixture()).with_filename("brief.docx");
    let item = document_item(&converter, &document, meta)
        .await
        .expect("convert Office document");
    engine.store(item).await.expect("store converted document");

    let filter = MetaFilter {
        workspace: Some(workspace),
        kinds: vec![ItemKind::Document],
        file_path: Some("/office/brief.docx".into()),
        ..MetaFilter::default()
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let page = engine
            .list(ListRequest::new(filter.clone(), 10))
            .await
            .expect("list converted document");
        if page
            .items
            .iter()
            .any(|item| item.text.contains("Office pipeline marker 7391"))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "converted document was not indexed"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
