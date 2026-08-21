//! Product shell over tinycortex on-demand ingestion.

use anyhow::Result;

use crate::engine::backend::ingest::canonicalize::{
    chat::{self, ChatBatch},
    document::{self, DocumentInput},
    email::{self, EmailThread},
    CanonicalisedSource,
};
use crate::store::chunks::store::RawRef;
use crate::Config;

// The input shapes this funnel accepts, re-exported so callers ingest through
// this module without naming the engine themselves (#18 §B1). The funnel is
// core's designated ingest seam; the engine reference belongs here, once.
pub use crate::engine::backend::ingest::canonicalize::document::DocumentInput as IngestDocumentInput;

pub use crate::engine::backend::ingest::IngestSummary as IngestResult;

pub async fn ingest_chat(
    config: &Config,
    source_id: &str,
    owner: &str,
    tags: Vec<String>,
    batch: ChatBatch,
) -> Result<IngestResult> {
    let canonical =
        chat::canonicalise(source_id, owner, &tags, batch.clone()).map_err(anyhow::Error::msg)?;
    let (memory, sink, scoring) = crate::engine::ingest_context(config);
    let result = crate::engine::backend::ingest::ingest_chat(
        &memory, source_id, owner, tags, batch, &sink, &scoring,
    )
    .await?;
    publish_canonicalized(source_id, canonical.as_ref(), &result);
    Ok(result)
}

pub async fn ingest_email(
    config: &Config,
    source_id: &str,
    owner: &str,
    tags: Vec<String>,
    thread: EmailThread,
) -> Result<IngestResult> {
    let canonical =
        email::canonicalise(source_id, owner, &tags, thread.clone()).map_err(anyhow::Error::msg)?;
    let (memory, sink, scoring) = crate::engine::ingest_context(config);
    let result = crate::engine::backend::ingest::ingest_email(
        &memory, source_id, owner, tags, thread, &sink, &scoring,
    )
    .await?;
    publish_canonicalized(source_id, canonical.as_ref(), &result);
    Ok(result)
}

pub async fn ingest_email_with_raw_refs(
    config: &Config,
    source_id: &str,
    owner: &str,
    tags: Vec<String>,
    thread: EmailThread,
    raw_refs: Vec<RawRef>,
) -> Result<IngestResult> {
    let canonical =
        email::canonicalise(source_id, owner, &tags, thread.clone()).map_err(anyhow::Error::msg)?;
    let (memory, sink, scoring) = crate::engine::ingest_context(config);
    let result = crate::engine::backend::ingest::ingest_email_with_raw_refs(
        &memory, source_id, owner, tags, thread, raw_refs, &sink, &scoring,
    )
    .await?;
    publish_canonicalized(source_id, canonical.as_ref(), &result);
    Ok(result)
}

pub async fn ingest_document(
    config: &Config,
    source_id: &str,
    owner: &str,
    tags: Vec<String>,
    doc: DocumentInput,
) -> Result<IngestResult> {
    ingest_document_with_scope(config, source_id, owner, tags, doc, None).await
}

pub async fn ingest_document_with_scope(
    config: &Config,
    source_id: &str,
    owner: &str,
    tags: Vec<String>,
    doc: DocumentInput,
    path_scope: Option<String>,
) -> Result<IngestResult> {
    ingest_document_versioned(config, source_id, owner, tags, doc, path_scope, None).await
}

pub async fn ingest_document_versioned(
    config: &Config,
    source_id: &str,
    owner: &str,
    tags: Vec<String>,
    doc: DocumentInput,
    path_scope: Option<String>,
    version_ms: Option<i64>,
) -> Result<IngestResult> {
    let canonical =
        document::canonicalise(source_id, owner, &tags, doc.clone(), path_scope.clone())
            .map_err(anyhow::Error::msg)?;
    let (memory, sink, scoring) = crate::engine::ingest_context(config);
    let result = crate::engine::backend::ingest::ingest_document_versioned(
        &memory, source_id, owner, tags, doc, path_scope, version_ms, &sink, &scoring,
    )
    .await?;
    publish_canonicalized(source_id, canonical.as_ref(), &result);
    Ok(result)
}

fn publish_canonicalized(
    source_id: &str,
    canonical: Option<&CanonicalisedSource>,
    result: &IngestResult,
) {
    let Some(canonical) = canonical else {
        return;
    };
    let source_kind = canonical.metadata.source_kind.as_str();
    let body_preview = if matches!(source_kind, "email" | "document") {
        utf8_suffix(&canonical.markdown, 2048)
    } else {
        utf8_prefix(&canonical.markdown, 2048)
    };
    crate::events::publish(crate::events::MemoryEvent::DocumentCanonicalized {
        source_id: source_id.into(),
        source_kind: canonical.metadata.source_kind.as_str().into(),
        chunks_written: result.chunks_written,
        chunk_ids: result.chunk_ids.clone(),
        canonicalized_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64(),
        body_preview: Some(body_preview),
    });
}

fn utf8_suffix(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let target = value.len().saturating_sub(max_bytes);
    let start = value
        .char_indices()
        .map(|(index, _)| index)
        .find(|index| *index >= target)
        .unwrap_or(value.len());
    value[start..].to_owned()
}

fn utf8_prefix(value: &str, max_bytes: usize) -> String {
    let end = value
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= max_bytes)
        .last()
        .unwrap_or(0);
    let end = if value.len() <= max_bytes {
        value.len()
    } else if end == 0 {
        0
    } else {
        end
    };
    value[..end].to_string()
}

#[cfg(test)]
#[path = "ingest_pipeline_tests.rs"]
mod tests;
