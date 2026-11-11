//! The brain: an agent-independent store of documents, kept apart by source:
//! the connector or app each came from.
//!
//! Company knowledge (local files, Notion pages, GitHub issues, mail) belongs
//! to no agent, so a brain document carries no agent id and lives at its
//! source's node in the [`MemoryLayout`] (`source:files`, `source:notion`),
//! or at one of the source's collections below it
//! (`source:github/project:acme-api`, [`MemoryLayout::brain_collection`]).
//! Every agent reads it through the holistic recall. Forgetting or
//! rebuilding a source covers its collections too.
//!
//! [`Brain::ingest`] stores one document (by default waiting until it is
//! readable, since ingestion is not on a live turn) and returns the
//! [`BackgroundJob`] that would build beliefs from its source — the host runs
//! it whenever suits, through [`crate::BackgroundRunner`]. An engine that
//! rebuilds beliefs on its own after writes
//! ([`tinymemory_api::Consolidation::Automatic`]) gets no such job;
//! [`Brain::build`] still asks for one at any time. Converting a
//! file's bytes to text is the integrations crate's job; the brain takes
//! text.
//!
//! # Example
//!
//! ```
//! use std::sync::Arc;
//! use tinymemory_api::conformance::ReferenceEngine;
//! use tinymemory_tools::{Brain, BrainDocument, BrainSource, MemoryLayout};
//!
//! # let runtime = tokio::runtime::Builder::new_current_thread().build()?;
//! # runtime.block_on(async {
//! let brain = Brain::new(Arc::new(ReferenceEngine::new()), MemoryLayout::default());
//! let ingested = brain
//!     .ingest(BrainDocument::new(BrainSource::Markdown, "Refunds take five business days.")
//!         .titled("refunds.md"))
//!     .await?;
//! assert!(!ingested.receipt.replayed);
//!
//! let hits = brain.search("refunds", Some(&BrainSource::Markdown), 5).await?;
//! assert_eq!(hits.len(), 1);
//! assert!(brain.search("refunds", Some(&BrainSource::Pdf), 5).await?.is_empty());
//! # Ok::<(), tinymemory_api::Error>(())
//! # })?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod types;

use std::collections::BTreeSet;
use std::sync::Arc;

use tinymemory_api::{
    ConsolidateRequest, ExploreRequest, Facet, FetchMode, FetchRequest, ForgetReport, ForgetTarget,
    Hit, ItemKind, MAX_STORE_MANY, MemoryEngine, Reach, Result, SegmentKind, StoreItem,
    WriteOptions,
};

use crate::background::{BackgroundJob, builds_on_its_own};
use crate::layout::{BrainSource, MemoryLayout};

pub use types::{BrainBatch, BrainDocument, BrainSourceCount, BrainSourcesView, Ingested};

/// The brain over one engine and layout. Cheap to clone.
#[derive(Clone)]
pub struct Brain {
    engine: Arc<dyn MemoryEngine>,
    layout: MemoryLayout,
}

impl std::fmt::Debug for Brain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Brain")
            .field("engine", &self.engine.descriptor().id)
            .field("root", &self.layout.root().to_string())
            .finish()
    }
}

impl Brain {
    /// The brain of `layout` on `engine`.
    #[must_use]
    pub fn new(engine: Arc<dyn MemoryEngine>, layout: MemoryLayout) -> Self {
        Self { engine, layout }
    }

    /// The layout the brain writes to.
    #[must_use]
    pub fn layout(&self) -> &MemoryLayout {
        &self.layout
    }

    /// Counts documents by source in this brain layout.
    ///
    /// Source collections such as GitHub projects are included in their
    /// parent's count. Documents without a source segment at the layout's
    /// source depth, and documents missing from the namespace facet, are
    /// reported as unfiled. Exploration uses the host-compatible bounds of
    /// 200 namespace buckets and 20,000 scanned items, so a truncated engine
    /// result is a lower bound.
    ///
    /// # Errors
    ///
    /// The engine's errors while exploring the layout.
    pub async fn sources(&self) -> Result<BrainSourcesView> {
        let page = self
            .engine
            .explore(ExploreRequest {
                facet: Facet::Namespace,
                filter: self.layout.brain_filter(None),
                limit: 200,
                scan_limit: 20_000,
            })
            .await?;
        let source_of = |node: &str| -> Option<String> {
            let node: tinymemory_api::Namespace = node.parse().ok()?;
            let segment = node.segments().get(self.layout.root().depth())?;
            (segment.kind() == SegmentKind::Source).then(|| segment.id().to_string())
        };
        let mut counts = std::collections::BTreeMap::<String, u64>::new();
        let mut unfiled = page.missing;
        for bucket in page.buckets {
            match source_of(&bucket.value) {
                Some(source) => *counts.entry(source).or_default() += bucket.count,
                None => unfiled += bucket.count,
            }
        }
        let mut sources: Vec<BrainSourceCount> = counts
            .into_iter()
            .map(|(source, documents)| BrainSourceCount { source, documents })
            .collect();
        sources.sort_by(|a, b| b.documents.cmp(&a.documents).then(a.source.cmp(&b.source)));
        Ok(BrainSourcesView {
            root: self.layout.root().to_string(),
            sources,
            unfiled,
        })
    }

    /// Stores one document at its source's node, returning once it is
    /// readable.
    ///
    /// # Errors
    ///
    /// An invalid document, and the engine's failures.
    pub async fn ingest(&self, document: BrainDocument) -> Result<Ingested> {
        self.ingest_with(document, WriteOptions::visible()).await
    }

    /// [`Brain::ingest`], returning as soon as `options` allows.
    ///
    /// # Errors
    ///
    /// As [`Brain::ingest`].
    pub async fn ingest_with(
        &self,
        document: BrainDocument,
        options: WriteOptions,
    ) -> Result<Ingested> {
        let source = document.source.clone();
        let item = document.into_item(&self.layout)?;
        let receipt = self.engine.store_with(item, options).await?;
        let job = if builds_on_its_own(self.engine.as_ref()) {
            None
        } else {
            Some(self.build(&source)?)
        };
        Ok(Ingested { receipt, job })
    }

    /// Stores many documents, in order, in batches of at most
    /// [`MAX_STORE_MANY`]; returns every receipt and one belief build per
    /// source touched.
    ///
    /// # Errors
    ///
    /// An invalid document (nothing is stored), and the engine's failures
    /// (earlier batches stay stored; storing again replays them).
    pub async fn ingest_many(&self, documents: Vec<BrainDocument>) -> Result<BrainBatch> {
        let sources: BTreeSet<BrainSource> =
            documents.iter().map(|doc| doc.source.clone()).collect();
        let items = documents
            .into_iter()
            .map(|document| document.into_item(&self.layout))
            .collect::<Result<Vec<StoreItem>>>()?;
        let mut receipts = Vec::with_capacity(items.len());
        let mut rest = items;
        while !rest.is_empty() {
            let tail = rest.split_off(rest.len().min(MAX_STORE_MANY));
            receipts.extend(self.engine.store_many(rest).await?);
            rest = tail;
        }
        let jobs = if builds_on_its_own(self.engine.as_ref()) {
            Vec::new()
        } else {
            sources
                .iter()
                .map(|source| self.build(source))
                .collect::<Result<Vec<_>>>()?
        };
        Ok(BrainBatch { receipts, jobs })
    }

    /// Ranked documents for `query`: one source's, or the whole brain's.
    ///
    /// # Errors
    ///
    /// An invalid query, an engine with no fetch mode, and the engine's
    /// failures.
    pub async fn search(
        &self,
        query: &str,
        source: Option<&BrainSource>,
        limit: usize,
    ) -> Result<Vec<Hit>> {
        let descriptor = self.engine.descriptor();
        let mode = if descriptor.supports(FetchMode::Hybrid) {
            FetchMode::Hybrid
        } else {
            descriptor.fetch_modes.first().copied().ok_or_else(|| {
                tinymemory_api::Error::Unsupported(format!(
                    "engine `{}` offers no fetch mode",
                    descriptor.id
                ))
            })?
        };
        let mut request = FetchRequest::new(query, mode, limit);
        request.filter = self.layout.brain_filter(source);
        Ok(self.engine.fetch(request).await?.hits)
    }

    /// Erases one source's documents, its collections' included (and the
    /// beliefs an engine built in those scopes are the engine's to drop).
    ///
    /// # Errors
    ///
    /// The engine's failures.
    pub async fn forget(&self, source: &BrainSource) -> Result<ForgetReport> {
        // The reach is set from `brain(source)?`, not left to
        // `brain_filter`, which falls back to the whole layout.
        let mut filter = self.layout.brain_filter(Some(source));
        filter.reach = Some(Reach::subtree(self.layout.brain(source)?));
        self.engine.forget(ForgetTarget::Filter(filter)).await
    }

    /// A belief build of `source`'s documents, its collections' included,
    /// for the host to run now or queue: what an ingest hands back, and a
    /// refresh on an engine that builds on its own.
    ///
    /// # Errors
    ///
    /// Never for a layout built by [`MemoryLayout::new`] (see
    /// [`MemoryLayout::brain`]).
    pub fn build(&self, source: &BrainSource) -> Result<BackgroundJob> {
        Ok(BackgroundJob::BuildBeliefs {
            request: ConsolidateRequest::new(Reach::subtree(self.layout.brain(source)?))
                .kinds([ItemKind::Document]),
        })
    }
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
