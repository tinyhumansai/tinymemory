//! Chat-model calls stay host-side and cross `TinyBus` as typed requests.

use std::sync::Arc;

use async_trait::async_trait;
use tinyagents::harness::model::{ChatModel, ModelRequest, ModelResponse};
use tinybus::Connection;

use crate::ModuleConfig;

/// Well-known host service used for memory summarisation and extraction.
pub const CHAT_HOST_BUS_NAME: &str = "ai.tinyhumans.tinymemory.ChatHost";
/// Host object path for [`CHAT_HOST_BUS_NAME`].
pub const CHAT_HOST_OBJECT_PATH: &str = "/ai/tinyhumans/tinymemory/ChatHost";
/// Interface name exported by the host.
pub const CHAT_HOST_INTERFACE: &str = "ai.tinyhumans.tinymemory.ChatHost";

/// Builds bus-backed chat models without receiving a provider credential.
pub struct BusChatHost {
    connection: Connection,
    provider: String,
    model_id: String,
}

impl std::fmt::Debug for BusChatHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BusChatHost")
            .field("provider", &self.provider)
            .field("model_id", &self.model_id)
            .finish_non_exhaustive()
    }
}

impl BusChatHost {
    /// Build the host bridge over the module connection.
    #[must_use]
    pub fn new(connection: Connection, config: &ModuleConfig) -> Self {
        Self {
            connection,
            provider: config
                .memory_provider
                .clone()
                .unwrap_or_else(|| "host".to_string()),
            model_id: config
                .default_model
                .clone()
                .unwrap_or_else(|| "host-default".to_string()),
        }
    }
}

impl tinymemory_core::chat_host::ChatHost for BusChatHost {
    fn provider_for_role(&self, _role: &str, _config: &tinymemory_core::Config) -> String {
        self.provider.clone()
    }

    fn create_chat_model_with_model_id(
        &self,
        role: &str,
        _config: &tinymemory_core::Config,
        _temperature: f64,
    ) -> Result<(Arc<dyn ChatModel<()>>, String), String> {
        Ok((
            Arc::new(BusChatModel {
                connection: self.connection.clone(),
                role: role.to_string(),
            }),
            self.model_id.clone(),
        ))
    }

    fn usage_from_response(
        &self,
        _response: &ModelResponse,
    ) -> Option<tinymemory_api::host::UsageInfo> {
        None
    }

    fn summarizer_available(&self, _config: &tinymemory_core::Config) -> (bool, &'static str) {
        (true, "served by the TinyMemory host callback")
    }
}

struct BusChatModel {
    connection: Connection,
    role: String,
}

#[async_trait]
impl ChatModel<()> for BusChatModel {
    fn cache_identity(&self) -> Option<String> {
        Some(format!("tinymemory-module-host:{}", self.role))
    }

    async fn invoke(
        &self,
        _state: &(),
        request: ModelRequest,
    ) -> tinyagents::Result<ModelResponse> {
        let proxy = self
            .connection
            .proxy(
                CHAT_HOST_BUS_NAME,
                CHAT_HOST_OBJECT_PATH,
                CHAT_HOST_INTERFACE,
            )
            .map_err(|error| tinyagents::TinyAgentsError::Model(error.to_string()))?;
        proxy
            .call("Complete", (self.role.clone(), request))
            .await
            .map_err(|error| tinyagents::TinyAgentsError::Model(error.to_string()))
    }
}
