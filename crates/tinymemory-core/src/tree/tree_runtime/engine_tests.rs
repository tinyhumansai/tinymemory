//! Tests for the time-tree model adapter and finite runtime operations.

use super::*;
use crate::tree::tree_runtime::store;
use chrono::TimeZone;
use std::sync::Mutex;
use tempfile::TempDir;
use tinyagents::harness::model::ModelResponse;
use tinymemory_api::host::test_support::TestHostConfig;

struct RecordingModel {
    requests: Mutex<Vec<ModelRequest>>,
    reply: Result<String, String>,
}

impl RecordingModel {
    fn success(reply: &str) -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            reply: Ok(reply.into()),
        }
    }
}

#[async_trait]
impl ChatModel<()> for RecordingModel {
    async fn invoke(
        &self,
        _state: &(),
        request: ModelRequest,
    ) -> tinyagents::Result<ModelResponse> {
        self.requests.lock().unwrap().push(request);
        match &self.reply {
            Ok(reply) => Ok(ModelResponse::assistant(reply.clone())),
            Err(message) => Err(tinyagents::TinyAgentsError::Memory(message.clone())),
        }
    }
}

fn config() -> (TempDir, TestHostConfig) {
    crate::test_seams::init();
    let tmp = TempDir::new().unwrap();
    let mut config = TestHostConfig::default();
    config.workspace_dir = tmp.path().join("workspace");
    (tmp, config)
}

#[tokio::test]
async fn chat_summariser_builds_system_and_user_requests() {
    let model = RecordingModel::success("summary");
    let summariser = ChatSummariser(&model);
    assert_eq!(
        summariser
            .summarise(Some("system prompt"), "user content")
            .await
            .unwrap(),
        "summary"
    );
    assert_eq!(
        summariser.summarise(None, "second").await.unwrap(),
        "summary"
    );
    let requests = model.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].messages.len(), 2);
    assert_eq!(requests[1].messages.len(), 1);
    assert_eq!(requests[0].temperature, Some(SUMMARIZATION_TEMP));
}

#[tokio::test]
async fn chat_summariser_adds_provider_failure_context() {
    let model = RecordingModel {
        requests: Mutex::new(Vec::new()),
        reply: Err("offline".into()),
    };
    let error = ChatSummariser(&model)
        .summarise(None, "content")
        .await
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("time-tree summarization provider call failed"));
}

#[tokio::test]
async fn summarization_empty_and_buffered_paths_emit_events() {
    let (_tmp, config) = config();
    let model = RecordingModel::success("summarized memory");
    let timestamp = Utc.with_ymd_and_hms(2024, 3, 15, 14, 30, 0).unwrap();
    assert!(run_summarization(&config, &model, "team", timestamp)
        .await
        .unwrap()
        .is_none());

    store::buffer_write(&config, "team", "important event", &timestamp, None).unwrap();
    let sink = crate::events::RecordingSink::install();
    let node = run_summarization(&config, &model, "team", timestamp)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(node.node_id, "2024/03/15/14");
    assert!(node.summary.contains("summarized memory"));
    assert!(sink.drain().iter().any(|event| matches!(
        event,
        crate::events::MemoryEvent::TreeSummarizerHourCompleted { namespace, .. }
            if namespace == "team"
    )));

    let status = rebuild_tree(&config, &model, "team").await.unwrap();
    assert!(status.total_nodes >= 1);
}
