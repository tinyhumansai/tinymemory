# TinyMemory

The engine-neutral memory layer for TinyHumans agents.

A host that embeds TinyMemory performs every memory operation through one
contract, and picks which engine answers it by configuration rather than by
recompiling. [TinyCortex](https://github.com/tinyhumansai/tinycortex) is the
default embedded engine; a second engine implements the same traits and binds in
its place without the host learning anything new.

## Layout

```text
api/                    tinymemory-api — the contract. Dependency-light on
                        purpose: depending on it never drags in SQLite, git2,
                        reqwest, or an async runtime.
src/
├── lib.rs              re-exports the contract wholesale, so a host takes one
│                       dependency and the types are the same types
├── registry/           driver admission — which ids exist, what class each
│                       binds as, and the fail-closed external-driver gate
└── mandatory/          the three mandatory capability families, composed once
                        over the `Memory` storage trait
adapters/
├── tinycortex/         the TinyCortex engine seen through the contract
└── remote/             native HTTP dialects for Supermemory, Mem0, and Cognee
vendor/
├── tinycortex/         the engine, pinned as a submodule
└── tinybus/            pinned TinyBus submodule
```

## The contract

`MemoryProvider` is an object-safe trait with **three mandatory** capability
families and **ten optional** ones. The mandatory three are supertraits, so a
driver missing any of them cannot be constructed; the optional ten are reached
through `as_ingest()` / `as_tree()` / … accessors that default to `None`, so a
minimal driver implements what it supports and inherits correct absence for
everything else.

A driver's advertised set and its reachable accessors must agree.
`audit_provider` checks exactly that, which turns "advertised but not
implemented" into a detectable, testable mistake rather than a runtime surprise
on the first call.

Capabilities are asked **once, at bind time, and cached**: a host filters its RPC
surface and its agent-tool list from the answer, so a set that changed
afterwards would not be noticed.

## What lives here, and what deliberately does not

| Here | In the host |
| --- | --- |
| the contract; capability negotiation; driver admission; the shared mandatory families; per-engine adapters | RPC surface, agent tools, security policy, credentials, schedulers, event bus, config mapping |

**Policy is not here, on purpose.** Tier enforcement, scope predicates, taint
stamping, redaction, egress checks and audit belong in a decorator the *host*
owns, on the path every caller takes. A driver that could be swapped for one
that skips enforcement is the entire reason the policy layer exists.

## Adding an engine

1. Implement `tinymemory_api::traits::Memory` for the backend, **overriding
   `store_with_taint`** — the trait default silently drops the taint, which
   would launder externally-sourced content into internal-trust content.
2. Wrap it: `MemoryTraitProvider::new(backend, "my-engine")`. That yields a
   driver advertising Core, Recall and Portability, with the four
   easy-to-get-wrong parts (see `src/mandatory/mod.rs`) already handled.
3. Implement any optional families over the engine's own entry points, and
   widen `capabilities()` in lockstep with the accessors.
4. Reserve the driver id: `DriverRegistry::builtin().with_reserved("my-engine", DriverClass::Embedded)`.

## Remote engines

The `tinymemory-remote` crate supports the self-hosted native APIs of
Supermemory, Mem0, and Cognee. Each adapter stores TinyMemory's key, category,
session, and provenance in backend metadata (or a Cognee raw-data envelope), so
exact CRUD and portability survive the seam while recall remains engine-native.

```rust
use tinymemory_remote::{SupermemoryMemory, supermemory_provider};

let memory = SupermemoryMemory::new("http://localhost:6767", Some("sm_..."))?;
let provider = supermemory_provider(memory);
# Ok::<_, anyhow::Error>(provider)
```

All three advertise the mandatory Core, Recall, and Portability families. The
live Docker harness and conformance command are documented in
[`integration/remote-engines/`](integration/remote-engines/README.md).

## Development

```bash
git submodule update --init --recursive
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

Engine adapters name their engines by **version requirement, not path**, so a
host that already pins its own engine checkout unifies onto one copy through its
own `[patch.crates-io]`. The workspace root patches them to the nested `vendor/`
submodules for a standalone build. A path dependency in an adapter would defeat
that and hand a host two copies of one engine with two incompatible `Memory`
traits.
