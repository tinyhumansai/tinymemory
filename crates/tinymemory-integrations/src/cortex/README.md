# cortex

The CortexDB memory engine for TinyMemory v2, the `cortex` module of
`tinymemory-integrations` (feature `cortex`, on by default). One type,
`CortexEngine`, implements `tinymemory_api::MemoryEngine` over CortexDB's
append-only event log on two wires:

| Engine id | Constructor | Wire | Auth | Default endpoint |
| --- | --- | --- | --- | --- |
| `cortexdb` | `CortexEngine::direct` | `/v1/*`, bare JSON | API key (`CortexCredential`) | `https://api-v1.cortexdb.ai` |
| `tinyhumans` | `CortexEngine::tinyhumans` | `/memory/*`, `{success,data}` envelopes | `BearerSource`, resolved per request | `https://api.tinyhumans.ai` |

Both descriptors declare `fetch_modes = [Hybrid]`: CortexDB's recall body
accepts only `scope`, `query`, `budgets`, `view`, `include`, `temporal` and
`filters`, with no keyword/vector switch. `Keyword` and `Vector` fail with
`Error::Unsupported` before any request.

This README is the short in-tree summary. The full reference is under
[`docs/architecture/`](../../../../docs/architecture/):

- [`cortex.md`](../../../../docs/architecture/cortex.md): surface, credentials,
  transport, failure mapping, endpoint security, the registry and `MemoryConfig`;
- [`cortex-wire.md`](../../../../docs/architecture/cortex-wire.md): every endpoint
  and its shapes, scope layout, the v2 envelope, lookup labels;
- [`cortex-flows.md`](../../../../docs/architecture/cortex-flows.md): step-by-step
  store, list, fetch, recall, forget, get, discovery;
- [`testing.md`](../../../../docs/architecture/testing.md): the doubles, the
  conformance suite and the live tests.

## Public surface

From `tinymemory_integrations::cortex`:

- `CortexEngine::{new, direct, tinyhumans, wire}` (requests time out after 60s)
- `CortexWire { Direct, TinyHumans }`, `CortexCredential { Static, Dynamic }`
- `BearerSource` (async `bearer()`), `StaticBearer` (redacted `Debug`)
- `CORTEXDB_ENGINE_ID`, `TINYHUMANS_ENGINE_ID`, `CORTEX_API_ENDPOINT`,
  `TINYHUMANS_API_ENDPOINT`, `cortexdb_descriptor()`, `tinyhumans_descriptor()`
- `Error`/`Result` (the contract's own `tinymemory_api::Error`),
  `error_code`, `is_insufficient_credits`

Beyond the contract's reads and writes, the engine consolidates: Direct
posts `v1/beliefs/build` once per held scope a `ConsolidateRequest` admits
(`engine/consolidate.rs`, declared `Consolidation::OnDemand`) and reports the
beliefs built, since the server builds within the request; hosted
declares `Consolidation::Scheduled` and sends nothing. What was built is
read back by `beliefs` (`engine/beliefs.rs`): a `beliefs`-only recall per
held scope for a query, or the `v1/beliefs` listing without one, each belief
a `Learning` hit tagged `belief`. Fetch and list are unchanged.

A host usually goes through the registry instead of naming the engine:
`tinymemory_integrations::{MemoryConfig, EngineCredential, build_engine,
list_engines}` (modules `config` and `registry`).

## Module layout

```text
cortex/
├── mod.rs          crate-facing docs and the public re-exports
├── credential/     CortexCredential, BearerSource, StaticBearer
├── descriptor/     the two registrations, CortexWire and its route table
├── engine/         CortexEngine and one file per operation:
│                   store, list, fetch, recall, forget, items (get), scopes, cursor
├── envelope/       the v2 event envelope, scope paths, lookup labels, rebuild
├── log/            the event log: write, read (list, scopes, recall, answer),
│                   visibility waits, forget
├── transport/      HttpClient: timeouts, retries, byte caps, failure mapping,
│                   the actor header
├── error/          the contract's Error, error_code, is_insufficient_credits
└── testing/        loopback doubles of both wires (cfg(test) only)
```

## Storage layout

**Scopes.** One per item kind per namespace node, under the TinyMemory root:

```text
app:tinymemory/app:{documents,conversations,learnings}                  the root node
app:tinymemory/agent:researcher/app:{documents,conversations,learnings} an agent
app:tinymemory/team:acme/agent:writer/app:learnings                     a team member
```

The hosted backend also re-roots every scope under the caller's tenant.
`MetaFilter.kinds` and `MetaFilter.reach` pick the scopes read: a reach's own
node and inherited ancestors are known; a subtree reach or an unscoped read
discovers the nodes below from the registered scopes (`v1/scopes/list`,
`memory/scopes`). Every read names its scopes exactly; server-side traversal
(`view: "descend"`) is used only for an unscoped multi-scope recall, so one
agent's read never reaches a sibling's scope.

Namespace segments use CortexDB's built-in `agent`, `team`, `user`, `ws` and
`project` types, and the root and kind segments its `app` type. From v0.10 a
deployment admits only the scope types in its policy's `allowed_scope_types`
(`org, dept, team, app, user, agent, service, ws, project, global, system,
source` in every shipped preset) and refuses any other with `422
UNREGISTERED_SCOPE_TYPE`, so a private type such as `tm:` would need every
operator to register it first. `integration/cortexdb/` runs the engine against
a real server (v0.10.4 by default; `CORTEXDB_VERSION=v0.9.9` checks the older
release).

**Actor.** On the direct wire every request also carries `X-Cortex-Actor`,
the caller `GET v1/auth/whoami` reports for the key (learned once per client,
re-learned after a rejected credential). The CortexDB cloud mints per-account
tokens and refuses a request without it (`401 ACTOR_MISMATCH`); a static
operator key is served as `user:local`; a server with no `whoami` route gets
no header. The hosted (TinyHumans) wire names the actor itself.

**Events.** A document or learning is one event; a conversation is one event
per turn, appended in order. Each event's `content.text` is a JSON envelope:

```json
{ "v": 2, "id": "<40-hex fingerprint>", "kind": "conversation",
  "text": "<body | turn text | statement>", "meta": { ... MemoryMeta ... },
  "title": "...", "mime": "...", "learning_kind": "...", "confidence": 0.8,
  "evidence": "...",
  "turn": { "index": 0, "count": 3, "role": "user", "at": "...", "tool_calls": [] } }
```

Kind-specific fields appear only when set. Text that is not a v2 envelope is
someone else's event and is ignored. `context.observed_at` carries the turn's
`at` or the item's `meta.observed_at`.

**Labels.** Each event carries up to eight `context.labels`, each a 16-hex
SHA-256 digest: `tm:i:` (item id) on every event, plus `tm:t:` thread,
`tm:s:` source id, `tm:r:` repo, `tm:w:` workspace, `tm:a:` agent,
`tm:l:` language, and `tm:k:` source kind. A read whose filter has a labelled
field sends **one** label filter (`labels=` comma list on events,
`filters.metadata.labels` on recall) to narrow server-side, then **always**
re-applies the full `MetaFilter` client-side. `folder` and `file_path` match
as prefixes, so they cannot be labelled and are filtered only client-side.

## Operations

- **Store.** `store` is `store_items(vec![item])`, so a single store and
  `store_many` share **one** path and one set of guarantees. The item id is
  `StoreItem::fingerprint()`. Each scope's items are looked up by label first:
  if all of an item's events are there, it is a replay (`replayed: true`) and
  nothing is written; if only some turns of a conversation are present (an
  earlier store failed part-way), only the missing turns are written. Direct
  writes `v1/experience?wait=indexed`, or `v1/experience/bulk?wait=indexed`
  with `ordering: strict_temporal` when an item has two or more events due.
  Hosted writes one event at a time, in order. `store_with` with
  `WaitFor::Accepted` drops `?wait=indexed` and skips the waits below: the
  agent lifecycle's live turns return once CortexDB captured them. Every write uses a fresh
  `idempotency_key`, never a content-derived one, because CortexDB keeps a
  forgotten event's key and would swallow a re-store. Then one listing wait per
  scope written (for its last event) and one ranked-recall wait (best-effort)
  for the final event.
- **List.** Pages the scopes read (kind order, then namespace), newest first.
  The opaque cursor holds the scope's path, the engine cursor, the offset into
  that page and the last event id, which is enough to drop the engine's
  duplicate copies across page boundaries. A conversation is emitted once, on
  the page holding its turn 0, with its text assembled from all its turns.
  Scores are `0`.
- **Fetch (hybrid).** One recall per scope read with
  `budgets.per_layer_limits.events`. Events are decoded to items and the full
  filter is applied. Each item is kept once, at its best rank, and scopes are
  interleaved rank by rank. The score is `1/(1+rank)`, because CortexDB
  reports none. The cursor is an offset into the merged ranking; the next page
  asks again with a larger budget, capped at 1000 events.
- **Recall.** One scope read: one pack over it. An unscoped read over several
  scopes: one pack over `app:tinymemory` with `view: "descend"`. A reach over
  several scopes: one pack per scope (four at a time), exact, and the answer
  comes from the pack holding the most admitted events. The answer route is
  called **once** with `use_pack_id`. Hosted omits a null
  `answer_instructions`, because its schema is strict; Direct sends `null`.
  Citations come from the packs' decoded events, filtered (reach included),
  one per item, the most specific node's first, capped at `limit`, with
  `score: None`. `model` is `diagnostics.answer_model`.
- **Get.** Overridden: by the items' id labels, one lookup per scope read,
  rather than a scan.
- **Explore.** Not overridden: the contract's default pages through `list`.
- **Forget.** `Ids` looks the items' labels up in every scope the engine
  holds. `Filter` (which must be non-empty) walks the scopes it reads and
  matches the full filter. Either way the matched events are then removed with
  `selector.memory_ids`, in batches of 100. An empty selector is never sent,
  and neither is `confirm_all`. `forgotten` counts items.
- **Health.** Direct probes `GET v1/admin/health`. Hosted lists
  `memory/scopes?prefix=tmh:probe&limit=1`. `Unavailable` maps to `Degraded`
  and any other failure to `Down`. The reason keeps the message head and
  withholds the backend's own text.

## Engine behaviours this module is shaped around

These were measured against a live CortexDB by the v1 adapter. The doubles in
`testing/` reproduce all of them.

- **Append-only.** There is no update route. Forget removes events but not
  their idempotency records.
- **Accepted is not readable.** A write first waits until the label-narrowed
  listing carries its event (fatal after 30s). It then waits until ranked
  recall returns it (best-effort, 10s); a recall that is down or slow does not
  fail a write that is already durable. Hosted polling backs off to a 2s
  ceiling and treats 429/5xx while waiting as "not yet".
- **The listing emits every event twice**, and `limit` counts the copies.
  Readers dedupe by event id. A full walk refuses past 500 pages, and a cursor
  that does not advance is an error.
- **Unknown query parameters are ignored**, so paging uses exactly `cursor`.
- **Recall renders text** as `[role] {...}`; the prefix is stripped when
  decoding.
- **The forget selector field is `memory_ids`.** An empty or unrecognised
  selector means the whole scope.

## Transport

- A host may fix non-credential headers on every request
  (`CortexEngine::with_default_headers`, or `EngineSettings::headers` through
  the registry), such as the `x-sdk-name` attribution the TinyHumans backend
  expects. Reserved headers (`Authorization`, `Proxy-Authorization`, `Cookie`,
  `Host`, `Content-Length`, `Idempotency-Key`, `X-Cortex-Actor`) are refused
  with `Error::Config`, and no refusal echoes a value.

- Credentialed cleartext endpoints that are not loopback are refused with
  `Error::Config`.
- The bearer is resolved on every attempt and sent in a header marked
  sensitive. A source failure, a blank token, or a token containing CR/LF is
  `Unauthorized`, and no request is sent.
- Success bodies are capped at 64 MiB and error bodies at 64 KiB.
- Status mapping: 401/403 → `Unauthorized`, 404 → `NotFound`,
  400/413/422 → `InvalidRequest`, 409 → `Conflict`,
  429/500/502/503/504 → `Unavailable`, anything else → `Engine`. Transport
  faults (timeout, DNS, TLS, connect) are `Unavailable`.
- Hosted failures carry the backend's `errorCode` as a `[CODE] ` message
  prefix. **402 is `Engine` with `[USER_INSUFFICIENT_CREDITS]`**: it is not
  transient, so `Unavailable` would invite a retry loop, and it is not a
  credential fault, so `Unauthorized` would send the host to sign in.
  `is_insufficient_credits` detects it.
- Reads (listings, recall) are retried 3 times with 250ms·2ⁿ backoff on
  `Unavailable`. Writes are sent once.
- Hosted writes carry a random `Idempotency-Key` claim, reused across that
  write's own transient retries (up to 3). A 409 on a retry means the earlier
  attempt reached the engine. The write is then looked for, by its exact
  stored text under its item label, until the visibility budget runs out. If
  it is never found, the error is `Unavailable` and says the outcome is
  unknown.

## Tests

`cargo test -p tinymemory-integrations` runs the unit tests and the shared
`tinymemory_api::conformance` suite against both wires, through loopback
doubles with short test-only timeouts. `tests/live_cortexdb.rs` runs against
a real server when `TINYMEMORY_LIVE_CORTEXDB_URL` is set. See
[`testing.md`](../../../../docs/architecture/testing.md).
