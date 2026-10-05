# Testing

How TinyMemory is tested, what CI runs, and how to add tests for a new engine
or integration. The rules for where tests live are in `AGENTS.md`; this page
is the map.

## The four contract commands

CI runs exactly these, so a green local run should mean a green CI run. Run
them from the repository root.

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo build --all-targets --all-features
cargo test --all-features
```

Supporting commands:

```sh
cargo test                                       # default features only
cargo test -p tinymemory-integrations <filter>   # a focused subset
cargo test --doc -p tinymemory-integrations --all-features   # doctests alone
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features
cargo run -p tinymemory-integrations --example basic
```

Never skip, ignore or delete a failing test to get a green run. Lints are one
workspace table (`[workspace.lints]`, opted into per crate): `unsafe_code`
is forbidden, `missing_docs` warns (and CI turns warnings into errors), and
clippy denies `unwrap`/`expect`/`panic` in library code. `clippy.toml` allows
them in tests.

## Where tests live

- **Unit tests** are never inline. They sit in a sibling `<module>_tests.rs`
  (`mod_tests.rs` beside `mod.rs`, `lib_tests.rs` beside `lib.rs`; a second
  group is `<module>_<topic>_tests.rs`), declared at the bottom of the module:

  ```rust
  #[cfg(test)]
  #[path = "foo_tests.rs"]
  mod tests;
  ```

  The file starts with a `//!` line and `use super::*;`, carries no
  `#[cfg(test)]` of its own, and is a child module so it reaches private
  items.
- **Test support** that is not a test lives in `<module>_test_support.rs` (for
  example `engine_test_support.rs`, which gives `CortexEngine` a
  `with_test_timing` knob) or a `test_support/` directory.
- **Integration tests** are in the crate's `tests/` directory and use only the
  public API. They are the regression suite for the contract.
- **Doctests** are compiled and run by `cargo test`, so examples cannot drift.
  An example that needs a network is `no_run`.

## CI

`.github/workflows/ci.yml` runs on every push and pull request. Jobs:

| Job | What it enforces |
| --- | --- |
| **Rust** | the four contract commands; `cargo test` with default features; `cargo run -p tinymemory-integrations --example basic`; and the **contract-crate dependency guard** |
| **Feature powerset and coverage** | `cargo hack --feature-powerset --depth 2 --workspace check --all-targets`; **coverage of at least 80% of lines** (`cargo llvm-cov --all-features --workspace --fail-under-lines 80`, ignoring `tests/`, `*_tests.rs`, `*_test_support.rs` and `test_support/`); and the **inline-test refusal** |
| **Test (feature matrix)** | `cargo test -p <package> <features>` for each row below |
| **Docs** | `cargo doc --no-deps --all-features` with `RUSTDOCFLAGS=-D warnings` |
| **MSRV** | reads `rust-version` of `tinymemory-api` and builds `--all-targets --all-features` with that toolchain (1.96) |
| **Supply chain** | `cargo-deny check all` (advisories, licenses, bans, sources; `deny.toml`) |
| **CortexDB live** | `./scripts/cortexdb-live.sh` against a pinned real server (see [live tests](#live-tests)) |

**Feature matrix.** Each row is its own `cargo test -p ...` so a feature
works on its own, not only in the union:

| Package | Features |
| --- | --- |
| `tinymemory-integrations` | `--no-default-features` |
| | `--no-default-features --features cortex` |
| | `--no-default-features --features documents` |
| | `--no-default-features --features documents-office` |
| | `--no-default-features --features sources-network` (proves it implies `sources`) |
| | `--no-default-features --features safety` |
| | `--no-default-features --features legacy-import` |
| | `--no-default-features --features full` |
| `tinymemory-api` | `--no-default-features` |
| | `--features conformance` |
| `tinymemory-tools` | defaults |

**Contract-crate dependency guard.** `tinymemory-api` is what engines and hosts
compile against, so it must stay free of storage engines, native libraries,
HTTP clients and async runtimes. The job runs `cargo tree -p tinymemory-api
-e normal,build --prefix none` and fails if it lists `rusqlite`, `libsqlite`,
`git2`, `reqwest`, `regex` or `tokio`. The forward form of `cargo tree` is
required: `cargo tree -i <crate> -p ...` discards the `-p` scope and looks
clean even when this crate is at fault.

**Inline-test refusal.** Any `#[cfg(test)]` (or `#[cfg(any(test, ...))]`) in a
`.rs` file other than `*_tests.rs` and `*_test_support.rs` must guard a `mod`
or `use` declaration. Anything else is test code inline in a production file,
and the job fails with `inline test-only executable code must live in a
*_tests.rs file`.

**Coverage.** Production-source line coverage across the workspace must be at
least 80%. Add tests with every behaviour change, and note a deliberately
untested edge case in the pull request.

## Release

`.github/workflows/release.yml` is a manual `workflow_dispatch` with a
`patch`, `minor` or `major` bump, and only runs on `main`. It re-runs
`cargo fmt --check`, clippy, `cargo test --all-features` and rustdoc. It then
reads the current version of `tinymemory-api` (every crate inherits
`[workspace.package] version`, so any one names it), computes the next
version, refuses an existing tag, and rewrites `version` in the
`[workspace.package]` table of the root `Cargo.toml` and refreshes
`Cargo.lock` (`cargo update --workspace`). Before the rewrite it fails if any
intra-workspace path dependency carries a `version = "..."` requirement,
since nothing is published and the one workspace version is enough. It
checks the bump took, commits `Release vX.Y.Z`, tags it, pushes both, and
creates a GitHub release. There are no binary artifacts, and nothing goes to
crates.io (`publish = false`). Do not hand-edit the workspace `version`.

## The conformance suite

`tinymemory_api::conformance` (feature `conformance` of `tinymemory-api`)
holds the behavioural suite every engine must pass:

```rust
tinymemory_api::conformance::run(&engine).await?;
```

`run(&dyn MemoryEngine) -> conformance::Result<()>` writes only under a
workspace unique to the run (`tinymemory-conformance/<nonce>`), filters by it,
and forgets it afterwards, so it can run against an engine that already holds
data. It stops at the first failed check. `Error::Check { check, detail }`
means the engine answered wrongly; `Error::Engine { check, source }` means it
failed a call it must serve. Cleanup runs even after a failure.

The checks, in order (`suite/checks.rs`, `explore.rs`, `bulk.rs`,
`namespaces.rs`):

| # | Check | What it proves |
| --- | --- | --- |
| 1 | `health` | the engine reports itself serving |
| 2 | `round_trip` | one item of each kind stores and lists back with the same kind, metadata and rendered text; paging terminates (the suite lists two at a time and fails on a repeated cursor or a non-zero listing score) |
| 3 | `replay` | storing an identical item again is a replay with the same id |
| 4 | `explore` | per-kind and per-workspace counts agree with `list`, buckets are largest first, and each bucket narrows to exactly its count |
| 5 | `get` | the run's items read back by id in the order asked, equal to their listing, with an unknown id left out |
| 6 | `store_many` | a batch stores in order, every item is listed on return, a repeat is all replays, an empty batch is refused |
| 7 | `fetch_filters` | for **every declared fetch mode**, a filter on each metadata field selects exactly the item carrying it (and `list` agrees) |
| 8 | `unsupported_modes` | every undeclared fetch mode fails `Unsupported` |
| 9 | `namespaces` | items at the root, two sibling agents and a sub-agent: each reach (own and inherited, exact, subtree) lists exactly its nodes, never a sibling's; `get` and `fetch` honour the reach; the same text in two namespaces is two items; the namespace facet counts each node; a forget scoped to one node removes only it |
| 10 | `empty_forget` | a forget with no ids or an empty filter is refused and removes nothing |
| 11 | `forget_by_id`, `forget_by_filter` | forgotten items stop listing and are counted; others stay |
| 12 | `recall` | an answer cites items that resolve through `list` |
| end | cleanup | forget by workspace filter; nothing survives |

### ReferenceEngine

`conformance::ReferenceEngine` (id `reference`) is the calibration subject: an
in-memory engine that is obvious by inspection, so a failure against it means
the assertion is wrong, not the engine. It serves every fetch mode (a trivial
keyword scorer and a deterministic 64-dimension toy vector), and answers
recall by quoting its best hybrid hits. It is also what the tools tests and
the import driver tests run against. `len()` and `is_empty()` let a test
assert that the suite cleaned up.

`crates/tinymemory-api/tests/conformance_reference.rs` runs the suite against
the reference engine **and** against deliberately broken wrappers of it, each
fault expected to be caught by the check written for it. Without the second
half a suite that asserted nothing would also be green.

### The isolation check

`conformance::run_isolation(a, b)` is a second check, over **two** engines a
host built for two users of one backing store, for example two `tinyhumans`
engines holding two different users' credentials against one backend. User A
stores one item of each kind and must read them back; then user B, through
the same workspace filter, A's ids and A's text, must reach none of them:

| Probe by B | Must |
| --- | --- |
| `list`, unscoped and over the root's subtree | list none of A's items |
| `get` of A's ids, unscoped and over the subtree | return nothing |
| `fetch` of A's text, every declared mode | find none of A's items |
| `recall` of A's text (unscoped: the read an engine may serve by descending from its root) | cite none of A's items |
| the namespace facet | count nothing |
| `forget` of A's ids, and by the shared workspace filter | remove nothing; A still lists all its items |
| `store` of A's exact item | be a new item for B, not a replay; A's copy unchanged |
| `store` at a namespace naming A (`user:<A>`) | be readable by B, invisible to A at every reach |

Both users' items are forgotten afterwards. The calibration in
`conformance_reference.rs` runs it on two reference engines (must pass) and on
one engine passed twice (must fail), so a check that asserted nothing would
not be green. The boundary itself is the service in front of the engine
(tinyhumansai/cortexdb-saas), so the check proves a deployment from the
outside rather than adding one.

## CortexDB engine tests

Inside `crates/tinymemory-integrations/src/cortex/`:

- **Unit tests** per module: `credential`, `descriptor`, `envelope` (and
  `labels`), `error`, `transport` (`actor`, `failure`, body cap, retry),
  and `engine` (`cursor`, `fetch`, `recall`, `scopes`, `store`, plus
  `mod_tests`, `mod_list_tests`, `mod_direct_tests` and `mod_hosted_tests`
  for each wire's behaviour through the doubles).
- **Conformance** (`conformance_tests.rs`): the shared suite against both
  wires, `the_direct_wire_upholds_the_contract` and
  `the_tinyhumans_wire_upholds_the_contract`.
- **The doubles** (`cortex/testing/`, compiled only under `cfg(test)`):
  real HTTP servers on an ephemeral loopback port, built with axum.

### The HTTP doubles

`direct_double()` and `hosted_double()` start a double and return its base URL
and shared state; `direct_engine(url)` and `hosted_engine(url)` build an engine
with fast test timing (5ms polls and backoff, a 2s visibility budget);
`both()` gives one engine per wire. `sample_items()` is one item of each kind.

Both doubles serve the same in-memory `CortexLog`, which is deliberately
**unaccommodating**, because a tidy double proves nothing. It reproduces every
behaviour in [the wire page](cortex-wire.md#cortexdb-behaviours-the-engine-is-shaped-around):

- append-only, a body `idempotency_key` remembered for ever (same key, same
  body is a replay; same key, different body is `409 IDEMPOTENCY_CONFLICT`;
  forget does not release it);
- the listing is newest first, emits **every event twice**, counts the copies
  in `limit`, ignores unknown query parameters, and pages by offset cursor;
- the forget selector reads only `memory_ids`; an empty selector without
  `confirm_all` is refused, and one with `confirm_all` is refused as
  ambiguous;
- recall renders text as `[role] {...}`, honours `view: "descend"`, metadata
  label filters, and the events budget, and returns `pack_id: "pack_test"`;
  the answer route requires that `use_pack_id`.

The **hosted** double additionally wraps bodies in `{success,data}`, reports
failures with `errorCode`, refuses a scope outside the memory API's grammar
(`type:id` segments), takes an `Idempotency-Key` claim per write (any replay of
a claimed key is a 409, never forwarded), refuses a repeated `labels=`
parameter, and enforces the strict answer schema.

**Knobs** make either fail the ways the real stacks fail, so tests aim at one
failure at a time: `fail_all` (every request), `accept_token` (the only token
accepted), `hide_listing_for` (empty listings), `rate_limit_events`,
`rate_limit_experience` (the backend's own 429 before any claim),
`apply_then_fail` (applied then 503), `claim_then_fail` (claimed, not applied,
502), `fail_nth_experience`, `rate_limit_forget`, `recall_down`, and
`arm_after_write` (rate limit or hide the reads a write makes *after* it is
sent). `Seen` records every request, the `Authorization` headers, the
idempotency pairs, and every recall, answer and forget body for assertions.

## Tools tests

`crates/tinymemory-tools/tests/`:

- `tool_contracts.rs` freezes the **tool names and argument schemas a model
  sees**. `fixtures/tool_contracts.json` is the serialised `specs()` of the
  writable tools over the reference engine (every fetch mode). A schema
  change tells every host's model something new, so it must be deliberate. To
  regenerate after an intended change:

  ```sh
  BLESS_TOOL_CONTRACTS=1 cargo test -p tinymemory-tools --test tool_contracts
  ```

  then review the diff. Without the variable the test fails on any mismatch
  and prints the new specs.
- `tools_roundtrip.rs`: every tool round-trips through `MemoryTools::call`
  against the reference engine, and read-only tools neither list nor run the
  writes.
- `tools_scoping.rs`: the security invariants: the namespace and reach are the
  host's, and a model cannot read, write or forget outside its scope. Its tree
  is `team:acme/agent:a`, its sibling `agent:b`, their parent and the root.

## Integrations tests

`crates/tinymemory-integrations/tests/` (public API only):

| File | Needs | Covers |
| --- | --- | --- |
| `feature_surface.rs` | `full` | the modules compose: scrub an item, store it in the reference engine, run the conformance suite, compile a context |
| `documents_office.rs` | `documents-office` | `OfficeConverter` prepends to the default `ConverterChain` and supports PDF, DOCX, XLSX, PPTX |
| `reader_dispatch.rs` | `sources` (`sources-network` for one test) | local readers are constructed for timers, network readers only for requests |
| `legacy_import.rs` | `legacy-import` | importing v1 workspaces, every mapping, ordering and resumption |
| `live_cortexdb.rs`, `office_live.rs` | `cortex` (+ `documents-office`) | a real CortexDB; skipped unless configured |

### Legacy import fixtures

`tests/support/mod.rs` builds v1 TinyCortex workspaces in temporary
directories (`tempfile`) using the **verbatim v1 DDL**: `MEMORY_DDL` (the
current `memory.db`), `OLD_MEMORY_DDL` (an early one without the `taint`,
`logical_namespace` and `tool_calls_json` columns and the later profile
columns), and `CHUNKS_DDL` (`memory_tree/chunks.db`, plus the migrated
`content_path` column). Helpers: `workspace(ddl)`, `doc`, `turn`, `facet`,
`chunk_store(root)` and `chunk` insert rows. `legacy_import.rs` builds a `rich()`
workspace exercising every section and edge case and imports it into the
reference engine.

## Live tests

The doubles check the wire cheaply; the live tests prove it against a real
CortexDB, pinned in `integration/cortexdb/` (see its README). They are **skipped
unless configured**, so a plain `cargo test` never needs Docker.

| Variable | Used by | Meaning |
| --- | --- | --- |
| `TINYMEMORY_LIVE_CORTEXDB_URL` | `live_cortexdb.rs`, `office_live.rs` | base URL of a live CortexDB; unset skips the test |
| `TINYMEMORY_TEST_CORTEX_KEY` | both | the bearer; defaults to `tinymemory-cortex-test`, the harness's key |
| `CORTEXDB_VERSION` | `scripts/cortexdb-live.sh` | server release to boot (default `v0.10.4`; `v0.9.9` checks the older one) |
| `CORTEXDB_PORT` | the script and compose file | published port (script default 3142; compose default 3141) |
| `KEEP` | the script | leave the server running afterwards |

```sh
./scripts/cortexdb-live.sh                          # boot, test, tear down
KEEP=1 ./scripts/cortexdb-live.sh                   # leave it running
TINYMEMORY_LIVE_CORTEXDB_URL=http://127.0.0.1:3141 \
  cargo test -p tinymemory-integrations --test live_cortexdb -- --nocapture
TINYMEMORY_LIVE_CORTEXDB_URL=http://127.0.0.1:3141 \
  cargo test -p tinymemory-integrations --features documents-office --test office_live -- --nocapture
```

- `live_cortexdb.rs`: (1) `the_live_server_upholds_the_contract` runs the
  conformance suite; (2) `documents_conversations_and_learnings_round_trip_into_context`
  stores a document, a conversation with a tool call and a learning, lists
  them back (polling up to 60s, since CortexDB indexes asynchronously), checks
  metadata filters narrow, `fetch` finds the document, `recall` answers with
  citations, compiles `context.md` carrying the learning (within the token
  budget), and forgets the run (3 items).
- `office_live.rs`: converts a generated DOCX with `OfficeConverter`, stores
  it through the engine, and lists it back with the extracted text.

The harness runs `mock-inference`, a deterministic OpenAI-compatible double for
CortexDB's embeddings, extraction and answer models, so it needs no
credential; it is a wiring fixture, not a quality benchmark. Quality is
measured by the agent memory eval instead (`./scripts/memory-eval.sh`, with
`MODELS=openrouter` for real models); see [`docs/evals/`](../evals/README.md). Live tests name
their file `live_*` (or `*_live`) so they are easy to exclude, and write only
under a workspace unique to the run.

## Adding tests

**A new engine** (any `MemoryEngine`):

1. Run the conformance suite against it in a test. Against a local or
   in-process engine, a test in the engine's `*_tests.rs` is enough:

   ```rust
   #[tokio::test]
   async fn the_engine_upholds_the_contract() {
       tinymemory_api::conformance::run(&engine).await.unwrap();
   }
   ```

   Enable `tinymemory-api`'s `conformance` feature as a dev-dependency. For an
   HTTP engine, run it against a loopback double, as `cortex/conformance_tests.rs`
   does.
2. Reproduce the backend's quirks in the double rather than tidying them away,
   and add a knob per failure mode.
3. Add unit tests per module and test each error variant's mapping (every new
   `Error` source needs a test that produces it); cover the failure paths, not
   just the happy one.
4. Gate any live or network test behind an environment variable, skip cleanly
   when it is unset, and name it `live_*`.
5. Register the engine in the registry and add it to `list_engines` tests
   (`registry/mod_tests.rs`).

**A new integration module** in `tinymemory-integrations`:

1. Put it behind a feature of (nearly) the same name in `Cargo.toml`, gate the
   module in `lib.rs`, add the feature to `full`, and add a row to the CI
   feature matrix so it is tested on its own.
2. Unit tests in `<module>_tests.rs` beside each `mod.rs` (each starting with a
   `//!` line), integration tests in `tests/` with `required-features`.
3. Keep dependencies optional and gated by the feature; do not add anything the
   contract crate must not have (see the dependency guard).
4. Document it: `//!` on every `mod.rs`, rustdoc with `# Errors` and `# Panics`
   on public items, and a module `README.md` if it is complex.

**Tools**: when a change alters a tool's name or schema, regenerate and review
`tool_contracts.json` as above.

**Doc changes**: run `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
--all-features` and `cargo test --doc -p <crate> --all-features`.
