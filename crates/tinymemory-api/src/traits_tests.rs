//! Tests for fail-closed defaults on [`super::Memory`].

use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;

use super::Memory;
use crate::types::{MemoryCategory, MemoryEntry, MemoryTaint, NamespaceSummary, RecallOpts};

#[derive(Default)]
struct MinimalMemory {
    stores: AtomicUsize,
}

#[async_trait]
impl Memory for MinimalMemory {
    fn name(&self) -> &str {
        "minimal"
    }

    async fn store(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: MemoryCategory,
        _: Option<&str>,
    ) -> anyhow::Result<()> {
        self.stores.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    async fn recall(
        &self,
        _: &str,
        _: usize,
        _: RecallOpts<'_>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        Ok(Vec::new())
    }
    async fn get(&self, _: &str, _: &str) -> anyhow::Result<Option<MemoryEntry>> {
        Ok(None)
    }
    async fn list(
        &self,
        _: Option<&str>,
        _: Option<&MemoryCategory>,
        _: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        Ok(Vec::new())
    }
    async fn forget(&self, _: &str, _: &str) -> anyhow::Result<bool> {
        Ok(false)
    }
    async fn namespace_summaries(&self) -> anyhow::Result<Vec<NamespaceSummary>> {
        Ok(Vec::new())
    }
    async fn count(&self) -> anyhow::Result<usize> {
        Ok(0)
    }
    async fn health_check(&self) -> bool {
        true
    }
}

#[tokio::test]
async fn default_taint_storage_delegates_only_for_internal_content() {
    let memory = MinimalMemory::default();
    memory
        .store_with_taint(
            "ns",
            "key",
            "value",
            MemoryCategory::Core,
            None,
            MemoryTaint::Internal,
        )
        .await
        .unwrap();
    assert_eq!(memory.stores.load(Ordering::Relaxed), 1);

    let error = memory
        .store_with_taint(
            "ns",
            "external",
            "value",
            MemoryCategory::Core,
            None,
            MemoryTaint::ExternalSync,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("taint-preserving"));
    assert_eq!(
        memory.stores.load(Ordering::Relaxed),
        1,
        "external content must not reach a backend that would drop its taint"
    );
}

#[tokio::test]
async fn optional_memory_defaults_are_empty_and_unhealthy_detail_is_absent() {
    let memory = MinimalMemory::default();
    assert!(memory
        .recall_relevant_by_vector("ns", "q", 10, 0.5)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(memory.health_probe().await, None);
}
