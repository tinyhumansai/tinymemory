//! CortexDB provider construction and capability implementations.

use std::sync::Arc;

use async_trait::async_trait;
use reqwest::Method;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tinymemory_api::capabilities::{Capabilities, Capability};
use tinymemory_api::error::MemoryError;
use tinymemory_api::health::MemoryHealth;
use tinymemory_api::learning::LearningCandidate;
use tinymemory_api::mandatory::{engine_error, MemoryTraitProvider};
use tinymemory_api::provider::types::{
    ExportPage, ExportRecord, ImportOutcome, IngestItem, IngestOutcome, SourceScope,
};
use tinymemory_api::provider::{
    AnswerCitation, AnswerRequest, AnswerResponse, AnswerStep, MemoryAnswer,
    MemoryConversationIngest, MemoryCore, MemoryDocumentIngest, MemoryEventIngest,
    MemoryLearningIngest, MemoryPortability, MemoryProvider, MemoryRecall, RawMemoryEvent,
};
use tinymemory_api::recall::OwnedRecallOpts;
use tinymemory_api::types::{
    MemoryCategory, MemoryEntry, MemoryTaint, NamespaceSummary, GLOBAL_NAMESPACE,
};

use crate::common::{encode, Attempts, HttpClient};
use crate::cortex::{CortexDialect, CortexMemory, CORTEX_DRIVER_ID};

use super::types::ExperienceInput;

/// CortexDB exposed as mandatory storage plus native ingestion and answers.
pub struct CortexProvider {
    mandatory: MemoryTraitProvider,
    client: HttpClient,
}

impl std::fmt::Debug for CortexProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CortexProvider")
            .field("client", &self.client)
            .finish_non_exhaustive()
    }
}

impl CortexProvider {
    /// Wrap a native CortexDB client.
    #[must_use]
    pub(crate) fn new(memory: CortexMemory, client: HttpClient) -> Self {
        Self {
            mandatory: MemoryTraitProvider::new(Arc::new(memory), CORTEX_DRIVER_ID),
            client,
        }
    }

    async fn experience(&self, input: ExperienceInput<'_>) -> Result<(String, bool), MemoryError> {
        let request = Self::experience_request(input)?;
        let answer: Value = self
            .client
            .json(
                Method::POST,
                "v1/experience?wait=indexed",
                Some(&request),
                Attempts::Once,
            )
            .await
            .map_err(engine_error)?;
        receipt(&answer)
    }

    fn experience_request(input: ExperienceInput<'_>) -> Result<Value, MemoryError> {
        let ExperienceInput {
            namespace,
            modality,
            role,
            key,
            body,
            session_id,
            taint,
            payload,
            idempotency_seed,
            observed_at,
            labels,
        } = input;
        if namespace.trim().is_empty()
            || modality.trim().is_empty()
            || key.trim().is_empty()
            || body.trim().is_empty()
        {
            return Err(MemoryError::Invalid(
                "namespace, modality, key, and content must not be empty".to_string(),
            ));
        }
        let scope = CortexDialect::scope_of(namespace).map_err(engine_error)?;
        let envelope = json!({
            "k": key,
            "c": body,
            "cat": category_for(modality).to_string(),
            "s": session_id,
            "t": taint.as_db_str(),
            "x": payload,
        });
        let text = serde_json::to_string(&envelope)?;
        let content = match role {
            Some(role) => json!({
                "kind": "message",
                "role": cortex_role(role),
                "text": text,
            }),
            None => json!({ "kind": "text", "text": text }),
        };
        let mut context = serde_json::Map::new();
        if let Some(observed_at) = observed_at {
            context.insert("observed_at".to_string(), json!(observed_at));
        }
        let labels = bounded_labels(labels);
        if !labels.is_empty() {
            context.insert("labels".to_string(), json!(labels));
        }
        Ok(json!({
            "scope": scope,
            "modality": modality,
            "content": content,
            "context": context,
            "directives": {
                "extract": ["facts", "entities", "beliefs", "episodes", "understanding"],
                "embed": "eager",
            },
            "idempotency_key": idempotency_key(idempotency_seed),
        }))
    }
}

pub(super) fn receipt(answer: &Value) -> Result<(String, bool), MemoryError> {
    let id = answer
        .get("event_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| MemoryError::Backend("CortexDB omitted event_id".to_string()))?;
    let replayed = answer
        .get("replayed_from_idempotency")
        .and_then(Value::as_bool)
        .ok_or_else(|| {
            MemoryError::Backend("CortexDB omitted boolean replayed_from_idempotency".to_string())
        })?;
    Ok((id, replayed))
}

fn idempotency_key(seed: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(seed.as_bytes());
    encode(digest.finalize())
}

pub(super) fn event_identity(namespace: &str, id: &str) -> String {
    format!("{}:{namespace}{}:{id}", namespace.len(), id.len())
}

pub(super) fn cortex_role(role: &str) -> &'static str {
    // Cortex's role is a four-value message class, while IngestItem::author is
    // deliberately open and often contains a person's name. Known agent roles
    // retain their class; every other speaker is a human/user. The exact author
    // is still preserved in the private payload (`x`) beside the indexed text.
    match role.trim().to_ascii_lowercase().as_str() {
        "assistant" => "assistant",
        "tool" => "tool",
        "system" => "system",
        _ => "user",
    }
}

fn bounded_labels(labels: Vec<String>) -> Vec<String> {
    labels
        .into_iter()
        .filter_map(|label| {
            let label = label.trim();
            (!label.is_empty()).then(|| label.chars().take(64).collect())
        })
        .take(64)
        .collect()
}

fn category_for(modality: &str) -> MemoryCategory {
    match modality {
        "conversation" => MemoryCategory::Conversation,
        "observation" => MemoryCategory::Core,
        other => MemoryCategory::Custom(other.to_string()),
    }
}

pub(super) fn layer_limits(limit: usize) -> Value {
    const LAYERS: [&str; 5] = ["events", "facts", "beliefs", "episodes", "understanding"];
    let base = limit / LAYERS.len();
    let remainder = limit % LAYERS.len();
    let mut limits = serde_json::Map::new();
    for (index, layer) in LAYERS.into_iter().enumerate() {
        limits.insert(
            layer.to_string(),
            json!(base + usize::from(index < remainder)),
        );
    }
    Value::Object(limits)
}

pub(super) fn observed_at(timestamp: f64) -> Result<String, MemoryError> {
    if !timestamp.is_finite() {
        return Err(MemoryError::Invalid(
            "learning observed_at must be a finite Unix timestamp".to_string(),
        ));
    }
    let seconds = timestamp.floor();
    if seconds < i64::MIN as f64 || seconds > i64::MAX as f64 {
        return Err(MemoryError::Invalid(
            "learning observed_at is outside the supported timestamp range".to_string(),
        ));
    }
    let nanos = ((timestamp - seconds) * 1_000_000_000.0).round();
    let nanos = nanos.clamp(0.0, 999_999_999.0) as u32;
    chrono::DateTime::from_timestamp(seconds as i64, nanos)
        .map(|value| value.to_rfc3339())
        .ok_or_else(|| {
            MemoryError::Invalid(
                "learning observed_at is outside the supported timestamp range".to_string(),
            )
        })
}

#[async_trait]
impl MemoryCore for CortexProvider {
    async fn store(
        &self,
        namespace: &str,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        taint: MemoryTaint,
    ) -> Result<(), MemoryError> {
        self.mandatory
            .store(namespace, key, content, category, session_id, taint)
            .await
    }

    async fn get(&self, namespace: &str, key: &str) -> Result<Option<MemoryEntry>, MemoryError> {
        self.mandatory.get(namespace, key).await
    }

    async fn forget(&self, namespace: &str, key: &str) -> Result<bool, MemoryError> {
        self.mandatory.forget(namespace, key).await
    }

    async fn list(
        &self,
        namespace: Option<&str>,
        category: Option<&MemoryCategory>,
        session_id: Option<&str>,
    ) -> Result<Vec<MemoryEntry>, MemoryError> {
        self.mandatory.list(namespace, category, session_id).await
    }

    async fn namespaces(&self) -> Result<Vec<NamespaceSummary>, MemoryError> {
        self.mandatory.namespaces().await
    }
}

#[async_trait]
impl MemoryRecall for CortexProvider {
    async fn recall(
        &self,
        query: &str,
        limit: usize,
        opts: &OwnedRecallOpts,
        scope: Option<&SourceScope>,
    ) -> Result<Vec<MemoryEntry>, MemoryError> {
        self.mandatory.recall(query, limit, opts, scope).await
    }
}

#[async_trait]
impl MemoryPortability for CortexProvider {
    async fn export_page(
        &self,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<ExportPage, MemoryError> {
        self.mandatory.export_page(cursor, limit).await
    }

    async fn import_records(
        &self,
        records: Vec<ExportRecord>,
    ) -> Result<ImportOutcome, MemoryError> {
        self.mandatory.import_records(records).await
    }
}

#[async_trait]
impl MemoryDocumentIngest for CortexProvider {
    async fn ingest_document(&self, document: IngestItem) -> Result<IngestOutcome, MemoryError> {
        if document.source_id.trim().is_empty() || document.content.trim().is_empty() {
            return Err(MemoryError::Invalid(
                "document source id and content must not be empty".to_string(),
            ));
        }
        let namespace = document
            .namespace
            .clone()
            .unwrap_or_else(|| format!("document:{}", document.source_id));
        let key = format!("document:{}", document.source_id);
        let payload = serde_json::to_value(&document)?;
        let seed = serde_json::to_string(&payload)?;
        let receipt = self
            .experience(ExperienceInput {
                namespace: &namespace,
                modality: "document",
                role: None,
                key: &key,
                body: &document.content,
                session_id: None,
                taint: document.taint,
                payload,
                idempotency_seed: &seed,
                observed_at: document.timestamp.map(|stamp| stamp.to_rfc3339()),
                labels: document.tags,
            })
            .await?;
        Ok(single_outcome(receipt))
    }
}

#[async_trait]
impl MemoryConversationIngest for CortexProvider {
    async fn ingest_conversation(
        &self,
        messages: Vec<IngestItem>,
    ) -> Result<IngestOutcome, MemoryError> {
        let Some(first) = messages.first() else {
            return Ok(IngestOutcome::default());
        };
        if first.source_id.trim().is_empty()
            || messages.iter().any(|message| {
                message.source_id != first.source_id || message.content.trim().is_empty()
            })
        {
            return Err(MemoryError::Invalid(
                "conversation batches must contain one non-empty conversation".to_string(),
            ));
        }
        let conversation_id = first.source_id.clone();
        let namespace = first
            .namespace
            .clone()
            .unwrap_or_else(|| format!("conversation:{conversation_id}"));
        if messages
            .iter()
            .any(|message| message.namespace.as_deref().unwrap_or(&namespace) != namespace)
        {
            return Err(MemoryError::Invalid(
                "conversation batches must use one namespace".to_string(),
            ));
        }
        let message_count = messages.len();
        let mut items = Vec::with_capacity(message_count);
        for (index, message) in messages.into_iter().enumerate() {
            let role = message.author.clone().unwrap_or_else(|| "user".to_string());
            let payload = serde_json::to_value(&message)?;
            let seed = format!(
                "{conversation_id}:{index}:{}",
                serde_json::to_string(&payload)?
            );
            let key = format!("message:{conversation_id}:{index}");
            items.push(Self::experience_request(ExperienceInput {
                namespace: &namespace,
                modality: "conversation",
                role: Some(&role),
                key: &key,
                body: &message.content,
                session_id: Some(&conversation_id),
                taint: message.taint,
                payload,
                idempotency_seed: &seed,
                observed_at: message.timestamp.map(|stamp| stamp.to_rfc3339()),
                labels: message.tags,
            })?);
        }
        let response: Value = self
            .client
            .json(
                Method::POST,
                "v1/experience/bulk?wait=indexed",
                Some(&json!({ "items": items, "ordering": "strict_temporal" })),
                Attempts::Once,
            )
            .await
            .map_err(engine_error)?;
        let results = response
            .get("results")
            .and_then(Value::as_array)
            .ok_or_else(|| MemoryError::Backend("CortexDB omitted bulk results".to_string()))?;
        if results.len() != message_count {
            return Err(MemoryError::Backend(format!(
                "CortexDB returned {} results for {} conversation messages",
                results.len(),
                message_count
            )));
        }
        let receipts = results.iter().map(receipt).collect::<Result<Vec<_>, _>>()?;
        let written = ingest_count(receipts.iter().filter(|(_, replayed)| !replayed).count())?;
        let ids = receipts
            .into_iter()
            .filter_map(|(id, replayed)| (!replayed).then_some(id))
            .collect();
        Ok(IngestOutcome {
            written,
            ids,
            already_ingested: written == 0,
            extract_jobs_enqueued: written,
            ..IngestOutcome::default()
        })
    }
}

#[async_trait]
impl MemoryLearningIngest for CortexProvider {
    async fn ingest_learning(
        &self,
        learning: LearningCandidate,
    ) -> Result<IngestOutcome, MemoryError> {
        if learning.key.trim().is_empty()
            || learning.value.trim().is_empty()
            || !learning.initial_confidence.is_finite()
            || !(0.0..=1.0).contains(&learning.initial_confidence)
        {
            return Err(MemoryError::Invalid(
                "learning key and value must be set and confidence must be between 0 and 1"
                    .to_string(),
            ));
        }
        let class = serde_json::to_value(learning.class)?
            .as_str()
            .unwrap_or("unknown")
            .to_string();
        let namespace = format!("learning:{class}");
        let observed_at = observed_at(learning.observed_at)?;
        let payload = serde_json::to_value(&learning)?;
        let seed = serde_json::to_string(&payload)?;
        let key = format!("learning:{}", learning.key);
        let body = format!("{}: {}", learning.key, learning.value);
        let receipt = self
            .experience(ExperienceInput {
                namespace: &namespace,
                modality: "observation",
                role: None,
                key: &key,
                body: &body,
                session_id: None,
                taint: MemoryTaint::Internal,
                payload,
                idempotency_seed: &seed,
                observed_at: Some(observed_at),
                labels: vec!["tinymemory-learning".to_string(), class],
            })
            .await?;
        Ok(single_outcome(receipt))
    }
}

#[async_trait]
impl MemoryEventIngest for CortexProvider {
    async fn ingest_event(&self, event: RawMemoryEvent) -> Result<IngestOutcome, MemoryError> {
        if event.id.trim().is_empty()
            || event.namespace.trim().is_empty()
            || event.event_type.trim().is_empty()
            || event.content.trim().is_empty()
        {
            return Err(MemoryError::Invalid(
                "event id, namespace, type, and content must not be empty".to_string(),
            ));
        }
        let payload = serde_json::to_value(&event)?;
        let key = format!("event:{}", event.id);
        let seed = event_identity(&event.namespace, &event.id);
        let receipt = self
            .experience(ExperienceInput {
                namespace: &event.namespace,
                modality: &event.event_type,
                role: None,
                key: &key,
                body: &event.content,
                session_id: event.session_id.as_deref(),
                taint: event.taint,
                payload,
                idempotency_seed: &seed,
                observed_at: event.occurred_at.map(|stamp| stamp.to_rfc3339()),
                labels: vec![format!("tinymemory-event:{}", event.event_type)],
            })
            .await?;
        Ok(single_outcome(receipt))
    }
}

#[async_trait]
impl MemoryAnswer for CortexProvider {
    async fn answer(&self, request: AnswerRequest) -> Result<AnswerResponse, MemoryError> {
        if request.query.trim().is_empty() || request.limit == 0 {
            return Err(MemoryError::Invalid(
                "answer query must not be empty and limit must be positive".to_string(),
            ));
        }
        let namespace = request
            .recall
            .namespace
            .as_deref()
            .unwrap_or(GLOBAL_NAMESPACE);
        if request.scope.is_some()
            || request.recall.category.is_some()
            || request.recall.session_id.is_some()
            || request.recall.min_score.is_some()
            || request.recall.exclude_session_id.is_some()
            || request.recall.cross_session
        {
            return Err(MemoryError::Invalid(
                "CortexDB answers cannot safely apply the requested recall filters".to_string(),
            ));
        }
        let scope = CortexDialect::scope_of(namespace).map_err(engine_error)?;
        let pack: Value = self
            .client
            .json(
                Method::POST,
                "v1/recall",
                Some(&json!({
                    "scope": scope,
                    "query": request.query,
                    "budgets": { "per_layer_limits": layer_limits(request.limit) },
                })),
                Attempts::RetryTransient,
            )
            .await
            .map_err(engine_error)?;
        let pack_id = pack
            .get("pack_id")
            .and_then(Value::as_str)
            .ok_or_else(|| MemoryError::Backend("CortexDB recall omitted pack_id".to_string()))?;
        let response: Value = self
            .client
            .json(
                Method::POST,
                "v1/answer",
                Some(&json!({
                    "scope": scope,
                    "question": request.query,
                    "use_pack_id": pack_id,
                    "answer_instructions": request.instructions,
                    "cite_sources": true,
                    "include_context": true,
                })),
                Attempts::Once,
            )
            .await
            .map_err(engine_error)?;
        let fallback_context = response
            .get("context_block")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let citations = response
            .get("citations")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(index, citation)| citation_of(citation, namespace, index, fallback_context))
            .collect();
        let answer = answer_text(&response)?;
        Ok(AnswerResponse {
            answer: answer.to_string(),
            citations,
            steps: vec![AnswerStep {
                operation: "cortexdb_answer".to_string(),
                detail: "CortexDB recalled evidence and synthesized a grounded answer".to_string(),
            }],
            model: response
                .pointer("/diagnostics/answer_model")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    }
}

fn citation_of(
    citation: &Value,
    namespace: &str,
    index: usize,
    fallback_context: &str,
) -> AnswerCitation {
    if let Some(id) = citation.as_str() {
        return AnswerCitation {
            id: id.to_string(),
            namespace: Some(namespace.to_string()),
            key: id.to_string(),
            content: fallback_context.to_string(),
            score: None,
        };
    }
    let id = citation
        .get("id")
        .or_else(|| citation.get("event_id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| format!("citation-{index}"));
    AnswerCitation {
        key: citation
            .get("key")
            .or_else(|| citation.get("title"))
            .and_then(Value::as_str)
            .unwrap_or(&id)
            .to_string(),
        content: citation
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        score: citation.get("score").and_then(Value::as_f64),
        id,
        namespace: Some(namespace.to_string()),
    }
}

pub(super) fn answer_text(response: &Value) -> Result<&str, MemoryError> {
    response
        .get("answer")
        .and_then(Value::as_str)
        .ok_or_else(|| MemoryError::Backend("CortexDB omitted string answer".to_string()))
}

fn single_outcome((id, replayed): (String, bool)) -> IngestOutcome {
    IngestOutcome {
        written: u32::from(!replayed),
        ids: (!replayed).then_some(id).into_iter().collect(),
        already_ingested: replayed,
        extract_jobs_enqueued: u32::from(!replayed),
        ..IngestOutcome::default()
    }
}

pub(super) fn ingest_count(count: usize) -> Result<u32, MemoryError> {
    u32::try_from(count).map_err(|_| {
        MemoryError::Backend(format!(
            "CortexDB returned {count} results, exceeding TinyMemory's u32 ingest count"
        ))
    })
}

#[async_trait]
impl MemoryProvider for CortexProvider {
    fn driver_id(&self) -> &str {
        CORTEX_DRIVER_ID
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::mandatory()
            .with(Capability::DocumentIngest)
            .with(Capability::ConversationIngest)
            .with(Capability::LearningIngest)
            .with(Capability::EventIngest)
            .with(Capability::Answer)
    }

    async fn health(&self) -> MemoryHealth {
        self.mandatory.health().await
    }

    fn as_document_ingest(&self) -> Option<&dyn MemoryDocumentIngest> {
        Some(self)
    }

    fn as_conversation_ingest(&self) -> Option<&dyn MemoryConversationIngest> {
        Some(self)
    }

    fn as_learning_ingest(&self) -> Option<&dyn MemoryLearningIngest> {
        Some(self)
    }

    fn as_event_ingest(&self) -> Option<&dyn MemoryEventIngest> {
        Some(self)
    }

    fn as_answer(&self) -> Option<&dyn MemoryAnswer> {
        Some(self)
    }
}
