# Memory v2: Recall, Fetch, Store

Status: accepted. Supersedes every other spec in this directory; those files are
deleted with the code they describe.

## Why

TinyMemory grew a 27-family capability contract, eight engines, an embedded
engine, a TinyBus module, a tool layer and a summary tree. A host needs three
things from memory, plus a way to choose who provides them:

| Operation | Meaning |
| --- | --- |
| **Recall** | A question in, a synthesized answer with citations out. The engine owns how it answers (agentic loop, native ask route, …). |
| **Fetch** | Raw retrieval: keyword, vector or hybrid search over stored items, filtered by metadata. No synthesis. |
| **Store** | Ingest one of three kinds of item — document, conversation, learning — each carrying typed metadata. |

On top of the engine sits one engine-neutral product: **`context.md`**, a
token-budgeted brief compiled from Recall and Fetch that a host injects at the
start of a session. A second, agent-facing surface, the memory **tools**, lets
a model call the same operations under a scope the host fixes (see
[Tools](#tools-tinymemory-tools)).

## Crates

Three crates, one directory each under `crates/`. Their relationships are in
[`docs/architecture/overview.md`](../architecture/overview.md).

| Crate | Owns |
| --- | --- |
| `tinymemory-api` | The contract: `MemoryEngine`, request/response types, `MemoryMeta`, `MetaFilter`, namespaces, `EngineDescriptor`, `Error`. No I/O. Feature `conformance`: the behavioural suite every engine must pass, plus a reference in-memory engine. |
| `tinymemory-tools` | The agent tool spec `MemoryTools` (seven tools, host-fixed namespace and reach) and `context`, the `ContextCompiler` that builds `context.md` from an engine. |
| `tinymemory-integrations` | Everything that talks to the outside world, each behind a feature: `cortex` (default; the CortexDB engine, registered twice as `cortexdb` and `tinyhumans`, plus the registry `list_engines`/`build_engine` and `MemoryConfig`), `documents` and `documents-office` (format sniffing and conversion to markdown, emitting `StoreItem::Document`; PDF/DOCX/PPTX/XLSX via `OfficeConverter` or a host `DocumentConverter`), `sources` and `sources-network` (readers for folder, file, link, GitHub, RSS, Composio payloads and conversations, with the SSRF guard), `safety` (secret/PII scrubbing applied to every item before `store`), and `legacy-import` (reads a v1 TinyCortex workspace and migrates it into any engine). |

Deleted: the earlier `tinymemory` facade, `tinymemory-cortex`,
`tinymemory-documents`, `tinymemory-sources`, `tinymemory-safety`,
`tinymemory-context`, `tinymemory-import` and `tinymemory-conformance` as
separate crates (their code moved into the three above: the engine, registry
and config, documents, sources, safety and import into
`tinymemory-integrations`; context into `tinymemory-tools`; conformance into
the `conformance` feature of `tinymemory-api`). Earlier still: `tinymemory-bus`,
`tinymemory-core`, `tinymemory-tinycortex`, `tinymemory-remote` (CortexDB is
the engine; mem0, supermemory, cognee, agentmemory, livingbrain are dropped),
`tinymemory-conversations`, `tinymemory-guard`, `tinymemory-gate`,
`tinymemory-sync` (normalisers moved into `sources`), `tinymemory-module`,
`tinymemory-testing-ui`, and the `vendor/tinycortex`, `vendor/tinybus` and
`vendor/tinyinference` submodules (`legacy-import` reads the v1 on-disk layout
directly, so it needs no engine dependency). `tinymemory-conversations` (the
chat thread store) moved to `tinyagents-session::threads` in tinyagents.

## Contract (`tinymemory-api`)

```rust
#[async_trait]
pub trait MemoryEngine: Send + Sync {
    fn descriptor(&self) -> &EngineDescriptor;
    async fn health(&self) -> EngineHealth;
    async fn recall(&self, req: RecallRequest) -> Result<RecallAnswer>;
    async fn fetch(&self, req: FetchRequest) -> Result<FetchPage>;
    async fn store(&self, item: StoreItem) -> Result<StoreReceipt>;
    async fn forget(&self, target: ForgetTarget) -> Result<ForgetReport>;
    async fn list(&self, req: ListRequest) -> Result<ListPage>;
    // Explorers; both have listing-based defaults (see "Explore and get").
    async fn explore(&self, req: ExploreRequest) -> Result<ExplorePage>;
    async fn get(&self, req: GetRequest) -> Result<Vec<Hit>>;
    // Bulk ingestion; default stores one at a time (see "Bulk store").
    async fn store_many(&self, items: Vec<StoreItem>) -> Result<Vec<StoreReceipt>>;
}
```

### Metadata

```rust
pub struct MemoryMeta {
    pub workspace: Option<String>,   // absolute path or logical workspace id
    pub folder: Option<String>,      // containing folder (absolute or workspace-relative)
    pub file_path: Option<String>,
    pub language: Option<String>,    // code language or natural language tag
    pub repo: Option<String>,        // "owner/name" or remote URL
    pub commit: Option<String>,
    pub url: Option<String>,
    pub thread_id: Option<String>,
    pub turns: Option<TurnRange>,    // { first: u32, last: u32 }
    pub agent_id: Option<String>,
    pub tool_call: Option<ToolCallRef>, // { name, id }
    pub source: SourceRef,           // { kind: SourceKind, id: Option<String> }
    pub tags: Vec<String>,
    pub observed_at: Option<DateTime<Utc>>,
}
pub enum SourceKind { Folder, File, Link, Github, Rss, Composio, Conversation, Agent, Import }
```

`MetaFilter` has the same optional fields (each an exact match, `folder` and
`file_path` also match as a prefix), plus `kinds: Vec<ItemKind>`,
`sources: Vec<SourceKind>`, `tags_any: Vec<String>`, and an
`observed_after`/`observed_before` window. An empty filter matches everything.

### Items

```rust
pub enum ItemKind { Document, Conversation, Learning }

pub enum StoreItem {
    Document { title: Option<String>, body: DocumentBody, mime: Option<String>, meta: MemoryMeta },
    Conversation { turns: Vec<Turn>, meta: MemoryMeta },
    Learning { text: String, kind: LearningKind, confidence: f32, evidence: Option<String>, meta: MemoryMeta },
}
pub enum DocumentBody { Text(String), Uri(String) } // Uri is resolved by sources before store
pub struct Turn { pub role: Role, pub text: String, pub at: Option<DateTime<Utc>>, pub tool_calls: Vec<ToolCallRef> }
pub enum LearningKind { Preference, Fact, Procedure, Correction, Other }
```

`StoreReceipt { id: ItemId, replayed: bool }`. Engines derive idempotency from
the full item except `meta.observed_at` (when it was seen, not what it is), so
an identical retry, or an unchanged file re-synced, is a replay, not a
duplicate.

### Recall

```rust
pub struct RecallRequest { pub question: String, pub filter: MetaFilter, pub limit: usize, pub instructions: Option<String> }
pub struct RecallAnswer { pub answer: String, pub citations: Vec<Citation>, pub model: Option<String> }
pub struct Citation { pub id: ItemId, pub kind: ItemKind, pub snippet: String, pub meta: MemoryMeta, pub score: Option<f32> }
```

How an engine answers is its own business. CortexDB builds a recall pack and
calls its answer route once with that pack (`/v1/answer`, `/memory/answer`).

### Fetch and list

```rust
pub enum FetchMode { Keyword, Vector, Hybrid }
pub struct FetchRequest { pub query: String, pub mode: FetchMode, pub filter: MetaFilter, pub limit: usize, pub cursor: Option<String> }
pub struct FetchPage { pub hits: Vec<Hit>, pub next_cursor: Option<String> }
pub struct Hit { pub id: ItemId, pub kind: ItemKind, pub text: String, pub meta: MemoryMeta, pub score: f32, pub confidence: Option<f32> }
pub struct ListRequest { pub filter: MetaFilter, pub limit: usize, pub cursor: Option<String> }
pub struct ListPage { pub items: Vec<Hit>, pub next_cursor: Option<String> } // score = 0
pub enum ForgetTarget { Ids(Vec<ItemId>), Filter(MetaFilter) } // Filter must not be empty
```

A mode the engine does not list in `EngineDescriptor::fetch_modes` fails with
`Error::Unsupported`. Hosts read the descriptor and never offer it.

`Hit::text` is the item's `StoreItem::render_text()` form, and
`Hit::confidence` carries a learning's confidence (`None` for other kinds), so a
listing can be ordered by it. An item's id is its `StoreItem::fingerprint()`.

### Namespaces

Memory is a tree of nodes (`Namespace`, written `team:acme/agent:writer`; the empty path is the root, written `root`). The root holds what every agent shares; each agent, sub-agent (nested under its spawner), team, user, workspace or project has its own node (`SegmentKind`). Segment ids are `[A-Za-z0-9_-]{1,128}`; `Segment::sanitized` maps any host id onto that charset without collisions; depth is at most 8.

- **Placement.** `MemoryMeta.namespace` (default root, omitted on the wire when root) puts an item at one node, and is part of its fingerprint: the same text at two nodes is two items. Old envelopes read as root.
- **Reach.** `MetaFilter.reach: Option<Reach>` confines every filtered read (recall, fetch, list, explore, forget by filter). `Reach { at, inherit, descendants }` admits `at`, its ancestors when `inherit` (the default, so an agent reads what its team and the root share), and everything below it when `descendants`. A sibling is never admitted. `None` reads every node.
- **Get and forget by id.** `GetRequest.reach` leaves out ids beyond it. `ForgetTarget::Ids` is not scoped; a confined caller reads the ids with `get` and its reach first.
- **Explore.** `Facet::Namespace` groups by node; narrowing a value reads exactly that node.
- **Context.** `ContextSpec.reach` compiles a document from one node's reach.

### Explore and get

An explorer walks stored items by **facet**, a metadata dimension fixed by the
contract rather than by an engine's storage layout, so one explorer works on
every engine:

```rust
pub enum Facet { Kind, Source, SourceId, Workspace, Folder, FilePath, Language, Repo, Url, Thread, Agent, ToolCall, Tag, Namespace }
pub struct ExploreRequest { pub facet: Facet, pub filter: MetaFilter, pub limit: usize /* 1..=500 buckets */, pub scan_limit: usize /* 1..=50_000, default 5_000 */ }
pub struct FacetBucket { pub value: String, pub count: u64 }
pub struct ExplorePage { pub facet: Facet, pub buckets: Vec<FacetBucket>, pub total: u64, pub missing: u64, pub more_buckets: u64, pub truncated: bool }
pub struct GetRequest { pub ids: Vec<ItemId> /* 1..=200 */, pub reach: Option<Reach> }
```

- `explore` groups the items `filter` admits by one facet: buckets largest
  first (ties by value), `missing` counts items with no value, `more_buckets`
  the values cut by `limit`. `Tag` is multi-valued; an item counts once per
  tag.
- `Facet::narrow(&mut filter, value)` turns a bucket back into the filter
  field, so drilling down is `explore`, pick a bucket, `narrow`, then
  `explore` or `list` again. `Folder` and `FilePath` narrow by prefix, as
  their filter fields do.
- `get` reads items whole, in the order named; unknown ids are left out.
- **Defaults.** `explore_by_listing` pages through `list` up to `scan_limit`
  items and sets `truncated` when it stops early, so counts are then a lower
  bound. `get_by_listing` pages until every id is found. An engine overrides
  either when it can do better: CortexDB looks ids up by their labels.

### Bulk store

`store_many(items)` (1 to `MAX_STORE_MANY` = 100 items) is for imports,
backfills and syncs. Receipts come back in item order; an item repeated in the
batch is a replay of its first copy. Every item is readable through `list`,
`get` and `forget` on return, as with `store`; ranked `fetch`/`recall` may lag
for all but the last. On an error the earlier items are stored, and storing
them again is a replay.

CortexDB pays per batch, not per item: one id lookup per kind for replay
detection, all missing events written without waiting, then one listing wait
per scope for the last event written there (a scope's log is indexed in
order), and the ranked-recall wait for the final event only. Its event
listing slows as a scope grows, so per-item waits made a 5,000-item import
take hours.

### Descriptor and health

```rust
pub struct EngineDescriptor {
    pub id: &'static str, pub label: &'static str, pub description: &'static str,
    pub hosted: bool, pub needs_endpoint: bool, pub needs_key: bool,
    pub default_endpoint: Option<&'static str>, pub fetch_modes: Vec<FetchMode>,
}
pub enum EngineHealth { Ok, Degraded(String), Down(String) }
```

### Errors

There is one `Error` enum: `Unsupported`, `InvalidRequest`, `Unauthorized`,
`NotFound`, `Conflict`, `Unavailable` (transient), `Engine` (the engine's own
failure, already sanitised), and `Config`. Messages never carry credentials.

## Registry and config (`tinymemory-integrations`, feature `cortex`)

There is no facade crate: a host depends on `tinymemory-api` and the
integrations it wants, and builds an engine with the registry.

```rust
pub struct MemoryConfig { pub engine: String, pub engines: BTreeMap<String, EngineSettings> }
pub struct EngineSettings { pub endpoint: Option<String>, pub headers: BTreeMap<String, String>, pub tenancy: Option<CortexTenancy> }
pub enum EngineCredential { None, Static(String), Dynamic(Arc<dyn BearerSource>) }
pub fn list_engines() -> Vec<EngineDescriptor>;
pub fn build_engine(id: &str, settings: &EngineSettings, credential: EngineCredential) -> Result<Arc<dyn MemoryEngine>>;
impl MemoryConfig { pub fn build(&self, credential: EngineCredential) -> Result<Arc<dyn MemoryEngine>>; }
```

`build_engine` refuses an unknown id, a missing required endpoint or key, and a
credentialed cleartext non-loopback endpoint, all as `Error::Config`.

## Engine: CortexDB (`tinymemory-integrations`, module `cortex`)

- **Wires.** `Direct` (`v1/experience`, `v1/events`, `v1/recall`, `v1/forget`, `v1/answer`) and `TinyHumans` (`memory/*` with `{success,data}` envelopes), as in the v1 adapter.
- **Store.**
  - Each item becomes one experience: a conversation becomes a bulk append of its turns.
  - The envelope carries `{v:2, kind, meta, title?, learning_kind?, confidence?}`, and `meta` maps to scope labels where CortexDB can filter.
  - Writes wait for the indexed barrier, keeping the v1 `await_readable` behaviour.
- **Scope.** One scope per item kind *per namespace node*, under the TinyMemory root `app:tinymemory` (which the hosted backend further roots under the tenant): the root node keeps `app:tinymemory/app:{documents,conversations,learnings}`, and a node adds its segments in between, e.g. `app:tinymemory/team:acme/agent:writer/app:learnings`. Namespace segments map to CortexDB's built-in `agent`, `team`, `user`, `ws` and `project` types and the kind leaf uses `app`, because CortexDB v0.10+ refuses scope types outside the deployment's `allowed_scope_types` (`422 UNREGISTERED_SCOPE_TYPE`); every shipped preset allows all of them. A `MetaFilter`'s `kinds` and `reach` pick the scopes read: a reach's nodes are known, and only a subtree reach or an unscoped read discovers nodes, from the registered scopes (`v1/scopes/list` / `memory/scopes`). Reads are always exact (`view=local`), never server-side traversal.
- **Fetch.**
  - `Hybrid` maps to `recall` layers. `Keyword` and `Vector` are declared only if the wire exposes a mode switch; otherwise `fetch_modes = [Hybrid]`. The recall body accepts only `scope`, `query`, `budgets`, `view`, `include`, `temporal` and `filters`, with no mode switch, so both wires declare `[Hybrid]`.
  - Metadata filters CortexDB cannot apply server-side are applied client-side on the page, and the cursor is still the engine's.
- **Recall.** Pack, then answer, as in v1. One scope: one pack over it. An unscoped read over several scopes: one pack over `app:tinymemory` with `view: "descend"`. A reach over several scopes: one pack per scope, built concurrently, and the answer route is asked once with the pack holding the most admitted events. Citations come from the packs' `layers.events`, decoded back to `Hit`s, the most specific node's first.
- **List / forget.** These use `v1/events` paging and `v1/forget` by `memory_ids`. `ForgetTarget::Filter` lists first, then forgets ids, and never sends an empty selector.

## Tools (`tinymemory-tools`)

`MemoryTools` exposes seven tools over any `MemoryEngine`, with no tool-runtime
dependency. `MemoryTools::specs()` returns `Vec<ToolSpec>` (`name`,
`description`, `parameters` as a JSON Schema); `MemoryTools::call(name, args)`
runs one by name with the model's JSON arguments and returns compact JSON.

| Tool | Kind | Maps to |
| --- | --- | --- |
| `memory_recall` | read | `recall` |
| `memory_fetch` | read | `fetch` (the `mode` enum lists exactly the engine's `fetch_modes`; absent when the engine serves none) |
| `memory_list` | read | `list` |
| `memory_get` | read | `get` (reports unknown or out-of-reach ids as `missing`) |
| `memory_explore` | read | `explore` (every facet except `namespace`) |
| `memory_store` | write | `store` of one learning, document or conversation |
| `memory_forget` | write | `forget` by ids or a non-empty filter (reports `skipped` ids) |

**The model never chooses whose memory it touches.** The host fixes a
`ToolScope { place: Namespace, reach: Option<Reach>, writes: bool }`
(`MemoryTools::new`, `placed_at`, `reach`, `read_only`, `with_scope`):

- writes land at `place`: the tool builds the metadata itself (namespace,
  source `agent`, the model's tags, `observed_at` now);
- every read filter's `reach` is overwritten with the scope's, and `memory_get`
  passes it as `GetRequest::reach`;
- forget by ids first reads the ids back under the reach and forgets only
  those found; by filter, the filter must set a field besides the reach;
- a `namespace` or `reach` key anywhere in the arguments, or any unknown key,
  is `Error::InvalidRequest`; write tools on read-only tools are
  `Error::Unsupported`.

Names and schemas are frozen by a fixture test. See
[`docs/architecture/tools.md`](../architecture/tools.md).

## Context (`tinymemory-tools`, module `context`)

`context.md` is now one preset of the holistic recall that the agent
lifecycle is built on; see [agent-memory.md](agent-memory.md). Its output is
unchanged.

```rust
pub struct ContextSpec { pub budget_tokens: usize, pub briefs: Vec<Brief>, pub learnings_limit: usize }
pub struct Brief { pub heading: String, pub question: String, pub filter: MetaFilter }
pub struct ContextDoc { pub markdown: String, pub tokens: usize, pub generated_at: DateTime<Utc>, pub engine: String, pub refs: Vec<ItemId> }
pub async fn compile(engine: &dyn MemoryEngine, spec: &ContextSpec) -> Result<ContextDoc>;
```

The default briefs are:
- **About the user:** identity, role, and how they like to work.
- **Active work:** current projects, workspaces and repos.
- **Preferences and standing instructions.**
- **Recent important events.**

After the briefs comes a "Learnings" list, from a `list` of `kind = Learning` sorted by recency and confidence.

Output rules:
- Each section is trimmed so the whole document fits `budget_tokens`, estimated at 4 chars per token. The briefs keep their order, and learnings are trimmed first.
- Frontmatter records `generated_at`, `engine`, `tokens` and `refs`.
- An engine with nothing stored yields an empty document (`markdown` is empty), not an error. A brief that fails is skipped and logged; it does not fail the document.

## Import (`tinymemory-integrations`, feature `legacy-import`)

`LegacyWorkspace::open(path)` detects a v1 TinyCortex store. `items()` yields
`StoreItem`s:
- Documents become `Document`.
- Episodic turns grouped by thread become `Conversation`.
- Learning-section and `global` records become `Learning`.
- Profile facets become `Learning(Preference)`.

Every item gets `source.kind = Import`. A `Checkpoint` (last yielded cursor per
section, persisted by the host) makes import resumable.

`import::migrate(engine, workspace, from)` copies a workspace into any engine:
it streams `items_from(checkpoint)` in batches of at most `MAX_STORE_MANY`,
hands each batch to `store_many`, and returns a `MigrationReport { stored,
replayed, batches, checkpoint }`. `migrate_with` also calls an `on_batch`
callback with the committed checkpoint after every stored batch, so the host
can persist it. A failed `store_many` is `Error::Engine`, carrying the last
committed checkpoint; resuming re-sends the failed batch, whose stored items
come back as replays. A legacy read failure is returned as is, and resuming
from any earlier checkpoint only replays.

## Testing

`tinymemory_api::conformance::run(engine)` (feature `conformance`) covers:
- store/list round-trip for each kind;
- replay idempotency;
- `explore` counts agreeing with `list` per kind and per workspace, and each
  bucket narrowing to exactly its count;
- `get` returning listed items by id, in request order, unknown ids left out;
- `store_many` storing in order, listing every item on return, replaying a
  repeated batch, and refusing an empty one;
- fetch filtering by every meta field;
- namespaces: each reach (inherited, exact, subtree) listing exactly its
  nodes and never a sibling's, `get` and `fetch` honouring the reach, the
  same text at two nodes being two items, the namespace facet counting each
  node, and a forget scoped to one node removing only it;
- forget by id and by filter;
- refusing an empty filter;
- `Unsupported` for undeclared modes;
- recall returning citations that resolve via `list`.

It runs against the reference engine and against both CortexDB wires through an HTTP double.
