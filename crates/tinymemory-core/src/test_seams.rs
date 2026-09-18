//! One-shot installation of stub host seams for this crate's own tests.
//!
//! The seams fail loudly when unwired — see [`crate::embedding_host`] for why
//! that is deliberate — so a test that reaches any of them needs a host
//! installed. These stubs are the smallest thing that makes the *core's*
//! behaviour observable: a noop embedder, known cloud defaults.
//!
//! # The chat stub answers availability, not routing
//!
//! [`TestChatHost`] reports that a summariser *is* available, so the doctor's
//! aggregation — which is core logic — is reachable. It refuses to build a
//! model. Which provider answers a role and what model id that resolves to is
//! host routing policy that a stub could only assert against itself; those
//! tests moved to the host, where the real implementation is.

use std::sync::{Arc, Once};

use async_trait::async_trait;

use crate::config_loader::ConfigLoader;
use crate::Config;

/// A [`ConfigLoader`] that hands back a default test config.
///
/// The background loops reload config on every tick by design; without a loader
/// they fail with the unwired-seam error before reaching the behaviour under
/// test.
#[derive(Debug)]
struct TestConfigLoader;

#[async_trait]
impl ConfigLoader for TestConfigLoader {
    async fn load(&self) -> Result<Box<Config>, String> {
        Ok(Box::new(
            tinymemory_api::host::test_support::TestHostConfig::default(),
        ))
    }

    async fn reload_snapshot(&self, _snapshot: &Config) -> Result<Arc<Config>, String> {
        Ok(Arc::new(
            tinymemory_api::host::test_support::TestHostConfig::default(),
        ))
    }
}

static INIT: Once = Once::new();

/// Install the stub seams. Idempotent; safe to call from every test.
pub(crate) fn init() {
    INIT.call_once(|| {
        crate::embedding_host::TestEmbeddingHost::install();
        crate::config_loader::set_config_loader(Arc::new(TestConfigLoader));
        crate::chat_host::set_chat_host(Arc::new(TestChatHost));
    });
}

/// A [`ChatHost`] that reports an available summariser and nothing else.
///
/// The doctor aggregates summariser availability into its report, and that
/// aggregation is core behaviour worth testing. Actually *building* a model is
/// host routing, so this refuses — no test in this crate should need one.
#[derive(Debug)]
struct TestChatHost;

impl crate::chat_host::ChatHost for TestChatHost {
    fn provider_for_role(&self, _role: &str, _config: &Config) -> String {
        "test".to_string()
    }

    fn create_chat_model_with_model_id(
        &self,
        _role: &str,
        _config: &Config,
        _temperature: f64,
    ) -> Result<(Arc<dyn tinyinference_llm::model::ChatModel<()>>, String), String> {
        Err("TestChatHost does not build models — model routing is host behaviour".to_string())
    }

    fn usage_from_response(
        &self,
        _response: &tinyinference_llm::model::ModelResponse,
    ) -> Option<tinymemory_api::host::UsageInfo> {
        None
    }

    fn summarizer_available(&self, _config: &Config) -> (bool, &'static str) {
        (true, "test chat host reports a summariser")
    }
}
