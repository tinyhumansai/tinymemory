//! The brain's document and receipt types.

use serde::{Deserialize, Serialize};
use tinymemory_api::{
    DocumentBody, Error, MemoryMeta, Result, SourceKind, StoreItem, StoreReceipt,
};

use crate::background::BackgroundJob;
use crate::layout::{BrainSource, MemoryLayout};

/// One document for the brain, as text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BrainDocument {
    /// The source type; decides the document's node.
    pub source: BrainSource,
    /// Title, when the source has one (a file name, a page title).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The text, normally markdown.
    pub text: String,
    /// The text's MIME type, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
    /// Metadata. Its namespace and agent id are overwritten: a brain
    /// document lives at its source's node and belongs to no agent.
    #[serde(default)]
    pub meta: MemoryMeta,
}

impl BrainDocument {
    /// A document of `source` with no title, MIME type or metadata.
    #[must_use]
    pub fn new(source: BrainSource, text: impl Into<String>) -> Self {
        Self {
            source,
            title: None,
            text: text.into(),
            mime: None,
            meta: MemoryMeta::default(),
        }
    }

    /// The document with `title`.
    #[must_use]
    pub fn titled(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// The document with `meta` (namespace and agent id are still
    /// overwritten on ingest).
    #[must_use]
    pub fn with_meta(mut self, meta: MemoryMeta) -> Self {
        self.meta = meta;
        self
    }

    /// The item this document is stored as, placed in `layout`.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRequest`] for blank text, and an invalid item.
    pub fn into_item(self, layout: &MemoryLayout) -> Result<StoreItem> {
        if self.text.trim().is_empty() {
            return Err(Error::InvalidRequest(format!(
                "a {} brain document has no text",
                self.source
            )));
        }
        let mut meta = self.meta;
        meta.namespace = layout.brain(&self.source)?;
        meta.agent_id = None;
        if meta.source.kind == SourceKind::default() && meta.source.id.is_none() {
            meta.source.kind = self.source.source_kind();
        }
        let item = StoreItem::Document {
            title: self.title,
            body: DocumentBody::Text(self.text),
            mime: self.mime,
            meta,
        };
        item.validate()?;
        Ok(item)
    }
}

/// What [`crate::Brain::ingest`] did.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Ingested {
    /// The engine's receipt.
    pub receipt: StoreReceipt,
    /// The belief build for the document's source, for the host to run when
    /// it suits; `None` on an engine that rebuilds beliefs on its own
    /// ([`tinymemory_api::Consolidation::Automatic`]).
    pub job: Option<BackgroundJob>,
}

/// What [`crate::Brain::ingest_many`] did.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BrainBatch {
    /// One receipt per document, in order.
    pub receipts: Vec<StoreReceipt>,
    /// One belief build per source touched; none on an engine that rebuilds
    /// beliefs on its own ([`tinymemory_api::Consolidation::Automatic`]).
    pub jobs: Vec<BackgroundJob>,
}

/// One brain source and the number of documents filed under it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BrainSourceCount {
    /// The source id (`pdf`, `notion`, …).
    pub source: String,
    /// Documents stored under the source, including its nested collections.
    pub documents: u64,
}

/// A bounded count of documents in one brain layout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BrainSourcesView {
    /// The layout root the brain lives under.
    pub root: String,
    /// Sources with documents, sorted by count descending and then id.
    pub sources: Vec<BrainSourceCount>,
    /// Documents without a source node at the layout's source depth, including
    /// items whose namespace facet was missing.
    pub unfiled: u64,
}
