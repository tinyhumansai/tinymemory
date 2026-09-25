# The TinyMemory TinyBus module

`crates/tinymemory-module` is a `cdylib` speaking the TinyBus module ABI. A host
loads it and gets a bound memory driver without compiling the engine.

The default build exports the TinyBus v1 C symbols for dynamic loading. A host
that compiles the module into its own executable can enable the module crate's
`static-link` feature and pass `tinymemory_module::TINYBUS_MODULE_ABI_V1`,
`tinymemory_module::tinybus_module_manifest_v1`, and
`tinymemory_module::tinybus_module_init_v1` to the linked TinyBus module host.
Both modes use the same manifest declaration; the linked entry points have no
unmangled global C symbol names.

## What it buys, and what it does not

**It sheds no dependencies.** This is measured, not assumed, and it is stated
first because the obvious motivation for a module port is dependency reduction
and here that motivation does not hold.

Cutting the whole memory-engine cohort (`tinycortex`, `tinycortex-api`,
`tinymemory-core`, `tinymemory-tinycortex`) from OpenHuman, via
`scripts/dep-sim.py`:

> Historical measurement: these dependency counts and timings were captured
> before the TinyInference migration. They document why the module boundary was
> introduced, not the current dependency graph. Re-measure before using them as
> present-day performance or dependency claims.

| Profile | Before | After | Delta |
| --- | --- | --- | --- |
| kernel (`flows`) | 307 pkg / 284 names / 2 native | 297 / 278 / 2 | −6 names, **0 native** |
| product (ships) | 431 / 398 / 5 native | 427 / 394 / 5 | −4 names, **0 native** |

At that snapshot, all four names leaving the shipping profile were first-party.
`libsqlite3-sys` did not leave, because `rusqlite` had five parents there — the
host crate directly, plus `tinyagents` (its session store), `tinychannels` and
`tinyflows`.
Everything else the engine used (`reqwest`, `chrono`, `regex`, `uuid`,
`walkdir`, `sha2`, `tokio`, `git2`) was shared with surface the host kept.

**What it demonstrated was compile time on the critical path.** The historical
`cargo build --timings` snapshot showed a strictly serial chain, each link
starting as the previous one ends:

```text
tinyagents        12.8 -> 25.4   (12.6s)
tinycortex        25.4 -> 35.1   ( 9.7s)
tinymemory-core   35.1 -> 40.1   ( 5.0s)
host crate        40.1 -> 174.7
wall                             176.0s
```

In that snapshot, the engine put **14.7s directly in front of** the host's own
compilation. Removing it from the host's graph moved a full build to roughly
161s, about 8.4%. The current TinyInference-based graph has not been re-measured.

Do not re-justify this module on dependency count.

## The interface

One object, `/ai/tinyhumans/tinymemory/Memory`, interface
`ai.tinyhumans.tinymemory.Memory`:

```text
DriverId()                                                   -> String
Capabilities()                                               -> Capabilities
Health()                                                     -> MemoryHealth
Shutdown()                                                   -> ()

Store(namespace, key, content, category, session_id, taint)   -> ()
Get(namespace, key)                                          -> Option<MemoryEntry>
Forget(namespace, key)                                       -> bool
List(namespace, category, session_id)                        -> [MemoryEntry]
Namespaces()                                                 -> [NamespaceSummary]
Recall(query, limit, opts, scope)                            -> [MemoryEntry]
ExportPage(cursor, limit)                                    -> ExportPage
ImportRecords(records)                                       -> ImportOutcome
```

These are `tinymemory_api`'s `MemoryProvider` and its three mandatory
supertraits, one method per method, borrows replaced by owned equivalents. The
host binds an `Arc<dyn MemoryProvider>`, so a client that forwards each method
one-for-one **is** a complete provider with no translation layer. Nothing
cleverer is offered on purpose: batching or combined calls would put engine
semantics on the wire where two sides could disagree about them.

**No new types were needed.** Every value crossing is already `Serialize` +
`Deserialize` in `tinymemory-api` — including `MemoryCategory` (a string with a
`custom:` prefix) and `Capabilities` (a JSON array of family names), both of
which carry hand-written impls. This is why there is no `wire` *type* module
here, unlike the tinywallet module.

### Only the mandatory three

`tinymemory-tinycortex` advertises Core, Recall and Portability, because the ten
optional families are reached through engine entry points needing a host's
configuration, embedding compute and job queue. This module serves exactly that.
Serving more would advertise capabilities whose accessors return nothing, which
`audit_provider` exists to catch, and would make the host register RPC methods
that answer errors.

### Everything travels inline, but not unbounded

A TinyBus frame is JSON capped at 16 MiB. For a generated document that is a real
constraint — a byte array costs ~3.5 bytes per byte — and here it is not: memory
entries are text, ~1.1× as JSON. So there is no blob store, no chunking and no
held output. The tinydocs module's whole staging apparatus is absent.

Inline is not the same as unbounded, though, and the three list-returning methods
are bounded differently:

| Method | Caller can bound | Module bounds |
| --- | --- | --- |
| `ExportPage` | count, via `limit` + `cursor` | — paged by contract |
| `Recall` | count, via `limit` | bytes, via `MAX_RESPONSE_BYTES` |
| `List` | **nothing** | bytes, via `MAX_RESPONSE_BYTES` |

`List` is the one that needed a decision. It takes no limit and no cursor, so
entries accumulate across individually valid `Store` calls until the response
cannot cross a frame — and at that point a host cannot enumerate its own valid
stored data at all. `Recall`'s `limit` bounds the count but not the bytes: fifty
entries each holding a large document overflow just the same.

Both are therefore checked against an 8 MiB ceiling on estimated content (plus a
512-byte per-entry allowance for the surrounding JSON, so a million empty entries
trip it too) and **refuse** with `BudgetExceeded`.

Refusing rather than truncating is the load-bearing part. With no cursor, a
short list is indistinguishable from a complete one, so a silently truncated
`List` would have the caller conclude the missing entries do not exist — a wrong
answer presented as a right one. The named error instead says to narrow by
namespace, category or session, which is a query the caller can actually issue.

`BudgetExceeded` is reused rather than a new name added, because
`tinymemory_api::wire` is what both ends agree on: a new name decodes to `Other`
on any host older than the module, turning an actionable "narrow your query" into
an opaque backend failure.

`Namespaces` is left unchecked — one small summary per namespace, and a host with
enough namespaces to fill 16 MiB of them has a different problem.

## Errors

`tinymemory_api::wire` holds the name table, and **both ends use it**. One name
per `MemoryError` variant, not one per outcome class:

- the host is itself a `MemoryProvider` to everything above it, so it must hand
  its own callers a real variant. Collapsing and guessing would turn a
  `NotFound` into an `Invalid`, and `get`'s contract makes a miss `Ok(None)`
  while an `Invalid` is a failure — the guess is observable.
- `PathEscape` reports a sandbox escape and is not interchangeable with a
  malformed argument.

An unrecognised name maps to `Other`, never `Invalid`: a driver newer than the
host may name something the table lacks, and telling a caller its input was wrong
when it was not sends it into a rewrite loop. `Io` and `Serde` degrade to `Other`
because neither foreign error can be rebuilt from a string; that is pinned rather
than papered over.

## Embeddings stay in the host

The engine cannot recall without embedding, and embedding needs an inference
credential. The credential stays host-side; the module asks the host to embed.

The host serves `ai.tinyhumans.tinymemory.EmbeddingHost` at
`/ai/tinyhumans/tinymemory/EmbeddingHost`:

```text
Embed(model: String, dimensions: usize, texts: [String]) -> [[f32]]
```

The module implements `tinymemory_api::host::EmbeddingHost` over that call and
installs it with `set_embedding_host` **before** constructing the store — the
engine resolves its embedder through a process-global during construction, and a
store built first would bind the inert zero-dimension provider and write vectors
nobody can search.

This is the same split the tinywallet module makes with a signing key, and the
reasoning transfers: a credential is not the only thing that would have crossed.
The host's provider routing, rate limiting, cost accounting and BYOK policy all
hang off where embedding happens.

`resolve_api_key` returns `None` unconditionally.

### Two refusals that matter

The provider checks the reply before handing vectors to the engine:

- **wrong width** — vectors of a different dimensionality than the space they are
  being written into. Accepting them splits one embedding space in two, and
  nothing fails at the time; every vector on the wrong side becomes unsearchable
  without a re-embed.
- **wrong count** — callers pair inputs to outputs positionally, so a short reply
  attaches the wrong vector to the wrong chunk.

A zero-dimension provider is exempt: that is the engine's "semantic search off"
state and is expected to return empty vectors.

### The synchronous getters carry data

`EmbeddingHost` is synchronous except for the embed itself, and its getters are
called from deep inside retrieval and sealing call stacks where nothing can
`await`. So `ollama_base_url`, `default_cloud_embedding_model` and the
dimension-support list are passed as configuration at load time. Only `embed`
touches the bus.

### The embedder is declared in neither `requires` nor `optional`

`requires` resolves against already-loaded **modules**. This dependency is served
by the *host*, so declaring it would leave the module permanently unresolved. It
is dialled lazily on the first embed, and a host that never served it gets a
named error rather than a module that never starts.

## Configuration, and the credential that had to be stripped

Config is JSON supplied by the host (`ModuleHost::set_config` /
`load_file_with_config`). `ModuleConfig` embeds
`tinymemory_api::host::MemoryConfig` verbatim, so a field added upstream reaches
the engine without an edit and cannot drift from the host's copy.

`workspace_dir` is the only required field. Everything else has a defensible
default; a missing workspace does not, and is refused rather than silently
resolved against the process working directory.

**`MemoryConfig` contains `agentmemory_secret`, a bearer token.** So "this
struct has no credential field" was true of `ModuleConfig`'s own keys and still
not sufficient — the token is one level down, carried verbatim along with
everything else. `strip_host_credentials` removes it at setup, before anything
else touches the config, and logs a warning.

It is stripped rather than refused because this module serves the local engine
and cannot use a remote-backend token; failing the whole load would turn an
irrelevant leftover config field into a hard failure for a host whose memory
would otherwise work. A host that genuinely wants a remote memory backend should
bind that driver directly.

The general lesson: **"carried verbatim" carries credentials verbatim too.**

### Three fields the periodic sync loops need

`memory_sync_interval_secs`, `composio_mode` and `composio_entity_id` are the
module's answer to settings the engine used to read off a host `Config` it no
longer has. All three are optional on the wire, like every other field.

**The cadence is the one that failed silently.** `EngineRuntimeConfig` answered
the constant `Some(0)`, which the contract defines as *manual only*, so both
loops skipped every source on every tick — no error, no warning, nothing in the
log. It now answers the host's value, and an absent field defaults to `None`
("the user chose nothing", so the 24h fallback) rather than to `Some(0)`. The
two are not symmetrical: an over-sync is bounded and a user can see it, a
no-sync is invisible by construction. Host and module are separately released,
so whatever the default says is what an older host silently means.

**The Composio pair is routing, not access.** The mode picks which branch
`sync::pipelines::host::composio_config` takes, and the entity says whose
connected accounts a call addresses; neither authorises anything. The
direct-mode API key still does not travel — it is fetched from the host per call
— and there is no field for a backend session bearer, which is the whole reason
only direct mode can run in here.

## The two periodic sync loops, and what they lose

`setup` starts `sync::workspace::start_workspace_periodic_sync`, and in direct
mode `sync::composio::start_periodic_sync`, for the reason it starts the queue
worker pool: a host that deletes its in-process engine can start neither, and a
memory that stops updating reports "no connections" — indistinguishable from a
user who has none.

Three things had to become true first, and all three were false. The cadence is
one (above). The second is that `composio_config`'s direct branch was never
selected, because `EngineRuntimeConfig` answered an empty mode; `ComposioHost`
cannot rescue that, because its key is consulted *inside* the branch not taken.
The third is that `global::client_if_ready()` — the first line of every pipeline
run — was `None`, because this module builds its store through
`store::factories`, which never touches the global slot.

The third is closed by `global::bind`, which publishes the **already-built**
client into the global slot *and* the per-workspace cache, so all three
resolution paths converge on it. `global::init` would have built a second
`MemoryClient` over the same SQLite file — two ingestion workers, duplicate
graph extraction, duplicate embedding — which is why `bind` refuses a different
client for a workspace rather than quietly absorbing it.

**What the loops lose here.** The scheduler gate is a stub that always answers
`Normal`, so neither honours `periodic_pause_reason`'s two pauses — "Memory Tree
off" and "signed out" — and re-enabling sync no longer wakes them early instead
of waiting out the tick. Each source's own `enabled` toggle still applies.
**Backend-mode Composio sync is not started at all**, and says so once at boot,
rather than listing the user's connections every 20 minutes and failing every
due one forever.

## Two operational constraints

**Two worker threads, not one.** A recall that triggers an embed makes an
outbound call while still inside its own inbound call. One worker deadlocks on
the first semantic query.

**Eager init, not lazy.** Bringing up a store opens a database and may run
migrations. Charging that to whichever call happens to arrive first would make an
ordinary recall time out on a cold start.

## Building and testing

The crate is **its own workspace root**, and this is not cosmetic. It depends on
`vendor/tinybus/crates/tinybus`, whose manifest inherits `edition`/`version` from
`vendor/tinybus`'s own `[workspace.package]`. As a member of the tinymemory
workspace, cargo resolves that inheritance against the *tinymemory* root and
fails with `workspace.package.edition was not defined`. `exclude` does not help:
it governs membership, not the root cargo picks for a dependency's inherited
fields. Verified by defining `[workspace.package]` at the tinymemory root
temporarily, which moved the error from `edition` to `version` rather than fixing
it. It also matches tinybus's own guidance that integrations are never workspace
members, and a separately released artifact wants its own lockfile.

```sh
cargo fmt   --manifest-path crates/tinymemory-module/Cargo.toml --all -- --check
cargo clippy --manifest-path crates/tinymemory-module/Cargo.toml --all-targets -- -D warnings
cargo build --manifest-path crates/tinymemory-module/Cargo.toml --release
cargo test  --manifest-path crates/tinymemory-module/Cargo.toml --lib
```

The root workspace's `--workspace --all-targets` does **not** reach this crate,
so CI gives it its own `module` job. A cdylib that fails to build is a release
that cannot be cut, and without that job it would surface at release time rather
than on the PR that broke it.

### The loader E2E must run one test per process

```sh
TINYMEMORY_TEST_MODULE=$PWD/crates/tinymemory-module/target/release/libtinymemory_module.so \
  cargo test --manifest-path crates/tinymemory-module/Cargo.toml \
  --test module_e2e -- --ignored --exact <one test name>
```

`--ignored` alone runs them all in one process and the second **hangs**.
`Broker::spawn` binds its tasks to the runtime that created them, `#[tokio::test]`
builds a fresh runtime per test, and the module is loaded once per process and
never unloaded — so the second test finds a broker whose tasks died with the
first runtime and waits for a deadline instead of failing. Every such test is
`#[ignore]`d for that reason, not for flakiness. CI loops over them one at a time
under `timeout`.

`recall_reaches_the_host_embedder` asserts the host embedder's **call count**
rather than a ranking. Whether a query ranks an entry above
`min_relevance_score` is engine retrieval behaviour — chunking, vector store,
relevance floor — which this port does not change and which would fail the test
for unrelated reasons. A non-zero count can only happen if the module built its
store against the bus embedder, the engine asked it to embed, the request crossed
the bus, and the reply passed the width check. Note also that the module's
`log::debug!` output is invisible to the test process: a cdylib has its own
uninitialized `log` instance, so absence of a log line proves nothing.

## Trust

A loaded module is trusted in-process native code with the host's full
privileges, and TinyBus never unloads a library — replacing an artifact needs a
restart. The ABI, manifest and SHA-256 gates decide what is *admitted*, never
what is *safe*. The credential split above is a refusal to widen a boundary that
already exists, not an isolation claim: a hostile module could read the host's
keys out of process memory regardless.
