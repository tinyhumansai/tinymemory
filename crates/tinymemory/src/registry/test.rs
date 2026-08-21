//! Admission tests.
//!
//! These mirror the host-side binding tests that guarded this logic before it
//! moved into the crate, so a refusal that used to be caught in OpenHuman is
//! still caught here.

// A failing assertion in a test *is* a panic; the crate-wide `expect_used` /
// `panic` lints exist to keep the library from panicking, not the tests.
#![allow(clippy::expect_used)]

use super::*;

fn labels() -> ConfigLabels<'static> {
    ConfigLabels::default()
}

fn entry(class: Option<&'static str>, trust_state: &'static str) -> DriverEntry<'static> {
    DriverEntry { class, trust_state }
}

#[test]
fn the_embedded_default_admits_without_an_entry() {
    let admitted = DriverRegistry::builtin()
        .admit(TINYCORTEX_DRIVER_ID, None, labels())
        .expect("the embedded default admits");
    assert_eq!(admitted.id, TINYCORTEX_DRIVER_ID);
    assert_eq!(admitted.class, DriverClass::Embedded);
}

#[test]
fn the_null_placeholder_admits_without_an_entry() {
    let admitted = DriverRegistry::builtin()
        .admit(NULL_DRIVER_ID, None, labels())
        .expect("null admits");
    assert_eq!(admitted.class, DriverClass::Null);
}

#[test]
fn an_empty_driver_id_is_refused_and_names_the_config_section() {
    let refusal = DriverRegistry::builtin()
        .admit("   ", None, labels())
        .expect_err("an empty driver id is refused");
    assert_eq!(refusal.configured_driver, "");
    assert_eq!(refusal.reason, "[subsystems.memory] driver is empty");
}

#[test]
fn an_external_builtin_without_an_entry_is_refused_for_missing_configuration() {
    let refusal = DriverRegistry::builtin()
        .admit("supermemory", None, labels())
        .expect_err("an external id without an entry is refused");
    assert_eq!(refusal.configured_driver, "supermemory");
    assert!(
        refusal.reason.contains("external drivers require endpoint"),
        "reason should name the missing external configuration: {}",
        refusal.reason
    );
    assert!(
        refusal
            .reason
            .contains("no [subsystems.memory.drivers.<id>] entry"),
        "reason should name the missing block: {}",
        refusal.reason
    );
}

#[test]
fn an_unreserved_id_with_a_classless_entry_is_refused() {
    let refusal = DriverRegistry::empty()
        .admit("supermemory", Some(entry(None, TRUSTED)), labels())
        .expect_err("a classless entry cannot admit an arbitrary id");
    assert!(
        refusal.reason.contains("has no class line"),
        "reason should name the missing class line: {}",
        refusal.reason
    );
}

#[test]
fn an_unparseable_class_is_refused_with_the_raw_value() {
    let refusal = DriverRegistry::builtin()
        .admit(
            "supermemory",
            Some(entry(Some("emebdded"), TRUSTED)),
            labels(),
        )
        .expect_err("a misspelled class is refused");
    assert_eq!(refusal.reason, "unknown driver class: emebdded");
}

/// The rule that keeps a bound engine truthfully labelled: a reserved id's
/// class may be confirmed by an explicit line, never overridden by one.
#[test]
fn a_class_override_cannot_smuggle_the_engine_in_under_the_null_id() {
    let refusal = DriverRegistry::builtin()
        .admit(
            NULL_DRIVER_ID,
            Some(entry(Some("embedded"), TRUSTED)),
            labels(),
        )
        .expect_err("null is always class null");
    assert!(
        refusal
            .reason
            .contains("is built in and is always class \"null\""),
        "reason should state the fixed class: {}",
        refusal.reason
    );
    assert!(
        refusal.reason.contains("class = \"embedded\""),
        "reason should quote the conflicting line: {}",
        refusal.reason
    );
}

#[test]
fn a_class_override_cannot_relabel_the_engine_as_null() {
    let refusal = DriverRegistry::builtin()
        .admit(
            TINYCORTEX_DRIVER_ID,
            Some(entry(Some("null"), TRUSTED)),
            labels(),
        )
        .expect_err("the embedded default is always class embedded");
    assert!(
        refusal
            .reason
            .contains("is built in and is always class \"embedded\""),
        "reason should state the fixed class: {}",
        refusal.reason
    );
}

#[test]
fn a_confirming_class_line_is_accepted() {
    let admitted = DriverRegistry::builtin()
        .admit(
            TINYCORTEX_DRIVER_ID,
            Some(entry(Some("embedded"), TRUSTED)),
            labels(),
        )
        .expect("a class line confirming the fixed class is fine");
    assert_eq!(admitted.class, DriverClass::Embedded);
}

#[test]
fn an_untrusted_external_driver_is_refused_for_trust() {
    let refusal = DriverRegistry::builtin()
        .admit(
            "remote",
            Some(entry(Some("external"), "untrusted")),
            labels(),
        )
        .expect_err("an untrusted external driver is refused");
    assert!(
        refusal.reason.contains("external driver is untrusted"),
        "reason should be the trust refusal: {}",
        refusal.reason
    );
    assert!(
        refusal
            .reason
            .contains("under [subsystems.memory.drivers] to allow this binding"),
        "reason should name the block to edit: {}",
        refusal.reason
    );
}

#[test]
fn a_trusted_external_driver_is_admitted() {
    let admitted = DriverRegistry::builtin()
        .admit("remote", Some(entry(Some("external"), TRUSTED)), labels())
        .expect("the HTTP transport exists");
    assert_eq!(admitted.class, DriverClass::External);
}

#[test]
fn supported_external_ids_have_a_fixed_class() {
    let registry = DriverRegistry::builtin();
    for id in [SUPERMEMORY_DRIVER_ID, MEM0_DRIVER_ID, COGNEE_DRIVER_ID] {
        let admitted = registry
            .admit(id, Some(entry(Some("external"), TRUSTED)), labels())
            .expect("supported external driver admits");
        assert_eq!(admitted.class, DriverClass::External);
    }
}

#[test]
fn a_host_can_reserve_an_additional_driver_id() {
    let registry = DriverRegistry::builtin().with_reserved("custom-memory", DriverClass::Embedded);
    let admitted = registry
        .admit("custom-memory", None, labels())
        .expect("a host-reserved id admits implicitly");
    assert_eq!(admitted.class, DriverClass::Embedded);

    let refusal = registry
        .admit(
            "custom-memory",
            Some(entry(Some("null"), TRUSTED)),
            labels(),
        )
        .expect_err("the confirm-never-override rule applies to host-reserved ids too");
    assert!(refusal.reason.contains("is built in and is always class"));
}

#[test]
fn an_empty_registry_reserves_nothing() {
    let refusal = DriverRegistry::empty()
        .admit(TINYCORTEX_DRIVER_ID, None, labels())
        .expect_err("nothing is reserved");
    assert!(refusal.reason.contains("unknown driver id"));
}

/// A refusal is rendered to operators, so it must carry only the id and the
/// shape of the config — never anything that could hold a secret. The type
/// system does most of the work here ([`DriverEntry`] carries no endpoint and no
/// credential reference); this pins the remaining gap, which is the id itself.
#[test]
fn a_refusal_reason_carries_only_the_id_and_config_shape() {
    let refusal = DriverRegistry::builtin()
        .admit(
            "remote",
            Some(entry(Some("external"), "untrusted")),
            labels(),
        )
        .expect_err("refused");
    assert!(!refusal.reason.contains("http"), "no endpoint may appear");
    assert!(
        !refusal.reason.contains("token"),
        "no credential may appear"
    );
    assert!(
        !refusal.reason.contains("secret"),
        "no credential may appear"
    );
}

#[test]
fn driver_class_round_trips_through_its_config_form() {
    for class in DriverClass::ALL {
        assert_eq!(
            DriverClass::parse(class.as_str()).expect("round trip"),
            class
        );
        assert_eq!(class.to_string(), class.as_str());
    }
    assert_eq!(
        DriverClass::parse("nope").expect_err("rejected"),
        DriverClassParseError::Unknown { raw: "nope".into() }
    );
}

/// The serde form is the config form. A host reads these out of a TOML file, so
/// a rename here would silently invalidate deployed configuration.
#[test]
fn driver_class_serde_matches_the_config_spelling() {
    for class in DriverClass::ALL {
        let json = serde_json::to_string(&class).expect("serialize");
        assert_eq!(json, format!("\"{}\"", class.as_str()));
    }
}

// ── Selection from configuration (issue #18 §A5) ─────────────────────────────
//
// Before this, the registry could answer "is this driver id real and allowed"
// and nothing asked it: `admit` had no production caller, and the memory client
// factory constructed TinyCortex unconditionally. These pin the wiring.

use tinymemory_api::host::test_support::TestHostConfig;

fn config_naming(driver: Option<&str>) -> TestHostConfig {
    // `TestHostConfig` is `#[non_exhaustive]`, so it is built and then mutated
    // rather than named field-by-field — which is what its own docs ask for.
    let mut config = TestHostConfig::default();
    config.memory_driver = driver.map(str::to_owned);
    config
}

#[test]
fn a_configuration_naming_no_driver_gets_the_embedded_default() {
    // The property that keeps an unconfigured host booting: a reserved embedded
    // id is admitted without any `drivers` entry.
    let admission = DriverRegistry::builtin()
        .select(&config_naming(None), None, labels())
        .expect("an unconfigured host still binds");
    assert_eq!(admission.id, TINYCORTEX_DRIVER_ID);
    assert_eq!(admission.class, DriverClass::Embedded);
}

#[test]
fn a_configuration_naming_an_engine_selects_that_engine() {
    let admission = DriverRegistry::builtin()
        .select(&config_naming(Some(NULL_DRIVER_ID)), None, labels())
        .expect("the null driver is admitted without an entry");
    assert_eq!(admission.id, NULL_DRIVER_ID);
    assert_eq!(admission.class, DriverClass::Null);
}

#[test]
fn selecting_a_hosted_engine_still_requires_its_entry() {
    // Selection does not loosen admission: an external driver named in config
    // but left unconfigured is refused fail-closed, exactly as `admit` refuses
    // it directly.
    let reason = DriverRegistry::builtin()
        .select(&config_naming(Some("supermemory")), None, labels())
        .expect_err("an external driver with no entry must be refused");
    assert_eq!(reason.configured_driver, "supermemory");
}

#[test]
fn selecting_a_hosted_engine_succeeds_once_it_is_configured_and_trusted() {
    let entry = DriverEntry {
        class: None,
        trust_state: TRUSTED,
    };
    let admission = DriverRegistry::builtin()
        .select(&config_naming(Some("supermemory")), Some(entry), labels())
        .expect("a configured, trusted external driver is admitted");
    assert_eq!(admission.class, DriverClass::External);
}

#[test]
fn selection_reads_the_engine_field_and_not_the_model_routing_one() {
    // `memory_provider` is a `provider:model` routing string choosing which
    // language model does summarisation. Reading it here would let a model
    // change repoint a company's storage, which is why selection has its own
    // field.
    let mut config = TestHostConfig::default();
    config.memory_provider = Some("ollama:llama3".to_owned());
    config.memory_driver = None;
    let admission = DriverRegistry::builtin()
        .select(&config, None, labels())
        .expect("model routing must not affect engine selection");
    assert_eq!(admission.id, TINYCORTEX_DRIVER_ID);
}

#[test]
fn classless_entries_use_reserved_classes_and_still_enforce_external_trust() {
    let registry = DriverRegistry::builtin();
    for id in [TINYCORTEX_DRIVER_ID, NAMESPACE_DRIVER_ID, NULL_DRIVER_ID] {
        let admission = registry
            .admit(id, Some(entry(None, "untrusted")), labels())
            .expect("non-external reserved class admits without a class line");
        assert_eq!(admission.id, id);
        assert_eq!(
            admission.class,
            registry.reserved_class(id).expect("reserved class")
        );
    }

    for id in [SUPERMEMORY_DRIVER_ID, MEM0_DRIVER_ID, COGNEE_DRIVER_ID] {
        let refusal = registry
            .admit(id, Some(entry(None, "untrusted")), labels())
            .expect_err("external reserved ids remain fail-closed");
        assert!(refusal.reason.contains("external driver is untrusted"));

        let admission = registry
            .admit(id, Some(entry(None, TRUSTED)), labels())
            .expect("trusted external reserved id admits implicitly");
        assert_eq!(admission.class, DriverClass::External);
    }
}

#[test]
fn explicit_classes_admit_unreserved_ids_without_guessing() {
    let registry = DriverRegistry::empty();
    for (raw, expected) in [
        ("null", DriverClass::Null),
        ("embedded", DriverClass::Embedded),
        ("external", DriverClass::External),
    ] {
        let admission = registry
            .admit("custom", Some(entry(Some(raw), TRUSTED)), labels())
            .expect("explicit class admits an unreserved id");
        assert_eq!(admission.id, "custom");
        assert_eq!(admission.class, expected);
    }
}

#[test]
fn reservation_is_first_writer_wins_and_driver_ids_are_trimmed() {
    let registry = DriverRegistry::empty()
        .with_reserved("custom", DriverClass::Embedded)
        .with_reserved("custom", DriverClass::Null);
    assert_eq!(
        registry.reserved_class("custom"),
        Some(DriverClass::Embedded)
    );
    let admission = registry
        .admit("  custom  ", None, labels())
        .expect("trimmed reserved id admits");
    assert_eq!(admission.id, "custom");
    assert_eq!(admission.class, DriverClass::Embedded);
}

#[test]
fn custom_labels_and_display_make_refusals_actionable() {
    let labels = ConfigLabels {
        section: "[memory]",
        drivers: "[memory.backends]",
        driver_entry: "[memory.backends.<id>]",
    };
    let refusal = DriverRegistry::empty()
        .admit("missing", None, labels)
        .expect_err("unknown id is refused");
    assert!(refusal.reason.contains("no [memory.backends.<id>] entry"));
    assert_eq!(
        refusal.to_string(),
        format!("driver 'missing' refused: {}", refusal.reason)
    );
    let as_error: &dyn std::error::Error = &refusal;
    assert!(as_error.source().is_none());
}
