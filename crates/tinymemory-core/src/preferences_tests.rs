//! Tests for the surrounding module.

use super::*;
use crate::store::UnifiedMemory;
use crate::MemoryCategory;
use tempfile::TempDir;
use tinymemory_api::host::NoopEmbedding;

#[derive(Default)]
struct VectorMemory {
    calls: std::sync::Mutex<Vec<(String, usize, f64)>>,
    general: Vec<(String, String)>,
    situational: Vec<(String, String)>,
    fail_general: bool,
}

#[async_trait::async_trait]
impl Memory for VectorMemory {
    fn name(&self) -> &str {
        "vector-fixture"
    }

    async fn store(
        &self,
        _namespace: &str,
        _key: &str,
        _content: &str,
        _category: MemoryCategory,
        _session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn recall(
        &self,
        _query: &str,
        _limit: usize,
        _opts: crate::RecallOpts<'_>,
    ) -> anyhow::Result<Vec<crate::MemoryEntry>> {
        Ok(Vec::new())
    }

    async fn recall_relevant_by_vector(
        &self,
        namespace: &str,
        _query: &str,
        limit: usize,
        minimum: f64,
    ) -> anyhow::Result<Vec<(String, String)>> {
        self.calls
            .lock()
            .unwrap()
            .push((namespace.into(), limit, minimum));
        if namespace == USER_PREF_GENERAL_NAMESPACE && self.fail_general {
            anyhow::bail!("unavailable")
        }
        let values = if namespace == USER_PREF_GENERAL_NAMESPACE {
            &self.general
        } else {
            &self.situational
        };
        Ok(values.iter().take(limit).cloned().collect())
    }

    async fn get(
        &self,
        _namespace: &str,
        _key: &str,
    ) -> anyhow::Result<Option<crate::MemoryEntry>> {
        Ok(None)
    }

    async fn list(
        &self,
        _namespace: Option<&str>,
        _category: Option<&MemoryCategory>,
        _session_id: Option<&str>,
    ) -> anyhow::Result<Vec<crate::MemoryEntry>> {
        Ok(Vec::new())
    }

    async fn forget(&self, _namespace: &str, _key: &str) -> anyhow::Result<bool> {
        Ok(false)
    }

    async fn namespace_summaries(&self) -> anyhow::Result<Vec<crate::NamespaceSummary>> {
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
async fn load_general_preferences_returns_values_newest_first_capped() {
    let tmp = TempDir::new().unwrap();
    let mem: Arc<dyn Memory> =
        Arc::new(UnifiedMemory::new(tmp.path(), Arc::new(NoopEmbedding), None).unwrap());

    mem.store(
        USER_PREF_GENERAL_NAMESPACE,
        "reply_language",
        "Reply in British English.",
        MemoryCategory::Core,
        None,
    )
    .await
    .unwrap();
    mem.store(
        USER_PREF_GENERAL_NAMESPACE,
        "tone",
        "Be terse.",
        MemoryCategory::Core,
        None,
    )
    .await
    .unwrap();

    let general = load_general_preferences(&mem, 10).await;
    // Returns the values (bodies), not the topic keys.
    assert!(general.iter().any(|v| v.contains("British English")));
    assert!(general.iter().any(|v| v.contains("Be terse")));
    assert!(!general.iter().any(|v| v == "reply_language"));

    // The limit caps the block.
    assert_eq!(load_general_preferences(&mem, 1).await.len(), 1);
}

#[tokio::test]
async fn situational_recall_is_bounded_thresholded_and_fail_closed() {
    let fixture = Arc::new(VectorMemory {
        situational: vec![("tone".into(), "Be concise".into())],
        ..Default::default()
    });
    let memory: Arc<dyn Memory> = fixture.clone();
    assert!(recall_situational_preferences(&memory, "  ")
        .await
        .is_empty());
    assert!(fixture.calls.lock().unwrap().is_empty());
    assert_eq!(
        recall_situational_preferences(&memory, "How should I reply?").await,
        vec!["Be concise"]
    );
    assert_eq!(
        fixture.calls.lock().unwrap().as_slice(),
        &[(
            USER_PREF_SITUATIONAL_NAMESPACE.into(),
            SITUATIONAL_RECALL_LIMIT,
            SITUATIONAL_MIN_SIMILARITY
        )]
    );
}

#[tokio::test]
async fn related_preferences_share_budget_exclude_topic_and_tolerate_lane_error() {
    let fixture = Arc::new(VectorMemory {
        general: vec![
            ("saved".into(), "new".into()),
            ("tone".into(), "concise".into()),
        ],
        situational: vec![("format".into(), "markdown".into())],
        ..Default::default()
    });
    let memory: Arc<dyn Memory> = fixture.clone();
    assert!(recall_related_preferences(&memory, " ", "saved", 3)
        .await
        .is_empty());
    assert_eq!(
        recall_related_preferences(&memory, "new preference", "saved", 2).await,
        vec![
            ("tone".into(), "concise".into()),
            ("format".into(), "markdown".into())
        ]
    );
    {
        let calls = fixture.calls.lock().unwrap();
        assert_eq!(calls[0].1, 2);
        assert_eq!(calls[1].1, 1);
    }

    let failing = Arc::new(VectorMemory {
        fail_general: true,
        situational: vec![("format".into(), "markdown".into())],
        ..Default::default()
    });
    let failing_memory: Arc<dyn Memory> = failing;
    assert_eq!(
        recall_related_preferences(&failing_memory, "format", "other", 1).await,
        vec![("format".into(), "markdown".into())]
    );
    assert!(
        recall_related_preferences(&failing_memory, "format", "other", 0)
            .await
            .is_empty()
    );
}
