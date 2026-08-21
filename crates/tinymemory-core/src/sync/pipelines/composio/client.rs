//! Minimal direct/proxied Composio action client.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::sync::pipelines::traits::{ComposioMode, ComposioSyncConfig};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecuteResponse {
    #[serde(default)]
    pub data: serde_json::Value,
    #[serde(default)]
    pub successful: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(rename = "costUsd", default)]
    pub cost_usd: f64,
    #[serde(rename = "markdownFormatted", default)]
    pub markdown_formatted: Option<String>,
    #[serde(skip, default = "one_attempt")]
    pub attempts: u32,
}

fn one_attempt() -> u32 {
    1
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ExecuteError {
    pub attempts: u32,
    message: String,
}

/// Time to establish a TCP/TLS connection to Composio or the proxy.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// Whole-request ceiling. Composio actions that page a large mailbox can run
/// long, so this is generous, but it is finite.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

#[derive(Clone)]
pub struct ComposioClient {
    http: reqwest::Client,
    config: ComposioSyncConfig,
}

#[async_trait]
pub trait ActionExecutor: Send + Sync {
    async fn execute(
        &self,
        action: &str,
        arguments: serde_json::Value,
        connection_id: Option<&str>,
    ) -> anyhow::Result<ExecuteResponse>;
}

#[async_trait]
impl ActionExecutor for ComposioClient {
    async fn execute(
        &self,
        action: &str,
        arguments: serde_json::Value,
        connection_id: Option<&str>,
    ) -> anyhow::Result<ExecuteResponse> {
        ComposioClient::execute(self, action, arguments, connection_id).await
    }
}

impl ComposioClient {
    /// A client with explicit connect and request timeouts.
    ///
    /// `reqwest::Client::new()` has none: a hung Composio or proxy connection
    /// would stall the sync task indefinitely and hold the sync-state
    /// mutation window open with it. The builder is fallible only on TLS
    /// backend initialisation, which cannot happen with the rustls feature this
    /// crate compiles; if it ever did, an untimed fallback would silently drop
    /// the guarantee, so it panics loudly instead of degrading.
    pub fn new(config: ComposioSyncConfig) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .unwrap_or_else(|error| {
                panic!("Composio HTTP client failed to build (TLS backend unavailable): {error}")
            });
        Self { http, config }
    }

    pub fn with_http_client(mut self, http: reqwest::Client) -> Self {
        self.http = http;
        self
    }

    pub async fn execute(
        &self,
        action: &str,
        arguments: serde_json::Value,
        connection_id: Option<&str>,
    ) -> anyhow::Result<ExecuteResponse> {
        let action = action.trim();
        anyhow::ensure!(!action.is_empty(), "Composio action must not be empty");
        const MAX_ATTEMPTS: u32 = 3;
        for attempt in 1..=MAX_ATTEMPTS {
            let result = match self.config.mode {
                ComposioMode::Direct => {
                    self.execute_direct(action, arguments.clone(), connection_id)
                        .await
                }
                ComposioMode::Proxied => self.execute_proxied(action, arguments.clone()).await,
            };
            match result {
                Ok(mut response)
                    if response.successful
                        || !retryable_provider_error(response.error.as_deref())
                        || attempt == MAX_ATTEMPTS =>
                {
                    response.attempts = attempt;
                    return Ok(response);
                }
                Ok(_) => tracing::warn!(
                    action,
                    attempt,
                    "[sync:composio] retrying provider rate limit"
                ),
                Err(error) if retryable_transport_error(&error) && attempt < MAX_ATTEMPTS => {
                    tracing::warn!(action, attempt, %error, "[sync:composio] retrying transient transport failure");
                }
                Err(error) => {
                    return Err(ExecuteError {
                        attempts: attempt,
                        message: error.to_string(),
                    }
                    .into())
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(
                250 * 2u64.pow(attempt - 1),
            ))
            .await;
        }
        unreachable!("retry loop always returns")
    }

    async fn execute_direct(
        &self,
        action: &str,
        arguments: serde_json::Value,
        connection_id: Option<&str>,
    ) -> anyhow::Result<ExecuteResponse> {
        let key = self
            .config
            .api_key
            .as_ref()
            .filter(|key| !key.is_empty())
            .map(|key| key.expose().to_owned())
            .filter(|key| !key.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("Composio direct API key is not configured"))?;
        let url = format!(
            "{}/tools/execute/{action}",
            self.config.base_url.trim_end_matches('/')
        );
        let mut body = serde_json::json!({ "arguments": arguments });
        if let Some(entity_id) = self
            .config
            .entity_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            body["user_id"] = serde_json::json!(entity_id);
        }
        if let Some(connection_id) = connection_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            body["connected_account_id"] = serde_json::json!(connection_id);
        }

        let response = self
            .http
            .post(url)
            .header("x-api-key", key)
            .json(&body)
            .send()
            .await
            .map_err(|error| anyhow::anyhow!("Composio direct transport error: {error}"))?;
        let status = response.status();
        if !status.is_success() {
            let _ = response.bytes().await;
            anyhow::bail!("Composio direct request failed with HTTP {status}");
        }
        let raw: serde_json::Value = decode_response(response, "direct").await?;
        Ok(decode_direct_response(raw))
    }

    async fn execute_proxied(
        &self,
        action: &str,
        arguments: serde_json::Value,
    ) -> anyhow::Result<ExecuteResponse> {
        let bearer = self
            .config
            .bearer_token
            .as_ref()
            .filter(|token| !token.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Composio proxy bearer token is not configured"))?;
        let url = format!(
            "{}/agent-integrations/composio/execute",
            self.config.base_url.trim_end_matches('/')
        );
        let response = self
            .http
            .post(url)
            .bearer_auth(bearer.expose())
            .json(&serde_json::json!({ "tool": action, "arguments": arguments }))
            .send()
            .await
            .map_err(|error| anyhow::anyhow!("Composio proxy transport error: {error}"))?;
        let status = response.status();
        if !status.is_success() {
            let _ = response.bytes().await;
            anyhow::bail!("Composio proxy request failed with HTTP {status}");
        }
        let raw: serde_json::Value = response
            .json()
            .await
            .map_err(|error| anyhow::anyhow!("Composio proxy response decode failed: {error}"))?;
        decode_proxy_response(raw)
    }
}

/// Shape a direct-API payload into an [`ExecuteResponse`].
///
/// A payload that reports an `error` is not a success, whatever the
/// `successful` flag says or omits: consumers gate document creation on the
/// flag, and an error body must never be stored as content.
fn decode_direct_response(raw: serde_json::Value) -> ExecuteResponse {
    let flagged = raw
        .get("successful")
        .and_then(serde_json::Value::as_bool)
        .or_else(|| raw.get("success").and_then(serde_json::Value::as_bool))
        .unwrap_or(true);
    let error = raw
        .get("error")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|error| !error.is_empty())
        .map(str::to_owned);
    let successful = flagged && error.is_none();
    let data = raw.get("data").cloned().unwrap_or(raw);
    ExecuteResponse {
        data,
        successful,
        error,
        cost_usd: 0.0,
        markdown_formatted: None,
        attempts: 1,
    }
}

fn decode_proxy_response(raw: serde_json::Value) -> anyhow::Result<ExecuteResponse> {
    let payload = if raw.get("successful").is_some() {
        raw
    } else {
        raw.get("data").cloned().unwrap_or(raw)
    };
    serde_json::from_value(payload)
        .map_err(|error| anyhow::anyhow!("Composio proxy response decode failed: {error}"))
}

fn retryable_provider_error(error: Option<&str>) -> bool {
    error.is_some_and(|error| {
        let lower = error.to_ascii_lowercase();
        lower.contains("ratelimit")
            || lower.contains("rate limit")
            || lower.contains("too many requests")
    })
}

/// Whether a failed execute is worth retrying with backoff.
///
/// Retryable: rate limiting and upstream unavailability (429/502/503/504),
/// and transport failures (connect/read errors, timeouts) — reported by the
/// request paths as `"… transport error: …"`. NOT retryable: any other HTTP
/// status. 400/401/403/404 are permanent — an invalid API key must fail once,
/// not storm three times — and used to be caught by a `"request failed"`
/// needle that both status-bail messages also matched.
fn retryable_transport_error(error: &anyhow::Error) -> bool {
    let message = error.to_string();
    [
        "HTTP 429",
        "HTTP 502",
        "HTTP 503",
        "HTTP 504",
        "transport error",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

async fn decode_response(
    response: reqwest::Response,
    mode: &str,
) -> anyhow::Result<serde_json::Value> {
    response
        .json()
        .await
        .map_err(|error| anyhow::anyhow!("Composio {mode} response decode failed: {error}"))
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
