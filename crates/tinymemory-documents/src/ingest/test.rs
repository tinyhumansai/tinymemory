//! Tests for routing a converted document into whichever engine is bound.

use std::sync::Mutex;

use async_trait::async_trait;

use tinymemory_api::capabilities::{Capabilities, Capability};
use tinymemory_api::health::MemoryHealth;
use tinymemory_api::provider::types::{ExportPage, ImportOutcome, IngestOutcome, SourceScope};
use tinymemory_api::provider::{
    MemoryCore, MemoryDocuments, MemoryIngest, MemoryPortability, MemoryRecall,
};
use tinymemory_api::recall::OwnedRecallOpts;
use tinymemory_api::types::{
    MemoryCategory, MemoryEntry, MemoryTaint, NamespaceRetrievalContext, NamespaceSummary,
    StoredMemoryDocument,
};

use super::*;
use crate::convert::{ConverterChain, NativeConverter};
use crate::format::DocumentFormat;

/// What a fake provider recorded, whichever family took the write.
#[derive(Debug, Default)]
struct Recorded {
    ingested: Vec<IngestItem>,
    documents: Vec<NamespaceDocumentInput>,
    entries: Vec<(String, String, String)>,
}

/// A provider whose optional families can be switched on and off, so one type
/// covers all three routes.
struct FakeProvider {
    has_ingest: bool,
    has_documents: bool,
    fail_writes: bool,
    recorded: Mutex<Recorded>,
}

impl FakeProvider {
    fn with_ingest() -> Self {
        Self {
            has_ingest: true,
            has_documents: true,
            fail_writes: false,
            recorded: Mutex::new(Recorded::default()),
        }
    }

    fn with_documents_only() -> Self {
        Self {
            has_ingest: false,
            has_documents: true,
            fail_writes: false,
            recorded: Mutex::new(Recorded::default()),
        }
    }

    fn mandatory_only() -> Self {
        Self {
            has_ingest: false,
            has_documents: false,
            fail_writes: false,
            recorded: Mutex::new(Recorded::default()),
        }
    }

    fn failing(has_ingest: bool, has_documents: bool) -> Self {
        Self {
            has_ingest,
            has_documents,
            fail_writes: true,
            recorded: Mutex::new(Recorded::default()),
        }
    }

    fn recorded(&self) -> std::sync::MutexGuard<'_, Recorded> {
        match self.recorded.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

#[async_trait]
impl MemoryCore for FakeProvider {
    async fn store(
        &self,
        namespace: &str,
        key: &str,
        content: &str,
        _category: MemoryCategory,
        _session_id: Option<&str>,
        _taint: MemoryTaint,
    ) -> Result<()> {
        if self.fail_writes {
            return Err(MemoryError::Backend("core write rejected".to_string()));
        }
        self.recorded()
            .entries
            .push((namespace.to_string(), key.to_string(), content.to_string()));
        Ok(())
    }

    async fn get(&self, _namespace: &str, _key: &str) -> Result<Option<MemoryEntry>> {
        Ok(None)
    }

    async fn forget(&self, _namespace: &str, _key: &str) -> Result<bool> {
        Ok(false)
    }

    async fn list(
        &self,
        _namespace: Option<&str>,
        _category: Option<&MemoryCategory>,
        _session_id: Option<&str>,
    ) -> Result<Vec<MemoryEntry>> {
        Ok(Vec::new())
    }

    async fn namespaces(&self) -> Result<Vec<NamespaceSummary>> {
        Ok(Vec::new())
    }
}

#[async_trait]
impl MemoryRecall for FakeProvider {
    async fn recall(
        &self,
        _query: &str,
        _limit: usize,
        _opts: &OwnedRecallOpts,
        _scope: Option<&SourceScope>,
    ) -> Result<Vec<MemoryEntry>> {
        Ok(Vec::new())
    }
}

#[async_trait]
impl MemoryPortability for FakeProvider {
    async fn export_page(&self, _cursor: Option<&str>, _limit: usize) -> Result<ExportPage> {
        Ok(ExportPage {
            records: Vec::new(),
            next_cursor: None,
        })
    }

    async fn import_records(
        &self,
        _records: Vec<tinymemory_api::provider::types::ExportRecord>,
    ) -> Result<ImportOutcome> {
        Ok(ImportOutcome::default())
    }
}

#[async_trait]
impl MemoryIngest for FakeProvider {
    async fn ingest_document(&self, item: IngestItem) -> Result<IngestOutcome> {
        if self.fail_writes {
            return Err(MemoryError::Backend("ingest write rejected".to_string()));
        }
        self.recorded().ingested.push(item);
        Ok(IngestOutcome {
            written: 4,
            skipped: 1,
            ids: vec!["chunk-1".to_string(), "chunk-2".to_string()],
        })
    }

    async fn ingest_chat(&self, _messages: Vec<IngestItem>) -> Result<IngestOutcome> {
        Ok(IngestOutcome::default())
    }
}

#[async_trait]
impl MemoryDocuments for FakeProvider {
    async fn put_document(&self, input: NamespaceDocumentInput) -> Result<String> {
        if self.fail_writes {
            return Err(MemoryError::Backend("document write rejected".to_string()));
        }
        self.recorded().documents.push(input);
        Ok("doc-7".to_string())
    }

    async fn get_document(
        &self,
        _namespace: &str,
        _key: &str,
    ) -> Result<Option<StoredMemoryDocument>> {
        Ok(None)
    }

    async fn list_documents(&self, _namespace: Option<&str>) -> Result<serde_json::Value> {
        Ok(serde_json::Value::Null)
    }

    async fn list_namespaces(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    async fn delete_document(
        &self,
        _namespace: &str,
        _document_id: &str,
    ) -> Result<serde_json::Value> {
        Ok(serde_json::Value::Null)
    }

    async fn clear_namespace(&self, _namespace: &str) -> Result<()> {
        Ok(())
    }

    async fn query_documents(
        &self,
        namespace: &str,
        _query: &str,
        _limit: usize,
    ) -> Result<NamespaceRetrievalContext> {
        Ok(NamespaceRetrievalContext {
            namespace: namespace.to_string(),
            query: None,
            context_text: String::new(),
            hits: Vec::new(),
        })
    }
}

#[async_trait]
impl MemoryProvider for FakeProvider {
    fn driver_id(&self) -> &str {
        "fake"
    }

    fn capabilities(&self) -> Capabilities {
        let mut set = Capabilities::mandatory();
        if self.has_ingest {
            set = set.with(Capability::Ingest);
        }
        if self.has_documents {
            set = set.with(Capability::Documents);
        }
        set
    }

    async fn health(&self) -> MemoryHealth {
        MemoryHealth::Ready
    }

    fn as_ingest(&self) -> Option<&dyn MemoryIngest> {
        self.has_ingest.then_some(self)
    }

    fn as_documents(&self) -> Option<&dyn MemoryDocuments> {
        self.has_documents.then_some(self)
    }
}

fn markdown_upload() -> RawDocument {
    RawDocument::new("# Handbook\n\nThe body.").with_filename("handbook.md")
}

#[tokio::test]
async fn a_provider_with_an_ingest_family_gets_the_chunked_route() {
    let provider = FakeProvider::with_ingest();
    let chain = ConverterChain::default();
    let intake = DocumentIntake::new(&provider, &chain);
    assert_eq!(intake.route(), IntakeRoute::Ingest);

    let receipt = intake
        .accept(&markdown_upload(), &IntakeRequest::new("document:handbook"))
        .await
        .unwrap();

    assert_eq!(receipt.route, IntakeRoute::Ingest);
    assert!(receipt.route.is_chunked());
    assert_eq!(receipt.written, 4);
    assert_eq!(receipt.skipped, 1);
    assert_eq!(
        receipt.ids,
        vec!["chunk-1".to_string(), "chunk-2".to_string()]
    );

    let recorded = provider.recorded();
    assert_eq!(recorded.ingested.len(), 1);
    assert_eq!(
        recorded.ingested[0].namespace.as_deref(),
        Some("document:handbook")
    );
}

#[tokio::test]
async fn the_ingest_route_tells_the_driver_the_body_is_markdown_now() {
    let provider = FakeProvider::with_ingest();
    let chain = ConverterChain::default();
    let html = RawDocument::new("<h1>Page</h1><p>Body.</p>")
        .with_mime("text/html")
        .with_filename("page.html");

    DocumentIntake::new(&provider, &chain)
        .accept(&html, &IntakeRequest::new("document:pages"))
        .await
        .unwrap();

    let recorded = provider.recorded();
    assert_eq!(recorded.ingested[0].mime.as_deref(), Some("text/markdown"));
    assert_eq!(recorded.ingested[0].content, "# Page\n\nBody.");
}

#[tokio::test]
async fn a_provider_without_ingest_falls_back_to_the_document_tier() {
    let provider = FakeProvider::with_documents_only();
    let chain = ConverterChain::default();
    let intake = DocumentIntake::new(&provider, &chain);
    assert_eq!(intake.route(), IntakeRoute::Documents);

    let receipt = intake
        .accept(&markdown_upload(), &IntakeRequest::new("document:handbook"))
        .await
        .unwrap();

    assert_eq!(receipt.route, IntakeRoute::Documents);
    assert!(!receipt.route.is_chunked());
    assert_eq!(receipt.ids, vec!["doc-7".to_string()]);

    let recorded = provider.recorded();
    assert_eq!(recorded.documents.len(), 1);
    assert_eq!(recorded.documents[0].title, "Handbook");
    assert_eq!(recorded.documents[0].content, "# Handbook\n\nThe body.");
}

#[tokio::test]
async fn a_mandatory_only_provider_still_accepts_a_document() {
    let provider = FakeProvider::mandatory_only();
    let chain = ConverterChain::default();
    let intake = DocumentIntake::new(&provider, &chain);
    assert_eq!(intake.route(), IntakeRoute::Core);

    let receipt = intake
        .accept(&markdown_upload(), &IntakeRequest::new("document:handbook"))
        .await
        .unwrap();

    assert_eq!(receipt.route, IntakeRoute::Core);
    let recorded = provider.recorded();
    assert_eq!(recorded.entries.len(), 1);
    assert_eq!(recorded.entries[0].0, "document:handbook");
    assert_eq!(recorded.entries[0].2, "# Handbook\n\nThe body.");
}

#[tokio::test]
async fn the_key_is_derived_from_the_filename_and_is_stable() {
    let provider = FakeProvider::with_documents_only();
    let chain = ConverterChain::default();
    let intake = DocumentIntake::new(&provider, &chain);
    let request = IntakeRequest::new("document:handbook");

    let first = intake.accept(&markdown_upload(), &request).await.unwrap();
    let second = intake.accept(&markdown_upload(), &request).await.unwrap();

    assert_eq!(first.key, "handbook.md");
    assert_eq!(
        first.key, second.key,
        "the same document must upsert rather than duplicate"
    );
}

#[tokio::test]
async fn a_url_origin_beats_a_filename_when_deriving_the_key() {
    let provider = FakeProvider::with_documents_only();
    let chain = ConverterChain::default();
    let document = RawDocument::new("# Page")
        .with_filename("index.md")
        .with_origin("https://example.com/docs/Guide Page");

    let receipt = DocumentIntake::new(&provider, &chain)
        .accept(&document, &IntakeRequest::from_url("document:web"))
        .await
        .unwrap();

    assert_eq!(receipt.key, "example.com/docs/guide-page");
}

#[tokio::test]
async fn an_explicit_key_overrides_every_derivation() {
    let provider = FakeProvider::with_documents_only();
    let chain = ConverterChain::default();
    let receipt = DocumentIntake::new(&provider, &chain)
        .accept(
            &markdown_upload(),
            &IntakeRequest::new("document:handbook").with_key("stable-id"),
        )
        .await
        .unwrap();
    assert_eq!(receipt.key, "stable-id");
}

#[tokio::test]
async fn the_document_route_records_what_the_file_used_to_be() {
    let provider = FakeProvider::with_documents_only();
    let chain = ConverterChain::default();
    let html = RawDocument::new("<h1>Page</h1><p>Body.</p>")
        .with_mime("text/html")
        .with_origin("https://example.com/page");

    DocumentIntake::new(&provider, &chain)
        .accept(&html, &IntakeRequest::from_url("document:web"))
        .await
        .unwrap();

    let recorded = provider.recorded();
    let metadata = &recorded.documents[0].metadata;
    assert_eq!(metadata["source_format"], "html");
    assert_eq!(metadata["origin"], "https://example.com/page");
    assert_eq!(recorded.documents[0].source_type, "web_page");
}

#[tokio::test]
async fn taint_is_passed_through_untouched() {
    let provider = FakeProvider::with_ingest();
    let chain = ConverterChain::default();

    for taint in [MemoryTaint::Internal, MemoryTaint::ExternalSync] {
        DocumentIntake::new(&provider, &chain)
            .accept(
                &markdown_upload(),
                &IntakeRequest::new("document:handbook").with_taint(taint),
            )
            .await
            .unwrap();
    }

    let recorded = provider.recorded();
    assert_eq!(recorded.ingested[0].taint, MemoryTaint::Internal);
    assert_eq!(recorded.ingested[1].taint, MemoryTaint::ExternalSync);
}

#[tokio::test]
async fn an_upload_defaults_to_the_external_taint() {
    assert_eq!(
        IntakeRequest::new("document:handbook").taint,
        MemoryTaint::ExternalSync,
        "content that arrived from outside is external until a host says otherwise"
    );
}

#[tokio::test]
async fn a_namespace_that_fails_the_convention_is_rejected_before_any_write() {
    let provider = FakeProvider::with_ingest();
    let chain = ConverterChain::default();
    let error = DocumentIntake::new(&provider, &chain)
        .accept(&markdown_upload(), &IntakeRequest::new("has space"))
        .await
        .unwrap_err();

    assert!(matches!(error, MemoryError::Invalid(_)), "got {error:?}");
    assert!(provider.recorded().ingested.is_empty());
}

#[tokio::test]
async fn an_unconvertible_document_never_reaches_the_driver() {
    let provider = FakeProvider::with_ingest();
    let chain = ConverterChain::default();
    let pdf = RawDocument::new(b"%PDF-1.7\nbinary".to_vec()).with_filename("report.pdf");

    let error = DocumentIntake::new(&provider, &chain)
        .accept(&pdf, &IntakeRequest::new("document:reports"))
        .await
        .unwrap_err();

    assert!(error.to_string().contains("pdf"), "got {error}");
    assert!(provider.recorded().ingested.is_empty());
}

#[tokio::test]
async fn store_writes_an_already_converted_document_without_converting_again() {
    let provider = FakeProvider::with_documents_only();
    let chain = ConverterChain::default();
    let document = RawDocument::new("ignored").with_filename("edited.md");
    let converted = NativeConverter
        .convert(&RawDocument::new("# Edited\n\nBy hand.").with_mime("text/markdown"))
        .await
        .unwrap();

    let receipt = DocumentIntake::new(&provider, &chain)
        .store(&document, &converted, &IntakeRequest::new("document:edits"))
        .await
        .unwrap();

    assert_eq!(receipt.title, "Edited");
    assert_eq!(
        provider.recorded().documents[0].content,
        "# Edited\n\nBy hand."
    );
}

#[tokio::test]
async fn a_receipt_reports_both_sizes() {
    let provider = FakeProvider::with_ingest();
    let chain = ConverterChain::default();
    let html = RawDocument::new("<h1>Page</h1>").with_mime("text/html");

    let receipt = DocumentIntake::new(&provider, &chain)
        .accept(&html, &IntakeRequest::new("document:web"))
        .await
        .unwrap();

    assert_eq!(receipt.source_bytes, "<h1>Page</h1>".len());
    assert_eq!(receipt.markdown_bytes, "# Page".len());
    assert_eq!(receipt.format, DocumentFormat::Html);
}

#[tokio::test]
async fn driver_failures_propagate_from_every_intake_route() {
    for (provider, expected) in [
        (FakeProvider::failing(true, true), "ingest write rejected"),
        (
            FakeProvider::failing(false, true),
            "document write rejected",
        ),
        (FakeProvider::failing(false, false), "core write rejected"),
    ] {
        let chain = ConverterChain::default();
        let error = DocumentIntake::new(&provider, &chain)
            .accept(&markdown_upload(), &IntakeRequest::new("document:failure"))
            .await
            .unwrap_err();
        assert!(matches!(error, MemoryError::Backend(_)), "got {error:?}");
        assert!(error.to_string().contains(expected), "got {error}");
        let recorded = provider.recorded();
        assert!(recorded.ingested.is_empty());
        assert!(recorded.documents.is_empty());
        assert!(recorded.entries.is_empty());
    }
}

#[test]
fn a_route_round_trips_through_its_wire_spelling() {
    for route in [
        IntakeRoute::Ingest,
        IntakeRoute::Documents,
        IntakeRoute::Core,
    ] {
        let wire = serde_json::to_string(&route).unwrap();
        assert_eq!(wire, format!("\"{}\"", route.as_str()));
        assert_eq!(serde_json::from_str::<IntakeRoute>(&wire).unwrap(), route);
    }
}

#[test]
fn a_key_derived_from_unusable_text_falls_back_to_a_name_rather_than_an_empty_string() {
    let request = IntakeRequest::new("document:x");
    let document = RawDocument::new("body").with_filename("???");
    assert_eq!(request.key(&document, ""), "document");
}

#[test]
fn two_origins_sharing_a_long_prefix_do_not_collide_into_one_key() {
    // Both origins agree on the first 200+ characters and only diverge in
    // their last path segment. Naive truncation to 120 characters would cut
    // both inside the shared prefix and collide the two documents onto one
    // upsert key.
    let request = IntakeRequest::new("document:x");
    let shared_prefix = "a".repeat(150);
    let one = RawDocument::new("body")
        .with_origin(format!("https://example.com/{shared_prefix}/chapter-one"));
    let two = RawDocument::new("body")
        .with_origin(format!("https://example.com/{shared_prefix}/chapter-two"));

    let key_one = request.key(&one, "");
    let key_two = request.key(&two, "");

    assert_ne!(key_one, key_two, "{key_one} vs {key_two}");
    assert!(key_one.len() <= 120, "{key_one} is {} bytes", key_one.len());
    assert!(key_two.len() <= 120, "{key_two} is {} bytes", key_two.len());
}

#[test]
fn a_truncated_key_is_deterministic_for_the_same_input() {
    let request = IntakeRequest::new("document:x");
    let long_origin = format!("https://example.com/{}", "a".repeat(200));
    let document = RawDocument::new("body").with_origin(long_origin);

    assert_eq!(request.key(&document, ""), request.key(&document, ""));
}
