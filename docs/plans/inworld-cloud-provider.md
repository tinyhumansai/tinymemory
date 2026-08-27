# Inworld cloud provider metadata plan

Specification: [Inworld cloud provider metadata](../specs/inworld-cloud-provider.md)

## Goal

Add the smallest host contract required for an Inworld LLM provider without
adding request transport or credential storage to TinyMemory.

## Tasks

1. Extend cloud-provider tests for the `basic` wire value, Inworld preset,
   legacy migration, host recognition, and chat-only classification.
2. Add `AuthStyle::Basic` and the Inworld built-in catalog entry in
   `crates/tinymemory-api/src/host/cloud_providers.rs`.
3. Run formatting, Clippy, build, and test contract commands.

## Completion checklist

- [x] Focused cloud-provider tests pass.
- [x] `cargo fmt --all -- --check` passes.
- [x] `cargo clippy --all-targets --all-features -- -D warnings` passes.
- [x] `cargo build --all-targets --all-features` passes.
- [x] `cargo test --all-features` passes.
