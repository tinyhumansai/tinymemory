//! Config defaults and the TOML form a host stores.

use super::*;

#[test]
fn the_default_selects_tinyhumans_with_default_settings() {
    let config = MemoryConfig::default();
    assert_eq!(config.engine, "tinyhumans");
    assert_eq!(config.settings(), EngineSettings::default());
}

#[test]
fn the_selected_engine_settings_are_read_from_toml() {
    let config: MemoryConfig = toml::from_str(
        r#"
        engine = "cortexdb"

        [engines.cortexdb]
        endpoint = "https://cortex.example.test"

        [engines.tinyhumans]
        "#,
    )
    .unwrap();
    assert_eq!(
        config.settings().endpoint.as_deref(),
        Some("https://cortex.example.test")
    );
    let back: MemoryConfig = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
    assert_eq!(back, config);
}

#[test]
fn building_from_config_applies_the_registry_rules() {
    let config = MemoryConfig {
        engine: "nope".to_string(),
        ..MemoryConfig::default()
    };
    assert!(matches!(
        config.build(EngineCredential::None),
        Err(tinymemory_api::Error::Config(_))
    ));
}
