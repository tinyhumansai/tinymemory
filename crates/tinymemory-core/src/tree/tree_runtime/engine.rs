//! Product adapters around the tinycortex markdown time-tree engine.

use std::sync::Arc;

use crate::engine::backend::tree::runtime::{
    NodeLevel, RuntimeObserver, Summariser, TreeNode, TreeStatus,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Timelike, Utc};
use tinyagents::harness::message::Message;
use tinyagents::harness::model::{ChatModel, ModelRequest};

use crate::engine::engine_config;
use crate::Config;

const SUMMARIZATION_TEMP: f64 = 0.3;

struct ChatSummariser<'a>(&'a dyn ChatModel<()>);

#[async_trait]
impl Summariser for ChatSummariser<'_> {
    async fn summarise(&self, system: Option<&str>, content: &str) -> Result<String> {
        log::debug!(
            "[tree_summarizer] provider call content_chars={} has_system={}",
            content.len(),
            system.is_some()
        );
        let mut messages = Vec::with_capacity(2);
        if let Some(system) = system {
            messages.push(Message::system(system.to_string()));
        }
        messages.push(Message::user(content.to_string()));
        let response = self
            .0
            .invoke(
                &(),
                ModelRequest::new(messages).with_temperature(SUMMARIZATION_TEMP),
            )
            .await
            .context("time-tree summarization provider call failed")?
            .text();
        log::debug!(
            "[tree_summarizer] provider call complete response_chars={}",
            response.len()
        );
        Ok(response)
    }
}

struct EventObserver;

impl RuntimeObserver for EventObserver {
    fn hour_completed(&self, namespace: &str, node_id: &str, token_count: u32) {
        crate::events::publish(crate::events::MemoryEvent::TreeSummarizerHourCompleted {
            namespace: namespace.to_string(),
            node_id: node_id.to_string(),
            token_count,
        });
    }

    fn node_propagated(&self, namespace: &str, node_id: &str, level: NodeLevel, token_count: u32) {
        crate::events::publish(crate::events::MemoryEvent::TreeSummarizerPropagated {
            namespace: namespace.to_string(),
            node_id: node_id.to_string(),
            level: level.as_str().to_string(),
            token_count,
        });
    }

    fn rebuild_completed(&self, namespace: &str, total_nodes: u64) {
        crate::events::publish(crate::events::MemoryEvent::TreeSummarizerRebuildCompleted {
            namespace: namespace.to_string(),
            total_nodes,
        });
    }
}

pub async fn run_summarization(
    config: &Config,
    provider: &dyn ChatModel<()>,
    namespace: &str,
    ts: DateTime<Utc>,
) -> Result<Option<TreeNode>> {
    log::debug!("[tree_summarizer] tinycortex run namespace={namespace}");
    let result = crate::engine::backend::tree::runtime::run_summarization_observed(
        &engine_config(config),
        &ChatSummariser(provider),
        namespace,
        ts,
        &EventObserver,
    )
    .await;
    log::debug!(
        "[tree_summarizer] tinycortex run complete namespace={} success={}",
        namespace,
        result.is_ok()
    );
    result
}

pub async fn rebuild_tree(
    config: &Config,
    provider: &dyn ChatModel<()>,
    namespace: &str,
) -> Result<TreeStatus> {
    log::debug!("[tree_summarizer] tinycortex rebuild namespace={namespace}");
    crate::engine::backend::tree::runtime::rebuild_tree_observed(
        &engine_config(config),
        &ChatSummariser(provider),
        namespace,
        &EventObserver,
    )
    .await
}

pub async fn run_hourly_loop(config: Arc<Config>, provider: Arc<dyn ChatModel<()>>) {
    log::debug!("[tree_summarizer] hourly loop started");
    loop {
        let now = Utc::now();
        let base = now
            .date_naive()
            .and_hms_opt(now.hour(), 0, 0)
            .unwrap_or(now.naive_utc());
        let next_hour =
            DateTime::<Utc>::from_naive_utc_and_offset(base + chrono::Duration::hours(1), Utc);
        let sleep_duration = (next_hour - now)
            .to_std()
            .unwrap_or(std::time::Duration::from_secs(3600));
        log::debug!(
            "[tree_summarizer] sleeping seconds={}",
            sleep_duration.as_secs()
        );
        tokio::time::sleep(sleep_duration).await;

        let ts = Utc::now();
        let namespaces = crate::engine::backend::tree::runtime::discover_active_namespaces(
            &engine_config(&*config),
        );
        log::debug!(
            "[tree_summarizer] hourly tick active_namespaces={}",
            namespaces.len()
        );
        for namespace in namespaces {
            if let Err(error) = run_summarization(&*config, provider.as_ref(), &namespace, ts).await
            {
                log::error!(
                    "[tree_summarizer] hourly run failed namespace={} error={error:#}",
                    namespace
                );
            }
        }
    }
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
