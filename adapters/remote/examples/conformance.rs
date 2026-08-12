//! Live mandatory-family smoke test for a self-hosted remote engine.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tinymemory_api::provider::MemoryProvider;
use tinymemory_api::recall::OwnedRecallOpts;
use tinymemory_api::types::{MemoryCategory, MemoryTaint};
use tinymemory_remote::{
    cognee_provider, mem0_provider, supermemory_provider, CogneeMemory, Mem0Memory,
    SupermemoryMemory,
};

/// Builds the command-line usage error returned for invalid arguments.
fn usage() -> anyhow::Error {
    anyhow::anyhow!("usage: conformance <supermemory|mem0|cognee> <endpoint> [credential]")
}

#[tokio::main]
/// Exercises mandatory capabilities against a live remote backend.
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let engine = args.next().ok_or_else(usage)?;
    let endpoint = args.next().ok_or_else(usage)?;
    let credential = args.next();
    let provider: Arc<dyn MemoryProvider> = match engine.as_str() {
        "supermemory" => Arc::new(supermemory_provider(SupermemoryMemory::new(
            &endpoint,
            credential.as_deref(),
        )?)),
        "mem0" => Arc::new(mem0_provider(Mem0Memory::new(
            &endpoint,
            credential.as_deref(),
        )?)),
        "cognee" => Arc::new(cognee_provider(CogneeMemory::new(
            &endpoint,
            credential.as_deref(),
        )?)),
        _ => return Err(usage()),
    };

    tinymemory_api::provider::audit_provider(provider.as_ref())?;
    let health = provider.health().await;
    anyhow::ensure!(health.is_usable(), "driver health is {health:?}");

    let suffix = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let namespace = format!("tinymemory-conformance-{suffix}");
    let key = "native-round-trip";
    let content = format!("TinyMemory native adapter conformance marker {suffix}");

    provider
        .store(
            &namespace,
            key,
            &content,
            MemoryCategory::Core,
            Some("live-conformance"),
            MemoryTaint::ExternalSync,
        )
        .await?;
    let stored = provider
        .get(&namespace, key)
        .await?
        .ok_or_else(|| anyhow::anyhow!("stored record was not readable"))?;
    anyhow::ensure!(stored.content == content, "stored content changed");
    anyhow::ensure!(
        stored.taint == MemoryTaint::ExternalSync,
        "stored taint changed"
    );

    let hits = provider
        .recall(
            "conformance marker",
            10,
            &OwnedRecallOpts {
                namespace: Some(namespace.clone()),
                ..OwnedRecallOpts::default()
            },
            None,
        )
        .await?;
    anyhow::ensure!(!hits.is_empty(), "native recall returned no record");

    let mut cursor = None;
    let mut exported_keys = Vec::new();
    loop {
        let page = provider.export_page(cursor.as_deref(), 100).await?;
        exported_keys.extend(page.records.iter().filter_map(|record| {
            record
                .payload
                .get("key")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        }));
        let Some(next) = page.next_cursor else {
            break;
        };
        cursor = Some(next);
    }
    anyhow::ensure!(
        exported_keys.iter().any(|exported| exported == key),
        "portability export omitted the record; exported keys: {exported_keys:?}"
    );
    anyhow::ensure!(
        provider.forget(&namespace, key).await?,
        "forget missed record"
    );

    println!(
        "{}: Core, Recall, and Portability passed",
        provider.driver_id()
    );
    Ok(())
}
