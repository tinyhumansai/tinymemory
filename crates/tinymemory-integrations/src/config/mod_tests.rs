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
        tenancy = "org:acme/user:alice"

        [engines.tinyhumans]
        "#,
    )
    .unwrap();
    assert_eq!(
        config.settings().endpoint.as_deref(),
        Some("https://cortex.example.test")
    );
    assert_eq!(
        config.settings().tenancy,
        Some(crate::cortex::CortexTenancy::pinned("org:acme/user:alice").unwrap())
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

#[test]
fn a_malformed_tenancy_is_refused_when_reading() {
    for tenancy in ["shared", "user:..", "app:tinymemory", ""] {
        let read = toml::from_str::<MemoryConfig>(&format!(
            "engine = \"cortexdb\"\n[engines.cortexdb]\ntenancy = \"{tenancy}\"\n"
        ));
        assert!(read.is_err(), "{tenancy}");
    }
}

#[test]
fn a_cortexdb_config_without_a_tenancy_does_not_build() {
    let config: MemoryConfig = toml::from_str("engine = \"cortexdb\"").unwrap();
    assert!(matches!(
        config.build(EngineCredential::Static("ctx_key".into())),
        Err(tinymemory_api::Error::Config(_))
    ));
}
