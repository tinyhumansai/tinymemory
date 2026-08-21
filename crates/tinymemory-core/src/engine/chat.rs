//! LLM chat seam — bridge OpenHuman's memory chat runtime onto the crate's
//! [`ChatProvider`] (W1).
//!
//! TinyCortex extracts entities/topics and summarises via an injected
//! `ChatProvider` (it never makes a network call). OpenHuman already owns a
//! memory LLM surface — `memory::chat::{ChatProvider, ChatPrompt}` with
//! `build_chat_provider(&Config)` routing through `openhuman::inference`
//! (provider selection, credit metering, usage accounting). This adapter wraps
//! that host provider and re-exposes it as the crate's `ChatProvider`, so the
//! engine's LLM entity extractor / summariser drive OpenHuman inference without
//! duplicating any routing.
//!
//! The two contracts are near-identical (`name` + async `chat_for_json`). The
//! only conversion is the prompt: the host `ChatPrompt.temperature` is `f64`,
//! the crate's is `f32`; every other field maps 1:1.

use std::sync::Arc;

use async_trait::async_trait;
use tinycortex::memory::score::extract::{
    ChatPrompt as CortexChatPrompt, ChatProvider as CortexChatProvider,
};

use crate::chat::{
    build_chat_provider as build_host_chat_provider, ChatPrompt as HostChatPrompt,
    ChatProvider as HostChatProvider,
};
use crate::Config;

/// Wraps an OpenHuman [`HostChatProvider`] as the crate's [`CortexChatProvider`].
pub struct SeamChatProvider {
    inner: Arc<dyn HostChatProvider>,
}

impl SeamChatProvider {
    /// Build the adapter over a host chat provider (already routed through
    /// `openhuman::inference`).
    pub fn new(inner: Arc<dyn HostChatProvider>) -> Self {
        tracing::debug!(
            provider = inner.name(),
            "[memory] constructing tinycortex chat seam over memory::chat::ChatProvider"
        );
        Self { inner }
    }
}

#[async_trait]
impl CortexChatProvider for SeamChatProvider {
    fn name(&self) -> &str {
        self.inner.name()
    }

    async fn chat_for_json(&self, prompt: &CortexChatPrompt) -> anyhow::Result<String> {
        let host = HostChatPrompt {
            system: prompt.system.clone(),
            user: prompt.user.clone(),
            // Crate temperature is f32; host takes f64.
            temperature: f64::from(prompt.temperature),
            kind: prompt.kind,
            max_tokens: prompt.max_tokens,
        };
        self.inner.chat_for_json(&host).await
    }
}

/// Build a crate [`CortexChatProvider`] from the host [`Config`], routed through
/// `openhuman::inference` — the entry point the seam's LLM entity extractor and
/// summariser construct from.
pub fn build_chat_provider(config: &Config) -> anyhow::Result<Arc<dyn CortexChatProvider>> {
    let host = build_host_chat_provider(config)?;
    Ok(Arc::new(SeamChatProvider::new(host)))
}

#[cfg(test)]
#[path = "chat_tests.rs"]
mod tests;
