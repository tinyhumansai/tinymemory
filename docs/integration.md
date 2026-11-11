# Integrating an agent with TinyMemory

A guide for anyone wiring an agent host to TinyMemory's memory API, such as
OpenHuman or another runtime. It covers what to call, when, and what each
call costs.

The contract behind it is [`specs/agent-memory.md`](specs/agent-memory.md).
The design is [`architecture/lifecycle.md`](architecture/lifecycle.md), and
how well it works is in [`evals/agent-memory.md`](evals/agent-memory.md).

## What you get

- **One object per agent** (`AgentMemory`) with four calls that fit the
  agent loop:
  - `start_session`, when a session starts or resumes;
  - `pre_turn`, before the model runs;
  - `post_turn`, after the model replies;
  - `recall_for_compaction`, when the prompt overflows.
- **A shared brain** (`Brain`) of documents filed by source type (pdf,
  markdown, notion, github, web), visible to every agent.
- **Context packs.** Every read returns one token-budgeted markdown block
  for the prompt, drawn from learnings, the brain, this agent's history and
  the team's conversations. The beliefs the engine built are merged into
  the learnings.
- **Background work as values.** Belief builds and deferred ingests come
  back as `BackgroundJob` values the host runs when it likes. The library
  never spawns a task.
- **Engine independence.** Everything is written against
  `Arc<dyn MemoryEngine>`. Moving from the reference engine to CortexDB
  (either wire), or to an engine of your own, changes one line.

## 1. Add the dependency

Nothing is published to crates.io. Take the crates by git, pinned to a
release tag:

```toml
[dependencies]
tinymemory-api = { git = "https://github.com/tinyhumansai/tinymemory", tag = "vX.Y.Z" }
tinymemory-tools = { git = "https://github.com/tinyhumansai/tinymemory", tag = "vX.Y.Z" }
tinymemory-integrations = { git = "https://github.com/tinyhumansai/tinymemory", tag = "vX.Y.Z", features = ["cortex", "brain", "documents-office"] }
```

| Feature (`tinymemory-integrations`) | Gives you |
| --- | --- |
| `cortex` (default) | the CortexDB engine, both wires, plus `registry` and `config` |
| `brain` | `brain_document`: a file converted and filed under its source |
| `documents-office` | PDF, DOCX and XLSX conversion for the brain |
| `full` | everything |

For tests and offline development, enable `tinymemory-api`'s `conformance`
feature as a dev-dependency to get `ReferenceEngine`, an in-memory engine
that needs nothing.

## 2. Build an engine

```rust
use std::sync::Arc;
use tinymemory_api::MemoryEngine;
use tinymemory_integrations::cortex::{CortexCredential, CortexEngine};

// A CortexDB server, direct.
let engine: Arc<dyn MemoryEngine> =
    Arc::new(CortexEngine::direct("https://cortex.example.com", CortexCredential::api_key(key))?);
```

Or build it from configuration, which keeps the engine choice out of code:

```rust
use tinymemory_integrations::{EngineCredential, MemoryConfig};

let config: MemoryConfig = serde_json::from_value(serde_json::json!({
    "engine": "cortexdb",                       // or "tinyhumans"
    "engines": { "cortexdb": { "endpoint": "https://cortex.example.com" } }
}))?;
let engine = config.build(EngineCredential::Static(key))?;
```

A host that must attribute its requests (the TinyHumans backend expects an
`x-sdk-name` header naming the product) fixes them per engine, in
configuration (`"engines": { "tinyhumans": { "headers": { "x-sdk-name":
"my-product" } } }`) or with `CortexEngine::with_default_headers`. They ride
every request; the transport refuses `Authorization`, `Idempotency-Key`, the
actor header and the headers the HTTP stack sets, so the credential stays
the `EngineCredential`'s alone.

To keep one person's memory as one subtree, give a CortexDB engine a scope
root (`"engines": { "cortexdb": { "scope_root": "org:42", "scope_owner":
"user:42" } }`, or `CortexEngine::with_scope_root`): layout v3, see
[cortex-layout.md](architecture/cortex-layout.md). A `tinyhumans` engine
takes `"tenant_root": true` instead, since the backend pins the tenant's
`org:<id>`. Without a root, the legacy `app:tinymemory` tree is kept.
Switching moves nothing.

Memory an earlier layout wrote below a `user:<id>` root stays readable and
forgettable only while the engine names that root as well, so during the move
configure both: `"engines": { "cortexdb": { "scope_root": "org:42",
"scope_owner": "user:42", "retired_scope_root": "user:42" } }` (or
`CortexEngine::with_retired_root`). Writes go to the new root only; once the
memory has moved, drop `retired_scope_root`.

To record who actually said or did something, set `"observed_actor": true`
on a `cortexdb` engine and fill `MemoryMeta::observed_actor` on items another
person wrote (an email's sender, `user:<address>`), or `PreTurn::observed_actor`
on a user turn someone other than the owner said (a channel's sender,
`user:+15551234567`). Off by default; see
[cortex.md](architecture/cortex.md#attribution).

| Engine id | Where it runs | Consolidation (belief builds) |
| --- | --- | --- |
| `cortexdb` | a CortexDB server, `v1/*` routes | automatic on the managed API (`api-v1.cortexdb.ai`), on demand elsewhere: `v1/beliefs/build`, built within the request; `EngineSettings::consolidation` overrides |
| `tinyhumans` | the hosted TinyHumans backend, `memory/*` routes | on the server's own schedule |
| `ReferenceEngine` | in process (tests) | on demand, a deterministic toy |

`engine.descriptor()` tells you, at run time, which fetch modes an engine
serves and how it consolidates.

## 3. Lay out the tree

One `MemoryLayout` per tenant or workspace. Everything lives below its root:

```rust
use tinymemory_tools::MemoryLayout;

let layout = MemoryLayout::new("team:acme".parse()?)?;   // or MemoryLayout::default()
```

```text
team:acme
├── source:files, source:notion, …  the brain: documents by connector, no agent id
├── agent:support-01               one agent's conversations (a turn per item)
├── agent:coder-42
└── (the root itself)              shared learnings; beliefs are built in every scope
```

Two layouts with different roots never see each other's memory. The eval
checks this (`isolation`, 0 leaks). Use one root per tenant.

## 4. The agent loop

```rust
use tinymemory_tools::{AgentMemory, PostTurn, PreTurn, RecallPolicy, SessionStart};

let memory = AgentMemory::new(engine.clone(), layout.clone(), "support-01")?
    .with_policy(RecallPolicy::default());

// Session start or resume: put the pack in the system prompt.
let initial = memory
    .start_session(SessionStart { thread_id: Some(thread.clone()), focus: None })
    .await?;

// Every user turn.
let mut pre = PreTurn::new(&thread, index, &user_text);
pre.in_prompt_from = first_turn_still_in_prompt;   // turns from here on are left out
pre.at = Some(message_sent_at);                    // the message's own time
let context = memory.pre_turn(pre).await?;          // never fails on an engine error
let reply = model.generate(&system, &context.pack.markdown, &user_text).await?;

let mut post = PostTurn::new(&thread, index + 1, &reply.text);
post.tool_calls = reply.tool_calls;                 // name and id only, see the rules
post.at = Some(reply_at);
let report = memory.post_turn(post).await?;
queue.extend(report.jobs);                          // belief builds, run off the turn
```

| Call | Writes | Reads | Typical cost |
| --- | --- | --- | --- |
| `start_session` | — | thread turns (if resuming), then the standard sections, newest first or ranked for `focus` | about 8 ms |
| `pre_turn` | the user turn, accepted but not yet indexed, concurrently with the read | the standard sections, ranked for the turn | 13–30 ms with local embeddings; one embedding round trip per scope with hosted ones |
| `post_turn` | the reply, accepted | — | about 2 ms |
| `recall_for_compaction` | — | an answered summary of the thread (a model runs), then the standard sections | seconds; off the hot path |
| `recall(query)` | — | the `pre_turn` read without logging | as `pre_turn` |

The **standard sections** of a pack, highest priority first, are:
Learnings (stored learnings and the engine's beliefs), Brain, This agent's
history, and Team conversations. An item appears once, in its first
section. When the pack is over budget, trimming starts from the end.

### Compaction

When the host drops turns from the prompt, pass them in:

```rust
let carried = memory
    .recall_for_compaction(Compaction { thread_id: thread.clone(), dropped, focus: None })
    .await?;
```

The summary section answers from the thread's turns. It reads as many as
were dropped, up to 24. The standard sections follow, ranked for a sample
of every dropped turn.

## 5. The brain

```rust
use tinymemory_tools::{Brain, BrainDocument, BrainSource};
use tinymemory_integrations::brain::brain_document;

let brain = Brain::new(engine.clone(), layout.clone());

// Text you already have:
let ingested = brain
    .ingest(BrainDocument::new(BrainSource::Notion, text).titled("Refund policy"))
    .await?;
queue.extend(ingested.job);                              // build this source's beliefs later (none on an `Automatic` engine)

// A file: converted, its source picked from the format (PDF → pdf, md → markdown).
let document = brain_document(&converters, &raw, None, MemoryMeta::default()).await?;
brain.ingest(document).await?;
```

`ingest` waits until the document is readable, which takes about 70 ms with
local embeddings and 2 s with hosted ones. Use `ingest_with(doc,
WaitFor::Accepted)`, or queue a `BackgroundJob::IngestBrain`, to skip the
wait. `search` and `forget` work per source or across the whole brain.

## 6. Background jobs

`BackgroundJob` is plain, serializable data: queue it, persist it, merge
duplicates, and run it when convenient:

```rust
let runner = memory.background();
for job in queue.drain(..) {
    let report = runner.run(job).await?;   // Done, Started, Scheduled or Skipped
}
```

- `post_turn` hands back a `BuildBeliefs` job every
  `RecallPolicy::build_beliefs_every` turns (10 by default). `Brain::ingest`
  hands one back per ingest. Neither does on an engine that declares
  `Consolidation::Automatic` (CortexDB's managed API rebuilds beliefs on its
  own within about a minute of a write); `Brain::build` and
  `AgentMemory::history_build` still ask for a refresh.
- **On CortexDB, a build is only as good as the extraction before it.**
  Beliefs are built from facts, which CortexDB extracts from every event
  with a model, in the background. With a hosted model that takes minutes.
  A build run straight after a write builds from nothing, so schedule builds
  a few minutes behind the writes. A periodic sweep works well.
- An engine that cannot consolidate answers `Skipped`, so the same host
  code runs on any engine.

## 7. Rules that matter

These come from the eval; each one cost accuracy when ignored.

1. **Pass the message's own time** as `PreTurn::at` and `PostTurn::at`.
   Packs show a turn's date (`[2026-09-15 09:01] user: …`). Without dates,
   a corrected value and its correction look alike. With them, a model
   picked the current value in 6 of 6 contradiction probes. Use the message
   time, not the call time, so a retried turn is recognised as a duplicate.
2. **Put tool results in the reply text.** `ToolCallRef` keeps a tool's name
   and id. A result the assistant does not mention is lost, and a long
   reply dilutes what it does mention. A short "tool → result" line per
   call is what the eval's agent does.
3. **Set `in_prompt_from`** to the first turn of the thread still in the
   prompt. Turns from there on are left out of the pack, so the model never
   sees the same turn twice.
4. **Keep thread ids and turn indices stable.** A turn's identity is its
   content, thread and index. A retried call with the same input is
   recognised as a duplicate and not stored twice.
5. **Never block a turn on memory.** `pre_turn` returns its pack even when
   logging fails; the reason is in `TurnContext::log_error`. Log it, and
   carry on with the turn.
6. **Run builds off the turn.** A build takes seconds per scope with a real
   model.
7. **Use one root per tenant.** Agents under one root share learnings, the
   brain and each other's conversations by design.

## 8. Tuning

`RecallPolicy` sets the shape of every pack:

| Field | Default | Meaning |
| --- | --- | --- |
| `budget_tokens` | 1200 | the pack's size, at about 4 characters per token |
| `learnings_limit` | 8 | learnings and beliefs together |
| `brain_limit` | 6 | brain documents |
| `history_limit` | 6 | this agent's turns |
| `team_limit` | 3 | other agents' turns; `0` leaves the section out |
| `build_beliefs_every` | `Some(10)` | turns between belief builds; `None` turns them off |

For a different shape altogether, build a `HolisticRecall` with your own
`ScopeSection`s and call `holistic_recall`. `context.md`
(`tinymemory_tools::context`) is one preset of it, with briefs answered by
the engine's model. It suits the top of a long-lived system prompt.

## 9. Letting the model use memory

`agent.tools()` gives `MemoryTools` scoped to that agent. It has seven
tools with JSON Schemas (`specs()`), called by name (`call(name, args)`).
Writes land at the agent's node; reads see that node and the shared root.
The model never chooses whose memory it touches.

## 10. Bringing your own engine

Implement `tinymemory_api::MemoryEngine`.

**Required:**
- `descriptor`
- `health`
- `recall`
- `fetch`
- `store`
- `forget`
- `list`

**Optional, with defaults:**

| Method | Default | Override when |
| --- | --- | --- |
| `store_with` | behaves as `store` | the engine can acknowledge a write before indexing it |
| `store_many_with` | behaves as `store_many` | the engine can acknowledge a batch before indexing it |
| `consolidate` | `Unsupported` | the engine builds beliefs (declare it in `descriptor().consolidation`) |
| `beliefs` | none | the engine keeps beliefs apart from its stored items |
| `fetch` beliefs (`FetchRequest::beliefs`) | none | as above, served from the same read as the fetch |
| `get`, `explore`, `store_many` | built from `list` and `store` | the engine can do them more cheaply |

Then run the conformance suite, the same one CortexDB passes:

```rust
#[tokio::test]
async fn my_engine_upholds_the_contract() {
    tinymemory_api::conformance::run(&MyEngine::new()).await.unwrap();
}
```

It checks every promise the lifecycle relies on:
- an accepted store answers with the item's own id;
- consolidation matches what the descriptor declares;
- beliefs are learnings within the reach asked for;
- reach never leaks across siblings.

## 11. Testing your integration

- **Unit tests:** run against `ReferenceEngine`, with no network.
- **Against a real CortexDB:** `integration/cortexdb/` runs a pinned server
  with deterministic mock models. `./scripts/cortexdb-live.sh` runs the live
  suite against it.
- **Measuring accuracy and latency:** `./scripts/memory-eval.sh` runs 12
  scenarios through a scripted agent, among them restarts, contradictions,
  tool-heavy runs, compaction, isolation, learnings, surprises and
  conflicts. `MODELS=openrouter` uses real models; see
  [`evals/README.md`](evals/README.md) for cost and model requirements.

## Checklist

- [ ] The engine is built from config, and the key is not in code.
- [ ] There is one `MemoryLayout` root per tenant.
- [ ] There is one `AgentMemory` per agent id.
- [ ] `start_session` feeds the system prompt.
- [ ] `pre_turn` runs before every model call, with `at` and
      `in_prompt_from` set.
- [ ] `post_turn` runs after every reply, with tool results in the text.
- [ ] `log_error` is logged, never raised.
- [ ] Jobs are queued and run off the turn, a few minutes behind the
      writes.
- [ ] `recall_for_compaction` is wired to prompt truncation.
- [ ] The brain is ingested per source.
- [ ] The integration passes on `ReferenceEngine` in CI and on the live
      harness before release.
