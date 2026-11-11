# tinymemory-tools

The agent-facing side of TinyMemory, over any `tinymemory_api::MemoryEngine`:

- **`tools`**: seven memory tools a host offers its model, as
  runtime-neutral specs (name, description, JSON Schema) plus one `call`
  entry point that runs them and returns compact JSON.
- **`context`**: the `context.md` compiler, a token-budgeted brief a host
  injects at the start of a session.
- **`recall`**: holistic recall, the one read across several scopes,
  rendered as one budgeted `ContextPack`. `context.md` is one preset of it.
- **`layout`**, **`brain`**, **`lifecycle`**, **`background`**: the agent
  memory lifecycle. `MemoryLayout` is the standard tree: brain documents by
  source, per-agent conversations, and learnings. `Brain` ingests documents.
  `AgentMemory` serves session start, pre-turn context, post-turn logging and
  compaction. `TurnHooks` composes those calls with host-supplied clock,
  engine, model, jobs, and citation ports. It starts no tasks; its owned
  pre-turn future can run in the existing bounded `Prefetcher`. `InPromptFrom`
  tracks the live prompt window, and `date_hint` parses optional model date
  ranges. `BackgroundJob` carries the belief builds those calls hand back.
  See [`docs/architecture/lifecycle.md`](../../docs/architecture/lifecycle.md)
  and the runnable `examples/agent_loop.rs` and `examples/brain.rs`.
- **`prefetch`**: bounded runtime-neutral background refreshes. Hosts can use
  `TurnCache<K>` for completed lifecycle packs and `TurnCacheRegistry<K>` to
  invalidate them across host-owned contexts. See the
  [prefetch module guide](src/prefetch/README.md).
- **`pending`**: a runtime-neutral durable FIFO for tool writes, plus weakly
  cached owner barriers and per-partition drain locks. The host supplies each
  partition's `ListStore`, maps its own owner and storage keys, and runs the
  engine executor. New owner epochs use a UUID so a recreated gate cannot
  mistake an older persisted fence epoch for its own.
- **`ops`**: validated host request builders, explorer paths, common read
  limits, learning metadata shaping and deletion helpers. `MemoryOps` applies
  a concrete `Confinement` policy before a request reaches an engine.
- **`backfill`**: builds and runs a resumable conversation backfill over a
  host `ThreadSource` and `CounterStore`. Consent, the thread-store adapter,
  and task scheduling remain with the host.
- **`layout_migration`**: migrates a legacy tree through a host `LayoutHost`.
  `MigrationService` owns run-slot and tick policy while a host
  `MigrationSpawner` supplies the executor; import and migration starts share
  a synchronous `StartGate`.
- **`run_slots`**: keyed RAII reservations shared across service instances.
  Dropping a run task releases its slot after cancellation or spawn rejection.

The crate has no tool-runtime dependency (no MCP, no `tinytools`). A host
adapts `ToolSpec` to whatever runtime it uses; see
[Adapting to a tool runtime](#adapting-to-a-tool-runtime). The design is
written up in [`docs/architecture/tools.md`](../../docs/architecture/tools.md).

The `fs-claim` Cargo feature opts into `layout_migration::FsLegacyClaim`, a
filesystem marker implementation of `LegacyClaim` for shared local trees. It
adds only the optional `url` and `tempfile` dependencies; the default build
keeps the claim implementation behind the host port.

The `schema` Cargo feature enables JSON Schema derives for host-facing types
such as `lifecycle::RecallSettings`. It adds optional `schemars` support; the
default build remains schema-free.

`lifecycle::RecallSettings` carries defaulted host recall and lifecycle timing
settings. It converts to `RecallPolicy` without applying the host's separate
identity-level `enabled` flag; `RecallSettings::validate` checks the settings
before a host applies them. `AgentMemory::preview` returns a typed turn/session
mode and pack without logging, while `AgentMemory::agents` lists conversation
counts within its layout root.

## The tools

| Tool | Kind | Arguments | Result |
| --- | --- | --- | --- |
| `memory_recall` | read | `question`, `filter?`, `limit?`, `instructions?`, `refers_to?` | `{answer, citations: [...]}` |
| `memory_fetch` | read | `query`, `mode?`, `filter?`, `limit?`, `cursor?`, `refers_to?` | `{hits: [...], next_cursor?}` |
| `memory_list` | read | `filter?`, `limit?`, `cursor?` | `{items: [...], next_cursor?}` |
| `memory_get` | read | `ids` | `{items: [...], missing: [ids]}` |
| `memory_explore` | read | `facet`, `filter?`, `limit?` | `{facet, buckets: [{value, count}], total, missing, more_buckets, truncated}` |
| `memory_store` | write | one of `learning` / `document` / `conversation`, `tags?` | `{id, replayed}` |
| `memory_forget` | write | `ids` **or** a non-empty `filter` | `{forgotten, skipped: [ids]}` |

Details:

- `limit` is `1..=100`, default `10`, across recall, fetch and list. Explore
  defaults to `50` buckets and accepts `1..=500`. `ids` lists `1..=200` ids.
- `memory_fetch`'s `mode` enum lists exactly the engine's
  `EngineDescriptor::fetch_modes`; with no mode named, `hybrid` is used when
  served, otherwise the engine's first mode. An engine serving no mode gets no
  `memory_fetch` tool at all.
- `memory_store` takes `learning: {text, learning_kind?, confidence?,
  evidence?}` (kind defaults to `fact`, confidence to `0.8`),
  `document: {title?, text}` or `conversation: {turns: [{role, text}]}`.
  Storing the same memory twice is a replay, not a duplicate.
- `memory_explore`'s facets are `kind`, `source`, `source_id`, `workspace`,
  `folder`, `file_path`, `language`, `repo`, `url`, `thread`, `agent`,
  `tool_call` and `tag`. The `namespace` facet is deliberately absent.
- The model-facing `filter` is a subset of `MetaFilter`: `kinds`, `sources`,
  `tags_any`, `workspace`, `folder`, `file_path`, `repo`, `url`, `thread_id`,
  `agent_id`, `observed_after` and `observed_before` (RFC 3339). It never has
  a namespace or a reach.
- `refers_to` (`{from, to}`, local `YYYY-MM-DD` dates) is the days the
  question is about: hits from them rank first and nothing is dropped
  (`tinymemory_api::TimeHint`), read in the host's zone
  (`MemoryTools::in_zone`). Unlike `observed_after`/`observed_before`, which
  filter by when a memory was recorded, it keeps a memory recorded on Monday
  about Tuesday.
- A hit renders as `{id, kind, text, score, confidence?, meta}` where `meta`
  is a subset: `source {kind, id?}`, `file_path`, `url`, `thread_id`, `tags`,
  `observed_at`. Scores are rounded to four places. The namespace is never
  rendered.

The exact names and schemas are frozen in
[`tests/fixtures/tool_contracts.json`](tests/fixtures/tool_contracts.json).
A change to them changes what every host's model is told, so the
`tool_contracts` test fails until the fixture is regenerated on purpose:

```sh
BLESS_TOOL_CONTRACTS=1 cargo test -p tinymemory-tools --test tool_contracts
```

## Single-name facade and run budget

SingleTool gives a host one registered name with an action field for recall,
fetch, learn or forget. Its action policy classifies recall and fetch as
read-only and concurrency-safe, and learn and forget as writes that require
exclusive scheduling. Unknown actions are treated as writes.

Reads reuse the seven-tool argument and request shaping. The single facade
returns recall evidence capped near 2,500 tokens at line boundaries, excludes
the live prompt window of the current thread, and pages fetch results five at
a time with the engine cursor. A mismatched prompt window falls back to
excluding the whole current thread. When no current thread is set in the
trusted scope, no thread is excluded.

Writes require a WriteSink, which is the host's prequeue policy hook.
PendingWriteSink adapts the existing PendingWrites queue, its ListStore,
Binding and WriteGate; it does not add a second queue implementation or scrub
learning content. The host must wrap it with the existing scrub and policy
checks before forwarding a write, so rejected or scrubbed learning content is
handled before local persistence. An acknowledgement means the operation is
durable in the local queue, not that the engine has applied it. The host keeps
the queue partition, gate and binding, schedules and executes drains, sends
settled notifications, and owns the exclusive barrier that makes an erase
wait for an in-flight engine write. Without a sink, the single facade returns
an error and never writes directly to the engine.

RunBudget takes a Clock from the host and allows at most eight calls and thirty
seconds per run, with a fifteen-second timeout per call. It refunds unused
reservation time when an action finishes or is cancelled, opens the run's
circuit on timeout, and retains at most 128 run records. A Denied refusal means
the action was never polled. TimedOut means it was polled and may have changed
memory; hosts should map only Denied to refusal metadata and report TimedOut as
an ordinary tool error. The budget contains no host product copy or
tool-runtime metadata.

## Scoping invariants

This crate exists so that a model can never choose whose memory it touches.
An earlier tool layer let the model pass a namespace, which let one tenant's
agent read another's memory. Here, the namespace and the reach are fixed by
the host in a `ToolScope` and are never tool arguments:

| Field | Meaning |
| --- | --- |
| `place: Namespace` | the node every stored item is written to |
| `reach: Option<Reach>` | the nodes every read (and forget) is confined to; `None` reads every namespace |
| `writes: bool` | whether `memory_store` and `memory_forget` are offered |

The single-name facade also uses the trusted workspace, current thread, agent
id and provider-assigned tool-call id in ToolScope. It stamps these values on
learnings and uses the thread id to validate a supplied prompt window. These
fields do not change the behavior of the original seven named tools.

Adding these fields affects downstream ToolScope struct literals that name
every field. Such callers should use a struct update from ToolScope::default()
or construct a scope with ToolScope::at.

What the tools enforce:

1. **Writes land at `place`.** `memory_store` builds the item's metadata
   itself: namespace `place`, source `agent`, the model's tags, and
   `observed_at` set to the time of the call.
2. **Reads use the scope's reach.** Every recall, fetch, list and explore
   filter has its `reach` overwritten with the scope's; `memory_get` passes it
   as `GetRequest::reach`, and ids outside it come back as `missing`.
3. **Forget cannot reach out.** By ids, the ids are first read back with
   `get` under the reach and only those found are forgotten; the rest are
   returned as `skipped` and survive. By filter, the model's filter must set
   at least one field (a reach alone would mean "everything in reach") and is
   then confined to the reach.
4. **Attempts are refused, not ignored.** A `namespace` or `reach` key
   anywhere in the arguments, at any depth, is an `Error::InvalidRequest`
   saying the host fixes it. Every schema object sets
   `additionalProperties: false`, and any other unknown key is refused too.

Note that a reach with `inherit` (the default from `Reach::of`) includes the
node's ancestors, so a forget may remove memory the agent shares with its
team or the root. A host that wants forgets confined to the agent's own node
sets `Reach::exact(place)`, or offers read-only tools.

### Building a scope

```rust,ignore
use std::sync::Arc;
use tinymemory_api::{Namespace, Reach};
use tinymemory_tools::MemoryTools;

// Single tenant: root, reads everything, writes enabled.
let tools = MemoryTools::new(engine.clone());

// One agent: writes to its node, reads it and its ancestors, never a sibling.
let agent = MemoryTools::new(engine.clone()).placed_at("team:acme/agent:writer".parse()?);

// Read-only, team-wide.
let auditor = MemoryTools::new(engine)
    .placed_at("team:acme".parse()?)
    .reach(Reach::subtree("team:acme".parse()?))
    .read_only();
```

`placed_at` resets the reach to `Reach::of(place)`, so a placed scope never
reads more than its own branch by accident; call `reach` after it to change
that. `MemoryTools::with_scope` takes a `ToolScope` directly.

## Errors

`MemoryTools::call` returns `tinymemory_api::Result<serde_json::Value>`:

- `Error::InvalidRequest` for an unknown tool name, arguments that are not an
  object, a host-fixed key, an unknown key, or a missing, mistyped or
  out-of-range value. Messages are lowercase and name the tool and the field
  (`memory_list: \`filter.kinds\` \`memo\` is not an item kind`), so they can
  be shown to the model as the tool's error output.
- `Error::Unsupported` for a write tool on read-only tools, and for
  `memory_fetch` on an engine serving no fetch mode. The call is well formed;
  the tools simply do not offer the operation, which is what this variant
  means across the contract.
- Anything the engine returns.

## Pending writes

`pending::PendingWrites` bounds each durable partition independently using
`Limits::max_entries` and `Limits::max_bytes`. Hosts may impose an additional
limit on the number of on-disk partitions because naming and enumerating those
files belongs to the storage adapter. `OwnerRegistry` holds weak references:
active mutations and fences retain shared owner state, and idle owners are
reclaimed on the next lookup. `PartitionLocks` applies the same lifetime rule
to drain serialization, with a separate cancellation-safe async lock per
partition key. `PendingPartitions` shares the in-memory `PendingWrites` queue
for an active partition and recreates it from a host-supplied `ListStore`
factory after eviction. Both partition registries accept exact `Eq + Hash`
keys, including `PathBuf`, so the host does not need to stringify filesystem
paths.

The queue's `Entry` is the current persisted representation. A host that has
older durable encodings should keep decoding them into `Entry` while those
records may still exist; dropping a legacy decoder can make an accepted write
unreadable after an upgrade.

## Adapting to a tool runtime

`ToolSpec` is three fields: `name`, `description` and `parameters` (a JSON
Schema object). Most runtimes want exactly that:

- **MCP**: `name` → `name`, `description` → `description`,
  `parameters` → `inputSchema`. On `tools/call`, pass `arguments` to
  `MemoryTools::call` and return the result serialised as text content;
  return an error result with the error's message on `Err`.
- **OpenAI-style function calling**: `{"type": "function", "function":
  {"name", "description", "parameters"}}`. The schemas are closed objects, so
  they also suit strict mode where the runtime accepts optional properties.
- **Anthropic tool use**: `name`, `description`, `input_schema`.

A typical adapter:

```rust,ignore
for spec in tools.specs() {
    runtime.register(spec.name, spec.description, spec.parameters);
}
// When the model calls a tool:
let output = match tools.call(&call.name, call.arguments).await {
    Ok(value) => value.to_string(),
    Err(error) => format!("error: {error}"),
};
```

Build one `MemoryTools` per agent session from the session's identity, never
from anything the model said. `specs()` is cheap; call it per session so a
read-only or differently scoped agent sees only its own tools.

## `context.md`

`tinymemory_tools::context::compile(&engine, &ContextSpec::default())` recalls
one question per `Brief` (four defaults: about the user, active work,
preferences and standing instructions, recent important events), lists the
stored learnings newest and most confident first, and renders markdown with
`generated_at`, `engine`, `tokens` and `refs` frontmatter. The document fits
`budget_tokens` (default 1,500, four characters per token): learnings are
trimmed first, then the last brief. A failing brief is skipped and logged, and
an empty engine yields an empty document. `ContextSpec::reach` confines the
brief to one agent's part of the tree. Details:
[`docs/architecture/tools.md`](../../docs/architecture/tools.md#contextmd).

## Memory identity and layout

Hosts adapt their configuration into `IdentityPins` and resolve the acting
`Identity` for each operation. Resolution preserves the documented precedence:
host-wide pins, a definition pin, the agent's team, then the default root and
agent id. With `IdentityPins::v3`, conversations are pooled at `ws:main` below
the resolved root.

`MemoryLayout::with_pooled_conversations` accepts an **absolute** namespace
inside the layout root, such as `team:acme/ws:main`; it does not append the
root. `pooled_conversations_node()` returns the same absolute namespace for
host migration placement. Callers upgrading from the previous relative-node
contract must include the layout root when constructing the node.

## Layout

```text
src/
├── lib.rs              # crate docs and re-exports
├── context/            # the context.md compiler
├── pending/            # durable write FIFO, owner fences, partition locks
└── tools/
    ├── mod.rs          # MemoryTools, ToolScope, dispatch
    ├── spec/           # ToolSpec, tool names, JSON Schemas, limits
    ├── args/           # strict argument reading and the model filter
    ├── read/           # recall, fetch, list, get, explore
    ├── write/          # store, forget
    └── render/         # compact result JSON
tests/
├── tools_roundtrip.rs  # every tool against the reference engine
├── tools_scoping.rs    # the scoping invariants
├── tool_contracts.rs   # frozen names and schemas
└── fixtures/tool_contracts.json
```
