//! Memory LLM adapter backed by the unified inference provider stack.
//!
//! Memory callers still want a tiny prompt surface: one system message, one
//! user message, and a string response. This module keeps that narrow contract
//! for the rest of the memory layer, but routes every production call through
//! `openhuman::inference::provider` so memory uses the same workload routing as
//! the rest of the app.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::chat_host::{create_chat_model_with_model_id, provider_for_role, UsageInfo};
use crate::Config;
use tinyagents::harness::message::Message;
use tinyagents::harness::model::{ChatModel, ModelRequest};

/// One pair of prompt messages handed to the memory LLM backend.
#[derive(Debug, Clone)]
pub struct ChatPrompt {
    pub system: String,
    pub user: String,
    pub temperature: f64,
    pub kind: &'static str,
    /// Optional output-token cap forwarded to the provider as `max_tokens`.
    /// `None` leaves generation open-ended. Memory callers with a bounded
    /// response (entity extraction) set a small value so credit-metered
    /// providers don't reserve the model's full output window in their
    /// balance pre-flight (TAURI-RUST-C62).
    pub max_tokens: Option<u32>,
}

/// Pluggable LLM surface used by the memory layer.
#[async_trait]
pub trait ChatProvider: Send + Sync {
    fn name(&self) -> &str;

    async fn chat_for_json(&self, prompt: &ChatPrompt) -> Result<String>;

    async fn chat_for_text(&self, prompt: &ChatPrompt) -> Result<String> {
        self.chat_for_json(prompt).await
    }

    /// Like `chat_for_text`, but also surfaces the provider-reported
    /// [`UsageInfo`] (real token counts + `charged_amount_usd`) when the
    /// backing provider returns it.
    ///
    /// The default implementation simply runs `chat_for_text` and reports
    /// `None` usage, so implementors (e.g. test doubles, external impls)
    /// that don't thread usage keep compiling unchanged. The production
    /// `InferenceChatProvider` overrides this to route through the
    /// inference `Provider::chat` API, which already parses usage out of
    /// the backend response.
    async fn chat_for_text_with_usage(
        &self,
        prompt: &ChatPrompt,
    ) -> Result<(String, Option<UsageInfo>)> {
        let text = self.chat_for_text(prompt).await?;
        Ok((text, None))
    }
}

struct InferenceChatProvider {
    inner: Arc<dyn ChatModel<()>>,
    model_id: String,
    display: String,
}

impl InferenceChatProvider {
    fn new(inner: Arc<dyn ChatModel<()>>, model_id: String) -> Self {
        let display = format!("inference:{model_id}");
        Self {
            inner,
            model_id,
            display,
        }
    }

    async fn run(&self, prompt: &ChatPrompt) -> Result<String> {
        let (text, _usage) = self.run_with_usage(prompt).await?;
        Ok(text)
    }

    /// Run the prompt through the inference `Provider::chat` API and return
    /// both the text and the provider-reported usage. Memory historically
    /// called `chat_with_history` (which returns only `String` and drops
    /// the parsed usage); routing through `chat` instead lets us thread the
    /// real token counts + `charged_amount_usd` into the sync audit log
    /// (issue #3110) without re-deriving them from `body.len() / 4`.
    async fn run_with_usage(&self, prompt: &ChatPrompt) -> Result<(String, Option<UsageInfo>)> {
        log::debug!(
            "[memory::chat] provider={} kind={} model={} sys_chars={} user_chars={}",
            self.display,
            prompt.kind,
            self.model_id,
            prompt.system.len(),
            prompt.user.len()
        );

        // One system + one user turn — the crate model interface's native shape.
        // Temperature and the output cap ride the request (the shared model is
        // reused across memory prompts of differing temperature/budget), and the
        // adapter honors both per-request values.
        let mut request = ModelRequest::new(vec![
            Message::system(prompt.system.clone()),
            Message::user(prompt.user.clone()),
        ])
        .with_temperature(prompt.temperature);
        if let Some(cap) = prompt.max_tokens {
            request = request.with_max_tokens(cap);
        }

        let response = self.inner.invoke(&(), request).await?;

        // Fail fast on a missing body rather than masking it as an empty
        // string: an empty summary would still be ingested (and, post-#3110,
        // counted against the run's real charge) as if it were valid output.
        // The caller's fallback path (`fallback_summary`) is the correct
        // recovery for a silent provider, and it only runs on `Err`.
        let text = response.text();
        if text.is_empty() {
            anyhow::bail!(
                "inference provider '{}' returned no text for {} summarise request",
                self.display,
                prompt.kind
            );
        }
        // Recover the full host usage (real token counts + backend-charged USD +
        // context window) the adapter round-tripped through the response (G1).
        let usage = crate::chat_host::usage_from_response(&response);

        log::debug!(
            "[memory::chat] provider={} kind={} response_chars={} usage_present={} input_tokens={} output_tokens={} charged_usd={}",
            self.display,
            prompt.kind,
            text.len(),
            usage.is_some(),
            usage.as_ref().map(|u| u.input_tokens).unwrap_or(0),
            usage.as_ref().map(|u| u.output_tokens).unwrap_or(0),
            usage.as_ref().map(|u| u.charged_amount_usd).unwrap_or(0.0),
        );

        Ok((text, usage))
    }
}

#[async_trait]
impl ChatProvider for InferenceChatProvider {
    fn name(&self) -> &str {
        &self.display
    }

    async fn chat_for_json(&self, prompt: &ChatPrompt) -> Result<String> {
        self.run(prompt).await
    }

    async fn chat_for_text(&self, prompt: &ChatPrompt) -> Result<String> {
        self.run(prompt).await
    }

    async fn chat_for_text_with_usage(
        &self,
        prompt: &ChatPrompt,
    ) -> Result<(String, Option<UsageInfo>)> {
        self.run_with_usage(prompt).await
    }
}

#[cfg(any(test, feature = "test-support"))]
pub use test_support::{test_override, StaticChatProvider};

// The task-local provider implementation is external test support. Keep the
// live runtime builder below at its established source coordinates so LLVM can
// merge identical copies linked into unit and integration-test executables.
//
// Runtime selection remains explicit in `runtime_override`; no test provider
// implementation or fixture state lives in this production source file.
//
/// Build the memory LLM provider and return the resolved model id.
pub fn build_chat_runtime(config: &Config) -> Result<(Arc<dyn ChatProvider>, String)> {
    if let Some(runtime) = runtime_override::current_runtime() {
        return Ok(runtime);
    }

    // The managed summarization tier is fixed at `summarization-v1`, resolved
    // inside `make_openhuman_backend` for the `summarization` role — so no
    // per-caller `default_model` pre-routing is needed here. BYOK/local routes
    // carry their own model in the provider string.
    let resolved_provider = provider_for_role("summarization", config);
    // Temperature is applied per-prompt via `ModelRequest::with_temperature`
    // (each memory `ChatPrompt` carries its own), so the construction temperature
    // is just a default the per-call value overrides.
    let (model, model_id) =
        create_chat_model_with_model_id("summarization", config, config.default_temperature())?;

    log::debug!(
        "[memory::chat] built provider route={} model={}",
        resolved_provider,
        model_id
    );

    Ok((
        Arc::new(InferenceChatProvider::new(model, model_id.clone())),
        model_id,
    ))
}

/// Build the memory LLM provider dictated by the inference workload routing.
pub fn build_chat_provider(config: &Config) -> Result<Arc<dyn ChatProvider>> {
    Ok(build_chat_runtime(config)?.0)
}

#[cfg(test)]
#[path = "chat_tests.rs"]
mod tests;

#[path = "chat_runtime_override.rs"]
mod runtime_override;
#[path = "chat_test_support.rs"]
mod test_support;
