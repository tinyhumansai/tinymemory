//! Tests for the process-global host integration seams.

// This is deliberately an integration-test binary. Its process globals are
// isolated from the library unit-test binary, where `test_seams::init` installs
// long-lived stubs behind a `Once`.

mod scheduler_gate {
    pub use tinymemory_core::scheduler_gate::*;
}

mod config_loader {
    pub use tinymemory_core::config_loader::*;
}

mod shutdown {
    pub use tinymemory_core::shutdown::*;
}

mod nlp_host {
    pub use tinymemory_core::nlp_host::*;
}

mod chat_host {
    pub use tinymemory_core::chat_host::*;
}

mod composio_host {
    pub use tinymemory_core::composio_host::*;
}

type Config = tinymemory_core::Config;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use tinymemory_api::host::test_support::TestHostConfig;
use tokio::sync::{Mutex, MutexGuard, Notify};

use crate::scheduler_gate::{Policy, SchedulerGate};

static SEAM_LOCK: Mutex<()> = Mutex::const_new(());

async fn seam_guard() -> MutexGuard<'static, ()> {
    SEAM_LOCK.lock().await
}

struct Restore(Option<Box<dyn FnOnce()>>);

impl Restore {
    fn new(restore: impl FnOnce() + 'static) -> Self {
        Self(Some(Box::new(restore)))
    }
}

impl Drop for Restore {
    fn drop(&mut self) {
        if let Some(restore) = self.0.take() {
            restore();
        }
    }
}

#[derive(Debug)]
struct TestGate {
    notify: Arc<Notify>,
    waited: Arc<AtomicBool>,
}

#[async_trait]
impl SchedulerGate for TestGate {
    fn current_policy(&self) -> Policy {
        Policy::Paused {
            reason: crate::scheduler_gate::PauseReason::UserDisabled,
        }
    }

    fn resume_notify(&self) -> Arc<Notify> {
        Arc::clone(&self.notify)
    }

    async fn wait_for_capacity(&self) -> Option<Box<dyn Send>> {
        self.waited.store(true, Ordering::SeqCst);
        Some(Box::new(()))
    }
}

#[tokio::test]
async fn scheduler_gate_delegates_and_clear_restores_ungated_defaults() {
    let _guard = seam_guard().await;
    let previous = crate::scheduler_gate::scheduler_gate();
    let _restore = Restore::new(move || match previous {
        Some(gate) => crate::scheduler_gate::set_scheduler_gate(gate),
        None => crate::scheduler_gate::clear_scheduler_gate(),
    });
    crate::scheduler_gate::clear_scheduler_gate();

    assert_eq!(crate::scheduler_gate::current_policy(), Policy::Normal);
    assert!(crate::scheduler_gate::wait_for_capacity().await.is_none());
    let idle = crate::scheduler_gate::resume_notify();
    assert!(Arc::ptr_eq(&idle, &crate::scheduler_gate::resume_notify()));

    let notify = Arc::new(Notify::new());
    let waited = Arc::new(AtomicBool::new(false));
    crate::scheduler_gate::set_scheduler_gate(Arc::new(TestGate {
        notify: Arc::clone(&notify),
        waited: Arc::clone(&waited),
    }));

    assert!(matches!(
        crate::scheduler_gate::current_policy(),
        Policy::Paused { .. }
    ));
    assert!(Arc::ptr_eq(
        &notify,
        &crate::scheduler_gate::resume_notify()
    ));
    assert!(crate::scheduler_gate::wait_for_capacity().await.is_some());
    assert!(waited.load(Ordering::SeqCst));
}

#[derive(Debug)]
struct TestLoader;

#[async_trait]
impl crate::config_loader::ConfigLoader for TestLoader {
    async fn load(&self) -> Result<Box<Config>, String> {
        let mut config = TestHostConfig::default();
        config.output_language = Some("fr".to_string());
        Ok(Box::new(config))
    }

    async fn reload_snapshot(&self, _snapshot: &Config) -> Result<Arc<Config>, String> {
        let mut config = TestHostConfig::default();
        config.output_language = Some("de".to_string());
        Ok(Arc::new(config))
    }
}

#[tokio::test]
async fn config_loader_reports_unwired_and_delegates_both_load_paths() {
    let _guard = seam_guard().await;
    let previous = crate::config_loader::config_loader();
    let _restore = Restore::new(move || match previous {
        Some(loader) => crate::config_loader::set_config_loader(loader),
        None => crate::config_loader::clear_config_loader(),
    });
    crate::config_loader::clear_config_loader();

    let error = crate::config_loader::load_config_with_timeout()
        .await
        .expect_err("an unwired loader must fail loudly");
    assert!(error.contains("no ConfigLoader installed"));

    crate::config_loader::set_config_loader(Arc::new(TestLoader));
    let loaded = crate::config_loader::load_config_arc()
        .await
        .expect("test loader should load");
    assert_eq!(loaded.output_language(), Some("fr"));
    let reloaded = crate::config_loader::reload_config_snapshot_with_timeout(loaded.as_ref())
        .await
        .expect("test loader should reload");
    assert_eq!(reloaded.output_language(), Some("de"));
}

#[derive(Default)]
struct TestShutdownHost {
    hooks: parking_lot::Mutex<Vec<crate::shutdown::ShutdownHook>>,
}

impl std::fmt::Debug for TestShutdownHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TestShutdownHost")
            .field("hook_count", &self.hooks.lock().len())
            .finish()
    }
}

impl crate::shutdown::ShutdownHost for TestShutdownHost {
    fn register(&self, hook: crate::shutdown::ShutdownHook) {
        self.hooks.lock().push(hook);
    }
}

#[tokio::test]
async fn shutdown_host_keeps_repeatable_hooks_and_unwired_registration_is_safe() {
    let _guard = seam_guard().await;
    let previous = crate::shutdown::shutdown_host();
    let _restore = Restore::new(move || match previous {
        Some(host) => crate::shutdown::set_shutdown_host(host),
        None => crate::shutdown::clear_shutdown_host(),
    });
    crate::shutdown::clear_shutdown_host();
    crate::shutdown::register(|| async {});

    let host = Arc::new(TestShutdownHost::default());
    crate::shutdown::set_shutdown_host(Arc::clone(&host) as Arc<dyn crate::shutdown::ShutdownHost>);
    let calls = Arc::new(AtomicUsize::new(0));
    crate::shutdown::register({
        let calls = Arc::clone(&calls);
        move || {
            let calls = Arc::clone(&calls);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
            }
        }
    });

    let (first_call, second_call) = {
        let hooks = host.hooks.lock();
        assert_eq!(hooks.len(), 1);
        ((hooks[0])(), (hooks[0])())
    };
    first_call.await;
    second_call.await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[derive(Debug)]
struct TestNlpHost;

#[async_trait]
impl crate::nlp_host::NlpHost for TestNlpHost {
    async fn extract_spacy(
        &self,
        _config: &Config,
        text: &str,
    ) -> Result<crate::nlp_host::SpacyResponse, String> {
        Ok(crate::nlp_host::SpacyResponse {
            entities: vec![crate::nlp_host::SpacyEntity {
                text: text.to_string(),
                label: "ORG".to_string(),
                start: 0,
                end: text.len() as u32,
            }],
            nouns: Vec::new(),
        })
    }
}

#[tokio::test]
async fn nlp_host_reports_unwired_then_returns_host_response() {
    let _guard = seam_guard().await;
    let previous = crate::nlp_host::nlp_host();
    let _restore = Restore::new(move || match previous {
        Some(host) => crate::nlp_host::set_nlp_host(host),
        None => crate::nlp_host::clear_nlp_host(),
    });
    crate::nlp_host::clear_nlp_host();
    let config = TestHostConfig::default();
    let error = crate::nlp_host::extract_spacy(&config, "TinyMemory")
        .await
        .expect_err("an unwired NLP host must request fallback");
    assert_eq!(error, "no NlpHost installed");

    crate::nlp_host::set_nlp_host(Arc::new(TestNlpHost));
    let response = crate::nlp_host::extract_spacy(&config, "TinyMemory")
        .await
        .expect("test NLP host should answer");
    assert_eq!(response.entities[0].text, "TinyMemory");
}

#[tokio::test]
async fn required_host_seams_fail_loudly_when_unwired() {
    let _guard = seam_guard().await;
    let chat = crate::chat_host::chat_host();
    let composio = crate::composio_host::composio_host();
    let _chat_restore = Restore::new(move || match chat {
        Some(host) => crate::chat_host::set_chat_host(host),
        None => crate::chat_host::clear_chat_host(),
    });
    let _composio_restore = Restore::new(move || match composio {
        Some(host) => crate::composio_host::set_composio_host(host),
        None => crate::composio_host::clear_composio_host(),
    });
    crate::chat_host::clear_chat_host();
    crate::composio_host::clear_composio_host();

    assert!(crate::chat_host::require_chat_host()
        .expect_err("chat host must be required")
        .contains("no ChatHost installed"));
    assert!(crate::composio_host::require_composio_host()
        .expect_err("Composio host must be required")
        .contains("no ComposioHost installed"));
    let config = TestHostConfig::default();
    assert_eq!(
        crate::chat_host::provider_for_role("memory", &config),
        "unknown"
    );
    assert_eq!(
        crate::chat_host::summarizer_available(&config),
        (false, "no chat host installed — summarisation cannot run")
    );
    assert!(!crate::composio_host::is_available(&config));
    assert_eq!(crate::composio_host::api_key(&config), None);
}
