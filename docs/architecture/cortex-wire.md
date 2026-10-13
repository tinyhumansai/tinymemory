# CortexDB engine: the wire

What `CortexEngine` sends to CortexDB and how it lays an item out as events.
Part of the CortexDB engine docs: [overview and transport](cortex.md) ·
this page · [operation flows](cortex-flows.md). The source is
`crates/tinymemory-integrations/src/cortex/`.

CortexDB is an append-only event log with ranked recall and a grounded answer
route. TinyMemory stores each item as one or more events in that log, and
reads them back through the listing and recall routes.

## Two wires

One engine type, `CortexEngine`, speaks two HTTP surfaces. `CortexWire`
selects the surface; `CortexWire::path` is the only place a route name lives.

| | `Direct` (`cortexdb`) | `TinyHumans` (`tinyhumans`) |
| --- | --- | --- |
| Constructor | `CortexEngine::direct(endpoint, CortexCredential)` | `CortexEngine::tinyhumans(base_url, Arc<dyn BearerSource>)` |
| Default endpoint | `https://api-v1.cortexdb.ai` (`CORTEX_API_ENDPOINT`) | `https://api.tinyhumans.ai` (`TINYHUMANS_API_ENDPOINT`) |
| Route prefix | `/v1/*` | `/memory/*` |
| Success body | bare JSON | `{"success": true, "data": ...}`; `data` is unwrapped |
| Failure body | any text (an excerpt is kept) | `{"success": false, "error": "...", "errorCode": "CODE"}` |
| Credential | API key (static) or a bearer source | bearer source (session JWT or `tiny_live_` key) |
| `X-Cortex-Actor` | learned from `v1/auth/whoami` | not sent (the backend names the actor) |
| Write extras | `?wait=indexed`, a bulk route | none: one event per request, an `Idempotency-Key` claim |
| Health route | `v1/admin/health` | none: lists one scope under a prefix |

Both descriptors declare `fetch_modes = [Hybrid]`. CortexDB's recall body
accepts only `scope`, `query`, `budgets`, `view`, `include`, `temporal` and
`filters`; nothing switches between lexical and embedding retrieval, so
declaring `Keyword` or `Vector` would promise a ranking the wire cannot ask
for. Both fail with `Error::Unsupported` before any request.

### Routes

| Logical route | Direct | TinyHumans | Method |
| --- | --- | --- | --- |
| Experience (append one event) | `v1/experience` | `memory/experience` | POST |
| Bulk (append an ordered batch) | `v1/experience/bulk` | `memory/experience` (never used for a batch) | POST |
| Events (list a scope) | `v1/events` | `memory/events` | GET |
| Recall (build a pack) | `v1/recall` | `memory/recall` | POST |
| Forget | `v1/forget` | `memory/forget` | POST |
| Answer | `v1/answer` | `memory/answer` | POST |
| Health | `v1/admin/health` | `memory/scopes` | GET |
| Scopes (registered scopes under a prefix) | `v1/scopes/list` | `memory/scopes` | GET |
| Build beliefs (one scope) | `v1/beliefs/build` | none (never sent) | POST |
| Erase (one scope) | `v1/erasures` | `memory/v1/erasures` (unwrapped) | POST |
| Erasure status (Direct only) | `v1/erasures/{id}` | | GET |
| Whoami (Direct only) | `v1/auth/whoami` | | GET |

The endpoint is joined with the route, so a base URL with a path prefix keeps
it (a trailing `/` is added when missing).

### The proxied wire

A third wire, `CortexWire::Proxied` (`cortexdb-proxied`, built with
`CortexEngine::proxied(endpoint, CortexCredential)`), speaks to CortexDB
behind a tenant-multiplexing cortexdb-saas proxy (memory-api). The proxy
holds one service credential for many tenants, pins each tenant's scopes
under its own root (`org:<id>`), and forwards only an allowlist of
CortexDB's own `/v1/*` routes. It answers in CortexDB's dialect, so bodies
are as on Direct; refusals carry memory-api's `error_code`, which leads the
message as `[CODE] `.

- **Tenant per call.** Every request carries `X-Memory-Tenant: <id>`, where
  `<id>` is the id of the leading `user:` segment of the call's namespace
  (the item's namespace on a write, the reach's `at` on a read, forget or
  erase). A call naming no such namespace (no reach, the root, `team:…`, a
  forget by bare ids) or two different tenants fails with `Error::Config`
  before anything is sent; no request ever goes out without a tenant. Ids
  are 1 to 96 characters of `[A-Za-z0-9_-]`. The header is reserved, so
  `with_default_headers` cannot fix one tenant for every call.
- **Tenant layout.** No root is sent and none is registered: the engine
  starts in the `with_tenant_root` layout, and `with_scope_root` is refused.
- **Routes.** As Direct for experience, bulk, events, recall, answer,
  forget, scopes/list, beliefs and beliefs/build, and `v1/admin/health`
  (probed as tenant `tinymemory-health`, under which nothing is written).
  Never sent: `v1/erasures`, `v1/scopes` and `v1/scopes/members`,
  `v1/admin/version`, `v1/auth/whoami`; no `observed_actor` attribution
  (the proxy strips it).
- **Erase.** A scope is erased by listing its events and forgetting them by
  `memory_ids` (no receipts). Erasing the tenant's `user:<id>` node with its
  descendants and every kind is `POST v1/tenant/purge`, which removes
  everything under the tenant's root, blobs included.
- **Cleartext inside a cluster.** A cleartext endpoint on a cluster-service
  host (`*.svc`, `*.svc.cluster.local`) builds, but every call fails with
  `Error::Config` until `with_private_network_cleartext()` opts in. Any
  other non-loopback `http://` endpoint is still refused when built.

## Endpoints and their shapes

Only the fields the engine reads or writes are listed. Unlisted response
fields are ignored.

### Append: `experience` and `bulk`

Request body (one event). It is the same on both wires:

```json
{
  "scope": "app:tinymemory/agent:researcher/app:documents",
  "modality": "document",
  "idempotency_key": "tm3:<the first 56 hex digits (224 bits) of the SHA-256 of this body without the key>",
  "content": { "kind": "message", "role": "user", "text": "<envelope JSON, see below>" },
  "context": {
    "labels": ["tm:i:<16 hex>", "tm:k:<16 hex>"],
    "observed_at": "2026-01-02T03:04:05+00:00"
  }
}
```

- `modality` is `document` for a document, `observation` for a learning, and
  `conversation` for a turn. `content.role` is `user` for documents and
  learnings and the turn's speaker (`user`, `assistant`, `system`, `tool`)
  for a conversation turn.
- `idempotency_key` is derived from the body, so an identical retry is a
  replay (see [flows: store](cortex-flows.md#store-and-store_many)). The
  answer's `replayed_from_idempotency` is read; absent counts as `false`.
- `context.observed_at` is the turn's `at`, else the item's
  `meta.observed_at`; it is omitted when neither is set.
- `context.labels[0]` is always the item label; the writer relies on that.

Response: `{"event_id": "..."}` (Direct answers `202`, with `status` and
`replayed_from_idempotency` the engine does not read). A response without
`event_id` is `Error::Engine`.

Direct appends with `?wait=indexed`, except for a store that waits only for
acceptance (`store_with` with `WaitFor::Accepted`, which the agent lifecycle
uses for live turns). That store omits the parameter and also skips the
visibility waits, so the call returns once CortexDB has captured the event.
A single event goes to `v1/experience`;
**two or more** go to `v1/experience/bulk` with

```json
{ "items": [ ...experience bodies... ], "ordering": "strict_temporal" }
```

and the response must carry `results` with one entry per request, the last
naming `event_id`. A missing `results`, or a count that differs from the
number sent, is `Error::Engine`. Note that a conversation of one turn, or one
with a single missing turn, goes the single-event route.

TinyHumans always sends one event per request, in order, each under an
`Idempotency-Key` header claim (see [transport](cortex.md#idempotency-claims)).

### List: `events`

```text
GET {events}?scope=<scope>&limit=200[&labels=<l1,l2,...>][&cursor=<cursor>]
```

Response:

```json
{ "items": [ { "id": "evt_1", "scope": "...", "content": { "text": "..." },
               "context": { "labels": [], "observed_at": "..." } } ],
  "has_more": true, "next_cursor": "..." }
```

- Newest first. The engine emits **every event twice** and `limit` counts the
  copies, so a page of 200 holds about 100 distinct events. Readers dedupe.
- `labels` is **one** comma-separated parameter (the hosted backend refuses a
  repeated `labels=`); at most 50 labels per request. An event matches when it
  carries any one of them.
- A next page exists only when `has_more` is `true` **and** `next_cursor` is
  present. A `next_cursor` equal to the cursor just sent is `Error::Engine`
  (a listing that does not advance).
- Unknown query parameters are ignored by the engine, so the paging parameter
  is exactly `cursor`; a misspelling would serve page one for ever.

### Recall: `recall`

```json
{ "scope": "app:tinymemory/app:documents", "query": "...",
  "view": "granular", "include": ["events"],
  "budgets": { "max_tokens": 23592960, "per_layer_limits": { "events": 30 } },
  "filters": { "metadata": { "labels": ["tm:t:<16 hex>"] } } }
```

`filters` is present only when the metadata filter has a labelled field.

- `view` is always `"granular"`: exactly the named scope. CortexDB's public
  recall defaults to `holistic` (the scope, its ancestors and its
  descendants), and a pack at a parent scope is filled from its children in
  storage order (0.10.4 marks it `parent_pack_unranked_sample`), so no read
  relies on either.
- `include` lists `events` first. `budgets.max_tokens` (4000 by default) is a
  cross-layer budget that funds `include`'s layers first, then
  facts > beliefs > episodes > understanding > events, so without it events
  are evicted first. A fetch that wants beliefs sends `["events", "beliefs"]`,
  an answer pack `["events", "facts", "beliefs", "episodes",
  "understanding"]`, a beliefs read `["beliefs"]`.
- `max_tokens` is sent, sized so every event asked for comes back whole: a
  token per byte of the largest event this crate writes (768 KiB), for each
  event (and, in an answer pack, each derived item). The default, 4000
  tokens (about 14 KB), cuts a longer event to a `budget_excerpt` (0.10.4
  API §9.5): a slice of the stored envelope that no longer decodes, so a
  document piece would never be a hit. It also evicts.
  The budget only stops the cutting: `per_layer_limits` still bounds a pack,
  so a pack of `n` events carries at most `n` × 768 KiB of event text (a
  fetch of 5 asks 18 events: at most 13.5 MiB, typically far less). The
  budget is capped at 8 Mi tokens, about 24 to 28 MiB at the 3 to 3.5 bytes
  a token CortexDB 0.10.4 counts (measured on English, CJK and random text),
  so a token per byte is at least three times the room an event needs. Only
  a pack holding more than that (at least 32 events of the largest size, or
  about 100 at the chunk target) gets excerpts again, which do not decode
  and are logged at warn (`log/notes.rs`).
- `temporal` carries only a fetch's `refers_to` (`TimeHint`), as
  `{"refers_during": {from, to}, "timezone"}`: boost-only (capability
  `refers_to_v1`), never `natural`/`valid_during`, which filter by capture
  time. Direct asks `v1/admin/version` once; hosted sends it. A hinted read
  refused as invalid is retried bare, and a bare success turns hints off for
  the engine (`engine/refers.rs`). The merged hits are lifted by day again,
  since the rank-by-rank scope merge would bury the server's boost.
- Every pack's `warnings[]` is logged (debug), with
  `parent_pack_unranked_sample` and any knapsack eviction
  (`diagnostics.knapsack_evictions`, or a `context_contributors` row with
  `evicted_from_layers: true`) at warn, with the scope (`log/notes.rs`).
Response: `{"pack_id": "...", "layers": {"events": [...]}}`. Events in a pack
render their text for a reader as `[role] {...}`; the decoder strips that
prefix. A pack's events are read from `/layers/events` and decoded exactly
like listing events.

For `recall` (the answer path) the budget also names the derived layers:
`events` is `2 * limit`, and `facts`, `beliefs`, `episodes` and
`understanding` share `limit` between them (the remainder goes to the first
ones).

### Answer: `answer`

```json
{ "scope": "...", "question": "...", "use_pack_id": "pack_...",
  "cite_sources": true, "include_context": true,
  "answer_instructions": "..." }
```

Response fields read: `answer` (required, string) and
`diagnostics.answer_model` (optional, becomes `RecallAnswer.model`).

A 404 means the pack is gone: packs live 60 s, and CortexDB drops every
pack it holds once anything is forgotten. Every scope's pack is then built
again and the answer asked from the new chosen pack, up to three rounds in
all (see
[recall](cortex-flows.md#recall), step 5).

`answer_instructions` is the request's instructions when set. When unset,
Direct sends `null` and TinyHumans **omits the key**: its answer schema is
strict (an unknown key, or a `null` instructions, is a 400).

### Forget: `forget`

```json
{ "scope": "...", "layers": ["events"],
  "selector": { "memory_ids": ["evt_1", "evt_2"] },
  "cascade": "redact_events",
  "audit_note": "tinymemory: forget" }
```

The cascade is always named. CortexDB's default, `derived_only`, removes what
was derived from the events and keeps the events, so a forget that left it out
would not remove anything written. memory-api tombstones a forget by
`memory_ids` (not one by labels or time range), so this is the forget to use
for a real delete.

At most 100 ids per request. The id field is exactly `memory_ids`: an
unrecognised or empty selector means *the whole scope* to CortexDB (an empty
selector needs `confirm_all`, and `confirm_all` beside a selector is refused).
The engine never sends an empty selector and never sends `confirm_all`; a
scope with nothing to remove sends no request at all.

What it removes (measured on 0.10.4 with real enrichment): the named raw
events **and** every derived record citing them (facts, beliefs, episodes,
understanding), so a forgotten item is not recallable through any layer;
records derived from other events stay. `layers: []` does the same; a
`layers` list that does not name `events` deletes nothing. A forget blanks
rows rather than producing an erasure manifest: GDPR-grade deletion of a
whole scope is `v1/erasures` (below), and of a whole hosted memory
`DELETE memory`.

### Scopes: `v1/scopes/list` and `memory/scopes`

```text
GET {scopes}?prefix=<scope prefix>&limit=1000
```

The reader accepts either `{"items": [{"path": "..."}]}` (Direct) or
`{"scopes": ["..."]}` (hosted), and for each entry either a bare string or an
object with `path`. A `404` means "no scope listing" and is treated as no
scopes. There is no cursor (v0.10.5): `limit` defaults to 50 and is clamped to
1000, and `prefix` matches whole segments. Both wires get the bare node path: CortexDB refuses a
separator-terminated prefix (`422 INVALID_SCOPE_GRAMMAR`, "segment N is
empty", measured on the live server). The reader keeps only paths at or below
the prefix on whole segments (behind a hosted tenant prefix too), so a backend
matching plain string prefixes could not add `user:anna` to a `user:ann`
listing. At 1000 paths a read logs a warning
and an export refuses, since some scopes may be missing.

### Erase: `v1/erasures` and `memory/v1/erasures`

```json
{ "scope": "app:tinymemory/agent:assistant/app:learnings", "confirm_all": true, "audit_note": "tinymemory: erase" }
```

The only request this crate sends with `confirm_all`, and it never carries a
selector. CortexDB runs the erasure before it answers `202` with
`erasure_id`, `status: "completed"`, `receipt_url` and `verify_url`. It needs
the `forget.gdpr` capability and owner membership of the scope: a static
operator key has both, and a `service:`/`agent:` token is refused with `403
POLICY_DENIED`. The scope's events are deleted and their write keys released.
Scopes below it are only redacted and keep their keys for 24 hours, so a
re-sent write there replays and stores nothing. A whole-scope erasure of six
240 KB events took about 8 s on v0.10.5; small scopes take well under a
second. Every erasure drops every recall pack the server holds, as a forget
does. A `running` answer is polled at `GET v1/erasures/{id}` until it
settles (five minutes at most); any final status but `completed` is an error.

Hosted, an erase narrower than the whole tree posts each kind scope to the
backend's `memory/v1/erasures` passthrough of memory-api's scoped erasure:

```json
{ "scope": "app:tinymemory/agent:assistant/app:learnings", "audit_note": "tinymemory: erase" }
```

No `confirm_all` (memory-api refuses an unknown field with `400
UNKNOWN_FIELD`). memory-api pins the scope under the tenant root, erases it and
everything below it with the tenant's user-actor token, and answers
synchronously and unwrapped:
`{"erased": true, "scope": "...", "scopes": <n>, "erasure_ids": [...]}`
(`scopes: 0` means nothing was stored, still a success). `502
ERASURE_INCOMPLETE` (`retriable: true`) is retried up to three times; the root
is `422 ROOT_ERASURE_REFUSED`. A backend without the route (404) is reported as
`Unsupported`, so a caller can fall back to a forget.

### Erase everything: `DELETE memory` (TinyHumans only)

The hosted engine erases the whole tree (`EraseRequest` with `whole_tree`,
the root with its descendants, every kind) in one `DELETE memory` with no body. It erases the
caller's **entire** hosted memory, every scope under their tenant, whichever
layout or client wrote it, and answers
`{"success": true, "data": {"erased": true, "scopes": <n>}}`;
`EraseReport.erased_scopes` is `n` and there are no receipts. It is sent once.
A narrower erase goes through `memory/v1/erasures` (above). A backend without
the route (404) is reported as `Unsupported`.

### Build beliefs: `v1/beliefs/build` (Direct only)

`consolidate` resolves its reach and kinds to the kind scopes that CortexDB
has registered. It reads them through `v1/scopes/list` under
`app:tinymemory`, keeping only the scopes the reach admits. It then posts one
request per scope, in order:

```json
{ "scope": "app:tinymemory/source:pdf/app:documents" }
```

CortexDB v0.10 builds within the request, from the facts its enrichment has
already extracted, and answers with what it built:

```json
{ "built": 2, "items": [ … ], "facts_scanned": 4, "events_scanned": 4,
  "reasons": { "no_subject_or_predicate": 2 } }
```

When every answer carries `built`, the receipt is `Completed` with the counts
summed in `built`. An answer naming a job instead (`job_id`, `build_id` or
`id`) means the build was queued: the receipt is `Started` with the handles.
A build takes seconds per scope with a real model (8–30 s for a whole
scenario in [the eval](../evals/agent-memory.md)), so it belongs off the turn.
Each build is sent once and never retried: a host can always ask again.

The beliefs land in a derived layer, read two ways:

- **Inside a fetch** (`FetchRequest::beliefs > 0`): each scope's recall
  pack also carries `"beliefs": N` in `per_layer_limits`. One pack, and one
  query embedding, serves both the events and the beliefs. This is how
  holistic recall reads them.
- **`beliefs` with a query** (`engine/beliefs.rs`), for a caller that wants
  beliefs alone: one recall per scope held in reach, with a budget for the
  `beliefs` layer only:

  ```json
  { "scope": "…", "query": "…",
    "budgets": { "max_tokens": 6291456,
                 "per_layer_limits": { "events": 0, "facts": 0, "episodes": 0,
                                       "understanding": 0, "beliefs": 8 } } }
  ```

- **Without one:** `GET v1/beliefs?scope=…&limit=…` per scope, ordered most
  confident and then newest. The hosted wire has no listing and answers
  none.
- **Each belief** (`{id, scope, claim: {subject, predicate, object},
  stance, confidence, valid_from}`) becomes a `Learning` hit:
  - its text is `subject predicate object`, with the predicate's
    underscores as spaces;
  - it is tagged `belief`, at its scope's node;
  - only `supported` and `contested` stances are read, and a contested
    belief says so.

The answer route reads the same layer, so a `SectionQuery::Answer` section
(a compaction summary, a `context.md` brief) uses beliefs as well. Fetch
and list are unchanged: they decode only this crate's events. The TinyHumans backend has no such route; its descriptor declares
`Consolidation::Scheduled`, and `consolidate` sends nothing.

### Health

Direct: `GET v1/admin/health`. TinyHumans has no health route, so it lists one
scope under a prefix the engine never writes:
`GET memory/scopes?prefix=tmh%3Aprobe&limit=1`. The memory API refuses a
prefix that is not `type:id` segments (a bare word is a 400, which would
report a healthy service as broken). This proves reachability and the
credential in one round trip. `Error::Unavailable` is `Degraded`, any other
failure is `Down`; the reason keeps the message head and withholds the
backend's own text (everything after a spaced em-dash).

### Whoami (Direct only)

`GET v1/auth/whoami` returns `{"caller": "user:local"}`. See
[the actor header](cortex.md#the-actor-header).

## Scope layout

Every item lives in the scope of its **kind** at its **namespace node**,
below the engine's root: `app:tinymemory` (legacy, the default), or a host's
own root such as `user:<id>` (v3). [cortex-layout.md](cortex-layout.md) has
both layouts. Legacy, for example:

```text
app:tinymemory/agent:researcher/app:{documents,conversations,learnings}  an agent
```

So within every node, documents, conversations and learnings are separate
scopes and CortexDB can recall, retain and erase each on its own. The
hosted backend re-roots every scope under the caller's tenant, which is
invisible to the engine except that scope paths it reads back may carry a
prefix: the root is found wherever it sits.

**Scope-type mapping.** A namespace segment `kind:id` becomes the CortexDB
scope segment of the same text, using the contract's prefixes:

| Namespace segment | Scope segment type |
| --- | --- |
| Agent | `agent` |
| Team | `team` |
| User | `user` |
| Workspace | `ws` |
| Project | `project` |
| Source | `source` |
| Service | `service` |
| TinyMemory root and each kind leaf | `app` |

These are CortexDB's built-in types, chosen on purpose. From CortexDB v0.10 a
deployment admits only the types in its policy's `allowed_scope_types`
(`org, dept, team, app, user, agent, service, ws, project, global, system,
source` in every shipped preset) and refuses any other with
`422 UNREGISTERED_SCOPE_TYPE`. A private type such as `tm:` would need every
operator to register it first, so the engine uses only types that every
preset allows. A namespace nests at most 8 deep, which keeps the path far
inside the hosted grammar (at most 31 `type:id` segments, as the hosted
double enforces).

**Which scopes a read touches.** `MetaFilter.kinds` picks the kinds and
`MetaFilter.reach` the nodes (`engine/scopes.rs`). Ordering is by kind
(`ItemKind::ALL`) and then namespace, so a cursor can resume by position.

- A reach **without descendants** reads `at` and, when it inherits, each
  ancestor. The nodes are known, so no request is made; a node nothing was
  written to simply lists empty.
- A **subtree reach, or no reach**, needs the nodes below. They are
  discovered once per call from the scopes registered under the reach's own
  node (its layout prefixes, [cortex-layout.md](cortex-layout.md); the whole
  root for no reach), and the root's kind scopes are always read. Neither
  enters a `service:` sandbox below its node.
- Reads are always exact: every pack is `view: "granular"` over one scope,
  so one agent's read never reaches a sibling's scope and no read is a
  parent-scope sample.
- A filter whose `kinds` admits nothing reads no scopes.

The envelope, the lookup labels and the CortexDB behaviours the engine is
shaped around continue in [cortex-wire-envelope.md](cortex-wire-envelope.md).
