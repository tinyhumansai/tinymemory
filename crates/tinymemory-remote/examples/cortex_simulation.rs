//! Full CortexDB ingestion and retrieval simulation.

use tinymemory_api::chunks::DataSource;
use tinymemory_api::evidence::EvidenceRef;
use tinymemory_api::learning::{CueFamily, FacetClass, LearningCandidate};
use tinymemory_api::provider::{
    AnswerRequest, IngestItem, MemoryProvider, MemoryRecall, RawMemoryEvent,
};
use tinymemory_api::recall::OwnedRecallOpts;
use tinymemory_api::types::MemoryTaint;
use tinymemory_remote::{cortex_provider, CortexMemory};

fn usage() -> anyhow::Error {
    anyhow::anyhow!("usage: cortex_simulation <endpoint> <api-key>")
}

fn item(namespace: &str, source_id: &str, content: &str, author: Option<&str>) -> IngestItem {
    IngestItem {
        namespace: Some(namespace.to_string()),
        source: DataSource::Conversation,
        source_id: source_id.to_string(),
        owner: "simulation-user".to_string(),
        source_ref: None,
        content: content.to_string(),
        mime: Some("text/plain".to_string()),
        timestamp: None,
        tags: vec!["tinymemory-simulation".to_string()],
        author: author.map(str::to_owned),
        channel_label: Some("CortexDB simulation".to_string()),
        platform: Some("tinymemory".to_string()),
        to: Vec::new(),
        cc: Vec::new(),
        subject: None,
        list_unsubscribe: None,
        taint: MemoryTaint::ExternalSync,
        path_scope: None,
    }
}

fn valid_simulation_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let endpoint = args.next().ok_or_else(usage)?;
    let key = args.next().ok_or_else(usage)?;
    anyhow::ensure!(args.next().is_none(), "{}", usage());

    let suffix = std::env::var("TINYMEMORY_CORTEX_SIMULATION_ID").unwrap_or_else(|_| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos().to_string())
            .unwrap_or_else(|_| "clock-error".to_string())
    });
    anyhow::ensure!(
        valid_simulation_id(&suffix),
        "TINYMEMORY_CORTEX_SIMULATION_ID must contain only ASCII letters, digits, '_' or '-'"
    );

    let provider = cortex_provider(CortexMemory::api(&endpoint, &key)?);
    tinymemory_api::provider::audit_provider(&provider)?;
    anyhow::ensure!(
        provider.health().await.is_usable(),
        "CortexDB is not usable"
    );

    let document_namespace = format!("simulation/{suffix}/documents");
    let conversation_namespace = format!("simulation/{suffix}/conversation");
    let event_namespace = format!("simulation/{suffix}/events");

    let learning_key = format!("simulation_package_manager_{suffix}");
    provider
        .as_document_ingest()
        .ok_or_else(|| anyhow::anyhow!("document ingestion is not advertised"))?
        .ingest_document(item(
            &document_namespace,
            "architecture",
            "CortexDB stores the launch architecture document.",
            None,
        ))
        .await?;

    provider
        .as_conversation_ingest()
        .ok_or_else(|| anyhow::anyhow!("conversation ingestion is not advertised"))?
        .ingest_conversation(vec![
            item(
                &conversation_namespace,
                "launch-thread",
                "When does Project Aurora launch?",
                Some("user"),
            ),
            item(
                &conversation_namespace,
                "launch-thread",
                "Project Aurora launches on Thursday.",
                Some("assistant"),
            ),
        ])
        .await?;

    provider
        .as_learning_ingest()
        .ok_or_else(|| anyhow::anyhow!("learning ingestion is not advertised"))?
        .ingest_learning(LearningCandidate {
            class: FacetClass::Tooling,
            key: learning_key.clone(),
            value: "pnpm".to_string(),
            cue_family: CueFamily::Explicit,
            evidence: EvidenceRef::ToolCall {
                tool_name: "shell".to_string(),
                episodic_id: 7,
            },
            initial_confidence: 0.95,
            observed_at: 1_700_000_000.0,
        })
        .await?;

    let event_ingest = provider
        .as_event_ingest()
        .ok_or_else(|| anyhow::anyhow!("event ingestion is not advertised"))?;
    event_ingest
        .ingest_event(RawMemoryEvent {
            id: format!("deploy-{suffix}"),
            namespace: event_namespace.clone(),
            event_type: "deployment".to_string(),
            content: "Project Aurora staging deployment completed.".to_string(),
            occurred_at: None,
            session_id: Some("launch-thread".to_string()),
            metadata: serde_json::json!({"environment": "staging", "status": "success"}),
            taint: MemoryTaint::Internal,
        })
        .await?;
    event_ingest
        .ingest_event(RawMemoryEvent {
            id: format!("tool-{suffix}"),
            namespace: event_namespace.clone(),
            event_type: "tool_call".to_string(),
            content: "The shell tool ran cargo test and every test passed.".to_string(),
            occurred_at: None,
            session_id: Some("launch-thread".to_string()),
            metadata: serde_json::json!({
                "tool_name": "shell",
                "arguments": {"command": "cargo test"},
                "outcome": "success"
            }),
            taint: MemoryTaint::Internal,
        })
        .await?;

    // CortexDialect::scope_of prefixes every slash-delimited TinyMemory
    // namespace segment with `tm:`; this is `simulation/{suffix}/events` in
    // CortexDB's scope grammar.
    let event_scope = format!("tm:simulation/tm:{suffix}/tm:events");
    let raw_events: serde_json::Value = reqwest::Client::new()
        .get(format!("{endpoint}/v1/events"))
        .query(&[("scope", event_scope.as_str()), ("limit", "20")])
        .bearer_auth(&key)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let tool_event = raw_events["items"]
        .as_array()
        .and_then(|items| items.iter().find(|event| event["modality"] == "tool_call"))
        .ok_or_else(|| anyhow::anyhow!("raw event log omitted the tool_call modality"))?;
    let tool_envelope: serde_json::Value = serde_json::from_str(
        tool_event["content"]["text"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("tool-call event omitted its envelope"))?,
    )?;
    anyhow::ensure!(
        tool_envelope["x"]["metadata"]["tool_name"] == "shell",
        "tool-call metadata did not survive ingestion"
    );

    for (namespace, query) in [
        (
            document_namespace.clone(),
            "launch architecture".to_string(),
        ),
        (
            conversation_namespace.clone(),
            "Project Aurora Thursday".to_string(),
        ),
        ("learning:tooling".to_string(), learning_key),
        (event_namespace.clone(), "cargo test passed".to_string()),
    ] {
        let hits = provider
            .recall(
                &query,
                10,
                &OwnedRecallOpts {
                    namespace: Some(namespace.clone()),
                    ..OwnedRecallOpts::default()
                },
                None,
            )
            .await?;
        anyhow::ensure!(!hits.is_empty(), "{namespace} was not recallable");
    }

    let answer = provider
        .as_answer()
        .ok_or_else(|| anyhow::anyhow!("answers are not advertised"))?
        .answer(AnswerRequest {
            query: "When does Project Aurora launch?".to_string(),
            limit: 10,
            recall: OwnedRecallOpts {
                namespace: Some(conversation_namespace),
                ..OwnedRecallOpts::default()
            },
            scope: None,
            instructions: Some("Answer using only recalled evidence.".to_string()),
        })
        .await?;
    anyhow::ensure!(
        !answer.answer.trim().is_empty(),
        "CortexDB returned an empty answer"
    );
    anyhow::ensure!(
        !answer.citations.is_empty(),
        "CortexDB returned an answer without citations"
    );

    println!(
        "cortex: document, conversation, learning, event, tool_call, recall, and answer passed"
    );
    Ok(())
}
