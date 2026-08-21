//! Public facade and Cargo feature implication tests.

#[test]
fn facade_reexports_the_contract_types_without_conversion() {
    fn accepts_api_category(_: tinymemory::api::types::MemoryCategory) {}
    let category = tinymemory::types::MemoryCategory::Core;
    accepts_api_category(category);

    let provider = tinymemory::null::NullMemoryProvider::new();
    let _: &dyn tinymemory::provider::MemoryProvider = &provider;
}

#[cfg(all(feature = "sources-network", not(feature = "sources")))]
compile_error!("sources-network must imply sources");
#[cfg(all(feature = "documents-network", not(feature = "documents")))]
compile_error!("documents-network must imply documents");
#[cfg(all(feature = "memory-git", not(feature = "tinycortex")))]
compile_error!("memory-git must imply tinycortex");
#[cfg(all(feature = "contacts", not(feature = "core")))]
compile_error!("contacts must imply core");
#[cfg(all(
    feature = "engines",
    not(all(
        feature = "tinycortex",
        feature = "supermemory",
        feature = "mem0",
        feature = "cognee"
    ))
))]
compile_error!("engines must expose every engine adapter");
#[cfg(all(
    feature = "full",
    not(all(
        feature = "engines",
        feature = "core",
        feature = "sync",
        feature = "sources-network",
        feature = "documents-network",
        feature = "conformance",
        feature = "memory-git"
    ))
))]
compile_error!("full must imply every production feature group");

#[cfg(feature = "conformance")]
#[test]
fn conformance_feature_exposes_the_reference_provider() {
    let _ = tinymemory::conformance::InMemoryProvider::new();
}
