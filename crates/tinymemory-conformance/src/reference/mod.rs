//! An in-memory reference driver.
//!
//! This is the driver the suite is calibrated against: the simplest thing that
//! upholds the contract, with no storage engine, no network, and no
//! configuration. It exists for two reasons.
//!
//! First, a conformance suite needs a known-good subject. An assertion that
//! only ever runs against real engines cannot distinguish "the engine is wrong"
//! from "the assertion is wrong"; running it against a driver whose behaviour is
//! obvious by inspection separates those.
//!
//! Second, it documents the contract by example. Everything here is the
//! minimum a driver must do — the `(namespace, key)` upsert, the fail-closed
//! taint handling, the cursor that terminates on `None` rather than on an empty
//! page — so a new engine author has something short to read.
//!
//! It advertises exactly the three mandatory families and leaves every optional
//! accessor at `None`, which is the honest answer for a store with no tree, no
//! graph, and no ingestion pipeline.

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use tinymemory_api::capabilities::Capabilities;
use tinymemory_api::error::MemoryError;
use tinymemory_api::health::MemoryHealth;
use tinymemory_api::provider::{
    ExportPage, ExportRecord, ImportOutcome, MemoryCore, MemoryPortability, MemoryProvider,
    MemoryRecall, SourceScope,
};
use tinymemory_api::recall::OwnedRecallOpts;
use tinymemory_api::types::{MemoryCategory, MemoryEntry, MemoryTaint, NamespaceSummary};

/// The driver id this reference binds under.
pub const REFERENCE_DRIVER_ID: &str = "reference";

/// How many records one [`MemoryPortability::export_page`] call returns when the
/// caller asks for more than this. Deliberately small so the suite's pagination
/// assertion has something to paginate over without building a large fixture.
const MAX_PAGE: usize = 64;

/// The row map, keyed by the `(namespace, key)` pair the contract upserts on.
type Rows = BTreeMap<(String, String), MemoryEntry>;

/// A held lock over [`Rows`].
type RowGuard<'a> = std::sync::MutexGuard<'a, Rows>;

/// An in-memory [`MemoryProvider`], keyed exactly as the contract specifies.
#[derive(Debug, Default)]
pub struct InMemoryProvider {
    rows: Mutex<Rows>,
}

impl InMemoryProvider {
    /// Builds an empty reference driver.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Locks the row map, mapping a poisoned lock onto a contract error.
    ///
    /// A poisoned lock means a previous caller panicked mid-write. Returning an
    /// error rather than propagating the panic keeps the driver's failure mode
    /// inside the contract, which is what the suite asserts of every driver.
    fn rows(&self) -> Result<RowGuard<'_>, MemoryError> {
        self.rows
            .lock()
            .map_err(|_| MemoryError::Other(anyhow_poisoned()))
    }
}

/// The one place this crate builds an `anyhow::Error`, so the dependency stays
/// visible rather than scattered.
fn anyhow_poisoned() -> anyhow::Error {
    anyhow::anyhow!("reference driver lock poisoned by a panicking caller")
}

#[async_trait]
impl MemoryCore for InMemoryProvider {
    async fn store(
        &self,
        namespace: &str,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        taint: MemoryTaint,
    ) -> Result<(), MemoryError> {
        // Upsert on `(namespace, key)` — the contract's words. A second store at
        // the same pair replaces content, category and session, and does not
        // create a duplicate.
        self.rows()?.insert(
            (namespace.to_string(), key.to_string()),
            MemoryEntry {
                id: format!("{namespace}::{key}"),
                key: key.to_string(),
                content: content.to_string(),
                namespace: Some(namespace.to_string()),
                category,
                timestamp: "1970-01-01T00:00:00Z".to_string(),
                session_id: session_id.map(str::to_owned),
                score: None,
                // Persisted as given. A driver that re-stamped this would
                // launder external content into internal-trust content, which
                // is the failure the parameter exists to prevent.
                taint,
            },
        );
        Ok(())
    }

    async fn get(&self, namespace: &str, key: &str) -> Result<Option<MemoryEntry>, MemoryError> {
        Ok(self
            .rows()?
            .get(&(namespace.to_string(), key.to_string()))
            .cloned())
    }

    async fn forget(&self, namespace: &str, key: &str) -> Result<bool, MemoryError> {
        Ok(self
            .rows()?
            .remove(&(namespace.to_string(), key.to_string()))
            .is_some())
    }

    async fn list(
        &self,
        namespace: Option<&str>,
        category: Option<&MemoryCategory>,
        session_id: Option<&str>,
    ) -> Result<Vec<MemoryEntry>, MemoryError> {
        Ok(self
            .rows()?
            .values()
            .filter(|e| namespace.is_none_or(|ns| e.namespace.as_deref() == Some(ns)))
            .filter(|e| category.is_none_or(|c| &e.category == c))
            .filter(|e| session_id.is_none_or(|s| e.session_id.as_deref() == Some(s)))
            .cloned()
            .collect())
    }

    async fn namespaces(&self) -> Result<Vec<NamespaceSummary>, MemoryError> {
        let rows = self.rows()?;
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for entry in rows.values() {
            if let Some(ns) = entry.namespace.as_deref() {
                *counts.entry(ns.to_string()).or_default() += 1;
            }
        }
        Ok(counts
            .into_iter()
            .map(|(namespace, count)| NamespaceSummary {
                namespace,
                count,
                last_updated: None,
            })
            .collect())
    }
}

#[async_trait]
impl MemoryRecall for InMemoryProvider {
    async fn recall(
        &self,
        query: &str,
        limit: usize,
        opts: &OwnedRecallOpts,
        scope: Option<&SourceScope>,
    ) -> Result<Vec<MemoryEntry>, MemoryError> {
        if scope.is_some_and(SourceScope::is_empty) {
            return Ok(Vec::new());
        }
        let needle = query.to_lowercase();
        Ok(self
            .rows()?
            .values()
            .filter(|e| {
                opts.namespace
                    .as_deref()
                    .is_none_or(|ns| e.namespace.as_deref() == Some(ns))
            })
            .filter(|e| e.content.to_lowercase().contains(&needle))
            .take(limit)
            .cloned()
            .collect())
    }
}

#[async_trait]
impl MemoryPortability for InMemoryProvider {
    async fn export_page(
        &self,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<ExportPage, MemoryError> {
        // The cursor is an offset rendered as a decimal string. A cursor this
        // driver did not issue is `Invalid`, not a silent restart from zero —
        // silently restarting would make a resumed export duplicate everything
        // it had already written.
        let offset: usize = match cursor {
            None => 0,
            Some(raw) => raw
                .parse()
                .map_err(|_| MemoryError::Invalid(format!("unknown export cursor: {raw}")))?,
        };
        let rows = self.rows()?;
        let take = limit.clamp(1, MAX_PAGE);
        let records: Vec<ExportRecord> = rows
            .values()
            .skip(offset)
            .take(take)
            .map(|e| ExportRecord {
                kind: "entry".to_string(),
                id: e.id.clone(),
                namespace: e.namespace.clone(),
                taint: e.taint,
                payload: serde_json::json!({
                    "key": e.key,
                    "content": e.content,
                    "category": e.category.to_string(),
                    "session_id": e.session_id,
                }),
            })
            .collect();
        let consumed = offset + records.len();
        // `None` terminates, not an empty page — the contract is explicit that
        // an empty `records` is not the terminator.
        let next_cursor = (consumed < rows.len()).then(|| consumed.to_string());
        Ok(ExportPage {
            records,
            next_cursor,
        })
    }

    async fn import_records(
        &self,
        records: Vec<ExportRecord>,
    ) -> Result<ImportOutcome, MemoryError> {
        let mut outcome = ImportOutcome::default();
        for record in records {
            let (Some(namespace), Some(key)) = (
                record.namespace.clone(),
                record
                    .payload
                    .get("key")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
            ) else {
                // Per-record rejection is reported, not returned as an error: a
                // migration must not abort a whole restore over one bad row.
                outcome.failed += 1;
                outcome
                    .errors
                    .push(format!("record {} lacks a namespace or key", record.id));
                continue;
            };
            let content = record
                .payload
                .get("content")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            let category = record
                .payload
                .get("category")
                .and_then(serde_json::Value::as_str)
                .and_then(|c| c.parse().ok())
                .unwrap_or(MemoryCategory::Core);
            let session_id = record
                .payload
                .get("session_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            // `record.taint` verbatim — see the note on `store`.
            self.store(
                &namespace,
                &key,
                &content,
                category,
                session_id.as_deref(),
                record.taint,
            )
            .await?;
            outcome.imported += 1;
        }
        Ok(outcome)
    }
}

#[async_trait]
impl MemoryProvider for InMemoryProvider {
    fn driver_id(&self) -> &str {
        REFERENCE_DRIVER_ID
    }

    fn capabilities(&self) -> Capabilities {
        // Exactly what is reachable. Advertising more would fail
        // `audit_provider`, which is itself one of the suite's assertions.
        Capabilities::mandatory()
    }

    async fn health(&self) -> MemoryHealth {
        MemoryHealth::Ready
    }
}
