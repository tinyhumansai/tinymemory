//! Tests for the host-owned chat bridge over an in-memory TinyBus.

use tinyagents::harness::message::{AssistantMessage, ContentBlock, Message};
use tinyagents::harness::model::{ModelRequest, ModelResponse};
use tinyagents::harness::usage::Usage;
use tinybus::broker::Broker;
use tinybus::transport::memory::MemoryBus;
use tinybus::{Connection, Result as BusResult};

use super::{BusChatHost, CHAT_HOST_BUS_NAME, CHAT_HOST_OBJECT_PATH};
use crate::config::ModuleConfig;

struct FakeChatHost;

#[tinybus::interface(name = "ai.tinyhumans.tinymemory.ChatHost")]
impl FakeChatHost {
    async fn complete(&self, role: String, request: ModelRequest) -> BusResult<ModelResponse> {
        std::future::ready(()).await;
        Ok(ModelResponse {
            message: AssistantMessage {
                id: None,
                content: vec![ContentBlock::Text(format!(
                    "{role}:{}",
                    request.messages.len()
                ))],
                tool_calls: Vec::new(),
                usage: Some(Usage::new(2, 1)),
            },
            usage: Some(Usage::new(2, 1)),
            finish_reason: Some("stop".into()),
            raw: None,
            resolved_model: None,
            continue_turn: None,
            served_from_cache: false,
        })
    }
}

async fn bus_with_chat_host() -> Connection {
    let bus = MemoryBus::new();
    let broker = Broker::new();
    let _broker_task = broker.spawn(bus.clone());
    let host = Connection::connect(bus.connect().await.expect("host transport"))
        .await
        .expect("host connection");
    host.serve_at(
        CHAT_HOST_OBJECT_PATH.try_into().expect("object path"),
        FakeChatHost,
    )
    .await
    .expect("serve chat host");
    host.request_name(CHAT_HOST_BUS_NAME)
        .await
        .expect("claim name");
    std::mem::forget(host);
    Connection::connect(bus.connect().await.expect("module transport"))
        .await
        .expect("module connection")
}

#[tokio::test]
async fn configured_role_and_model_cross_the_chat_bridge() {
    use tinymemory_core::chat_host::ChatHost;

    let config = ModuleConfig {
        memory_provider: Some("host-router".into()),
        default_model: Some("host-model".into()),
        ..ModuleConfig::default()
    };
    let bridge = BusChatHost::new(bus_with_chat_host().await, &config);
    let runtime = tinymemory_tinycortex::engine::EngineRuntimeConfig::from(&config);
    let rendered = format!("{bridge:?}");
    assert!(rendered.contains("BusChatHost"), "{rendered}");
    assert!(rendered.contains("host-router"), "{rendered}");
    assert!(rendered.contains("host-model"), "{rendered}");
    assert!(!rendered.contains("Connection"), "{rendered}");
    assert_eq!(
        bridge.provider_for_role("summarizer", &runtime),
        "host-router"
    );
    assert_eq!(
        bridge.summarizer_available(&runtime),
        (true, "served by the TinyMemory host callback")
    );
    let (model, model_id) = bridge
        .create_chat_model_with_model_id("summarizer", &runtime, 0.2)
        .expect("create bus model");
    assert_eq!(model_id, "host-model");
    assert_eq!(
        model.cache_identity().as_deref(),
        Some("tinymemory-module-host:summarizer")
    );
    let response = model
        .invoke(&(), ModelRequest::new(vec![Message::user("summarize")]))
        .await
        .expect("chat call");
    assert!(bridge.usage_from_response(&response).is_none());
    assert!(matches!(
        response.message.content.as_slice(),
        [ContentBlock::Text(text)] if text == "summarizer:1"
    ));
}

#[tokio::test]
async fn defaults_are_credential_free_and_an_absent_host_fails_cleanly() {
    use tinymemory_core::chat_host::ChatHost;

    let bus = MemoryBus::new();
    let broker = Broker::new();
    let _broker_task = broker.spawn(bus.clone());
    let connection = Connection::connect(bus.connect().await.expect("transport"))
        .await
        .expect("connection");
    let bridge = BusChatHost::new(connection, &ModuleConfig::default());
    let runtime =
        tinymemory_tinycortex::engine::EngineRuntimeConfig::from(&ModuleConfig::default());
    assert_eq!(bridge.provider_for_role("role", &runtime), "host");
    let (model, model_id) = bridge
        .create_chat_model_with_model_id("role", &runtime, 0.0)
        .expect("create model");
    assert_eq!(model_id, "host-default");
    assert!(model
        .invoke(&(), ModelRequest::default())
        .await
        .expect_err("no host name is served")
        .to_string()
        .contains(CHAT_HOST_BUS_NAME));
}
