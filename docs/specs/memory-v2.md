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

Three memory crates, plus the standalone redaction crate `tinyredact`. Their relationships are in
[`docs/architecture/overview.md`](../architecture/overview.md).

| Crate | Owns |
| --- | --- |
| `tinymemory-api` | Pure safety vocabulary (classification/redaction behind `local-safety`) and the contract: `MemoryEngine`, request/response types, `MemoryMeta`, `MetaFilter`, namespaces, `EngineDescriptor`, `EngineStatus`, `Error`. No I/O. Feature `conformance`: the behavioural suite every engine must pass, plus a reference in-memory engine. |
| `tinymemory-tools` | The agent tool spec `MemoryTools` (seven tools, host-fixed namespace and reach) and `context`, the `ContextCompiler` that builds `context.md` from an engine. |
| `tinymemory-integrations` | Everything that talks to the outside world, each behind a feature: `cortex` (default; the CortexDB engine, registered twice as `cortexdb` and `tinyhumans`, plus the registry `list_engines`/`build_engine` and `MemoryConfig`), `documents` and `documents-office` (format sniffing and conversion to markdown, emitting `StoreItem::Document`; PDF/DOCX/PPTX via `OfficeConverter` or a host `DocumentConverter`; XLSX remains recognized but temporarily unsupported), `sources` and `sources-network` (readers for folder, file, link, GitHub, RSS and conversations, with the SSRF guard), `safety` (secret/PII scrubbing applied to every item before `store`), and `legacy-import` (reads a v1 TinyCortex workspace and migrates it into any engine). |
| `tinyredact` | Standalone local secret and personal identifier detection and scrubbing, usable independently of memory integrations. |

Earlier designs split the engine, documents, sources, safety, context and
legacy import into separate crates. That code now lives in the three memory
crates listed above; `tinyredact` remains the separate redaction library.
Conformance is a feature of `tinymemory-api`. The older TinyCortex engine and
its vendor submodule are gone, but `legacy-import` still reads the v1
TinyCortex on-disk layout directly, without an engine dependency. The former
chat thread store now lives in `tinyagents-session::threads` in tinyagents.

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
    // Forget by id looking only inside a reach; default gets, then forgets.
    async fn forget_within(&self, ids: Vec<ItemId>, reach: Reach) -> Result<ForgetReport>;
    async fn list(&self, req: ListRequest) -> Result<ListPage>;
    // Explorers; both have listing-based defaults (see "Explore and get").
    async fn explore(&self, req: ExploreRequest) -> Result<ExplorePage>;
    async fn get(&self, req: GetRequest) -> Result<Vec<Hit>>;
    // Bulk ingestion; default stores one at a time (see "Bulk store").
    async fn store_many(&self, items: Vec<StoreItem>) -> Result<Vec<StoreReceipt>>;
    // Moving memory; default refuses with Unsupported (see "Export").
    async fn export(&self, req: ListRequest) -> Result<ExportPage>;
    // Deleting for good; default refuses with Unsupported (see "Erase").
    async fn erase(&self, req: EraseRequest) -> Result<EraseReport>;
}
```

### Metadata

```rust
pub struct MemoryMeta {
    pub namespace: Namespace,         // caller-owned memory scope
    pub workspace: Option<String>,   // absolute path or logical workspace id
    pub folder: Option<String>,      // containing folder (absolute or workspace-relative)
    pub file_path: Option<String>,
    pub language: Option<String>,    // code language or natural language tag
    pub repo: Option<String>,        // "owner/name" or remote URL
    pub commit: Option<String>,
    pub url: Option<String>,
    pub thread_id: Option<String>,
    pub channel: Option<String>,
    pub turns: Option<TurnRange>,    // { first: u32, last: u32 }
    pub agent_id: Option<String>,
    pub tool_call: Option<ToolCallRef>, // { name, id }
    pub source: SourceRef,           // { kind: SourceKind, id: Option<String> }
    pub tags: Vec<String>,
    pub observed_at: Option<DateTime<Utc>>,
    pub observed_actor: Option<ObservedActor>,
}
pub enum SourceKind { Folder, File, Link, Github, Rss, Conversation, Agent, Import }
```

`MemoryMeta::overlay(host)` takes set host optional fields over caller values
and preserves optional fields the host leaves unset. The host's namespace and
source always replace caller values, including root/default values; host tags
append uniquely after caller tags. `ObservedActor::for_channel_sender`
keeps the host sender identity rules in this contract: phone ids are
`user:<dial digits>` on `whatsapp`, `signal`, `imessage`, `linq` and `sms`,
emails on any channel are lower-cased `user:<email>`, and other senders use
`<channel>:<sender>`. TinyMemory owns this channel vocabulary so the memory
contract does not depend on a host channel crate.

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

Memory is a tree of nodes (`Namespace`, written `team:acme/agent:writer`; the empty path is the root, written `root`). The root holds what every agent shares; each agent, sub-agent (nested under its spawner), team, user, workspace, project, knowledge source or service (one automation, such as a workflow) has its own node (`SegmentKind`, non-exhaustive). Segment ids are `[A-Za-z0-9_-]{1,128}`; `Segment::sanitized` maps any host id onto that charset without collisions; depth is at most 8.

- **Placement.** `MemoryMeta.namespace` (default root, omitted on the wire when root) puts an item at one node, and is part of its fingerprint: the same text at two nodes is two items. Old envelopes read as root.
- **Reach.** `MetaFilter.reach: Option<Reach>` confines every filtered read (recall, fetch, list, explore, forget by filter). `Reach { at, inherit, descendants }` admits `at`, its ancestors when `inherit` (the default, so an agent reads what its team and the root share), and everything below it when `descendants`. A sibling is never admitted, and a descendant read never enters a `service:` sandbox below `at`. `None` reads as `Reach::subtree(Namespace::ROOT)`: every node except a sandbox. `Reach::within(outer)` tests confinement: it is true when every namespace the reach admits, `outer` admits too (`at` in `outer`; every ancestor in `outer` when `inherit`; `outer.descendants` and `at` at or below `outer.at` when `descendants`, except at maximum depth where no descendant exists), so a host can accept a caller's reach only when it does not widen the one it allows.
- **Get and forget by id.** `GetRequest.reach` leaves out ids beyond it. `ForgetTarget::Ids` is not scoped: an engine may look for the ids in every node it holds. A confined caller forgets with `forget_within(ids, reach)`, which removes only the ids inside the reach and reads no node outside it while looking (the default reads the ids back with `get` and the reach, then forgets what came back).
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

### Export

`export(req)` pages like `list` (same `ListRequest`, filter and cursor) but
hands each item back whole as the `StoreItem` it was stored as, under its id:

```rust
pub struct Exported { pub id: ItemId, pub item: StoreItem }
pub struct ExportPage { pub items: Vec<Exported>, pub incomplete: Vec<ItemId>, pub next_cursor: Option<String> }
```

It exists for moving memory: to another engine, or to another node, where the
namespace changes and so does the fingerprint. A listing's `Hit` cannot be
stored again faithfully: its text is the rendered form, which drops a
document's mime, a conversation's turn times and tool calls, and a learning's
kind and evidence. Storing an exported item where it was is a replay. An item
the engine holds only part of (a chunked document missing a piece) is named in
`incomplete` rather than dropped, so a caller moving memory knows what it could
not move. The default refuses with `Unsupported`; the reference and CortexDB
engines export.

### Erase

`erase(req)` deletes for good every item of `req.kinds` (all when empty) at
the nodes `req.reach` names (`at`, and below it when `descendants`;
`inherit` is ignored). It is for deleting a source, a workflow, a workspace or
an account. Unlike a forget, an erased item stored again is stored anew, never
a replay.

```rust
pub struct EraseRequest { pub reach: Reach, pub kinds: Vec<ItemKind>, pub whole_tree: bool }
pub struct EraseReport { pub erased_scopes: usize, pub receipts: Vec<String> }
```

- **Interlock:** the root with its descendants and every kind is the whole
  tree, refused (`InvalidRequest`) unless `whole_tree` is set.
- **Default:** refuses with `Unsupported`, and a caller that is refused falls
  back to `forget`.
- **The reference engine** removes the items.
- **CortexDB, Direct wire:** sends one `/v1/erasures` (`confirm_all`) per
  registered kind scope, deepest first, and returns the erasure ids as
  `receipts`.
- **CortexDB, hosted wire:** the whole tree in one `DELETE memory`; anything
  narrower the same as Direct, through the TinyHumans backend's
  `memory/v1/erasures` passthrough (memory-api's synchronous scoped erasure,
  pinned under the tenant's root).

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
pub struct EngineSettings { pub endpoint: Option<String> }
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
  - Each item becomes one experience: a conversation becomes a bulk append of its turns, and a document whose encoded envelope as a chunked piece would pass 256 KiB becomes an ordered append of its pieces, cut at page breaks and headings, and a page or section still over the target cut again at blank lines, then line ends, then characters (`chunk {index, count, pages?, section?}` in the envelope; `pages` an inclusive `[first, last]` range counted from 1, only when the text marks pages; `section` only under a heading). No event over 768 KiB of encoded envelope is sent (CortexDB refuses one over 1 MiB): an item that cannot fit even split (a learning or a turn that long, or a document whose title and metadata leave no room for a piece and which does not fit whole) is `InvalidRequest`, and nothing of its batch is sent. `get`/`list` reassemble a chunked document, returning it only when every piece is present, and a ranked read gives one hit per document, its best-ranked piece, with `page:`/`section:` tags.
  - An event's `content.text` is the item's own text (body or piece, turn text, statement), and its envelope `{v:3, id, kind, meta, title?, learning_kind?, confidence?, turn?, chunk?}` rides in `tm:e:` labels beside readable `kind:`/`file:`/`page:`/`section:` labels (v3); an event with empty text or too many labels for 64 carries the whole envelope as JSON text (v2), as every event before v3 did, and both read. A reader decides by the labels: an event whose `tm:e:` parts, numbered from 0 with none missing, join to a `v:3` envelope is v3 (its `content.text` is the item's text, empty or not); any other event's `content.text` is tried as a `v:2` envelope; an event that is neither is not this crate's. `meta` maps to lookup labels where CortexDB can filter. No local path leaves the machine: `meta.file_path` is sent as the file's name, `folder` and an absolute `workspace` are not sent, and filters on them match digests the envelope carries instead ([cortex-local-paths](../architecture/cortex-local-paths.md)).
  - `MemoryMeta.derive: Some(false)`, and every tool turn, is written with `directives.extract: []`: indexed and searchable, nothing derived.
  - Writes wait for the indexed barrier, keeping the v1 `await_readable` behaviour.
- **Scope.** One scope per item kind *per namespace node*, under the TinyMemory root `app:tinymemory` (which the hosted backend further roots under the tenant): the root node keeps `app:tinymemory/app:{documents,conversations,learnings}`, and a node adds its segments in between, e.g. `app:tinymemory/team:acme/agent:writer/app:learnings`. That is the legacy layout and the default; an engine given a scope root (`EngineSettings::scope_root`, such as one person's `org:<id>`; on the hosted wire `EngineSettings::tenant_root`, the tenant's own `org:<id>`) lays everything out below that root instead, with a leaf per kind and `app:brain`/`app:flows` grouping nodes (layout v3, [cortex-layout.md](../architecture/cortex-layout.md)), and switching moves nothing. Namespace segments map to CortexDB's built-in `agent`, `team`, `user`, `ws`, `project`, `source` and `service` types and the kind leaf uses `app`, because CortexDB v0.10+ refuses scope types outside the deployment's `allowed_scope_types` (`422 UNREGISTERED_SCOPE_TYPE`); every shipped preset allows all of them. A `MetaFilter`'s `kinds` and `reach` pick the scopes read, each admitted kind at each node: an ordinary reach (`Reach::of`, `inherit: true`) reads its node **and every ancestor**, an exact reach its node alone, and a subtree reach its node and every node below except a `service:` sandbox below it (a missing reach reads as the root's subtree, so it skips sandboxes too; a forget by id still reaches them, unless it is a `forget_within`, which reads exactly the scopes its reach does); those nodes are known, and only a subtree reach or an unscoped read discovers nodes, from the registered scopes (`v1/scopes/list` / `memory/scopes`). Reads are always exact (`view: "granular"`, sent explicitly because public recall defaults to `holistic`), never server-side traversal.
- **Fetch.**
  - `Hybrid` maps to `recall` layers. `Keyword` and `Vector` are declared only if the wire exposes a mode switch; otherwise `fetch_modes = [Hybrid]`. The recall body accepts only `scope`, `query`, `budgets`, `view`, `include`, `temporal` and `filters`, with no mode switch, so both wires declare `[Hybrid]`.
  - Metadata filters CortexDB cannot apply server-side are applied client-side on the page, and the cursor is still the engine's.
- **Recall.** Pack, then answer, as in v1. One pack per scope, built concurrently and exact (`view: "granular"`): a reach's scopes, or, for an unscoped read, every kind scope the engine holds; nothing to read answers empty without a request. No pack is asked of a parent scope, which CortexDB fills from its children in storage order. The answer route is asked once with the pack holding the most admitted events: `/v1/answer` takes one `use_pack_id` whose scope must match, so **the answer text is grounded on that one pack while the citations come from every pack**. The removed parent-scope (`view: "descend"`) pack was itself an unranked storage-order sample, so no coverage was lost; grounding the answer on every scope is follow-up work for when CortexDB's ranked `subtree` lane is on by default. Citations come from the packs' `layers.events`, decoded back to `Hit`s and merged rank by rank, the most specific node's first.
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
  those found, with `forget_within` and the reach when the scope has one; by filter, the filter must set a field besides the reach;
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
- for an engine that exports, every item handed back whole and equal to what
  was stored, under its own id, and a replay when stored again;
- for an engine that erases, a node and everything below it removed, its
  sibling kept, an erased item stored anew, and the whole tree refused
  without its interlock;
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

## Pure identification-risk helper

`tinymemory-api::safety` exposes stable risk/category vocabulary and count-only
results. Opt-in `local-safety` adds synchronous local pattern/keyword
classification and structured identifier redaction. It performs no I/O, invokes
no global logger, and links no runtime, engine or storage. The default contract
remains regex-free. Hosts own decisions, diagnostics and warning/blocking policy.

The scoring, strong-identifier override and replacement text preserve the
migrated OpenHuman behavior. Matching is heuristic rather than a guarantee
that content has no identifiers. Storage-item secret/PII scrubbing remains
`tinymemory-integrations::safety`; this helper neither installs that pipeline
nor enforces a write policy.
