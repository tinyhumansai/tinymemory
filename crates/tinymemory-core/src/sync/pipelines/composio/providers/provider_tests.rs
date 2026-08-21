//! Deterministic contract tests for the document-oriented Composio providers.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{
    ClickUpSyncPipeline, GitHubSyncPipeline, GoogleCalendarSyncPipeline, GoogleDocsSyncPipeline,
    GoogleDriveSyncPipeline, GoogleSheetsSyncPipeline, LinearSyncPipeline, NotionSyncPipeline,
    OutlookSyncPipeline, SlackSearchBackfillPipeline, SlackSyncPipeline, TodoistSyncPipeline,
};
use crate::sync::composio::providers::sync_state::{SyncState, SyncStateStore};
use crate::sync::pipelines::composio::{
    ActionExecutor, ComposioClient, ExecuteResponse, IncrementalSource, SyncItem, SyncScope,
};
use crate::sync::pipelines::traits::{
    ComposioSyncConfig, PipelineConfig, SkillDocSink, SkillDocument, SyncContext, SyncEvent,
    SyncEventSink, SyncPipeline, SyncPipelineKind,
};

#[derive(Debug)]
struct StubExecutor {
    response: anyhow::Result<ExecuteResponse, &'static str>,
    calls: Mutex<Vec<(String, Value, Option<String>)>>,
}

impl StubExecutor {
    fn succeeds(data: Value) -> Self {
        Self {
            response: Ok(ExecuteResponse {
                data,
                successful: true,
                error: None,
                cost_usd: 0.25,
                markdown_formatted: None,
                attempts: 2,
            }),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn provider_failure(message: &'static str) -> Self {
        Self {
            response: Ok(ExecuteResponse {
                data: Value::Null,
                successful: false,
                error: Some(message.into()),
                cost_usd: 0.0,
                markdown_formatted: None,
                attempts: 1,
            }),
            calls: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl ActionExecutor for StubExecutor {
    async fn execute(
        &self,
        action: &str,
        arguments: Value,
        connection_id: Option<&str>,
    ) -> anyhow::Result<ExecuteResponse> {
        self.calls.lock().expect("calls lock").push((
            action.into(),
            arguments,
            connection_id.map(str::to_owned),
        ));
        match &self.response {
            Ok(response) => Ok(response.clone()),
            Err(message) => anyhow::bail!(*message),
        }
    }
}

#[derive(Debug)]
struct QueueExecutor {
    responses: Mutex<VecDeque<ExecuteResponse>>,
    calls: Mutex<Vec<(String, Value, Option<String>)>>,
}

impl QueueExecutor {
    fn new(data: impl IntoIterator<Item = Value>) -> Self {
        Self {
            responses: Mutex::new(
                data.into_iter()
                    .map(|data| ExecuteResponse {
                        data,
                        successful: true,
                        error: None,
                        cost_usd: 0.0,
                        markdown_formatted: None,
                        attempts: 1,
                    })
                    .collect(),
            ),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn from_responses(responses: impl IntoIterator<Item = ExecuteResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().collect()),
            calls: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl ActionExecutor for QueueExecutor {
    async fn execute(
        &self,
        action: &str,
        arguments: Value,
        connection_id: Option<&str>,
    ) -> anyhow::Result<ExecuteResponse> {
        self.calls.lock().expect("calls lock").push((
            action.into(),
            arguments,
            connection_id.map(str::to_owned),
        ));
        self.responses
            .lock()
            .expect("responses lock")
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("no queued response for {action}"))
    }
}

#[derive(Default)]
struct NoopSyncHost(Mutex<HashMap<String, Value>>);

#[async_trait]
impl SkillDocSink for NoopSyncHost {
    async fn store(&self, _: SkillDocument) -> anyhow::Result<()> {
        Ok(())
    }

    async fn delete(&self, _: &str, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl SyncEventSink for NoopSyncHost {
    async fn emit(&self, _: SyncEvent) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl SyncStateStore for NoopSyncHost {
    async fn get(&self, namespace: &str, key: &str) -> anyhow::Result<Option<Value>> {
        Ok(self
            .0
            .lock()
            .expect("state lock")
            .get(&format!("{namespace}:{key}"))
            .cloned())
    }

    async fn set(&self, namespace: &str, key: &str, value: &Value) -> anyhow::Result<()> {
        self.0
            .lock()
            .expect("state lock")
            .insert(format!("{namespace}:{key}"), value.clone());
        Ok(())
    }
}

fn sync_context(host: Arc<NoopSyncHost>) -> SyncContext {
    SyncContext {
        events: host.clone(),
        documents: host.clone(),
        state: host,
    }
}

fn client() -> ComposioClient {
    ComposioClient::new(ComposioSyncConfig::default())
}

fn item(raw: Value, dedup_key: &str) -> SyncItem {
    SyncItem {
        dedup_key: dedup_key.into(),
        sort_cursor: None,
        raw,
    }
}

#[test]
fn providers_publish_stable_action_and_paging_contracts() {
    let calendar = GoogleCalendarSyncPipeline::new(client(), "calendar").with_limits(0, 9_999);
    let docs = GoogleDocsSyncPipeline::new(client(), "docs");
    let drive = GoogleDriveSyncPipeline::new(client(), "drive").with_limits(0, 9_999);
    let sheets = GoogleSheetsSyncPipeline::new(client(), "sheets");
    let outlook = OutlookSyncPipeline::new(client(), "outlook").with_limits(0, 0);
    let todoist = TodoistSyncPipeline::new(client(), "todoist").with_limits(0, 500);

    let cases: [(&dyn IncrementalSource, &str, &str, usize, bool, bool); 6] = [
        (
            &calendar,
            "googlecalendar",
            "GOOGLECALENDAR_EVENTS_LIST",
            1,
            false,
            true,
        ),
        (
            &docs,
            "googledocs",
            "GOOGLEDOCS_SEARCH_DOCUMENTS",
            1,
            false,
            true,
        ),
        (
            &drive,
            "googledrive",
            "GOOGLEDRIVE_FIND_FILE",
            1,
            false,
            true,
        ),
        (
            &sheets,
            "googlesheets",
            "GOOGLESHEETS_SEARCH_SPREADSHEETS",
            1,
            false,
            false,
        ),
        (&outlook, "outlook", "OUTLOOK_LIST_MESSAGES", 1, true, true),
        (&todoist, "todoist", "TODOIST_GET_ALL_TASKS", 1, true, false),
    ];
    for (provider, toolkit, action, pages, stop_on_empty, server_depth) in cases {
        assert_eq!(provider.toolkit(), toolkit);
        assert_eq!(provider.action(), action);
        assert_eq!(provider.max_pages(), pages);
        assert_eq!(provider.stop_on_empty_pending(), stop_on_empty);
        assert_eq!(provider.server_side_depth(), server_depth);
    }

    let pipelines: [(&dyn SyncPipeline, &str); 6] = [
        (&calendar, "composio:googlecalendar"),
        (&docs, "composio:googledocs"),
        (&drive, "composio:googledrive"),
        (&sheets, "composio:googlesheets"),
        (&outlook, "composio:outlook"),
        (&todoist, "composio:todoist"),
    ];
    for (pipeline, id) in pipelines {
        assert_eq!(pipeline.id(), id);
        assert_eq!(pipeline.kind(), SyncPipelineKind::Composio);
    }
}

#[test]
fn calendar_uses_cursor_page_and_normalizes_event_shapes() {
    let provider = GoogleCalendarSyncPipeline::new(client(), "connection").with_limits(3, 0);
    let mut state = SyncState::new("googlecalendar", "connection");
    state.cursor = Some("2026-01-02T03:04:05Z".into());
    let args = provider.arguments(
        &SyncScope::flat(),
        &PipelineConfig::default(),
        &state,
        Some(" next "),
    );
    assert_eq!(args["calendar_id"], "primary");
    assert_eq!(args["max_results"], 1);
    assert_eq!(args["page_token"], " next ");
    assert_eq!(args["updated_min"], "2026-01-02T03:04:05Z");
    assert!(args.get("time_min").is_none());

    let page = provider.extract_page(
        &json!({"data":{"events":[{"id":"event"}],"nextPageToken":" token "}}),
        None,
    );
    assert_eq!(page.items, vec![json!({"id":"event"})]);
    assert_eq!(page.next.as_deref(), Some("token"));
    let event = json!({"iCalUID":"ical", "updated":"2026-01-03T00:00:00Z"});
    assert_eq!(
        provider.dedup_key(&event).as_deref(),
        Some("ical@2026-01-03T00:00:00Z")
    );
    assert_eq!(
        provider.sort_cursor(&event).as_deref(),
        Some("2026-01-03T00:00:00Z")
    );
    assert_eq!(provider.dedup_key(&json!({})), None);
}

#[tokio::test]
async fn calendar_document_preserves_external_metadata_and_fallbacks() {
    let provider = GoogleCalendarSyncPipeline::new(client(), "unused");
    let mut state = SyncState::new("googlecalendar", "connection");
    let executor = StubExecutor::succeeds(Value::Null);
    let document = provider
        .document(
            &SyncScope::flat(),
            "connection",
            item(json!({"iCalUID": 42, "summary":"Planning"}), "fallback"),
            &executor,
            &mut state,
        )
        .await
        .expect("calendar document");
    assert_eq!(document.document_id, "googlecalendar:42");
    assert_eq!(document.title, "Planning");
    assert_eq!(document.metadata["provider_id"], "42");
    assert_eq!(document.metadata["taint"], "external_sync");
    assert!(document.content.contains("Planning"));
}

#[test]
fn docs_validate_cursor_and_accept_wrapped_search_results() {
    let provider = GoogleDocsSyncPipeline::new(client(), "connection");
    let mut state = SyncState::new("googledocs", "connection");
    state.cursor = Some("2026-04-05T06:07:08Z".into());
    let args = provider.arguments(
        &SyncScope::flat(),
        &PipelineConfig::default(),
        &state,
        Some("ignored"),
    );
    assert_eq!(args["q"], "modifiedTime > '2026-04-05T06:07:08Z'");
    assert_eq!(args["max_results"], 25);
    assert!(args.get("page_token").is_none());
    state.cursor = Some("' or trashed = false".into());
    assert!(provider
        .arguments(&SyncScope::flat(), &PipelineConfig::default(), &state, None)
        .get("q")
        .is_none());

    let page = provider.extract_page(
        &json!({"data":{"documents":[{"documentId":"doc"}]},"next_page_token":" p2 "}),
        None,
    );
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.next.as_deref(), Some("p2"));
    let doc = json!({"data":{"documentId":"doc","modifiedTime":"2026-05-01T00:00:00Z"}});
    assert_eq!(
        provider.dedup_key(&doc).as_deref(),
        Some("doc@2026-05-01T00:00:00Z")
    );
}

#[tokio::test]
async fn docs_fetch_plaintext_and_propagate_provider_failures() {
    let provider = GoogleDocsSyncPipeline::new(client(), "unused");
    let executor = StubExecutor::succeeds(json!({"data":{"plaintext":"body text"}}));
    let mut state = SyncState::new("googledocs", "connection");
    let document = provider
        .document(
            &SyncScope::flat(),
            "connection",
            item(json!({"documentId":"doc-1","name":"Roadmap"}), "key"),
            &executor,
            &mut state,
        )
        .await
        .expect("docs document");
    assert_eq!(document.title, "Roadmap");
    assert_eq!(document.content, "body text");
    assert_eq!(state.run_requests, 2);
    assert_eq!(state.run_provider_cost_usd, 0.25);
    assert_eq!(
        executor.calls.lock().expect("calls lock")[0],
        (
            "GOOGLEDOCS_GET_DOCUMENT_PLAINTEXT".into(),
            json!({"id":"doc-1"}),
            Some("connection".into()),
        )
    );

    let failing = StubExecutor::provider_failure("permission denied");
    let error = provider
        .document(
            &SyncScope::flat(),
            "connection",
            item(json!({"id":"doc-2"}), "key"),
            &failing,
            &mut state,
        )
        .await
        .expect_err("provider failure must propagate");
    assert!(error.to_string().contains("permission denied"));

    let empty = StubExecutor::succeeds(json!({"text":"   "}));
    let fallback = provider
        .document(
            &SyncScope::flat(),
            "connection",
            item(json!({"id":"doc-3","name":"Empty body"}), "key"),
            &empty,
            &mut state,
        )
        .await
        .expect("empty plaintext falls back to search metadata");
    assert!(fallback.content.contains("Empty body"));
}

#[test]
fn drive_clamps_limits_validates_cursor_and_paginates() {
    let provider = GoogleDriveSyncPipeline::new(client(), "connection").with_limits(2, 2_000);
    let mut state = SyncState::new("googledrive", "connection");
    state.cursor = Some("2026-06-01T12:00:00+00:00".into());
    let args = provider.arguments(
        &SyncScope::flat(),
        &PipelineConfig::default(),
        &state,
        Some("page-2"),
    );
    assert_eq!(provider.max_pages(), 2);
    assert_eq!(args["page_size"], 1_000);
    assert_eq!(args["page_token"], "page-2");
    assert_eq!(args["q"], "modifiedTime > '2026-06-01T12:00:00+00:00'");
    assert!(args["fields"]
        .as_str()
        .expect("fields")
        .contains("modifiedTime"));
    let page = provider.extract_page(
        &json!({"data":{"data":{"files":[{"fileId":7}],"nextPageToken":"p3"}}}),
        None,
    );
    assert_eq!(page.items, vec![json!({"fileId":7})]);
    assert_eq!(page.next.as_deref(), Some("p3"));
    let file = json!({"fileId":7,"modified_time":"cursor"});
    assert_eq!(provider.dedup_key(&file).as_deref(), Some("7@cursor"));
}

#[tokio::test]
async fn drive_document_serializes_metadata_without_fetching_body() {
    let provider = GoogleDriveSyncPipeline::new(client(), "unused");
    let executor = StubExecutor::provider_failure("must not execute");
    let mut state = SyncState::new("googledrive", "connection");
    let document = provider
        .document(
            &SyncScope::flat(),
            "connection",
            item(
                json!({"fileId":"file-1","name":"Budget.pdf","mimeType":"application/pdf"}),
                "key",
            ),
            &executor,
            &mut state,
        )
        .await
        .expect("drive document");
    assert_eq!(document.document_id, "googledrive:file-1");
    assert_eq!(document.title, "Budget.pdf");
    assert!(document.content.contains("application/pdf"));
    assert!(executor.calls.lock().expect("calls lock").is_empty());
}

#[test]
fn sheets_use_bounded_search_and_normalize_spreadsheet_shapes() {
    let provider = GoogleSheetsSyncPipeline::new(client(), "connection");
    let args = provider.arguments(
        &SyncScope::flat(),
        &PipelineConfig::default(),
        &SyncState::new("googlesheets", "connection"),
        Some("ignored"),
    );
    assert_eq!(args, json!({"query":"","max_results":25}));
    let page = provider.extract_page(
        &json!({"data":{"files":[{"spreadsheetId":"sheet"}],"next_page_token":" next "}}),
        None,
    );
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.next.as_deref(), Some("next"));
    let sheet = json!({"spreadsheetId":"sheet","modified_time":"cursor"});
    assert_eq!(provider.dedup_key(&sheet).as_deref(), Some("sheet@cursor"));
}

#[tokio::test]
async fn sheets_fetch_info_with_canonical_argument_and_accounting() {
    let provider = GoogleSheetsSyncPipeline::new(client(), "unused");
    let executor = StubExecutor::succeeds(json!({"data":{"properties":{"locale":"en_US"}}}));
    let mut state = SyncState::new("googlesheets", "connection");
    let document = provider
        .document(
            &SyncScope::flat(),
            "connection",
            item(
                json!({"spreadsheetId":"sheet-1","properties":{"title":"Forecast"}}),
                "key",
            ),
            &executor,
            &mut state,
        )
        .await
        .expect("sheets document");
    assert_eq!(document.title, "Forecast");
    assert!(document.content.contains("en_US"));
    assert_eq!(state.run_requests, 2);
    assert_eq!(
        executor.calls.lock().expect("calls lock")[0].1,
        json!({"spreadsheet_id":"sheet-1"})
    );
}

#[test]
fn outlook_filters_by_cursor_and_extracts_graph_skiptoken() {
    let provider = OutlookSyncPipeline::new(client(), "connection").with_limits(4, 0);
    let mut state = SyncState::new("outlook", "connection");
    state.cursor = Some("2026-07-01T01:02:03Z".into());
    let args = provider.arguments(
        &SyncScope::flat(),
        &PipelineConfig::default(),
        &state,
        Some("already-bare"),
    );
    assert_eq!(args["top"], 1);
    assert_eq!(args["skip_token"], "already-bare");
    assert_eq!(args["filter"], "receivedDateTime ge 2026-07-01T01:02:03Z");
    let page = provider.extract_page(
        &json!({
            "value":[{"messageId":"mail"}],
            "@odata.nextLink":"https://graph.example/messages?foo=1&$skiptoken=A%2BB&top=25"
        }),
        None,
    );
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.next.as_deref(), Some("A%2BB"));
    let mail =
        json!({"messageId":"mail","received_date_time":"cursor","lastModifiedDateTime":"wrong"});
    assert_eq!(provider.dedup_key(&mail).as_deref(), Some("mail@cursor"));
    assert_eq!(
        provider.sort_cursor(&json!({"lastModifiedDateTime":"wrong"})),
        None
    );
}

#[tokio::test]
async fn outlook_document_uses_subject_and_raw_message_body() {
    let provider = OutlookSyncPipeline::new(client(), "unused");
    let executor = StubExecutor::provider_failure("must not execute");
    let mut state = SyncState::new("outlook", "connection");
    let document = provider
        .document(
            &SyncScope::flat(),
            "connection",
            item(
                json!({"id":"mail-1","subject":"Hello","body":{"content":"World"}}),
                "key",
            ),
            &executor,
            &mut state,
        )
        .await
        .expect("outlook document");
    assert_eq!(document.title, "Hello");
    assert!(document.content.contains("World"));
    assert!(executor.calls.lock().expect("calls lock").is_empty());
}

#[test]
fn todoist_handles_bare_and_wrapped_arrays_and_fingerprints_edits() {
    let provider = TodoistSyncPipeline::new(client(), "connection");
    assert_eq!(
        provider.arguments(
            &SyncScope::flat(),
            &PipelineConfig::default(),
            &SyncState::new("todoist", "connection"),
            Some("ignored"),
        ),
        json!({})
    );
    assert_eq!(
        provider
            .extract_page(&json!([{"id":"1"}]), None)
            .items
            .len(),
        1
    );
    assert_eq!(
        provider
            .extract_page(&json!({"data":{"tasks":[{"id":"2"}]}}), None)
            .items
            .len(),
        1
    );
    let first = json!({"id":"task","content":"write","nested":{"b":2,"a":1}});
    let reordered = json!({"nested":{"a":1,"b":2},"content":"write","id":"task"});
    let edited = json!({"id":"task","content":"ship","nested":{"a":1,"b":2}});
    assert_eq!(provider.dedup_key(&first), provider.dedup_key(&reordered));
    assert_ne!(provider.dedup_key(&first), provider.dedup_key(&edited));
    assert_eq!(provider.sort_cursor(&first), None);
}

#[tokio::test]
async fn todoist_document_combines_task_text_and_description() {
    let provider = TodoistSyncPipeline::new(client(), "unused");
    let executor = StubExecutor::provider_failure("must not execute");
    let mut state = SyncState::new("todoist", "connection");
    let document = provider
        .document(
            &SyncScope::flat(),
            "connection",
            item(
                json!({"task_id":9,"content":"Write tests","description":"Cover failures"}),
                "key",
            ),
            &executor,
            &mut state,
        )
        .await
        .expect("todoist document");
    assert_eq!(document.document_id, "todoist:9");
    assert_eq!(document.content, "Write tests\n\nCover failures");
    assert!(executor.calls.lock().expect("calls lock").is_empty());

    let fallback = provider
        .document(
            &SyncScope::flat(),
            "connection",
            item(json!({"id":"task-raw","priority":4}), "key"),
            &executor,
            &mut state,
        )
        .await
        .expect("task without content uses raw payload");
    assert_eq!(fallback.title, "Todoist task task-raw");
    assert!(fallback.content.contains("priority"));
}

#[tokio::test]
async fn scoped_work_providers_normalize_directory_pages_and_documents() {
    let clickup = ClickUpSyncPipeline::new(client(), "connection");
    assert_eq!(clickup.id(), "composio:clickup");
    assert_eq!(clickup.kind(), SyncPipelineKind::Composio);
    let click_executor = QueueExecutor::new([
        json!({"user":{"id":42}}),
        json!({"teams":[{"id":"team-1"},{"name":"missing id"}]}),
    ]);
    let mut click_state = SyncState::new("clickup", "connection");
    let click_scopes = clickup
        .scopes(&click_executor, "connection", &mut click_state)
        .await
        .expect("clickup scopes");
    assert_eq!(click_scopes.len(), 1);
    assert_eq!(click_scopes[0].label, "workspace:team-1");
    assert_eq!(click_scopes[0].metadata["user_id"], "42");
    assert_eq!(click_state.run_requests, 2);
    let click_args = clickup.arguments(
        &click_scopes[0],
        &PipelineConfig::default(),
        &click_state,
        Some("3"),
    );
    assert_eq!(click_args["team_id"], "team-1");
    assert_eq!(click_args["assignees"], json!(["42"]));
    assert_eq!(click_args["page"], 3);
    assert_eq!(clickup.max_pages(), 20);
    let click_tasks = vec![json!({"id":"task"}); 50];
    let click_page = clickup.extract_page(&json!({"tasks":click_tasks}), Some("3"));
    assert_eq!(click_page.next.as_deref(), Some("4"));
    let click_raw = json!({"task_id":"task-1","name":"Ship","dateUpdated":123});
    assert_eq!(clickup.dedup_key(&click_raw).as_deref(), Some("task-1@123"));
    let click_document = clickup
        .document(
            &click_scopes[0],
            "connection",
            item(click_raw, "key"),
            &click_executor,
            &mut click_state,
        )
        .await
        .expect("clickup document");
    assert_eq!(click_document.title, "Ship");
    assert_eq!(click_document.metadata["workspace_id"], "team-1");

    let github = GitHubSyncPipeline::new(client(), "connection");
    assert_eq!(github.id(), "composio:github");
    let github_executor = QueueExecutor::new([json!({"data":{"login":"alice"}})]);
    let mut github_state = SyncState::new("github", "connection");
    let github_scopes = github
        .scopes(&github_executor, "connection", &mut github_state)
        .await
        .expect("github scopes");
    assert_eq!(github_scopes[0].label, "involves:alice");
    github_state.cursor = Some("2026-01-02T00:00:00Z".into());
    let github_args = github.arguments(
        &github_scopes[0],
        &PipelineConfig::default(),
        &github_state,
        Some("2"),
    );
    assert_eq!(
        github_args["q"],
        "involves:alice updated:>2026-01-02T00:00:00Z"
    );
    assert_eq!(github_args["page"], 2);
    assert!(github.server_side_depth());
    let github_items = vec![json!({"id":"issue"}); 50];
    assert_eq!(
        github
            .extract_page(&json!({"data":{"items":github_items}}), None)
            .next
            .as_deref(),
        Some("2")
    );
    let github_raw = json!({
        "html_url":"https://github.com/acme/widget/issues/7",
        "title":"Bug",
        "updatedAt":"cursor"
    });
    assert_eq!(
        github.dedup_key(&github_raw).as_deref(),
        Some("acme/widget#7@cursor")
    );
    let github_document = github
        .document(
            &github_scopes[0],
            "connection",
            item(github_raw, "key"),
            &github_executor,
            &mut github_state,
        )
        .await
        .expect("github document");
    assert_eq!(github_document.document_id, "github:acme/widget#7");

    let linear = LinearSyncPipeline::new(client(), "connection");
    assert_eq!(linear.id(), "composio:linear");
    let linear_executor = QueueExecutor::new([json!({"data":{"nodes":[{"id":"user-1"}]}})]);
    let mut linear_state = SyncState::new("linear", "connection");
    let linear_scopes = linear
        .scopes(&linear_executor, "connection", &mut linear_state)
        .await
        .expect("linear scopes");
    assert_eq!(linear_scopes[0].id, "user-1");
    let linear_args = linear.arguments(
        &linear_scopes[0],
        &PipelineConfig::default(),
        &linear_state,
        Some("cursor-2"),
    );
    assert_eq!(linear_args["assigneeId"], "user-1");
    assert_eq!(linear_args["after"], "cursor-2");
    let linear_page = linear.extract_page(
        &json!({"data":{"issues":{"nodes":[{"identifier":"ENG-7"}],"pageInfo":{"hasNextPage":true,"endCursor":"next"}}}}),
        None,
    );
    assert_eq!(linear_page.items.len(), 1);
    assert_eq!(linear_page.next.as_deref(), Some("next"));
    let linear_raw = json!({"identifier":"ENG-7","title":"Fix it","updated_at":"cursor"});
    assert_eq!(
        linear.dedup_key(&linear_raw).as_deref(),
        Some("ENG-7@cursor")
    );
    let linear_document = linear
        .document(
            &linear_scopes[0],
            "connection",
            item(linear_raw, "key"),
            &linear_executor,
            &mut linear_state,
        )
        .await
        .expect("linear document");
    assert_eq!(linear_document.title, "Fix it");
}

#[tokio::test]
async fn notion_and_slack_cover_secondary_fetch_and_per_scope_cursor_contracts() {
    let notion = NotionSyncPipeline::new(client(), "connection");
    assert_eq!(notion.id(), "composio:notion");
    assert_eq!(notion.max_pages(), 20);
    let notion_args = notion.arguments(
        &SyncScope::flat(),
        &PipelineConfig::default(),
        &SyncState::new("notion", "connection"),
        Some("page-2"),
    );
    assert_eq!(notion_args["page_size"], 25);
    assert_eq!(notion_args["start_cursor"], "page-2");
    let notion_page = notion.extract_page(
        &json!({"data":{"results":[{"pageId":"page-1"}],"next_cursor":"next"}}),
        None,
    );
    assert_eq!(notion_page.items.len(), 1);
    assert_eq!(notion_page.next.as_deref(), Some("next"));
    let notion_raw = json!({
        "pageId":"page-1",
        "lastEditedTime":"cursor",
        "properties":{"Name":{"type":"title","title":[{"plain_text":"Road"},{"plain_text":"map"}]}}
    });
    assert_eq!(
        notion.dedup_key(&notion_raw).as_deref(),
        Some("page-1@cursor")
    );
    let notion_executor = QueueExecutor::new([json!({"response_data":{"markdown":"# Body"}})]);
    let mut notion_state = SyncState::new("notion", "connection");
    let notion_document = notion
        .document(
            &SyncScope::flat(),
            "connection",
            item(notion_raw, "key"),
            &notion_executor,
            &mut notion_state,
        )
        .await
        .expect("notion document");
    assert_eq!(notion_document.title, "Roadmap");
    assert_eq!(notion_document.content, "# Body");
    assert_eq!(notion_state.run_requests, 1);

    let slack = SlackSyncPipeline::new(client(), "connection");
    assert_eq!(slack.id(), "composio:slack");
    assert!(slack.per_scope_cursors());
    assert!(slack.server_side_depth());
    assert!(slack.tolerate_scope_errors());
    assert!(slack.retain_dedup_keys());
    let slack_executor = QueueExecutor::new([
        json!({"members":[
            {"id":"U1","profile":{"display_name":"Alice"}},
            {"id":"U2","real_name":"Bob"},
            {"name":"missing id"}
        ]}),
        json!({"channels":[
            {"id":"C1","name":"general","is_private":false},
            {"id":"C2","name":"secret","is_private":true}
        ]}),
    ]);
    let mut slack_state = SyncState::new("slack", "connection");
    let slack_scopes = slack
        .scopes(&slack_executor, "connection", &mut slack_state)
        .await
        .expect("slack scopes");
    assert_eq!(slack_scopes.len(), 2);
    assert_eq!(slack_scopes[0].label, "#general");
    assert_eq!(slack_scopes[1].label, "private:secret");
    assert_eq!(slack_scopes[0].metadata["users"]["U1"], "Alice");
    slack.advance_scope_cursor(&mut slack_state, &slack_scopes[0], "1700000000.000001");
    let slack_args = slack.arguments(
        &slack_scopes[0],
        &PipelineConfig::default(),
        &slack_state,
        Some("page-2"),
    );
    assert_eq!(slack_args["channel"], "C1");
    assert_eq!(slack_args["oldest"], "1700000000.000001");
    assert_eq!(slack_args["cursor"], "page-2");
    let slack_page = slack.extract_page(
        &json!({"messages":[{"ts":"1700000001.000002","text":"Hi"}],"response_metadata":{"next_cursor":" next "}}),
        None,
    );
    assert_eq!(slack_page.items.len(), 1);
    assert_eq!(slack_page.next.as_deref(), Some("next"));
    let slack_raw = json!({"ts":"1700000001.000002","text":"Hi <@U2>","user":"U1"});
    assert_eq!(
        slack.dedup_key(&slack_raw).as_deref(),
        Some("1700000001.000002")
    );
    assert_eq!(slack.dedup_key(&json!({"ts":"bad","text":"Hi"})), None);
    let slack_document = slack
        .document(
            &slack_scopes[0],
            "connection",
            item(slack_raw, "key"),
            &slack_executor,
            &mut slack_state,
        )
        .await
        .expect("slack document");
    assert_eq!(slack_document.title, "Slack #general from Alice");
    assert_eq!(slack_document.content, "[1700000001.000002] Alice: Hi @Bob");
    assert_eq!(slack_document.metadata["channel_id"], "C1");
}

#[tokio::test]
async fn scoped_provider_failures_and_content_fallbacks_are_explicit() {
    let successful = |data| ExecuteResponse {
        data,
        successful: true,
        error: None,
        cost_usd: 0.0,
        markdown_formatted: None,
        attempts: 1,
    };
    let rejected = ExecuteResponse {
        data: Value::Null,
        successful: false,
        error: Some("directory denied".into()),
        cost_usd: 0.0,
        markdown_formatted: None,
        attempts: 2,
    };

    let clickup = ClickUpSyncPipeline::new(client(), "connection");
    let missing_click_user = QueueExecutor::new([json!({"user":{}})]);
    let mut click_state = SyncState::new("clickup", "connection");
    let error = clickup
        .scopes(&missing_click_user, "connection", &mut click_state)
        .await
        .expect_err("clickup user id is required");
    assert!(error.to_string().contains("returned no user id"));

    let github = GitHubSyncPipeline::new(client(), "connection");
    let missing_login = QueueExecutor::new([json!({"data":{}})]);
    let mut github_state = SyncState::new("github", "connection");
    let error = github
        .scopes(&missing_login, "connection", &mut github_state)
        .await
        .expect_err("github login is required");
    assert!(error.to_string().contains("returned no login"));
    assert_eq!(
        github.dedup_key(&json!({"html_url":"https://github.com/too-short"})),
        None
    );

    let linear = LinearSyncPipeline::new(client(), "connection");
    let missing_viewer = QueueExecutor::new([json!({"nodes":[]})]);
    let mut linear_state = SyncState::new("linear", "connection");
    let error = linear
        .scopes(&missing_viewer, "connection", &mut linear_state)
        .await
        .expect_err("linear viewer id is required");
    assert!(error.to_string().contains("returned no viewer id"));
    assert_eq!(
        linear
            .extract_page(
                &json!({"nodes":[],"pageInfo":{"hasNextPage":false,"endCursor":"ignored"}}),
                None,
            )
            .next,
        None
    );

    let slack_executor = QueueExecutor::from_responses([
        successful(json!({
            "members":[{"id":"U1","name":"alice"}],
            "response_metadata":{"next_cursor":"users-2"}
        })),
        rejected,
        successful(json!({
            "channels":[{"id":"C1","name":"one"}],
            "response_metadata":{"next_cursor":"channels-2"}
        })),
        successful(json!({"channels":[{"id":"C2"}]})),
    ]);
    let slack = SlackSyncPipeline::new(client(), "connection");
    let mut slack_state = SyncState::new("slack", "connection");
    let scopes = slack
        .scopes(&slack_executor, "connection", &mut slack_state)
        .await
        .expect("slack directory tolerates rejected second user page");
    assert_eq!(scopes.len(), 2);
    assert_eq!(scopes[1].label, "#C2");
    assert_eq!(slack_state.run_requests, 5);
    {
        let calls = slack_executor.calls.lock().expect("calls lock");
        assert_eq!(calls[1].0, "SLACK_LIST_ALL_USERS");
        assert_eq!(calls[1].1["cursor"], "users-2");
        assert_eq!(calls[3].1["cursor"], "channels-2");
    }

    let notion = NotionSyncPipeline::new(client(), "connection");
    let empty_markdown = QueueExecutor::new([json!({"markdown":"   "})]);
    let mut notion_state = SyncState::new("notion", "connection");
    let fallback = notion
        .document(
            &SyncScope::flat(),
            "connection",
            item(json!({"id":"page-2","name":"Fallback title"}), "key"),
            &empty_markdown,
            &mut notion_state,
        )
        .await
        .expect("blank markdown falls back to page JSON");
    assert_eq!(fallback.title, "Fallback title");
    assert!(fallback.content.contains("page-2"));
}

#[tokio::test]
async fn pipeline_initialization_is_noop_and_backfill_honors_exhausted_budget() {
    let host = Arc::new(NoopSyncHost::default());
    let context = sync_context(host.clone());
    let config = PipelineConfig::default();
    let calendar = GoogleCalendarSyncPipeline::new(client(), "connection");
    let docs = GoogleDocsSyncPipeline::new(client(), "connection");
    let drive = GoogleDriveSyncPipeline::new(client(), "connection");
    let sheets = GoogleSheetsSyncPipeline::new(client(), "connection");
    let outlook = OutlookSyncPipeline::new(client(), "connection");
    let todoist = TodoistSyncPipeline::new(client(), "connection");
    let clickup = ClickUpSyncPipeline::new(client(), "connection");
    let github = GitHubSyncPipeline::new(client(), "connection");
    let linear = LinearSyncPipeline::new(client(), "connection");
    let notion = NotionSyncPipeline::new(client(), "connection");
    let slack = SlackSyncPipeline::new(client(), "connection");
    let backfill = SlackSearchBackfillPipeline::new(client(), "connection", 0);
    let pipelines: [&dyn SyncPipeline; 12] = [
        &calendar, &docs, &drive, &sheets, &outlook, &todoist, &clickup, &github, &linear, &notion,
        &slack, &backfill,
    ];
    for pipeline in pipelines {
        pipeline
            .init(&config, &context)
            .await
            .expect("provider initialization");
    }
    assert_eq!(backfill.id(), "composio:slack:search-backfill");
    assert_eq!(backfill.kind(), SyncPipelineKind::Composio);

    let mut state = SyncState::new("slack", "connection");
    state.daily_budget.limit = 0;
    state
        .save(host.as_ref())
        .await
        .expect("seed exhausted state");
    let outcome = backfill
        .tick(&config, &context)
        .await
        .expect("exhausted backfill exits without provider I/O");
    assert_eq!(outcome.records_ingested, 0);
    assert_eq!(
        outcome.note.as_deref(),
        Some("slack search-backfill skipped: daily budget exhausted")
    );
}
