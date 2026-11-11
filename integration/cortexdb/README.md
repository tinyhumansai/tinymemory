# Live CortexDB harness

A real CortexDB server for the `cortexdb` engine's live tests
(`crates/tinymemory-integrations/tests/live_cortexdb.rs`), so the wire is proven
against the server and not only against the HTTP doubles in
`crates/tinymemory-integrations/src/cortex/testing/`.

```sh
./scripts/cortexdb-live.sh                          # boot, test, tear down
KEEP=1 ./scripts/cortexdb-live.sh                   # leave it running on :3142
CORTEXDB_VERSION=v0.9.9 ./scripts/cortexdb-live.sh  # an explicit older release
CORTEXDB_IMAGE=registry.example/cortexdb@sha256:... ./scripts/cortexdb-live.sh
```

Or by hand:

```sh
docker compose -f integration/cortexdb/docker-compose.yml up -d --build
TINYMEMORY_LIVE_CORTEXDB_URL=http://127.0.0.1:3141 \
  cargo test -p tinymemory-integrations --test live_cortexdb
docker compose -f integration/cortexdb/docker-compose.yml down --volumes
```

Without `TINYMEMORY_LIVE_CORTEXDB_URL` the live tests skip, so a plain
`cargo test` never needs Docker.

The script runs its own Compose project (`tinymemory-cortexdb-test`) on port
3142 by default and removes it, volumes included, when it ends. It refuses to
start when something already answers on its port, so it never replaces a
server you started by hand (which uses the compose file's own project on
3141).
The lifecycle target runs its tests serially because concurrent test cases
mutate authorization and derived-layer state on the same live CortexDB server.
The target still runs every assertion against the real server.

## What runs

- `cortex`: `cortexdb/cortexdb` pinned to the verified v0.10.4 multi-architecture
  digest `sha256:636e0ed0b8cd4ae2f307e3a90f99050f7879f73c556b1ccbbabf2d3a383398fc`
  (override with `CORTEXDB_IMAGE`, or use `CORTEXDB_VERSION` in the live script),
  listening on `127.0.0.1:3141` (`CORTEXDB_PORT`) with the static bearer
  `tinymemory-cortex-test` (`TINYMEMORY_TEST_CORTEX_KEY`).
- `mock-inference`: a deterministic OpenAI-compatible double
  (`mock_inference.py`) for CortexDB's embeddings, extraction and answer
  models, so the harness needs no credential. Point `CORTEX_INFERENCE_URL` and
  `CORTEX_INFERENCE_KEY` at a real compatible endpoint to exercise real models;
  the double is a wiring fixture, not a quality benchmark.

## Flag profiles

The server's feature flags live in `flags/`: `baseline.env` holds the
harness's standing ones, and every other profile is loaded on top of it and
names only what it changes. Compose reads `CORTEX_FLAGS_FILE` (default: the
baseline) and `CORTEX_TOML` (default `cortex.toml`; `cortex.no-learning.toml`
pushes the background learning jobs out to once a year).
`scripts/memory-flag-sweep.sh` runs the memory eval under each profile and
compares them; see [docs/evals/cortex-flags.md](../../docs/evals/cortex-flags.md).

Each profile's header says what it changes, which KPI it targets
(`# targets:`), where CortexDB documents it (`# docs:`) and any credential
it needs (`# requires:`). A variable the compose file sets in
`environment` wins over a profile; the exceptions it interpolates instead
(`CORTEX_VERIFIER_API_KEY`, `CORTEX_FALLBACK_KEY`, `CORTEX_TOML`) are
honoured because the sweep also sources the profile into its shell.

## What the tests prove

- The shared conformance suite (`tinymemory_api::conformance::run`) passes against
  the live server.
- A document, a conversation with a tool call and a learning, each with its
  metadata, store and list back; metadata filters narrow; hybrid `fetch` finds
  the document; `recall` (CortexDB's answer route) answers with citations;
  `tinymemory_tools::context::compile` builds a `context.md` that carries the
  learning; and a filter `forget` removes all three.
- With `documents-office`, a generated DOCX is converted by `OfficeConverter`,
  stored through the CortexDB engine, and listed back with its extracted text.

OpenHuman runs its own end-to-end test against this harness through the real
core binary (`scripts/test-memory-cortexdb-live.sh` in that repository).
