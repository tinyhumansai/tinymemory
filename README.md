# TinyMemory

The engine-neutral memory layer for TinyHumans agents.

A host that embeds TinyMemory performs every memory operation through one
contract, and picks which engine answers it by configuration rather than by
recompiling. [TinyCortex](https://github.com/tinyhumansai/tinycortex) is the
default embedded engine; a second engine implements the same traits and binds in
its place without the host learning anything new.

## Layout

```text
crates/
├── tinymemory/         the facade a host depends on. Re-exports the contract
│                       wholesale, so the types are the same types, and reaches
│                       every other crate here through a feature named after it
│   ├── src/lib.rs      the entire public re-export surface
│   ├── src/registry/   driver admission — which ids exist, what class each
│   │                   binds as, and the fail-closed external-driver gate
│   ├── tests/          integration tests against the public API only
│   └── examples/       runnable, compiled-in-CI usage examples
├── tinymemory-api/     the driver contract: the traits an engine implements and
│                       the host seam it binds through, plus every
│                       `tinymemory-bus` type re-exported at its historical path.
│                       Dependency-light on purpose: depending on it never drags
│                       in SQLite, git2, reqwest, or an async runtime
├── tinymemory-bus/     the wire vocabulary: every type that crosses the module
│                       boundary, plus the member names. Sits *below* the
│                       contract — `tinymemory-api` depends on it and re-exports
│                       it — so a host that only makes calls into
│                       `tinymemory-module` links this alone and compiles no
│                       traits, no null driver and no config surface
├── tinymemory-core/    the substance: ingestion, the summary tree, chunk
│                       storage, entities, the graph, the diff ledger, goals,
│                       tool-memory, and the Composio sync layer. The largest
│                       crate here by a wide margin. Unlike the contract it is
│                       not dependency-light: today it links the TinyCortex
│                       engine, a bundled SQLite, and an HTTP stack
│                       unconditionally
├── tinymemory-sync/    the engine-neutral Composio payload normalisers, so a
│                       host binding a driver that is not TinyCortex can run
│                       them
├── tinymemory-sources/ memory-source contracts and readers — local folders
│                       always, GitHub/RSS/web pages behind `network`
├── tinymemory-documents/ document and URL intake: sniff a format, convert it
│                       to markdown, and write it into whichever engine is
│                       bound. The URL half is behind `network` and reuses the
│                       source readers' SSRF guard rather than growing a second
├── tinymemory-tinycortex/  the TinyCortex engine seen through the contract
├── tinymemory-remote/  native HTTP dialects for Supermemory, Mem0, and Cognee
├── tinymemory-conformance/ the behavioural suite every driver must pass
├── tinymemory-testing-ui/  a local HTTP + web harness for driving engines by
│                       hand. A workspace member, but held out of
│                       `default-members` so the contract commands skip it
└── tinymemory-module/  the TinyBus loadable-module driver. Excluded from the
                        workspace on purpose — see the note in `Cargo.toml`.
vendor/
├── tinycortex/         the engine, pinned as a submodule
├── tinyagents/         pinned TinyAgents submodule
└── tinybus/            pinned TinyBus submodule
```

Every crate lives under `crates/`, one directory per package, each directory
named for the package it holds. The workspace root is virtual — there is no
root package, so the facade is a member like any other and `members` is the
glob `crates/*`: a new crate joins the workspace by existing.

## Features

`tinymemory` is the one dependency a host takes, and every other crate in the
workspace is reachable from it by a feature named after it. Nothing is on by
default: naming no feature gets the contract, the registry and the mandatory
composition — no storage engine, no HTTP stack, no native library.
`scripts/ci/dependency-budget.sh` holds that to a ceiling on every run.

| Feature | Brings in |
| --- | --- |
| `tinycortex` | the embedded TinyCortex engine, as `tinymemory::tinycortex` |
| `supermemory`, `mem0`, `cognee` | the matching HTTP adapter, as `tinymemory::remote` |
| `engines` | all four of the above |
| `core` | `tinymemory::core` — the memory subsystem |
| `sync` | `tinymemory::sync` — the Composio normalisers |
| `sources` | `tinymemory::sources` — source contracts and local readers |
| `sources-network` | `sources`, plus the GitHub/RSS/web-page readers |
| `documents` | `tinymemory::documents` — document intake and markdown conversion |
| `documents-network` | `documents`, plus the URL fetch path |
| `conformance` | `tinymemory::conformance` — the driver contract suite |
| `memory-git` | git-backed diff snapshots (implies `tinycortex`; links libgit2) |
| `contacts` | the macOS address-book seeding path (implies `core`) |
| `test-support` | the workspace's test doubles and helpers |
| `full` | every feature above except `test-support` |

Capability features imply the engine that serves them, so asking for a
capability cannot produce a build where nothing implements it. `test-support`
is deliberately outside `full`: "give me the whole workspace" is not the same
request as "give me the test doubles".

This table says which crate each feature brings in. For what each *engine*
feature actually serves — driver class, and how many of the eighteen capability
families answer — see the engine table under
[Using from your project](#using-from-your-project).


Run `git submodule update --init --recursive` after cloning. Nothing in the
workspace builds without it — `tinymemory-core` names `tinyagents` and
`tinycortex` by path through `vendor/`, so an uninitialized checkout fails at
manifest resolution rather than at compile time, which reads as a confusing
error.

## Using from your project

None of these crates are on crates.io yet, so you take the facade by git.
Which patch table you need depends on the engine you pick.

**Remote engines (Supermemory, Mem0, Cognee — hosted or self-hosted) — no patch table:**

```toml
[dependencies]
tinymemory = { git = "https://github.com/tinyhumansai/tinymemory", features = ["supermemory"] }
```

```rust,ignore
use std::sync::Arc;

let backend = tinymemory::remote::SupermemoryMemory::cloud("sm_...")?;
let provider = Arc::new(tinymemory::remote::supermemory_provider(backend));
```

The remote adapter reaches only crates.io dependencies, so cargo resolves it
without any `[patch]` entries.

**The embedded engine (TinyCortex) — vendor this repository as a submodule.**

The remote recipe above works by git because the remote adapter reaches only
published crates. The embedded engine does not: it pulls `tinycortex`,
`tinycortex-api` and `tinyagents`, none of which are published, and
`tinycortex-api` takes `tinymemory-api` *by git*, which cargo will resolve as a
second copy of a crate this workspace also provides by path. Patching that away
needs the crates on disk, so the embedded path is a submodule dependency until
these crates are published:

```sh
git submodule add https://github.com/tinyhumansai/tinymemory vendor/tinymemory
git -C vendor/tinymemory submodule update --init --recursive
```

```toml
[dependencies]
tinymemory = { path = "vendor/tinymemory", features = ["tinycortex"] }

# All four are required. The first three are unpublished crates the engine
# needs; the fourth collapses `tinycortex-api`'s git dependency on
# `tinymemory-api` onto the copy in this tree — without it two distinct
# `tinymemory_api::MemoryEntry` types exist and the seam stops type-checking.
[patch.crates-io]
tinycortex = { path = "vendor/tinymemory/vendor/tinycortex" }
tinycortex-api = { path = "vendor/tinymemory/vendor/tinycortex/api" }
tinyagents = { path = "vendor/tinymemory/vendor/tinyagents" }
[patch."https://github.com/tinyhumansai/tinymemory"]
tinymemory-api = { path = "vendor/tinymemory/api" }
```

This exact patch set is what the reference consumer in
`crates/tinymemory/examples/` and the repository's own root manifest use; a
build missing any of the four fails at resolution, before compiling a line.

```rust,ignore
use std::sync::Arc;
use tinymemory::tinycortex::{provider, InMemoryMemoryStore};

let provider = Arc::new(provider(Arc::new(InMemoryMemoryStore::new())));
```

That is a complete embedded setup for the mandatory three families. The full
eighteen-family engine (`TinycortexProvider`) additionally needs the host
seams (`EmbeddingHost` et al.) installed — see
`crates/tinymemory-tinycortex/tests/full_provider_conformance.rs` for the
minimal working wiring.

| Feature | Engine | Class | Families served |
| --- | --- | --- | --- |
| `tinycortex` | TinyCortex, in-process | embedded | 3 (mandatory) via `provider`; all 18 via `TinycortexProvider` |
| `supermemory` | Supermemory, hosted | external | 3 (mandatory) |
| `mem0` | Mem0, hosted (`cloud`) or self-hosted | external | 3 (mandatory) |
| `cognee` | Cognee, hosted or self-hosted | external | 3 (mandatory) |
| `memory-git` | add-on: git-backed diff snapshots | — | requires `tinycortex` |
| *(none)* | `NullMemoryProvider` | null | contract + registry only, 40 crates |

The `namespace` driver id you may see in the registry's reserved table is
host-internal: it names `tinymemory-core`'s own store, whose constructors live
in that crate — it is not selectable from the facade.

**A note on remote-engine performance:** recall is native to each hosted API,
but exact-CRUD operations (`get`, `list`, `count`, upsert-by-key) are
enumeration-based — the adapter pages the hosted API to find the record. Fine
for assistant-memory workloads; wrong for high-volume keyed storage.

## The contract

`MemoryProvider` is an object-safe trait with **three mandatory** capability
families and **fifteen optional** ones. The mandatory three are supertraits, so a
driver missing any of them cannot be constructed; the optional fifteen are reached
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

The `tinymemory-remote` crate supports the managed and self-hosted native APIs
of Supermemory, Cognee, and Mem0. Each adapter stores TinyMemory's key,
category, session, and provenance in backend metadata (or a Cognee raw-data
envelope), so exact CRUD and portability survive the seam while recall remains
engine-native. Provider-facing dataset names, container tags,
and filenames are bounded stable hashes, so every namespace and key accepted by
the TinyMemory contract remains valid on the remote API.

```rust
use tinymemory_remote::{SupermemoryMemory, supermemory_provider};

let memory = SupermemoryMemory::self_hosted("http://localhost:6767", "sm_...")?;
let provider = supermemory_provider(memory);
# Ok::<_, anyhow::Error>(provider)
```

Managed APIs have explicit constructors so their authentication cannot be
confused with a self-hosted token:

```rust
use tinymemory_remote::{CogneeMemory, Mem0Memory, SupermemoryMemory};

// Cognee Cloud issues a per-tenant base URL (the API-key dashboard shows it);
// there is no shared endpoint, so its constructor takes one.
let cognee = CogneeMemory::api("https://tenant-<uuid>.aws.cognee.ai", "cognee-api-key")?;

// Supermemory and Mem0 both serve one hosted origin, so theirs take only a key.
let supermemory = SupermemoryMemory::cloud("sm_...")?;
let mem0 = Mem0Memory::cloud("m0-...")?;
# Ok::<_, anyhow::Error>((cognee, supermemory, mem0))
```

Cognee Cloud uses `X-Api-Key`; authenticated self-hosted Cognee uses a bearer
access token. Supermemory uses bearer API keys for both deployment modes. Mem0's
hosted platform uses `Authorization: Token`, and self-hosted Mem0 uses
`X-API-Key`. All constructors redact credentials from `Debug` output, from
transport errors, and from the request's own header rendering.

All three advertise the mandatory Core, Recall, and Portability families. The
live Docker harness and conformance command are documented in
[`integration/remote-engines/`](integration/remote-engines/README.md).

One of them restricts what it will store. Supermemory removes `U+0000` and
`U+FFFD` from content server-side, so the adapter refuses such content with
`MemoryError::Invalid` rather than storing a value the service would quietly
rewrite: `MemoryCore::store` promises that what is read back equals what was
stored, and a driver may refuse a shape but may not accept one and hand back
another. The restriction is no wider than the defect — every other C0 control,
plus DEL, NEL, ZWSP, BOM and U+2028, survives — and identity is untouched,
because keys and namespaces travel in metadata, which the service does not
sanitise. Callers that might hold either character should strip or replace it
first; `U+FFFD` in particular arrives in any text that has been through a lossy
decode (issue #80).

Behaviour like that is visible only against the real service, so
`tinymemory-remote` carries a live target that runs the full contract suite
against a hosted endpoint when credentials are present and skips when they are
not. Point it at a scratch account: the suite writes and deletes records.

```bash
TINYMEMORY_TEST_SUPERMEMORY_URL=https://api.supermemory.ai \
TINYMEMORY_TEST_SUPERMEMORY_KEY=sm_... \
  cargo test -p tinymemory-remote --test live_remote_engines
```

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
