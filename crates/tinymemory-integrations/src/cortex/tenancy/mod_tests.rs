//! Tests for tenancy declarations, tenant pins and the scope root.

use super::*;

fn pinned(pin: &str) -> ScopeRoot {
    ScopeRoot::direct(&CortexTenancy::pinned(pin).unwrap())
}

#[test]
fn a_pin_is_cortexdb_scope_segments() {
    for pin in [
        "user:alice",
        "org:acme/user:alice",
        "org:acme/dept:eng/team:platform",
        "org:acme/ws:q3-launch",
        "user:u-6512f0c4.v2_x",
    ] {
        assert_eq!(TenantScope::new(pin).unwrap().as_str(), pin);
    }
    assert_eq!(
        TenantScope::new("  user:alice ").unwrap().as_str(),
        "user:alice",
        "surrounding space is not part of the path"
    );
}

#[test]
fn a_pin_that_could_name_another_scope_is_refused() {
    for pin in [
        "",
        "alice",
        "user:",
        ":alice",
        "user:alice/",
        "/user:alice",
        "user:alice//org:acme",
        "user:..",
        "user:.",
        "user:a/b",
        "user:al ice",
        "user:alice%2Fbob",
        "User:alice",
        "1org:acme",
        "app:tinymemory",
        "org:acme/app:tinymemory",
        "org:a/dept:b/team:c/user:d/agent:e",
    ] {
        let error = TenantScope::new(pin).unwrap_err();
        assert!(matches!(error, Error::Config(_)), "{pin}: {error:?}");
    }
    assert!(TenantScope::new(&format!("user:{}", "a".repeat(65))).is_err());
    assert!(TenantScope::new(&format!("user:{}", "a".repeat(64))).is_ok());
}

#[test]
fn tenancy_reads_and_writes_as_one_string() {
    assert_eq!(
        "single_user".parse::<CortexTenancy>().unwrap(),
        CortexTenancy::SingleUser
    );
    let pin: CortexTenancy = "org:acme/user:alice".parse().unwrap();
    assert_eq!(pin, CortexTenancy::pinned("org:acme/user:alice").unwrap());
    assert_eq!(pin.to_string(), "org:acme/user:alice");
    let json = serde_json::to_string(&pin).unwrap();
    assert_eq!(json, "\"org:acme/user:alice\"");
    assert_eq!(serde_json::from_str::<CortexTenancy>(&json).unwrap(), pin);
    assert!(serde_json::from_str::<CortexTenancy>("\"shared\"").is_err());
    assert!(serde_json::from_str::<CortexTenancy>("\"\"").is_err());
}

#[test]
fn a_direct_root_is_the_pin_then_the_tinymemory_root() {
    assert_eq!(
        ScopeRoot::direct(&CortexTenancy::SingleUser).path(),
        "app:tinymemory"
    );
    assert_eq!(
        pinned("org:acme/user:alice").path(),
        "org:acme/user:alice/app:tinymemory"
    );
}

#[test]
fn a_direct_root_admits_only_scopes_that_start_with_it() {
    let alice = pinned("org:acme/user:alice");
    assert_eq!(
        alice.strip("org:acme/user:alice/app:tinymemory/app:documents"),
        Some("app:documents")
    );
    for other in [
        "org:acme/user:bob/app:tinymemory/app:documents",
        "org:acme/user:alice2/app:tinymemory/app:documents",
        "org:acme/app:tinymemory/app:documents",
        "app:tinymemory/app:documents",
        "org:evil/org:acme/user:alice/app:tinymemory/app:documents",
        "org:acme/user:alice/app:tinymemory",
    ] {
        assert_eq!(alice.strip(other), None, "{other}");
    }
    let single = ScopeRoot::direct(&CortexTenancy::SingleUser);
    assert_eq!(
        single.strip("user:bob/app:tinymemory/app:documents"),
        None,
        "a single-user engine does not adopt a pinned tenant's scopes"
    );
}

#[test]
fn a_hosted_root_is_found_under_the_backend_tenant() {
    let hosted = ScopeRoot::hosted();
    assert_eq!(
        hosted.strip("app:tinymemory/app:documents"),
        Some("app:documents")
    );
    assert_eq!(
        hosted.strip("oc:u-1/app:tinymemory/agent:x/app:learnings"),
        Some("agent:x/app:learnings")
    );
    assert_eq!(
        hosted.strip("oc:u-1/xapp:tinymemory/app:learnings"),
        None,
        "only a whole segment matches"
    );
}
