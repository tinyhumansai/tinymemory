//! Public linked-module entry points remain callable by a Rust host.

#![cfg(feature = "static-link")]

use tinybus::broker::Broker;
use tinybus::module::abi::{TbModuleInit, TbSlice, ABI_MAGIC};
use tinybus::module::manifest::ModuleManifest;
use tinybus::module::ModuleHost;
use tinybus::transport::memory::MemoryBus;
use tinybus::Connection;
use tinymemory_module::{
    tinybus_module_init_v1, tinybus_module_manifest_v1, BUS_NAME, OBJECT_PATH,
    TINYBUS_MODULE_ABI_V1,
};

#[test]
fn linked_module_exposes_its_descriptor_manifest_and_initializer() {
    assert_eq!(TINYBUS_MODULE_ABI_V1.magic, ABI_MAGIC);
    let manifest: extern "C" fn() -> TbSlice = tinybus_module_manifest_v1;
    let slice = manifest();
    assert!(!slice.ptr.is_null());
    assert!(slice.len > 0);
    let initialize: TbModuleInit = tinybus_module_init_v1;
    assert_ne!(initialize as usize, 0);
}

#[allow(unsafe_code)]
#[tokio::test]
async fn linked_module_serves_memory_calls_through_tinybus(
) -> Result<(), Box<dyn std::error::Error>> {
    let workspace = tempfile::tempdir()?;
    let bus = MemoryBus::new();
    let broker = Broker::new();
    let _broker_task = broker.spawn(bus.clone());
    let host = ModuleHost::new(broker);

    let slice = tinybus_module_manifest_v1();
    // SAFETY: the generated manifest owns its bytes in a process-lifetime
    // OnceLock; its non-null pointer and length remain valid during parsing.
    let bytes = unsafe { std::slice::from_raw_parts(slice.ptr, slice.len) };
    let manifest: ModuleManifest = serde_json::from_slice(bytes)?;
    let config = serde_json::json!({ "workspace_dir": workspace.path() });

    // SAFETY: these three entries come from the linked module in this process.
    // They remain mapped and callable until exit, including all callbacks the
    // host retains after initialization.
    let loaded = unsafe {
        host.attach_raw_with_config(
            "linked-tinymemory",
            TINYBUS_MODULE_ABI_V1,
            manifest,
            tinybus_module_init_v1,
            config,
        )
    }?;
    assert_eq!(loaded.manifest.bus_name.as_str(), BUS_NAME);

    let client = Connection::connect(bus.connect().await?).await?;
    let proxy = client.proxy(BUS_NAME, OBJECT_PATH, "ai.tinyhumans.tinymemory.Memory")?;
    let driver_id: String = proxy.call("DriverId", ()).await?;
    assert_eq!(driver_id, "tinycortex");
    Ok(())
}
