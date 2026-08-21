//! Tests for the shared mandatory-family logic.
//!
//! These run against [`VecMemory`], a deliberately dumb in-process [`Memory`](crate::traits::Memory)
//! backend defined here rather than borrowed from an engine crate: the point of
//! this module is that the logic is engine-neutral, and a test that needed a
//! real engine would not demonstrate that.

// A failing assertion in a test *is* a panic; the crate-wide `expect_used` /
// `panic` lints exist to keep the library from panicking, not the tests.
#![allow(clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::provider::audit_provider;

use super::*;

/// A minimal `Memory` over a `BTreeMap`, keyed `(namespace, key)`.
///
/// `store_with_taint` is **overridden**, which is the whole point: the trait
/// default silently drops the taint, so a backend relying on it could not
/// preserve provenance across an import and the taint tests below would pass
/// for the wrong reason.
#[derive(Default)]
struct VecMemory {
    entries: Mutex<BTreeMap<(String, String), MemoryEntry>>,
    healthy: bool,
    failure: Option<Failure>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Failure {
    Store,
    Recall,
    List,
    Summaries,
}

impl VecMemory {
    fn healthy() -> Arc<Self> {
        Arc::new(Self {
            entries: Mutex::new(BTreeMap::new()),
            healthy: true,
            failure: None,
        })
    }

    fn unhealthy() -> Arc<Self> {
        Arc::new(Self {
            entries: Mutex::new(BTreeMap::new()),
            healthy: false,
            failure: None,
        })
    }

    fn failing(failure: Failure) -> Arc<Self> {
        Arc::new(Self {
            entries: Mutex::new(BTreeMap::new()),
            healthy: true,
            failure: Some(failure),
        })
    }
}

use std::sync::Arc;

use crate::types::NamespaceSummary;
use async_trait::async_trait;

#[async_trait]
impl Memory for VecMemory {
    async fn store(
        &self,
        namespace: &str,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        self.store_with_taint(
            namespace,
            key,
            content,
            category,
            session_id,
            crate::types::MemoryTaint::Internal,
        )
        .await
    }

    async fn store_with_taint(
        &self,
        namespace: &str,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        taint: crate::types::MemoryTaint,
    ) -> anyhow::Result<()> {
        if self.failure == Some(Failure::Store) {
            anyhow::bail!("store failed");
        }
        let entry = MemoryEntry {
            id: format!("{namespace}/{key}"),
            key: key.to_string(),
            content: content.to_string(),
            namespace: Some(namespace.to_string()),
            category,
            timestamp: "2026-08-10T00:00:00Z".to_string(),
            session_id: session_id.map(str::to_string),
            score: None,
            taint,
        };
        self.entries
            .lock()
            .expect("lock")
            .insert((namespace.to_string(), key.to_string()), entry);
        Ok(())
    }

    async fn recall(
        &self,
        query: &str,
        limit: usize,
        opts: RecallOpts<'_>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        if self.failure == Some(Failure::Recall) {
            anyhow::bail!("recall failed");
        }
        let entries = self.entries.lock().expect("lock");
        Ok(entries
            .values()
            .filter(|e| {
                opts.namespace
                    .is_none_or(|ns| e.namespace.as_deref() == Some(ns))
            })
            .filter(|e| e.content.contains(query))
            .take(limit)
            .cloned()
            .collect())
    }

    async fn get(&self, namespace: &str, key: &str) -> anyhow::Result<Option<MemoryEntry>> {
        Ok(self
            .entries
            .lock()
            .expect("lock")
            .get(&(namespace.to_string(), key.to_string()))
            .cloned())
    }

    /// Deliberately reproduces the trap the shared layer exists to avoid: a
    /// `None` namespace is normalised to the global namespace rather than
    /// meaning "everything".
    async fn list(
        &self,
        namespace: Option<&str>,
        category: Option<&MemoryCategory>,
        session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        if self.failure == Some(Failure::List) {
            anyhow::bail!("list failed");
        }
        let wanted = namespace.unwrap_or(GLOBAL_NAMESPACE);
        let entries = self.entries.lock().expect("lock");
        Ok(entries
            .values()
            .filter(|e| e.namespace.as_deref() == Some(wanted))
            .filter(|e| category.is_none_or(|c| &e.category == c))
            .filter(|e| session_id.is_none_or(|s| e.session_id.as_deref() == Some(s)))
            .cloned()
            .collect())
    }

    async fn forget(&self, namespace: &str, key: &str) -> anyhow::Result<bool> {
        Ok(self
            .entries
            .lock()
            .expect("lock")
            .remove(&(namespace.to_string(), key.to_string()))
            .is_some())
    }

    async fn namespace_summaries(&self) -> anyhow::Result<Vec<NamespaceSummary>> {
        if self.failure == Some(Failure::Summaries) {
            anyhow::bail!("summaries failed");
        }
        let entries = self.entries.lock().expect("lock");
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for entry in entries.values() {
            *counts
                .entry(entry.namespace.clone().unwrap_or_default())
                .or_default() += 1;
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

    async fn count(&self) -> anyhow::Result<usize> {
        Ok(self.entries.lock().expect("lock").len())
    }

    async fn health_check(&self) -> bool {
        self.healthy
    }

    fn name(&self) -> &'static str {
        "vec"
    }
}

async fn seeded() -> Arc<VecMemory> {
    let memory = VecMemory::healthy();
    memory
        .store(GLOBAL_NAMESPACE, "a", "alpha", MemoryCategory::Core, None)
        .await
        .expect("store");
    memory
        .store("projects", "b", "beta", MemoryCategory::Core, None)
        .await
        .expect("store");
    memory
        .store("projects", "c", "gamma", MemoryCategory::Core, None)
        .await
        .expect("store");
    memory
}

fn provider(memory: Arc<VecMemory>) -> MemoryTraitProvider {
    MemoryTraitProvider::new(memory, "vec")
}

/// The bug this layer exists to prevent: a backend that normalises a `None`
/// namespace to the global one would report one namespace as "everything".
#[tokio::test]
async fn list_everything_spans_every_namespace() {
    let memory = seeded().await;

    let naive = memory.list(None, None, None).await.expect("naive list");
    assert_eq!(naive.len(), 1, "the backend alone narrows to one namespace");

    let all = list_everything(memory.as_ref(), None, None)
        .await
        .expect("list everything");
    assert_eq!(all.len(), 3);
}

#[tokio::test]
async fn list_with_a_namespace_still_narrows() {
    let all = provider(seeded().await)
        .list(Some("projects"), None, None)
        .await
        .expect("list");
    assert_eq!(all.len(), 2);
}

/// `store` must route through `store_with_taint`, or externally-sourced content
/// is laundered into internal-trust content.
#[tokio::test]
async fn store_preserves_the_taint_it_is_given() {
    let memory = VecMemory::healthy();
    provider(Arc::clone(&memory))
        .store(
            "ns",
            "k",
            "body",
            MemoryCategory::Core,
            None,
            MemoryTaint::ExternalSync,
        )
        .await
        .expect("store");

    let stored = memory.get("ns", "k").await.expect("get").expect("present");
    assert_eq!(stored.taint, MemoryTaint::ExternalSync);
}

#[tokio::test]
async fn a_scoped_recall_is_refused_rather_than_answered_in_full() {
    let scope = SourceScope::default();
    let error = recall(
        seeded().await.as_ref(),
        "a",
        10,
        &OwnedRecallOpts::default(),
        Some(&scope),
    )
    .await
    .expect_err("a scoped recall is refused");

    match error {
        MemoryError::Invalid(reason) => assert_eq!(reason, SCOPE_UNAPPLIED),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[tokio::test]
async fn an_unscoped_recall_delegates() {
    let hits = provider(seeded().await)
        .recall("alpha", 10, &OwnedRecallOpts::default(), None)
        .await
        .expect("recall");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].key, "a");
}

#[tokio::test]
async fn backend_failures_cross_each_mandatory_boundary_as_other() {
    let list_error = list_everything(VecMemory::failing(Failure::Summaries).as_ref(), None, None)
        .await
        .expect_err("summary failure");
    assert!(matches!(list_error, MemoryError::Other(_)));

    let list_memory = VecMemory::failing(Failure::List);
    list_memory.entries.lock().expect("lock").insert(
        ("ns".into(), "key".into()),
        MemoryEntry {
            id: "ns/key".into(),
            key: "key".into(),
            content: "body".into(),
            namespace: Some("ns".into()),
            category: MemoryCategory::Core,
            timestamp: "2026-01-01T00:00:00Z".into(),
            session_id: None,
            score: None,
            taint: MemoryTaint::Internal,
        },
    );
    let list_error = list_everything(list_memory.as_ref(), None, None)
        .await
        .expect_err("list failure");
    assert!(matches!(list_error, MemoryError::Other(_)));

    let recall_error = recall(
        VecMemory::failing(Failure::Recall).as_ref(),
        "query",
        10,
        &OwnedRecallOpts::default(),
        None,
    )
    .await
    .expect_err("recall failure");
    assert!(matches!(recall_error, MemoryError::Other(_)));

    let export_error = export_page(VecMemory::failing(Failure::Summaries).as_ref(), None, 10)
        .await
        .expect_err("export summary failure");
    assert!(matches!(export_error, MemoryError::Other(_)));

    let import_error = import_records(
        VecMemory::failing(Failure::Store).as_ref(),
        vec![ExportRecord {
            kind: ENTRY_KIND.into(),
            id: "record".into(),
            namespace: Some("ns".into()),
            taint: MemoryTaint::Internal,
            payload: serde_json::json!({
                "key": "key",
                "content": "body",
                "category": "core"
            }),
        }],
    )
    .await
    .expect_err("import write failure");
    assert!(matches!(import_error, MemoryError::Other(_)));
}

#[test]
fn engine_error_preserves_an_existing_contract_error() {
    let error = engine_error(anyhow::Error::new(MemoryError::Unauthorized("key".into())));
    assert!(matches!(error, MemoryError::Unauthorized(reason) if reason == "key"));
}

#[tokio::test]
async fn export_pages_across_namespaces_and_terminates_on_a_none_cursor() {
    let driver = provider(seeded().await);

    let mut seen = Vec::new();
    let mut cursor = None;
    let mut pages = 0;
    loop {
        let page = driver
            .export_page(cursor.as_deref(), 2)
            .await
            .expect("export page");
        pages += 1;
        assert!(pages < 10, "export did not terminate");
        seen.extend(page.records.iter().map(|r| r.id.clone()));
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }

    seen.sort();
    assert_eq!(seen, vec!["global/a", "projects/b", "projects/c"]);
}

#[tokio::test]
async fn an_empty_store_exports_one_empty_terminal_page() {
    let page = provider(VecMemory::healthy())
        .export_page(None, 10)
        .await
        .expect("export page");
    assert!(page.records.is_empty());
    assert!(page.next_cursor.is_none());
}

#[tokio::test]
async fn a_zero_limit_is_refused() {
    let error = provider(seeded().await)
        .export_page(None, 0)
        .await
        .expect_err("a zero page size cannot make progress");
    assert!(matches!(error, MemoryError::Invalid(_)));
}

#[tokio::test]
async fn a_cursor_this_driver_did_not_issue_is_refused() {
    let driver = provider(seeded().await);
    for bogus in ["nonsense", "1", "x:0", "0:y"] {
        let error = driver
            .export_page(Some(bogus), 10)
            .await
            .expect_err("bogus cursor");
        assert!(
            matches!(error, MemoryError::Invalid(_)),
            "cursor {bogus:?} should be Invalid"
        );
    }

    let error = driver
        .export_page(Some("99:0"), 10)
        .await
        .expect_err("out-of-range namespace index");
    assert!(matches!(error, MemoryError::Invalid(_)));

    let error = driver
        .export_page(Some("0:99"), 10)
        .await
        .expect_err("out-of-range offset");
    assert!(matches!(error, MemoryError::Invalid(_)));
}

/// The round trip is the point of the family: a driver you cannot export from
/// is a driver you cannot unbind.
#[tokio::test]
async fn export_round_trips_through_import_with_provenance_intact() {
    let source = VecMemory::healthy();
    source
        .store_with_taint(
            "ns",
            "external",
            "from a sync",
            MemoryCategory::Core,
            Some("s1"),
            MemoryTaint::ExternalSync,
        )
        .await
        .expect("store");
    source
        .store_with_taint(
            "ns",
            "internal",
            "typed by the user",
            MemoryCategory::Daily,
            None,
            MemoryTaint::Internal,
        )
        .await
        .expect("store");

    let page = provider(Arc::clone(&source))
        .export_page(None, 10)
        .await
        .expect("export");
    assert_eq!(page.records.len(), 2);

    let target = VecMemory::healthy();
    let outcome = provider(Arc::clone(&target))
        .import_records(page.records)
        .await
        .expect("import");
    assert_eq!(outcome.imported, 2);
    assert_eq!(outcome.failed, 0);

    let external = target
        .get("ns", "external")
        .await
        .expect("get")
        .expect("present");
    assert_eq!(
        external.taint,
        MemoryTaint::ExternalSync,
        "an importing driver must not re-stamp provenance"
    );
    assert_eq!(external.content, "from a sync");
    assert_eq!(external.session_id.as_deref(), Some("s1"));
    assert_eq!(external.category, MemoryCategory::Core);

    let internal = target
        .get("ns", "internal")
        .await
        .expect("get")
        .expect("present");
    assert_eq!(internal.taint, MemoryTaint::Internal);
    assert_eq!(internal.category, MemoryCategory::Daily);
}

/// A malformed record is reported, not fatal — a large restore must not abort
/// on one bad row.
#[tokio::test]
async fn a_malformed_record_is_reported_without_aborting_the_batch() {
    let target = VecMemory::healthy();
    let good = to_record(MemoryEntry {
        id: "ns/ok".to_string(),
        key: "ok".to_string(),
        content: "body".to_string(),
        namespace: Some("ns".to_string()),
        category: MemoryCategory::Core,
        timestamp: "2026-08-10T00:00:00Z".to_string(),
        session_id: None,
        score: None,
        taint: MemoryTaint::Internal,
    });
    let wrong_kind = ExportRecord {
        kind: "document".to_string(),
        id: "ns/doc".to_string(),
        namespace: Some("ns".to_string()),
        taint: MemoryTaint::Internal,
        payload: serde_json::json!({}),
    };
    let missing_content = ExportRecord {
        kind: ENTRY_KIND.to_string(),
        id: "ns/partial".to_string(),
        namespace: Some("ns".to_string()),
        taint: MemoryTaint::Internal,
        payload: serde_json::json!({ "key": "partial", "category": "core" }),
    };

    let outcome = provider(Arc::clone(&target))
        .import_records(vec![wrong_kind, good, missing_content])
        .await
        .expect("import");

    assert_eq!(outcome.imported, 1);
    assert_eq!(outcome.failed, 2);
    assert_eq!(
        outcome.errors.len(),
        2,
        "every rejection must be diagnosable"
    );
    assert!(target.get("ns", "ok").await.expect("get").is_some());
}

/// Rejection reasons are logged, so they must name the record and the problem
/// and carry none of its content.
#[tokio::test]
async fn a_rejection_reason_carries_no_record_content() {
    let secret = "hunter2-do-not-log-me";
    let record = ExportRecord {
        kind: ENTRY_KIND.to_string(),
        id: "ns/partial".to_string(),
        namespace: Some("ns".to_string()),
        taint: MemoryTaint::Internal,
        payload: serde_json::json!({ "key": "partial", "content": secret }),
    };
    let reason = read_record(&record).expect_err("missing category");
    assert!(
        reason.contains("ns/partial"),
        "reason should name the record"
    );
    assert!(
        reason.contains("category"),
        "reason should name the problem"
    );
    assert!(!reason.contains(secret), "reason must not carry content");
}

/// A record with no namespace lands in the global namespace rather than being
/// dropped.
#[tokio::test]
async fn a_namespaceless_record_imports_globally() {
    let target = VecMemory::healthy();
    let record = ExportRecord {
        kind: ENTRY_KIND.to_string(),
        id: "orphan".to_string(),
        namespace: None,
        taint: MemoryTaint::Internal,
        payload: serde_json::json!({ "key": "k", "content": "v", "category": "core" }),
    };
    let outcome = provider(Arc::clone(&target))
        .import_records(vec![record])
        .await
        .expect("import");
    assert_eq!(outcome.imported, 1);
    assert!(target
        .get(GLOBAL_NAMESPACE, "k")
        .await
        .expect("get")
        .is_some());
}

/// Advertised capabilities and reachable accessors must agree, or a host
/// filters its RPC surface from a claim the driver cannot honour.
#[tokio::test]
async fn the_advertised_set_matches_what_is_actually_reachable() {
    let driver = provider(VecMemory::healthy());
    audit_provider(&driver).expect("advertised capabilities match the accessors");

    let capabilities = driver.capabilities();
    for mandatory in [
        Capability::Core,
        Capability::Recall,
        Capability::Portability,
    ] {
        assert!(capabilities.contains(mandatory));
        assert!(driver.provides(mandatory));
    }
    for optional in [
        Capability::Ingest,
        Capability::Documents,
        Capability::Tree,
        Capability::Entities,
        Capability::Graph,
        Capability::Diff,
        Capability::Goals,
        Capability::ToolMemory,
        Capability::Sources,
        Capability::Maintenance,
    ] {
        assert!(
            !capabilities.contains(optional),
            "{optional:?} must be absent, not present-and-failing"
        );
        assert!(!driver.provides(optional));
    }
}

#[tokio::test]
async fn health_follows_the_backend() {
    assert_eq!(
        provider(VecMemory::healthy()).health().await,
        MemoryHealth::Ready
    );
    assert!(matches!(
        provider(VecMemory::unhealthy()).health().await,
        MemoryHealth::Down { .. }
    ));
}

/// A driver id appears in logs and audit events, so it must not be rendered
/// from a backend handle that could hold a connection string.
#[test]
fn debug_renders_the_driver_id_and_not_the_backend() {
    let rendered = format!("{:?}", provider(VecMemory::healthy()));
    assert!(rendered.contains("vec"));
    assert!(!rendered.contains("VecMemory"));
}

use crate::capabilities::Capability;
use crate::health::MemoryHealth;
use crate::provider::{MemoryCore, MemoryPortability, MemoryProvider, MemoryRecall};
use crate::types::MemoryTaint;
