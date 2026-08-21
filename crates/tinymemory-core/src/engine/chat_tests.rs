//! Tests for the surrounding module.

use super::*;

/// Echoes the fields it received back as a JSON body so the test can assert
/// the crate->host prompt conversion (incl. the f32->f64 temperature).
struct EchoHostProvider;

#[async_trait]
impl HostChatProvider for EchoHostProvider {
    fn name(&self) -> &str {
        "echo"
    }
    async fn chat_for_json(&self, prompt: &HostChatPrompt) -> anyhow::Result<String> {
        Ok(format!(
            "system={};user={};temp={};kind={};max={:?}",
            prompt.system, prompt.user, prompt.temperature, prompt.kind, prompt.max_tokens
        ))
    }
}

#[tokio::test]
async fn converts_prompt_and_delegates_to_host_provider() {
    let seam = SeamChatProvider::new(Arc::new(EchoHostProvider));
    assert_eq!(CortexChatProvider::name(&seam), "echo");

    let prompt = CortexChatPrompt {
        system: "sys".to_string(),
        user: "usr".to_string(),
        temperature: 0.5,
        kind: "extract",
        max_tokens: Some(64),
    };
    let out = seam.chat_for_json(&prompt).await.unwrap();
    // Every field maps 1:1; temperature widens f32 0.5 -> f64 0.5.
    assert_eq!(
        out,
        "system=sys;user=usr;temp=0.5;kind=extract;max=Some(64)"
    );
}
