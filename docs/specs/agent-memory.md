# Agent memory lifecycle: brain, conversations, learnings

**Status:** Accepted · **Builds on:** [memory-v2.md](memory-v2.md) ·
**Plan:** [../plans/agent-memory.md](../plans/agent-memory.md)

## Problem

A host such as OpenHuman calls memory at fixed points of every agent turn:

- before the model runs, to inject context;
- after the model replies, to log the reply;
- when a session starts or resumes;
- when the prompt is truncated (compaction);
- in the background, to distil beliefs.

The v2 contract offers the primitives (store, fetch, recall, namespaces) but
no shared shape for these moments. Each host would invent its own scope tree
and its own latency rules, and moving to another engine would mean
rewriting every one of those moments.

The host also needs a company **brain**: documents (PDF, markdown, Notion,
GitHub) shared by every agent, kept apart by source type, and carrying no
agent id.

## Goals

- **One standard layout** for the brain, agent conversations and learnings,
  on top of the existing namespace model.
- **One read primitive.** A holistic recall across several scopes, rendered
  as one token-budgeted block. `context.md`, session start, pre-turn and
  compaction are all presets of it.
- **A fast live turn.** The turn logs and recalls without waiting for
  indexing and without running a model.
- **Explicit background work.** Belief builds are values the host schedules,
  never hidden threads.
- **Engine-agnostic.** Everything is written against `MemoryEngine`, and the
  conformance suite pins down every new engine obligation. Swapping engines
  changes only the engine the host constructs.

## Non-goals

- Running a model inside this library. The host owns generation.
- Spawning tasks or owning a runtime.
- Thread storage beyond memory. `tinyagents-session` owns session threads.

## Layout

The layout sits below one root node. That root is `core`: the root namespace
by default, or a host node such as `team:acme`.

| Plan scope | Namespace | Kind |
| --- | --- | --- |
| `core` | root | everything (holistic) |
| `core/brain/<source>` | `root/source:<source>` | documents |
| `core/conversations/<agent>` | `root/agent:<agent>` | conversations |
| `core/learnings` | root (shared) and any node (built beliefs) | learnings |

- `source` is a new namespace segment kind (`SegmentKind::Source`). CortexDB
  accepts `source` as a scope type, so a brain source is a real scope:
  `app:tinymemory/source:files/app:documents`.
- `BrainSource` names the connector a document came from: `files` (local
  files of any format), `web`, `notion`, `github`, or any other id (a
  connected app's slug, `gmail`). `pdf` and `markdown` name the per-format
  nodes used before `files`. A large source may be split into
  `project:<collection>` nodes below it (`MemoryLayout::brain_collection`);
  forgetting or rebuilding the source covers them.
- A brain document carries **no agent id**. Its namespace is always its
  source's node, whatever metadata the caller passes.
- Each turn is stored as its own one-turn conversation item at the agent's
  node. The item carries `thread_id`, `turns {first,last}`, `agent_id`, and
  the `conversation` source keyed by thread. A retried turn with the same
  input is therefore a replay.

## Contract additions (`tinymemory-api`)

- **`MemoryEngine::store_with(item, WriteOptions { wait })`**
  - `WaitFor::Visible` is `store`.
  - `WaitFor::Accepted` may return once the engine has durably accepted the
    item.
  - The default implementation serves both as `store`.
- **`MemoryEngine::store_many_with(items, WriteOptions { wait })`** is the
  same for a batch: `WaitFor::Visible` is `store_many`, `WaitFor::Accepted`
  may return once every item is durably accepted. The default serves both as
  `store_many`.
- **`MemoryEngine::consolidate(ConsolidateRequest { reach, kinds })`** asks
  for a belief build and returns as soon as the job is taken.
  - The default refuses with `Unsupported`.
  - The receipt status is one of `Started`, `Scheduled` or `Completed`, and
    the receipt names any job handles, the number of scopes covered and,
    for a completed build that reports it, the beliefs `built`.
- **`EngineDescriptor::consolidation`** declares how the engine consolidates:
  `None`, `OnDemand`, `Scheduled` or `Automatic` (it rebuilds beliefs on its
  own after writes, and an explicit `consolidate` still builds at once).
- **`MemoryEngine::beliefs(BeliefsRequest { reach, query, limit })`** reads
  the beliefs an engine built and keeps apart from its stored items. With a
  query they are ranked for it; without one, the most confident come first,
  then the newest.
  - Each is a `Learning` hit tagged `BELIEF_TAG` (`"belief"`) at the node of
    its sources.
  - A belief is not a stored item: it cannot be listed, fetched or
    forgotten by id. Forgetting its sources removes it.
  - The default holds none. An engine whose beliefs are ordinary learning
    items (the reference engine) keeps it.
- **`FetchRequest::beliefs`** (default `0`) asks a fetch for up to that many
  beliefs from what it read, returned in `FetchPage::beliefs` (first page
  only). An engine that keeps no beliefs apart returns none.
- **Conformance** adds two checks:
  - `store_with`: a visible store is listed on return and replays; an
    accepted store answers with the item's own id.
  - `consolidate`: a malformed request is refused, and the answer matches
    what the descriptor promises. `beliefs` refuses a zero limit, and every
    belief it returns is a tagged learning within the reach asked for.
- **`ReferenceEngine`** consolidates on demand and deterministically: one
  `Fact` per document or conversation, tagged `consolidated`.

Adding `SegmentKind::Source` and the descriptor field is a breaking change to
exhaustive matches and struct literals, so it ships in a major release.

## Behaviour (`tinymemory-tools`)

### Holistic recall

`HolisticRecall { query, sections, budget_tokens, title, exclude_ids,
exclude_thread }` produces a `ContextPack { markdown, tokens, refs, sections,
skipped, engine }`.

- **Each section** is `ScopeSection { heading, filter, limit, query }`, filled
  in one of three ways:
  - `Fetch`: ranked retrieval, with no model.
  - `Answer`: a synthesised answer, optionally falling back to fetch.
  - `Latest`: newest first, then most confident, then latest turn.
- **Reads** run concurrently. A failing or empty section is reported in
  `skipped` and never fails the pack. The only error is an invalid request.
- **Deduplication.** An item is listed once, in its first (highest-priority)
  section. An answer citing an item does not hide it.
- **Beliefs are learnings.** When a pack has a learnings section (a fetched
  or latest section that admits learnings), it gathers the engine's beliefs
  into it:
  - with a query, every fetched section asks its fetch for beliefs too
    (`FetchRequest::beliefs`), so they come from reads the pack makes
    anyway;
  - without one, the learnings section lists them (`beliefs` with no query);
  - what all sections returned is merged, each belief once, and
    interleaved with the stored learnings rank by rank, stored learnings
    first.
  - The section's filter applies to beliefs too.
  - A failed belief read leaves the section to its stored learnings.
  - Answered sections ask for none, because the engine's answer draws on
    its beliefs itself.
  - So on CortexDB, what a build produced reaches every pack's Learnings
    section: in `pre_turn`, `start_session`, compaction and `context.md`.
- **Exclusions:**
  - `exclude_ids` drops named items, such as the turn just logged.
  - `exclude_thread { thread_id, from_turn }` drops the turns still in the
    prompt. The same `ThreadWindow` rides on `RecallRequest` and
    `FetchRequest` (`tinymemory-api`), so an answer section's recall leaves
    the window out at the engine and a host's own reads can too.
- **Dates.** A conversation bullet whose turn carries a time
  (`meta.observed_at`, set from `PreTurn::at` and `PostTurn::at`) is led by
  it, `[YYYY-MM-DD HH:MM]`. Hosts should pass the time the message was sent:
  without it, a superseded value and its correction look alike.
- **Budget.** The block fits `budget_tokens` at four characters per token.
  Bullets are trimmed from the last section first, then the last answer
  shortens.
- **`context.md`** is this with fixed sections: one answered section per
  brief, then the latest learnings, under `# Context`, with frontmatter. Its
  output is unchanged.

### `AgentMemory` (one agent)

| Call | Writes | Reads |
| --- | --- | --- |
| `start_session { thread_id?, focus? }` | — | the resumed thread's latest turns, then the standard sections |
| `pre_turn { thread_id, turn_index, user_text, in_prompt_from, at? }` | the user turn, `Accepted`, run concurrently with the read | the standard sections fetched for `user_text`, without this turn or the prompt's window |
| `post_turn { thread_id, turn_index, assistant_text, tool_calls, at? }` | the reply, `Accepted` | — |
| `recall_for_compaction { thread_id, dropped, focus? }` | — | an answered summary of the thread (falling back to fetch), then the standard sections for the focus or the dropped turns' gist |
| `recall(query)` | — | the pre-turn read without logging |
| `run_background(job)` | the job's | — |

- **Standard sections**, in priority order: Learnings (the whole tree), one
  section per core scope ([core-scopes.md](core-scopes.md); none by default),
  Brain (all documents, reading at most `BRAIN_SCOPES_PER_TURN` (4) scopes:
  those the query names, then the most recently written), this agent's
  history, and team conversations (other agents under the same layout root).
  In a pooled layout both conversation sections read the same chat node:
  history selects this agent's id, while team excludes it. The team limit
  counts selected turns, not agents or threads. Excluded own turns cannot
  exhaust a fixed retrieval page cap before eligible team turns are reached.
  A zero limit in `RecallPolicy` leaves a section out. Team recall never
  reaches another person's root.
- **`pre_turn` never fails on an engine error.** A failed log is reported in
  `TurnContext::log_error` and the pack is still returned.
- **`post_turn` reports belief builds.** It returns a `BuildBeliefs` job for
  the agent's conversations every `RecallPolicy::build_beliefs_every` turns,
  counted as `turn_index + 1`, unless the engine declares `Automatic`; then
  it returns none, and `history_build` still asks for one.

### Brain and background

- **`Brain`** stores documents:
  - `ingest` and `ingest_with(WaitFor)` store a `BrainDocument` at its
    source's node. The source kind defaults from the `BrainSource`.
  - `ingest_many` batches by `MAX_STORE_MANY`.
  - Each ingest returns the `BuildBeliefs` job for its source scope, and
    `ingest_many` one per source touched; on an engine that declares
    `Automatic` neither returns any (`Ingested::job` is `None`, `jobs` is
    empty), and `Brain::build` asks for one explicitly.
- **`Brain::search`** fetches within one source or across the whole brain,
  and **`Brain::forget`** erases one source.
- **`BackgroundJob`** is `BuildBeliefs { request }` or
  `IngestBrain { documents }`. It is serializable so a host can queue it.
- **`BackgroundRunner::run`** maps `Unsupported` to `JobOutcome::Skipped`, so
  the same host code runs on any engine.

### Integrations

- **`brain::brain_document(converter, raw, source?, meta)`** turns a file into
  a `BrainDocument`. Its source is the one the caller names, or the one the
  detected format implies.
- **CortexDB, Direct wire:**
  - `store_with(Accepted)` writes without `?wait=indexed` and skips the
    visibility waits.
  - `consolidate` posts `{ "scope": … }` to `v1/beliefs/build` once per scope
    that is held in reach and admitted. The server builds within the
    request, so the receipt is `Completed` with the beliefs `built`; an
    answer that names a job instead makes it `Started` with the handles.
  - It declares `Automatic` when its endpoint is CortexDB's managed API,
    which rebuilds beliefs on its own after writes, and `OnDemand`
    anywhere else. `EngineSettings::consolidation` overrides that.
- **CortexDB, TinyHumans wire:** declares `Scheduled` and sends nothing.

## Invariants

- The live turn (`pre_turn`, `post_turn`) never waits for indexing and never
  runs a model.
- A pack never contains the turn being logged, nor any turn of its thread at
  or after `in_prompt_from`.
- A brain document never carries an agent id and always lives at its
  source's node.
- Siblings stay invisible except through the holistic reach. The layout
  reads the subtree of its root deliberately, because the brain and the
  learnings are shared.
- No lifecycle call spawns work. Every slow step is a returned
  `BackgroundJob`.

## Acceptance criteria

- The conformance suite passes against the reference engine and both
  CortexDB doubles. Fault injections for `store_with` and `consolidate` trip
  their checks.
- The existing `context.md` tests pass unchanged after the rebase onto
  holistic recall.
- The same agent loop passes over both CortexDB doubles. On the Direct wire,
  turns log without `wait=indexed` and builds hit `v1/beliefs/build` per
  scope.
- `cargo run -p tinymemory-tools --example agent_loop` shows the lifecycle
  offline.
- `examples/cortex_agent` and `tests/live_cortex_lifecycle.rs` run against
  the harness.

## Open questions

- Whether the hosted TinyHumans backend will expose a belief-build route.
  When it does, only the hosted descriptor and `cortex/engine/consolidate.rs`
  change.
