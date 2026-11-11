# TinyMemory

The memory layer for TinyHumans agents: **recall, fetch and store** over
pluggable engines, a token-budgeted `context.md` compiled from whatever is
stored, and a set of agent tools that let a model use memory without ever
choosing whose memory it touches.

| Operation | Meaning |
| --- | --- |
| **Recall** | A question in, a synthesised answer with citations out. The engine owns how it answers. |
| **Fetch** | Raw keyword, vector or hybrid retrieval over stored items, filtered by metadata. No synthesis. |
| **Store** | Ingest a document, a conversation or a learning, each with typed metadata. |

Around those three sit `list`, `forget`, `explore` and `get`, and the
namespace tree that keeps one tenant's or agent's memory apart from another's.

- **Specified behaviour:** [`docs/specs/memory-v2.md`](docs/specs/memory-v2.md)
  is the source of truth for what the system does and why.
- **Integrating an agent:** [`docs/integration.md`](docs/integration.md) is
  the guide for a host wiring its agent loop to the memory API.
- **How it is built:** [`docs/architecture/README.md`](docs/architecture/README.md)
  has one page per concern: the contract, operations, namespaces, the CortexDB
  engine, the agent tools, the integrations and the test strategy.

## Layout

The workspace has three memory crates plus the standalone `tinyredact` library.

```text
crates/
├── tinymemory-api/           the contract: `MemoryEngine`, `StoreItem`, `MemoryMeta`,
│                             `MetaFilter`, `Namespace`/`Reach`, request and response
│                             types, `EngineDescriptor`, `Error`. No I/O. Feature
│                             `conformance` adds the suite every engine must pass and
│                             an in-memory reference engine
├── tinymemory-tools/         the agent surface over any engine: `MemoryTools` (seven
│                             model-callable tools with JSON Schemas and host-fixed
│                             scoping), holistic recall and the `context.md` compiler,
│                             and the agent lifecycle (`AgentMemory`, `Brain`,
│                             `MemoryLayout`, background jobs)
├── tinymemory-integrations/  everything that touches the outside world, one module
                              per feature: `cortex` (+ `registry`, `config`),
                              `documents`, `sources`, `safety`, `import`
└── tinyredact/               standalone on-device secret and PII redaction
docs/
├── architecture/             how the code delivers the spec, one page per concern
├── specs/                    behaviour and architecture specifications
├── plans/                    test-first implementation plans
└── adr/                      immutable architecture decision records
```

`tinymemory-tools` and `tinymemory-integrations` each depend only on
`tinymemory-api`, never on each other, so the contract is the one coupling
point. A host takes the crates it needs.

## Integrations features

`tinymemory-integrations` enables the CortexDB engine by default and everything
else on request, so a host pays only for what it uses.

| Feature | Module | Adds |
| --- | --- | --- |
| `cortex` (default) | `cortex`, `registry`, `config`, `registry::binder` | `CortexEngine` over both wires; `list_engines`, `build_engine`, `EngineCredential`, cached guarded `EngineBinder`, `MemoryConfig` (implies `safety`) |
| `documents` | `documents` | Format sniffing and conversion to markdown, producing `StoreItem::Document` |
| `documents-office` | `documents::OfficeConverter` | PDF, DOCX and PPTX to markdown; XLSX is temporarily unsupported (implies `documents`) |
| `brain` | `brain` | Files into `tinymemory_tools::BrainDocument`s, by the source type their format implies (implies `documents`) |
| `sources` | `sources` | Folder, file and conversation readers (implies `documents`) |
| `sources-network` | `sources::fetch` and the network readers | GitHub, RSS and web-page readers and `fetch_url`, behind the SSRF guard (implies `sources`) |
| `safety` | `safety` | Secret and PII scrubbing plus `ScrubbingEngine` and `TimedEngine` |
| `legacy-import` | `import` | Migrating a v1 (embedded TinyCortex) workspace into any engine |
| `full` | all of the above | `cortex`, `documents-office`, `brain`, `sources-network`, `safety`, `legacy-import` |

Dependency weight per feature is tabulated in
[`crates/tinymemory-integrations/README.md`](crates/tinymemory-integrations/README.md).

## Using from your project

Nothing is published to crates.io. Take the crates you need by git, pinned to
a tag:

```toml
[dependencies]
tinymemory-api = { git = "https://github.com/tinyhumansai/tinymemory", tag = "vX.Y.Z" }
tinymemory-tools = { git = "https://github.com/tinyhumansai/tinymemory", tag = "vX.Y.Z" }
tinymemory-integrations = { git = "https://github.com/tinyhumansai/tinymemory", tag = "vX.Y.Z", features = ["sources", "safety"] }
```

All workspace crates carry the same version, so one tag names them all. A host that
only implements an engine needs just `tinymemory-api`; one that only offers
tools over an engine it already has needs `tinymemory-api` and
`tinymemory-tools`.

## Quickstart

Choose an engine by configuration and hand it a credential from your own
secret store, give a model memory tools scoped to one agent, and run what the
model asks for:

```rust,no_run
use std::sync::Arc;
use serde_json::json;
use tinymemory_api::Namespace;
use tinymemory_integrations::{EngineCredential, MemoryConfig, StaticBearer};
use tinymemory_tools::{MEMORY_STORE, MemoryTools};

# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
// The config names the engine; it never holds a credential.
let config: MemoryConfig = toml::from_str(r#"engine = "tinyhumans""#)?;
let engine = config.build(EngineCredential::Dynamic(Arc::new(StaticBearer::new("tiny_live_..."))))?;

// The host fixes where this agent writes and how far it reads. The model can
// name neither: a `namespace` or `reach` argument is refused at any depth.
let tools = MemoryTools::new(engine).placed_at(Namespace::agent("researcher"));

// Hand these (name, description, JSON Schema) to your tool runtime.
for spec in tools.specs() {
    println!("{}: {}", spec.name, spec.description);
}

// Run a tool call the model produced.
let receipt = tools
    .call(MEMORY_STORE, json!({ "learning": { "text": "The user prefers short answers" } }))
    .await?;
println!("stored {}", receipt["id"]);
# Ok(())
# }
```

The tools can also be used without a model. Call the engine directly:

```rust,ignore
use tinymemory_api::{FetchMode, FetchRequest, MemoryMeta, SourceKind, StoreItem};

let meta = MemoryMeta::from_source(SourceKind::Folder, Some("notes".into()));
engine.store(StoreItem::document("Ownership moves values.", meta)).await?;
let page = engine.fetch(FetchRequest::new("ownership", FetchMode::Hybrid, 5)).await?;
```

`build_engine` refuses an unknown engine id, a missing required endpoint or
credential, and a credentialed cleartext endpoint that is not loopback.

To compile a `context.md` for the start of a session, call
`tinymemory_tools::context::compile(&*engine, &ContextSpec::default())`.

### The agent lifecycle

For memory around every turn, use `tinymemory_tools::AgentMemory`. It
implements a standard layout: a global brain of documents by source type,
each agent's conversations, and shared learnings. Call it at each point of
the agent loop:

```rust,ignore
let memory = AgentMemory::new(engine.clone(), MemoryLayout::default(), "support-01")?;
let turn = memory.pre_turn(PreTurn::new("thread-1", 0, user_text)).await?;   // log + recall, no indexing wait
let reply = llm.generate(&turn.pack.markdown, user_text).await;
let report = memory.post_turn(PostTurn::new("thread-1", 1, reply)).await?;   // log
for job in report.jobs { queue.push(job) }                                    // belief builds, off the turn
```

`Brain` ingests documents (with `tinymemory_integrations::brain::brain_document`
converting PDFs and markdown first). `start_session` and
`recall_for_compaction` cover session resume and prompt truncation. Run
`cargo run -p tinymemory-tools --example agent_loop` for the whole loop
offline, and see
[`docs/architecture/lifecycle.md`](docs/architecture/lifecycle.md).

## Engines

| Id | What | Fetch modes |
| --- | --- | --- |
| `cortexdb` | CortexDB's own `/v1/*` API with an API key | `hybrid` |
| `tinyhumans` | CortexDB behind the TinyHumans backend `/memory/*`, with a per-request bearer | `hybrid` |

CortexDB is an append-only event log: writes wait until they are readable,
listings are de-duplicated, forgets always name event ids, and an empty forget
selector (which CortexDB reads as "the whole scope") is never sent. Its recall
route has no keyword/vector switch, so both wires declare hybrid fetch only. See
[`docs/architecture/cortex.md`](docs/architecture/cortex.md) and
[`crates/tinymemory-integrations/src/cortex/README.md`](crates/tinymemory-integrations/src/cortex/README.md).

### Where a person's memory lives

One root per person, `org:<id>`, and no `user:` segment below it (`user:<id>`
is the person's actor, never a scope):

```text
org:<id>/app:learnings                         learnings
org:<id>/app:brain/source:gmail                a connector's documents
org:<id>/ws:main/app:conversations             chats
org:<id>/ws:main/app:flows/service:<flow>/…    a workflow's memory
```

A direct `cortexdb` engine gets `scope_root = "org:<id>"`; a `tinyhumans`
engine gets `tenant_root = true` and sends paths relative to the tenant root
the backend pins. Memory an earlier layout wrote below `user:<id>` stays
readable and forgettable through `retired_scope_root` until it has moved. See
[`docs/architecture/cortex-layout.md`](docs/architecture/cortex-layout.md).

### Adding an engine

An engine is a module of `tinymemory-integrations` behind a feature named after
it. Implement `tinymemory_api::MemoryEngine`, declare its fetch modes honestly
in its `EngineDescriptor`, pass `tinymemory_api::conformance::run` against it
(enable the `conformance` feature of `tinymemory-api` in dev-dependencies), and
register it in `registry`.

## Migrating from v1

A v1 (embedded TinyCortex) workspace is read in place and copied into any
engine, resumably. `migrate` takes the checkpoint of an earlier run (`None` to
start from the beginning) and returns where it got to:

```rust,ignore
use tinymemory_integrations::import::{LegacyWorkspace, migrate};

let report = migrate(engine.as_ref(), LegacyWorkspace::open(path)?, None).await?;
println!("stored {}, replayed {}", report.stored, report.replayed);
```

Use `migrate_with` to receive each committed `Checkpoint` as it happens, so a
host can persist it. The
legacy workspace is opened read-only, and re-running never duplicates. Needs
the `legacy-import` feature. See
[`docs/architecture/integrations.md`](docs/architecture/integrations.md#legacy-v1-import).

## Development

Run from the repository root; CI runs exactly these:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo build --all-targets --all-features
cargo test --all-features
```

Also useful:

```bash
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features
cargo test --doc --all-features
cargo run -p tinymemory-integrations --example basic
```

The example lists the registered engines and builds one from configuration,
with no network access. The `-p` is required because the workspace root is
virtual. Live tests against a real CortexDB are described in
[`docs/architecture/testing.md`](docs/architecture/testing.md). Contribution
rules are in [`AGENTS.md`](AGENTS.md).

## License

GPL-3.0-only. See [`LICENSE`](LICENSE).
