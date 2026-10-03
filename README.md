# TinyMemory

The memory layer for TinyHumans agents: **recall, fetch and store** over
pluggable engines, plus a token-budgeted `context.md` compiled from whatever is
stored.

| Operation | Meaning |
| --- | --- |
| **Recall** | A question in, a synthesised answer with citations out. The engine owns how it answers. |
| **Fetch** | Raw keyword, vector or hybrid retrieval over stored items, filtered by metadata. No synthesis. |
| **Store** | Ingest a document, a conversation or a learning, each with typed metadata. |

The behaviour is specified in [`docs/specs/memory-v2.md`](docs/specs/memory-v2.md),
which is the source of truth.

## Layout

```text
crates/
├── tinymemory/              the facade a host depends on: re-exports the
│                            contract, the engine registry (`list_engines`,
│                            `build_engine`), `MemoryConfig`, and every other
│                            crate behind a feature named after it
├── tinymemory-api/          the contract: `MemoryEngine`, `StoreItem`,
│                            `MemoryMeta`, `MetaFilter`, request/response
│                            types, `EngineDescriptor`, `Error`. No I/O
├── tinymemory-cortex/       the CortexDB engine, registered twice: `cortexdb`
│                            (direct `/v1/*`) and `tinyhumans` (CortexDB behind
│                            the TinyHumans backend `/memory/*`)
├── tinymemory-documents/    format sniffing and conversion to markdown
│                            (markdown, text, HTML, code; PDF/DOCX through a
│                            host converter), emitting `StoreItem::Document`
├── tinymemory-sources/      readers turning a source into `StoreItem`s: folder,
│                            file, link, GitHub, RSS, Composio payloads, local
│                            conversations; includes the SSRF guard
├── tinymemory-safety/       secret and PII scrubbing applied before `store`
├── tinymemory-context/      `ContextCompiler`: builds `context.md` from an engine
├── tinymemory-import/       reads a legacy v1 (embedded TinyCortex) workspace
│                            and yields resumable `StoreItem`s
└── tinymemory-conformance/  the suite every engine must pass, plus a reference
                             in-memory engine
docs/
├── specs/                   behaviour and architecture specifications
├── plans/                   test-first implementation plans
└── adr/                     immutable architecture decision records
```

## Features

The facade reaches every optional crate through a feature of the same name.
Nothing is on by default: naming no feature gets the contract, the registry and
the CortexDB engines.

| Feature | Adds |
| --- | --- |
| `documents` | `tinymemory::documents` |
| `documents-office` | `tinymemory::documents::OfficeConverter` (PDF, DOCX, PPTX, XLSX) |
| `sources` | `tinymemory::sources` (local readers) |
| `sources-network` | the GitHub, RSS, web-page and URL-fetch readers (implies `sources`) |
| `safety` | `tinymemory::safety` |
| `context` | `tinymemory::context` |
| `import` / `legacy-import` | `tinymemory::import` |
| `conformance` | `tinymemory::conformance` |
| `full` | all of the above |

## Using from your project

Nothing is published to crates.io; take the facade by git, pinned to a tag:

```toml
[dependencies]
tinymemory = { git = "https://github.com/tinyhumansai/tinymemory", tag = "vX.Y.Z", features = ["context", "safety"] }
```

Choose an engine by configuration and hand it a credential from your own
secret store:

```rust,no_run
use std::sync::Arc;
use tinymemory::{
    EngineCredential, FetchMode, FetchRequest, MemoryConfig, MemoryMeta, SourceKind, StaticBearer,
    StoreItem,
};

# async fn demo() -> tinymemory::Result<()> {
let config: MemoryConfig = toml::from_str(r#"engine = "tinyhumans""#).unwrap();
let engine = config.build(EngineCredential::Dynamic(Arc::new(StaticBearer::new("tiny_live_..."))))?;

let mut meta = MemoryMeta::from_source(SourceKind::Folder, Some("notes".into()));
meta.file_path = Some("/notes/rust/ownership.md".into());
engine.store(StoreItem::document("Ownership moves values.", meta)).await?;

let page = engine.fetch(FetchRequest::new("ownership", FetchMode::Hybrid, 5)).await?;
# let _ = page;
# Ok(())
# }
```

`build_engine` refuses an unknown engine id, a missing required endpoint or
credential, and a credentialed cleartext endpoint that is not loopback.

## Engines

| Id | What | Fetch modes |
| --- | --- | --- |
| `cortexdb` | CortexDB's own `/v1/*` API with an API key | `hybrid` |
| `tinyhumans` | CortexDB behind the TinyHumans backend `/memory/*`, with a per-request bearer | `hybrid` |

CortexDB is an append-only event log: writes wait until they are readable,
listings are de-duplicated, forgets always name event ids, and an empty forget
selector (which CortexDB reads as "the whole scope") is never sent. Its recall
route has no keyword/vector switch, so both wires declare hybrid fetch only. See
`crates/tinymemory-cortex/README.md`.

### Adding an engine

Implement `tinymemory_api::MemoryEngine` in its own crate, declare its fetch
modes honestly in its `EngineDescriptor`, pass `tinymemory_conformance::run`
against it, and register it in `crates/tinymemory/src/registry/`.

## Development

Run from the repository root; CI runs exactly these:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo build --all-targets --all-features
cargo test --all-features
```

`cargo run -p tinymemory --example basic` lists the engines and builds one
from configuration. Contribution rules are in [`AGENTS.md`](AGENTS.md).

## License

GPL-3.0-only. See [`LICENSE`](LICENSE).
