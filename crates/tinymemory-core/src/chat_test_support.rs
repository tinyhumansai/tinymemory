#![cfg(any(test, feature = "test-support"))]
//! Task-local deterministic chat provider support for tests and test hosts.

use super::*;

pub struct StaticChatProvider {
    pub response: String,
    pub calls: std::sync::atomic::AtomicUsize,
}

impl StaticChatProvider {
    pub fn new(response: impl Into<String>) -> Self {
        Self {
            response: response.into(),
            calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl ChatProvider for StaticChatProvider {
    fn name(&self) -> &str {
        "test:static"
    }
    async fn chat_for_json(&self, _prompt: &ChatPrompt) -> Result<String> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(self.response.clone())
    }
}

pub mod test_override {
    use super::ChatProvider;
    use std::sync::Arc;

    tokio::task_local! { static OVERRIDE: Arc<dyn ChatProvider>; }

    pub fn current() -> Option<Arc<dyn ChatProvider>> {
        OVERRIDE.try_with(Arc::clone).ok()
    }

    pub async fn with_provider<F, T>(provider: Arc<dyn ChatProvider>, future: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        OVERRIDE.scope(provider, future).await
    }
}

pub(super) fn current_runtime() -> Option<(Arc<dyn ChatProvider>, String)> {
    test_override::current().map(|provider| (provider, "test:override".to_string()))
}
