//! Driver admission: which ids exist, what class each binds as, and what is
//! refused.
//!
//! Exercises only the public surface of the `tinymemory` facade.
//!
//! # Selection
//!
//! Configuration now chooses the engine (§A5). One correction to the issue is
//! worth recording here, because it would otherwise have wired the wrong thing:
//! §A5 names `MemoryHostConfig::memory_provider()` as the selector, but that
//! method is a `provider:model` routing string for the memory *workload* —
//! which language model summarises — not the engine the memory lives in.
//! Selection reads `memory_driver()` instead, added for the purpose.
//!
//! What still does not exist is the last clause of §A5, that `create_memory_*`
//! return a bound `Arc<dyn MemoryProvider>`. It cannot, and the reason is
//! structural rather than unfinished: `crates/tinymemory-tinycortex` depends on
//! `tinymemory-core` since §C3, so a core factory returning a constructed
//! adapter provider would be a dependency cycle. Selection resolves the
//! *decision*; the host constructs — which is what `src/registry`'s own module
//! docs have always said.

// A failing assertion in a test *is* a panic; the crate-wide `expect_used` /
// `unwrap_used` / `panic` lints exist to keep the library from panicking, not
// the tests. Same allowance, and same reasoning, as `src/registry/test.rs`.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use tinymemory::api::host::test_support::TestHostConfig;
use tinymemory::registry::{
    ConfigLabels, DriverClass, DriverEntry, DriverRegistry, COGNEE_DRIVER_ID, CORTEX_DRIVER_ID,
    MEM0_DRIVER_ID, SUPERMEMORY_DRIVER_ID, TINYCORTEX_DRIVER_ID, TRUSTED,
};

fn labels() -> ConfigLabels<'static> {
    ConfigLabels {
        section: "[memory]",
        drivers: "[memory.drivers]",
        driver_entry: "[memory.drivers.<id>]",
    }
}

fn trusted_external() -> DriverEntry<'static> {
    DriverEntry {
        class: None,
        trust_state: TRUSTED,
    }
}

#[test]
fn a_reserved_embedded_id_is_admitted_without_any_config_entry() {
    // The embedded default's options live in the host's own config blocks, so
    // it must not require a `drivers` entry to be selectable at all.
    let admission = DriverRegistry::builtin()
        .admit(TINYCORTEX_DRIVER_ID, None, labels())
        .expect("the built-in embedded engine is admitted");
    assert_eq!(admission.id, TINYCORTEX_DRIVER_ID);
    assert_eq!(admission.class, DriverClass::Embedded);
}

#[test]
fn the_null_driver_is_admitted_and_is_class_null() {
    let admission = DriverRegistry::builtin()
        .admit(tinymemory::registry::NULL_DRIVER_ID, None, labels())
        .expect("the null driver is admitted");
    assert_eq!(admission.class, DriverClass::Null);
}

#[test]
fn every_reserved_external_id_resolves_to_the_external_class() {
    let registry = DriverRegistry::builtin();
    for id in [
        SUPERMEMORY_DRIVER_ID,
        MEM0_DRIVER_ID,
        COGNEE_DRIVER_ID,
        CORTEX_DRIVER_ID,
    ] {
        let admission = registry
            .admit(id, Some(trusted_external()), labels())
            .unwrap_or_else(|reason| panic!("{id} was refused: {}", reason.reason));
        assert_eq!(admission.class, DriverClass::External, "{id}");
        assert_eq!(admission.id, id);
    }
}

#[test]
fn an_external_driver_without_an_entry_is_refused_fail_closed() {
    // The fail-closed half: an external engine needs endpoint, credential and
    // trust configuration, so admitting it implicitly would bind an
    // out-of-process backend nobody configured.
    let reason = DriverRegistry::builtin()
        .admit(SUPERMEMORY_DRIVER_ID, None, labels())
        .expect_err("an external driver with no entry must be refused");
    assert_eq!(reason.configured_driver, SUPERMEMORY_DRIVER_ID);
    assert!(
        reason.reason.contains("external"),
        "the refusal should say why: {}",
        reason.reason
    );
}

#[test]
fn an_untrusted_external_driver_is_refused_even_with_an_entry() {
    let entry = DriverEntry {
        class: None,
        trust_state: "untrusted",
    };
    let reason = DriverRegistry::builtin()
        .admit(SUPERMEMORY_DRIVER_ID, Some(entry), labels())
        .expect_err("trust must be raised explicitly before an external bind");
    assert!(
        reason.reason.contains(TRUSTED),
        "the refusal should name the value to set: {}",
        reason.reason
    );
}

#[test]
fn a_reserved_id_cannot_have_its_class_overridden_by_config() {
    // A reserved id names a fixed implementation. An explicit `class` line may
    // confirm it but never override it — otherwise config could run the
    // embedded engine under the checks meant for an external one.
    let entry = DriverEntry {
        class: Some("external"),
        trust_state: TRUSTED,
    };
    let reason = DriverRegistry::builtin()
        .admit(TINYCORTEX_DRIVER_ID, Some(entry), labels())
        .expect_err("a reserved id's class must not be overridable");
    assert!(
        reason.reason.contains("built in"),
        "the refusal should explain why: {}",
        reason.reason
    );
}

#[test]
fn an_unknown_driver_id_is_refused_rather_than_defaulted() {
    let reason = DriverRegistry::builtin()
        .admit("not-an-engine", None, labels())
        .expect_err("an unreserved id with no entry must be refused");
    assert_eq!(reason.configured_driver, "not-an-engine");
}

#[test]
fn an_empty_driver_id_is_refused() {
    let reason = DriverRegistry::builtin()
        .admit("   ", None, labels())
        .expect_err("a blank driver id must be refused");
    assert!(
        reason.reason.contains("empty"),
        "the refusal should name the problem: {}",
        reason.reason
    );
}

#[test]
fn a_config_class_typo_is_echoed_back_to_the_operator() {
    // The offending value comes from the host's own config file, not from a
    // driver or the network, so echoing it discloses nothing the reader did not
    // write — and without it the message cannot point at the line to fix.
    let entry = DriverEntry {
        class: Some("embeded"),
        trust_state: TRUSTED,
    };
    let reason = DriverRegistry::builtin()
        .admit("some-driver", Some(entry), labels())
        .expect_err("an unparseable class must be refused");
    assert!(
        reason.reason.contains("embeded"),
        "the refusal should quote the typo: {}",
        reason.reason
    );
}

// ── The selection half, through the public facade ────────────────────────────

fn config_naming(driver: Option<&str>) -> TestHostConfig {
    let mut config = TestHostConfig::default();
    config.memory_driver = driver.map(str::to_owned);
    config
}

#[test]
fn configuration_chooses_the_engine_and_admission_gates_it() {
    let admission = DriverRegistry::builtin()
        .select(
            &config_naming(Some(COGNEE_DRIVER_ID)),
            Some(trusted_external()),
            labels(),
        )
        .expect("a configured, trusted external engine binds");
    assert_eq!(admission.id, COGNEE_DRIVER_ID);
    assert_eq!(admission.class, DriverClass::External);
}

#[test]
fn an_unconfigured_host_still_binds_the_embedded_default() {
    // The property that matters most operationally: adding engine selection
    // must not turn "I configured nothing" into a host that fails to start.
    let admission = DriverRegistry::builtin()
        .select(&config_naming(None), None, labels())
        .expect("an unconfigured host binds the embedded default");
    assert_eq!(admission.id, TINYCORTEX_DRIVER_ID);
    assert_eq!(admission.class, DriverClass::Embedded);
}

#[test]
fn selection_does_not_loosen_the_fail_closed_external_gate() {
    // Going through `select` rather than `admit` must not become a way around
    // the trust requirement.
    let untrusted = DriverEntry {
        class: None,
        trust_state: "untrusted",
    };
    let reason = DriverRegistry::builtin()
        .select(
            &config_naming(Some(MEM0_DRIVER_ID)),
            Some(untrusted),
            labels(),
        )
        .expect_err("an untrusted external engine is refused however it was chosen");
    assert!(reason.reason.contains(TRUSTED), "{}", reason.reason);
}

#[test]
fn the_model_routing_field_cannot_repoint_the_store() {
    // `memory_provider` chooses a language model; `memory_driver` chooses the
    // store. Conflating them would let a model change move a company's memory.
    let mut config = TestHostConfig::default();
    config.memory_provider = Some("ollama:llama3".to_owned());
    let admission = DriverRegistry::builtin()
        .select(&config, None, labels())
        .expect("model routing leaves engine selection alone");
    assert_eq!(admission.id, TINYCORTEX_DRIVER_ID);
}
