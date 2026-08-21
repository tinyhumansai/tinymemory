//! Tests for the surrounding module.

use super::*;

#[test]
fn subsystems_config_defaults_reproduce_today_behavior() {
    let cfg = SubsystemsConfig::default();
    assert_eq!(cfg.memory.driver, "tinycortex");
    assert!(cfg.memory.hooks.auto_recall);
    assert!(cfg.memory.hooks.auto_capture);
    assert_eq!(cfg.memory.hooks.max_context_tokens, 2000);
    assert_eq!(cfg.memory.hooks.recall_max_chars, 1000);
    assert_eq!(cfg.memory.hooks.capture_max_chars, 500);
    assert!(cfg.memory.drivers.is_empty());
}

#[test]
fn absent_subsystems_block_deserializes_to_default() {
    let cfg: SubsystemsConfig = toml::from_str("").expect("empty toml parses");
    assert_eq!(
        serde_json::to_value(&cfg).unwrap(),
        serde_json::to_value(SubsystemsConfig::default()).unwrap()
    );
}

#[test]
fn memory_driver_config_debug_never_leaks_credential_ref() {
    let driver = MemoryDriverConfig {
        class: Some("external".into()),
        transport: Some("http".into()),
        endpoint: Some("https://api.supermemory.ai".into()),
        credential_ref: Some("keychain:supermemory-super-secret-value".into()),
        trust_state: "untrusted".into(),
    };
    let debug_output = format!("{driver:?}");
    assert!(
        !debug_output.contains("keychain:supermemory-super-secret-value"),
        "Debug output must never contain the credential_ref value: {debug_output}"
    );
    assert!(
        debug_output.contains("<redacted>"),
        "Debug output should show a redaction marker: {debug_output}"
    );
}

#[test]
fn memory_driver_config_default_trust_state_is_untrusted() {
    assert_eq!(MemoryDriverConfig::default().trust_state, "untrusted");
}
