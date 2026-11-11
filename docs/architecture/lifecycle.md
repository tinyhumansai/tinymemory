# The agent memory lifecycle

How `tinymemory-tools` turns the contract into the calls a host makes around
every agent turn. The accepted behaviour is in
[`specs/agent-memory.md`](../specs/agent-memory.md).

## The pieces

| Module | Type | Role |
| --- | --- | --- |
| `layout` | `MemoryLayout`, `BrainSource` | The standard tree: brain sources, agent conversations, learnings, below one root |
| `recall` | `HolisticRecall` → `ContextPack` | The one read: several scopes at once, deduplicated, rendered within a budget |
| `brain` | `Brain`, `BrainDocument` | Global documents by source type: ingest, search, forget |
| `lifecycle` | `AgentMemory`, `RecallPolicy`, `TurnHooks` | One agent's session start, pre-turn, post-turn and compaction |
| `lifecycle::window` | `InPromptFrom` | Live, deduplicated prompt-window advancement as compaction runs |
| `lifecycle::date_hint` | `DateHintModel`, `parse_range` | Runtime-neutral date-range instruction and parsing |
| `background` | `BackgroundJob`, `BackgroundRunner` | The slow work the others hand back |
| `context` | `ContextCompiler` | `context.md`: a holistic recall preset with frontmatter |

Every module takes an `Arc<dyn MemoryEngine>`. None of them knows which engine
is behind it, so swapping engines is a one-line change in the host.

`TurnHooks` is the runtime-neutral orchestration layer around `AgentMemory`.
The host supplies an `EngineResolver`, monotonic `Clock`, optional
`DateHintModel`, `JobsSink`, and `CitationSink`. Pre-turn and compaction work
race engine work against one absolute deadline; dropping the returned future
drops the in-flight engine and model futures. The library starts no tasks and
has no Tokio dependency. A host may pass the owned `TurnHooks::pre_turn_work`
future to the existing `Prefetcher`; its `PrefetchRuntime` owns spawning,
bounds, and cancellation. Citation delivery is explicit, so a background pack
that the turn did not receive cannot emit citations.

## The tree

```text
<root>                          (Namespace::ROOT, or a host node like team:acme)
├── source:files      documents   — brain, no agent id (one node per connector)
├── source:notion     documents
├── source:github     documents
│   └── project:acme-api  documents of one collection (MemoryLayout::brain_collection)
├── agent:support-01  conversations (one item per turn)
├── agent:coder-42    conversations
└── <root> itself     learnings (shared); built beliefs land in each scope
```

On CortexDB every node becomes a scope family under `app:tinymemory/…` (see
[cortex-wire.md](cortex-wire.md#scope-layout)). For example,
`source:files/app:documents` and `agent:coder-42/app:conversations`.

**Pooled conversations.** `MemoryLayout::with_pooled_conversations(ws:main)`
keeps every agent's turns at one node (`<root>/ws:main`) instead of a node
per agent. Each turn carries its `agent_id`, so **History** is that node
filtered to the agent's id, and **Team** would be the whole node, so a
pooled layout has no Team section. Brain and learnings
stay where they were. The pool is flat: a sub-agent logs under its own id,
so unlike a node per agent, where a sub-agent's node sits below its
parent's, its turns are not in the parent's **History**, only in the whole
node.

## One turn

```text
host                         AgentMemory                     engine
 │ pre_turn(thread, i, text) ──▶│
 │                              ├─ store_with(turn, Accepted) ─▶ (≈ capture, no indexing wait)
 │                              ├─ holistic recall, concurrently:
 │                              │    Learnings  fetch ──────────▶
 │                              │    <core>     fetch ──────────▶ (one per CoreScope, exact node)
 │                              │    Brain      fetch ──────────▶
 │                              │    History    fetch ──────────▶
 │                              │    Team       fetch ──────────▶
 │                              ├─ drop: the logged turn, the thread from in_prompt_from
 │                              ├─ dedupe across sections, render within budget
 │◀── TurnContext { pack, logged | log_error }
 │ model.generate(system + pack.markdown + text)
 │ post_turn(thread, i+1, reply) ▶ store_with(reply, Accepted) ─▶
 │◀── PostTurnReport { receipt, jobs: [BuildBeliefs?] }
 │ queue jobs; later: run_background(job) ──▶ consolidate ─────▶ v1/beliefs/build
```

The hot path runs no model. `fetch` is ranked retrieval. On CortexDB it is
one recall pack per scope, with no `/answer`. Against the pinned harness a
warm `pre_turn` measured about 40 ms and a `post_turn` about 2 ms; see
`examples/cortex_agent.rs`.

`pre_turn` returns a pack even when logging fails. The failure is in
`log_error`, so a write outage degrades memory but never blocks a turn.

`PreTurn::resumed` defaults to `false` for older serialized requests and makes
resumed-thread context part of `AgentMemory::pre_turn`. The legacy
`pre_turn_resumed` helper still forces `true`; `pre_turn_dated` keeps its
explicit `resumed` argument as the override. When recall is disabled but turn
logging remains enabled, `RecallPolicy::log_only` clears read limits while
preserving the configured budget and build cadence; `TurnHooks` also clears
host-added core scopes so recall-off turns perform no memory reads.

Channel hosts may set `PreTurn::channel` and `PostTurn::channel`. Both user
and assistant conversation items then carry the same `MemoryMeta::channel`;
the channel is part of item metadata, while stable thread and turn positions
keep identical retries idempotent.

The optional date-hint model receives `lifecycle::date_hint::INSTRUCTION`, the
user's text, and the zone. The host may add its current-date line. The tools
crate parses the raw JSON through shared `ops::DateRange` and `ops::time_hint`;
an absent, invalid, or late answer leaves ordinary recall unchanged. The hint
deadline comes from the pre-turn deadline minus its margin, never a new timeout.

## Holistic recall in detail

1. **Validate.** The only error a pack can return is an invalid request.
2. **Gather** (`recall/gather.rs`) every section concurrently with
   `join_all`, which links no runtime. Each section asks for more than its
   limit, to leave room for the exclusions and for items earlier sections
   may already list:
   - `Fetch` uses the section's query, else the pack's, else `Latest`.
   - `Answer` uses recall, falling back to fetch when asked.
   - `Latest` pages the listing, then sorts by `observed_at`, confidence
     and the turn's index.
3. **Settle** the sections in order:
   - Answers pass through untouched.
   - Hit lists lose the kinds the section does not admit, the excluded ids,
     the thread window, and anything an earlier section listed. Each list
     is then cut to its limit.
   - A section left empty is reported as `SkipReason::Empty`; a failed section
     carries `SkipReason::Engine(error)` with the original typed engine error.
4. **Render** (`recall/render.rs`). A section is prose (an answer) or lines
   (one bullet per hit). A titled document is shown as `title: body`, and a
   turn that carries a time is led by it (`[2026-09-15 09:01] user: …`), so
   a reader can tell a corrected value from its correction. When
   the block overflows its budget, the last line is dropped first, from the
   last section; once no lines are left, the last answer shortens and is
   then dropped. `context.md` adds frontmatter whose token count includes
   itself.

## Background work

`BackgroundJob` is plain, serializable data, so the host decides what
happens to it: run it inline, push it to a queue, persist it, or coalesce
duplicates.

- `Brain::ingest` returns a `BuildBeliefs` job for the source's scope.
- `post_turn` returns one every `build_beliefs_every` turns, for the agent's
  conversations.
- Neither does on an engine that declares `Automatic`: it rebuilds beliefs
  on its own after writes. `Brain::build` and `AgentMemory::history_build`
  still hand back a build to run at once, as a refresh.
- `BackgroundJob::IngestBrain` defers a whole ingestion. Its report hands
  back the follow-up builds.

What a build does depends on the engine's `consolidation`:

| Engine | `consolidation` | `run_background(BuildBeliefs)` |
| --- | --- | --- |
| Reference | `OnDemand` | distils one `Fact` per item at once → `Done` |
| CortexDB direct, managed API | `Automatic` | none queued; an explicit build posts `v1/beliefs/build` per held scope → `Done` |
| CortexDB direct, elsewhere | `OnDemand` | `POST v1/beliefs/build` per held scope, built within the request → `Done` |
| CortexDB via TinyHumans | `Scheduled` | nothing sent → `Scheduled` |
| an engine without it | `None` | `Skipped { reason }` |

Built beliefs reach the Learnings section of every pack:

- On the reference engine they are `Learning` items, so the Learnings
  section lists them like any other.
- On CortexDB they live in a derived layer, which fetch and list (stored
  items only) never return. Holistic recall therefore asks
  for them whenever a pack has a learnings section, and interleaves them
  with the stored learnings:
  - with the pack's query, every fetched section's fetch asks for beliefs
    (`FetchRequest::beliefs`). CortexDB adds the `beliefs` layer to the
    recall pack it builds for each scope anyway, so a scope's query is
    embedded once. Brain, history and team between them read every scope
    a belief can be built in;
  - without one (a cold `start_session`, `context.md`), the learnings
    section lists them (`MemoryEngine::beliefs`, the `v1/beliefs` listing).

  Answered sections, such as a compaction summary or a `context.md` brief,
  also draw on beliefs through CortexDB's answer route.

## Compaction and session start

- **`start_session`** puts a resumed thread's newest turns first, under
  "Earlier in this thread", followed by the standard sections. Without a
  `focus`, each section shows its newest items.
- **`recall_for_compaction`** answers "what was discussed, decided and left
  open" over the thread, using recall (a model on engines that have one),
  from as many of the thread's turns as were dropped (at least the policy's
  history limit, at most 24). When the engine cannot answer, it fetches
  instead. The standard sections follow, ranked for the focus or for the
  dropped turns' gist: the start of every dropped turn, oldest first, within
  600 characters, so a fact stated early still steers the query.

Neither call is on the hot path.

## Integrations

- `tinymemory_integrations::brain::brain_document` (feature `brain`) runs a
  file through the `documents` converters and picks its `BrainSource` from
  the detected format: PDF to `pdf`, markdown and text to `markdown`, HTML
  to `web`.
- `cortex/lifecycle_tests.rs` runs one agent loop over both wires' doubles.
- `tests/live_cortex_lifecycle.rs` runs one against the harness.
