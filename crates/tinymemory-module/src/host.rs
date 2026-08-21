//! Host-owned runtime services used by the compiled memory engine.

use std::sync::Arc;

use async_trait::async_trait;
use tinybus::Connection;
use tinymemory_api::host::{ErrorReporter, MemoryEvent, MemoryEventSink, SpacyResponse};

/// Host callback routing constants.
pub const RUNTIME_HOST_BUS_NAME: &str = "ai.tinyhumans.tinymemory.RuntimeHost";
/// Object path for the host callbacks.
pub const RUNTIME_HOST_OBJECT_PATH: &str = "/ai/tinyhumans/tinymemory/RuntimeHost";
/// Interface exported by the host.
pub const RUNTIME_HOST_INTERFACE: &str = "ai.tinyhumans.tinymemory.RuntimeHost";

#[derive(Clone)]
pub(crate) struct BusRuntimeHost {
    connection: Connection,
}

impl std::fmt::Debug for BusRuntimeHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BusRuntimeHost")
            .finish_non_exhaustive()
    }
}

impl BusRuntimeHost {
    pub(crate) fn new(connection: Connection) -> Self {
        Self { connection }
    }

    fn proxy(&self) -> Result<tinybus::Proxy, tinybus::Error> {
        self.connection.proxy(
            RUNTIME_HOST_BUS_NAME,
            RUNTIME_HOST_OBJECT_PATH,
            RUNTIME_HOST_INTERFACE,
        )
    }

    fn notify<T>(&self, method: &'static str, arguments: T)
    where
        T: serde::Serialize + Send + 'static,
    {
        let host = self.clone();
        tokio::spawn(async move {
            let result = match host.proxy() {
                Ok(proxy) => proxy.call::<()>(method, arguments).await,
                Err(error) => Err(error),
            };
            if let Err(error) = result {
                log::debug!("[tinymemory:module] host callback {method} failed: {error}");
            }
        });
    }
}

impl MemoryEventSink for BusRuntimeHost {
    fn publish(&self, event: MemoryEvent) {
        self.notify("PublishEvent", (event,));
    }
}

impl ErrorReporter for BusRuntimeHost {
    fn report_error(&self, rendered: &str, domain: &str, operation: &str, tags: &[(&str, &str)]) {
        self.notify(
            "ReportError",
            (
                false,
                rendered.to_string(),
                domain.to_string(),
                operation.to_string(),
                owned_tags(tags),
            ),
        );
    }

    fn report_error_or_expected(
        &self,
        rendered: &str,
        domain: &str,
        operation: &str,
        tags: &[(&str, &str)],
    ) {
        self.notify(
            "ReportError",
            (
                true,
                rendered.to_string(),
                domain.to_string(),
                operation.to_string(),
                owned_tags(tags),
            ),
        );
    }
}

fn owned_tags(tags: &[(&str, &str)]) -> Vec<(String, String)> {
    tags.iter()
        .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
        .collect()
}

#[async_trait]
impl tinymemory_core::nlp_host::NlpHost for BusRuntimeHost {
    async fn extract_spacy(
        &self,
        _config: &tinymemory_core::Config,
        text: &str,
    ) -> Result<SpacyResponse, String> {
        let proxy = self.proxy().map_err(|error| error.to_string())?;
        proxy
            .call("ExtractSpacy", (text.to_string(),))
            .await
            .map_err(|error| error.to_string())
    }
}

pub(crate) fn install(connection: Connection) {
    let host = Arc::new(BusRuntimeHost::new(connection));
    tinymemory_core::events::set_event_sink(Arc::clone(&host) as Arc<dyn MemoryEventSink>);
    tinymemory_core::observability::set_error_reporter(Arc::clone(&host) as Arc<dyn ErrorReporter>);
    tinymemory_core::nlp_host::set_nlp_host(host);
}

#[cfg(test)]
#[path = "host_test.rs"]
mod test;
