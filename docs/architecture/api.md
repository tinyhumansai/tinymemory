# The core contract: `tinymemory-api`

`tinymemory-api` is the contract between a host, the tools and an engine. It
performs no I/O. Everything here is re-exported from the crate root
(`tinymemory_api::MemoryEngine`, ...).

Items, metadata and filters are in [api-items.md](api-items.md); the behaviour
of each operation is in [operations.md](operations.md); namespaces are in
[namespaces.md](namespaces.md).

## Modules

| Module | Holds |
| --- | --- |
| `engine` | `MemoryEngine`, `EngineDescriptor`, `EngineHealth`, `EngineStatus`, `MAX_STORE_MANY`, `validate_many` |
| `error` | `Error`, `Result` |
| `item` | `StoreItem`, `ItemKind`, `ItemId`, `DocumentBody`, `Turn`, `Role`, `LearningKind`, `StoreReceipt` |
| `meta` | `MemoryMeta`, `MetaFilter`, `ObservedActor`, `SourceKind`, `SourceRef`, `ToolCallRef`, `TurnRange` |
| `namespace` | `Namespace`, `Segment`, `SegmentKind`, `Reach` |
| `query` | Requests and responses for recall, fetch, list and forget |
| `explore` | `Facet`, explore and get requests, the listing-based defaults, limits |
| `conformance` | (feature `conformance`) `run`, `ReferenceEngine` |

The crate also re-exports `async_trait` and `chrono` so an engine and the
contract name the same versions.

## `MemoryEngine`

An object-safe `#[async_trait]` trait, `Send + Sync`; hosts hold it as
`Arc<dyn MemoryEngine>`.

| Method | Required? | Behaviour |
| --- | --- | --- |
| `descriptor(&self) -> &EngineDescriptor` | required | What the engine is and offers. |
| `health(&self) -> EngineHealth` | required | Whether it can serve now. Infallible: trouble is reported as `Degraded` or `Down`. |
| `recall(RecallRequest) -> Result<RecallAnswer>` | required | A synthesised answer with citations. |
| `fetch(FetchRequest) -> Result<FetchPage>` | required | Ranked raw retrieval in one `FetchMode`. |
| `store(StoreItem) -> Result<StoreReceipt>` | required | Store one item; an identical item is a replay. |
| `forget(ForgetTarget) -> Result<ForgetReport>` | required | Remove by ids or by a non-empty filter. |
| `forget_within(Vec<ItemId>, Reach) -> Result<ForgetReport>` | **default** | Remove the ids that lie within the reach, looking nowhere else. `forget_within_by_get`: `get` with the reach (batches of `MAX_GET_IDS`), then `forget` the ids found. An engine whose `forget` by id searches beyond the reach overrides it. No ids or a blank one is `InvalidRequest`. |
| `list(ListRequest) -> Result<ListPage>` | required | Query-free paging. |
| `store_many(Vec<StoreItem>) -> Result<Vec<StoreReceipt>>` | **default** | Calls `validate_many`, then `store` one item at a time, in order, stopping at the first error. An engine overrides it to batch. |
| `explore(ExploreRequest) -> Result<ExplorePage>` | **default** | `explore_by_listing`: pages through `list`. An engine that can aggregate server-side overrides it. |
| `get(GetRequest) -> Result<Vec<Hit>>` | **default** | `get_by_listing`: pages through `list` until every id is found. An engine that can look ids up directly overrides it. |

The trait documents the rule every method follows: **validate first**, using
the `validate` method of the request type, so every engine refuses the same
malformed call with the same `Error::InvalidRequest`. A `FetchMode` the
descriptor does not list fails with `Error::Unsupported`
(`EngineDescriptor::ensure_mode`). Note `FetchRequest::validate` does not
check the mode; the engine does, by calling `ensure_mode`.

### Constants and limits

| Constant | Value | Where it applies |
| --- | --- | --- |
| `MAX_STORE_MANY` | 100 | Items per `store_many` call (1 to 100). |
| `MAX_GET_IDS` | 200 | Ids per `GetRequest` (1 to 200). |
| `MAX_BUCKETS` | 500 | `ExploreRequest::limit` (1 to 500). |
| `MAX_SCAN_LIMIT` | 50 000 | `ExploreRequest::scan_limit` (1 to 50 000). The default when omitted is 5 000. |

Other limits live in the types they bound: a namespace nests at most 8 deep,
and a segment id is 1 to 128 characters ([namespaces.md](namespaces.md)).
`recall`, `fetch` and `list` take a `limit` that must be positive; the contract
sets no upper bound for them.

### `validate_many`

`validate_many(&[StoreItem]) -> Result<()>` checks a batch: `1..=MAX_STORE_MANY`
items, each passing `StoreItem::validate`. Engines overriding `store_many`
call it first. It returns `Error::InvalidRequest` for an empty or oversized
batch, otherwise the first invalid item's error.

### Free helpers

| Function | Purpose |
| --- | --- |
| `explore_by_listing(&engine, req)` | The default `explore`: scan `list`, count facet values, build the page. |
| `get_by_listing(&engine, req)` | The default `get`. |
| `in_request_order(&ids, found)` | Orders a `BTreeMap<ItemId, Hit>` by the requested ids, each once. |

`explore_by_listing` and `get_by_listing` accept any `E: MemoryEngine + ?Sized`.
`in_request_order` is public in `explore` but not re-exported from the crate
root.

## `EngineDescriptor`

A value an engine returns from `descriptor()`; it is `Serialize` only (it holds
`&'static str` fields).

| Field | Meaning |
| --- | --- |
| `id: &'static str` | Stable id used in configuration (`cortexdb`, `tinyhumans`, `reference`). |
| `label` | Human-readable name. |
| `description` | One sentence. |
| `hosted: bool` | A third party runs the engine. |
| `needs_endpoint: bool` | Configuration must name an endpoint. |
| `needs_key: bool` | Configuration must supply a credential. |
| `default_endpoint: Option<&'static str>` | Used when configuration names none. |
| `fetch_modes: Vec<FetchMode>` | The modes the engine serves. |

Methods: `supports(mode) -> bool`, and `ensure_mode(mode) -> Result<()>`, which
fails with `Error::Unsupported("engine `<id>` does not offer <mode> fetch")`.

## `EngineHealth`

| Variant | Meaning |
| --- | --- |
| `Ok` | Serving. |
| `Degraded(String)` | Serving, impaired (rate limited, partially available). |
| `Down(String)` | Not serving. |

`is_serving()` is `false` only for `Down`. Wire form is adjacently tagged:

```json
{ "state": "ok" }
{ "state": "degraded", "reason": "rate limited" }
{ "state": "down", "reason": "connection refused" }
```

## `EngineStatus`

`EngineStatus::from_engine(&descriptor, health)` maps engine health into the
host subsystem vocabulary (`ready`, `degraded`, `down`) and supplies the
contract's five required operations (`recall`, `fetch`, `store`, `forget`, and
`list`) as capabilities; these are required by `MemoryEngine`, not optional
descriptor-specific features. It also sets `contract_version: "2.0"`.
`EngineStatus::off(reason)` reports no usable
engine with no capabilities.

## `Error`

One enum, built with `thiserror`. Variants classify a failure by what a host
can do about it. Messages are lowercase, carry no trailing punctuation, and
never carry a credential: an engine sanitises its own failure before it becomes
`Error::Engine`. `Error` is `Clone + PartialEq + Eq` and serializes its variant
and sanitized message so a caller can retain its category without parsing
`Display` text. `Error::code()` returns the stable host code; `is_account_wide()`
identifies failures that apply to every request on the account.

| Variant | Display prefix | Used when | Raised by `tinymemory-api` itself? |
| --- | --- | --- | --- |
| `Off(String)` | `memory is off:` | Memory is disabled or no usable engine is configured. | no, host configuration |
| `Unsupported(String)` | `unsupported:` | The engine does not offer the operation or fetch mode; the host should have read the descriptor. Also what `tinymemory-tools` returns for a write tool on read-only tools. | yes (`ensure_mode`) |
| `InvalidRequest(String)` | `invalid request:` | The request is malformed: a blank query, zero limit, empty forget target, unresolved document URI, bad namespace, out-of-range confidence, unknown cursor. | yes (every `validate`) |
| `Unauthorized(String)` | `unauthorized:` | The credential was missing, expired or rejected. | no, engines |
| `InsufficientCredits(String)` | `insufficient credits:` | The hosted account cannot make requests until its balance is topped up. | no, hosted engine |
| `NotFound(String)` | `not found:` | The addressed item or route does not exist. | no, engines |
| `Conflict(String)` | `conflict:` | The write conflicts with what the engine holds. | no, engines |
| `Unavailable(String)` | `unavailable:` | Transient (timeout, rate limit, unavailable upstream); the same call may succeed later. | no, engines |
| `Engine(String)` | `engine error:` | The engine's own failure, already sanitised. | only by the reference engine (poisoned lock) |
| `Config(String)` | `configuration error:` | The engine was configured wrongly (unknown id, missing endpoint or key, credentialed cleartext endpoint). | no, the registry in `tinymemory-integrations` |

`Error::is_transient()` is `true` only for `Unavailable`; hosts retry on it.
`get` of an unknown id is **not** an error: the id is left out of the result.

Stable `code()` values are `MEMORY_OFF`, `UNSUPPORTED`, `INVALID_REQUEST`,
`UNAUTHORIZED`, `INSUFFICIENT_CREDITS`, `PLAN_REQUIRED`, `UNAVAILABLE`, and
`ENGINE`. `Off`, `Unauthorized`, and `InsufficientCredits` are account-wide.
`Unavailable` is transient, but its generic request-level error carries no
proof of an account-wide outage. A hosted authorization message with a
`*_PLAN_REQUIRED` code has stable code `PLAN_REQUIRED` and
`Error::is_plan_required() == true`, while retaining the existing
`Unauthorized` variant so downstream exhaustive matches remain source-compatible.

`Error` is public and exhaustive. Adding a variant breaks downstream exhaustive
matches; a release that includes a new variant must use a major version bump.

`Result<T>` is `std::result::Result<T, Error>`. The conformance feature has
its own `conformance::Error` (`Check` and `Engine` variants) naming the check
that failed.

## Requests and responses

All derive `Debug, Clone, PartialEq, Serialize, Deserialize`. `filter` fields
default to the empty filter when absent on the wire.

| Request | Response | Validation (`Error::InvalidRequest`) |
| --- | --- | --- |
| `RecallRequest { question, filter, limit, instructions? }` | `RecallAnswer { answer, citations, model? }` | blank question; `limit == 0` |
| `FetchRequest { query, mode, filter, limit, cursor? }` | `FetchPage { hits, next_cursor? }` | blank query; `limit == 0` |
| `ListRequest { filter, limit, cursor? }` | `ListPage { items, next_cursor? }` | `limit == 0` |
| `ForgetTarget::Ids(Vec<ItemId>)` or `Filter(MetaFilter)` | `ForgetReport { forgotten }` | no ids; an empty filter |
| `ExploreRequest { facet, filter, limit, scan_limit }` | `ExplorePage { facet, buckets, total, missing, more_buckets, truncated }` | limit not in `1..=500`; scan_limit not in `1..=50 000` |
| `GetRequest { ids, reach? }` | `Vec<Hit>` | no ids, more than 200, or a blank id |
| `StoreItem` | `StoreReceipt { id, replayed }` | see [api-items.md](api-items.md) |

Constructors: `RecallRequest::new(question, limit)`,
`FetchRequest::new(query, mode, limit)`, `ListRequest::new(filter, limit)`,
`ExploreRequest::new(facet, limit)` (scan limit 5 000); each starts with an
empty filter and no cursor. `GetRequest` has no constructor.

`Citation { id, kind, snippet, meta, score? }` is one item an answer drew on;
its id resolves through `list`. `Hit { id, kind, text, meta, score,
confidence? }` is one stored item as a read returns it: `text` is
`StoreItem::render_text()`, `score` is `0.0` in a listing, `confidence` is a
learning's confidence and absent for other kinds.

`FacetBucket { value, count }`; `FetchMode` is `Keyword | Vector | Hybrid`
(`FetchMode::ALL`, `as_str`).

### Wire shapes

Serde names are `snake_case`. Optional fields are omitted when `None`, and
`meta` omits unset fields, an empty `tags` list and a root namespace.

A `Hit`:

```json
{
  "id": "9f2c1c6e0a8b4d3e7f5a1b2c3d4e5f6a7b8c9d0e",
  "kind": "learning",
  "text": "prefers tabs",
  "meta": {
    "namespace": "team:acme/agent:writer",
    "source": { "kind": "agent" },
    "tags": ["style"],
    "observed_at": "2026-10-04T09:30:00Z"
  },
  "score": 0.0,
  "confidence": 0.8
}
```

A `FetchRequest` page two:

```json
{
  "query": "ownership",
  "mode": "hybrid",
  "filter": { "kinds": ["document"], "folder": "/notes/rust" },
  "limit": 10,
  "cursor": "10"
}
```

A `RecallAnswer`:

```json
{
  "answer": "The user prefers tabs.",
  "citations": [
    {
      "id": "9f2c1c6e0a8b4d3e7f5a1b2c3d4e5f6a7b8c9d0e",
      "kind": "learning",
      "snippet": "prefers tabs",
      "meta": { "source": { "kind": "agent" } },
      "score": 0.91
    }
  ],
  "model": "reference"
}
```

`ForgetTarget` is externally tagged; `ForgetReport` counts only items actually
removed:

```json
{ "ids": ["9f2c1c6e0a8b4d3e7f5a1b2c3d4e5f6a7b8c9d0e"] }
{ "filter": { "workspace": "scratch" } }
{ "forgotten": 1 }
```

`ExploreRequest` and `ExplorePage` ([operations.md](operations.md#explore)):

```json
{ "facet": "folder", "filter": { "kinds": ["document"] }, "limit": 20 }
{
  "facet": "folder",
  "buckets": [{ "value": "/notes/rust", "count": 12 }],
  "total": 14, "missing": 2, "more_buckets": 0, "truncated": false
}
```

`StoreReceipt`: `{ "id": "...", "replayed": false }`.

## Conformance feature

With `features = ["conformance"]`, `tinymemory_api::conformance` provides
`run(&dyn MemoryEngine) -> conformance::Result<()>` and `ReferenceEngine`
(id `reference`, an in-memory engine serving every fetch mode). See
[testing.md](testing.md).
