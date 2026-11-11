//! Tests for credential redaction and resolution.

use super::*;

#[test]
fn debug_never_shows_a_token() {
    let bearer = StaticBearer::new("tiny_live_secret");
    assert!(!format!("{bearer:?}").contains("secret"));
    let credential = CortexCredential::api_key("ctx_secret");
    assert!(!format!("{credential:?}").contains("secret"));
    let dynamic = CortexCredential::from(Arc::new(bearer) as Arc<dyn BearerSource>);
    assert!(!format!("{dynamic:?}").contains("secret"));
}

#[tokio::test]
async fn both_kinds_resolve_to_their_token() {
    assert_eq!(CortexCredential::api_key("k").resolve().await.unwrap(), "k");
    let dynamic = CortexCredential::Dynamic(Arc::new(StaticBearer::new("t")));
    assert_eq!(dynamic.resolve().await.unwrap(), "t");
}

#[tokio::test]
async fn callback_bearer_source_resolves_a_token() {
    let source = crate::cortex::CallbackBearerSource::new(|| async {
        Ok::<String, crate::cortex::Error>("host-session-token".to_owned())
    });

    assert_eq!(format!("{source:?}"), "CallbackBearerSource(<callback>)");
    assert!(!format!("{source:?}").contains("host-session-token"));
    assert_eq!(source.bearer().await.unwrap(), "host-session-token");
}

#[tokio::test]
async fn callback_bearer_source_resolves_each_token_dynamically() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let calls = Arc::new(AtomicUsize::new(0));
    let source = crate::cortex::CallbackBearerSource::new({
        let calls = calls.clone();
        move || {
            let call = calls.fetch_add(1, Ordering::SeqCst);
            async move { Ok::<String, crate::cortex::Error>(format!("host-session-{call}")) }
        }
    });

    assert_eq!(source.bearer().await.unwrap(), "host-session-0");
    assert_eq!(source.bearer().await.unwrap(), "host-session-1");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn callback_bearer_source_propagates_credential_errors() {
    let source = crate::cortex::CallbackBearerSource::new(|| async {
        Err(crate::cortex::Error::Unauthorized("signed out".into()))
    });

    assert!(matches!(
        source.bearer().await,
        Err(crate::cortex::Error::Unauthorized(message)) if message == "signed out"
    ));
}
