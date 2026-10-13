# cortex

The CortexDB memory engine for TinyMemory v2, the `cortex` module of
`tinymemory-integrations` (feature `cortex`, on by default). One type,
`CortexEngine`, implements `tinymemory_api::MemoryEngine` over CortexDB's
append-only event log on three wires:

| Engine id | Constructor | Wire | Auth | Default endpoint |
| --- | --- | --- | --- | --- |
| `cortexdb` | `CortexEngine::direct` | `/v1/*`, bare JSON | API key (`CortexCredential`) | `https://api-v1.cortexdb.ai` |
| `tinyhumans` | `CortexEngine::tinyhumans` | `/memory/*`, `{success,data}` envelopes | `BearerSource`, resolved per request | `https://api.tinyhumans.ai` |
| `cortexdb-proxied` | `CortexEngine::proxied` | a subset of CortexDB's `/v1/*` routes behind a tenant-multiplexing proxy, `X-Memory-Tenant` per call | the proxy's service credential | none |

The proxied wire names each call's tenant from the leading `user:<id>`
segment of its namespace and refuses a call without one; see
[`cortex-wire.md`](../../../../docs/architecture/cortex-wire.md#the-proxied-wire).

Every descriptor declares `fetch_modes = [Hybrid]`: CortexDB's recall body
accepts only `scope`, `query`, `budgets`, `view`, `include`, `temporal` and
`filters`, with no keyword/vector switch. `Keyword` and `Vector` fail with
`Error::Unsupported` before any request.

This README is the short in-tree summary. The full reference is under
[`docs/architecture/`](../../../../docs/architecture/):

- [`cortex.md`](../../../../docs/architecture/cortex.md): surface, credentials,
  transport, failure mapping, endpoint security, the registry and `MemoryConfig`;
- [`cortex-wire.md`](../../../../docs/architecture/cortex-wire.md): every endpoint
  and its shapes, scope layout, the envelope (v3 and v2);
- [`cortex-labels.md`](../../../../docs/architecture/cortex-labels.md): the
  lookup labels and their digests;
- [`cortex-flows.md`](../../../../docs/architecture/cortex-flows.md): step-by-step
  store, list, fetch, recall, forget, get, discovery;
- [`testing.md`](../../../../docs/architecture/testing.md): the doubles, the
  conformance suite and the live tests.

## Public surface

From `tinymemory_integrations::cortex`:

- `CortexEngine::{new, direct, tinyhumans, proxied, wire}` (requests time
  out after 60s), `CortexEngine::with_private_network_cleartext`
- `CortexWire { Direct, TinyHumans, Proxied }`, `CortexCredential { Static, Dynamic }`
- `BearerSource` (async `bearer()`), `StaticBearer` (redacted `Debug`)
- `CORTEXDB_ENGINE_ID`, `TINYHUMANS_ENGINE_ID`, `PROXIED_ENGINE_ID`,
  `CORTEX_API_ENDPOINT`, `TINYHUMANS_API_ENDPOINT`, `cortexdb_descriptor()`,
  `tinyhumans_descriptor()`, `proxied_descriptor()`
- `Error`/`Result` (the contract's own `tinymemory_api::Error`),
  `error_code`, `is_insufficient_credits`

Beyond the contract's reads and writes, the engine consolidates: Direct
posts `v1/beliefs/build` once per held scope a `ConsolidateRequest` admits
(`engine/consolidate.rs`) and reports the beliefs built, since the server
builds within the request. It declares `Consolidation::Automatic` on the
managed API, which rebuilds beliefs on its own after writes, so the lifecycle
queues no build there, and `Consolidation::OnDemand` on any other endpoint
(`descriptor::direct_consolidation`, overridden by
`CortexEngine::with_consolidation`); hosted
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
├── descriptor/     the registrations, CortexWire and its route table
├── engine/         CortexEngine and one file per operation:
│                   store, list, explore, fetch, recall, forget, items (get), scopes, cursor,
│                   attribution (observed_actor and subject on a write),
│                   tenant (the proxied wire's tenant per call)
├── envelope/       the v2 event envelope, scope paths, lookup labels, rebuild
├── log/            the event log: write, read (list, scopes, recall, answer),
│                   visibility waits, forget
├── transport/      HttpClient: timeouts, retries, byte caps, failure mapping,
│                   the actor header
├── error/          the contract's Error, error_code, is_insufficient_credits
└── testing/        loopback doubles of every wire (cfg(test) only)
```

## Storage layout

**Scopes.** One per item kind per namespace node, under the TinyMemory root:

```text
app:tinymemory/app:{documents,conversations,learnings}                  the root node
app:tinymemory/agent:researcher/app:{documents,conversations,learnings} an agent
app:tinymemory/team:acme/agent:writer/app:learnings                     a team member
```

That is the legacy layout, the default. With a scope root
(`EngineSettings::scope_root`, `CortexEngine::with_scope_root`), such as one
person's `org:<id>`, every item is laid out below that root instead, each
kind under a leaf of its own (`org:42/ws:main/app:conversations`,
`org:42/app:brain/source:gmail`), and a direct engine registers the root
with its owner (the actor `user:<id>`) before the first write. On the
hosted wire the root is the tenant's own (`EngineSettings::tenant_root`):
the engine sends `ws:main/app:conversations` and the backend stores it at
`org:<id>/ws:main/app:conversations`. A retired root
(`EngineSettings::retired_scope_root`, the earlier `user:<id>`) is still
read and forgotten, never written, until its memory has moved. See
[cortex-layout.md](../../../../docs/architecture/cortex-layout.md).

The hosted backend also re-roots every scope under the caller's tenant.
`MetaFilter.kinds` and `MetaFilter.reach` pick the scopes read: a reach's own
node and inherited ancestors are known; a subtree reach or an unscoped read
discovers the nodes below from the registered scopes (`v1/scopes/list`,
`memory/scopes`). Every read names its scopes exactly: each recall pack is
`view: "granular"` over one scope (CortexDB's public recall defaults to
`holistic`, which also reads ancestors and descendants), so one agent's read
never reaches a sibling's scope and no read is ever a parent-scope sample.

Namespace segments use CortexDB's built-in `agent`, `team`, `user`, `ws`,
`project`, `source` and `service` types, and the root and kind segments its
`app` type. From v0.10 a
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

**Events.** A learning is one event; a conversation is one event per turn,
appended in order; a document is one event, or, when its text with a piece's
envelope (metadata plus the `chunk` field) would pass 256 KiB
(`envelope::chunks::DOCUMENT_CHUNK_TARGET_BYTES`, the one granularity knob),
one event per piece of its body, cut at page breaks and headings and
packed up to that size (`envelope/chunks.rs`). No event is sent over 768 KiB of
encoded envelope (CortexDB refuses an experience over 1 MiB); an item that
cannot fit is refused before anything of its batch is sent. `get` and `list`
reassemble a chunked document, and return it only when every piece is present; `fetch` and `recall` give one hit per document,
its best-ranked piece, with a `page:<n>` (or `page:<first>-<last>`) tag when the
document marks its pages and a `section:<title>` tag when the piece starts
under a heading; a piece with neither carries no extra tag. Each event's
`content.text` is the item's own text (the body or piece, the turn's text, or
the statement), and the rest of its envelope rides in `context.labels` as
`tm:e:<NN>:` parts of at most 240 bytes of JSON (v3). The parts carry the
envelope with `text` left empty, because the text is the event's
`content.text`; joined, they read:

```json
{ "v": 3, "id": "<40-hex fingerprint>", "kind": "conversation", "text": "",
  "meta": { ... MemoryMeta ... },
  "title": "...", "mime": "...", "learning_kind": "...", "confidence": 0.8,
  "evidence": "...",
  "turn": { "index": 0, "count": 3, "role": "user", "at": "...", "tool_calls": [] } }
```

Kind-specific fields appear only when set. `meta` never holds a local path:
`file_path` is the file's name, `folder` and an absolute `workspace` are left
out, and digests stand in for filters (`"ws"`, `"fp"`, `"fd"`; see
[cortex-local-paths.md](../../../../docs/architecture/cortex-local-paths.md)).
Readable labels (`kind:`, `file:<name>`, `page:`, `section:`) sit beside the
parts. An event with empty text, or whose
labels would pass 64, is written as v2 (the whole envelope as JSON text, as
every event was before), and both layouts read. An event that is neither is
someone else's and is ignored. An item that opts out of derivation
(`MemoryMeta::derive == Some(false)`), and every tool turn, is sent with
`directives.extract: []`: indexed and searchable, but no facts, beliefs or
concepts are derived from it. `context.observed_at` carries the turn's
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
- **Export.** The same walk and cursor as List. Each item is handed back as the
  `StoreItem` its events rebuild (not the rendered text), under its id. Every
  chunked document is assembled from all its pieces and checked whole
  (`rebuild_whole`), never rebuilt from one: one missing a later piece is
  named in `incomplete`, where List leaves it out. An item is found by its
  first event (turn 0, piece 0), so one whose first piece is gone is not seen
  at all; a caller moving memory confirms what remains by walking the scopes
  again after its cleanup. It fails as List does (a bad cursor, a page the
  engine does not answer, a walk past its cap), never with a partial page, so
  a caller retries from its last cursor.
- **Fetch (hybrid).** One recall per scope read with
  `budgets.per_layer_limits.events`. A request with `max_scopes` reads at
  most that many of its scopes (`engine/recency.rs`): those a query word
  names (a segment id, or a `-`/`_` part of one), then the most recently
  written (the newest `observed_at` of a short listing, cached ten minutes
  and set to now by this engine's own writes), read in the usual order. Events are decoded to items and the full
  filter is applied. Each item is kept once, at its best rank, and scopes are
  interleaved rank by rank. A one-turn conversation hit is whole in its pack
  event; only longer conversations are assembled from their turns (four
  namespace lookups at a time). The score is `1/(1+rank)`, because CortexDB
  reports none. The cursor is an offset into the merged ranking; the next page
  asks again with a larger budget, capped at 1000 events.
- **Recall.** One pack per scope read (four at a time), exact: a reach's
  scopes, or, for an unscoped read, every kind scope the engine holds. With
  nothing to read the answer is empty and nothing is sent. The answer comes
  from the pack holding the most admitted events. The answer route is
  called **once** with `use_pack_id` (again, after every pack is built
  anew, when the packs were dropped in between, up to three rounds:
  CortexDB drops every pack on any forget). Hosted omits a null
  `answer_instructions`, because its schema is strict; Direct sends `null`.
  Citations come from the packs' decoded events, filtered (reach included),
  merged rank by rank (the most specific node's first), one per item, capped
  at `limit`, with
  `score: None`. `model` is `diagnostics.answer_model`.
- **Get.** Overridden: by the items' id labels, one lookup per scope read,
  rather than a scan.
- **Explore.** Overridden: counts each item from the event that starts it
  (turn 0, piece 0, or its only event, all of which carry the item's full
  metadata), walking the scopes in reach four at a time, so no conversation
  or chunked document is assembled. Same buckets as the contract's listing
  walk, except that a chunked document missing a piece is counted where
  `list` leaves it out. A scope that reaches the page cap marks the page
  `truncated` instead of refusing.
- **List preview.** Overridden: the `list` walk, with each conversation and
  chunked document taken from its first event (its first turn, its first
  piece) instead of being assembled; same items, order and cursors.
- **Forget.** `Ids` looks the items' labels up in every scope the engine
  holds. `forget_within` looks them up only in the scopes its reach admits,
  found as a read with that reach finds them (no listing for a reach without
  descendants, one listing under the reach's own node for a subtree), so it
  never lists or reads another tree. `Filter` (which must be non-empty) walks the scopes it reads and
  matches the full filter. Either way the matched events are then removed with
  `selector.memory_ids` and `cascade: "redact_events"`, in batches of 100.
  The cascade is always named: CortexDB's default, `derived_only`, keeps the
  events. An empty selector is never sent, and neither is `confirm_all`. `forgotten` counts items.
- **Erase.** Hosted erases the whole tree (`whole_tree`) in one
  `DELETE memory` that erases the caller's entire hosted memory. Anything
  narrower goes scope by scope, as on Direct, through the backend's
  `memory/v1/erasures` passthrough (`{scope, audit_note}`, memory-api's
  synchronous scoped erasure, unwrapped; a retriable `502
  ERASURE_INCOMPLETE` is retried; a missing route is `Unsupported`). Direct lists the registered kind scopes in reach
  with the complete scope listing, then sends `v1/erasures` with
  `confirm_all` (never a selector) once per scope, deepest first, and
  returns the erasure ids as receipts. CortexDB deletes an erased scope's
  events and releases their write keys, but only redacts the scopes below it,
  which keep theirs. Kind scopes are leaves, so this never happens; the order
  guards a layout where it could.
- **Health.** Direct probes `GET v1/admin/health`. Hosted lists
  `memory/scopes?prefix=tmh:probe&limit=1`. `Unavailable` maps to `Degraded`
  and any other failure to `Down`. The reason keeps the message head and
  withholds the backend's own text.

## Engine behaviours this module is shaped around

These were measured against a live CortexDB by the v1 adapter. The doubles in
`testing/` reproduce all of them.

- **Append-only.** There is no update route. A body `idempotency_key` replays
  the same body for 24 hours and refuses another body (409); forget by
  `memory_ids` releases it. Keys are derived from the body, so a retry is a
  replay.
- **Accepted is not readable.** A write first waits until the label-narrowed
  listing carries its event (fatal after 30s). It then waits until ranked
  recall returns it (best-effort, 10s); a recall that is down or slow does not
  fail a write that is already durable. Hosted polling backs off to a 2s
  ceiling and treats 429/5xx while waiting as "not yet"; a timeout counts
  the polls that answered without the event and those that failed.
- **A 429's numeric `Retry-After` is honoured** (else a numeric
  `RateLimit-Reset`; an HTTP-date `Retry-After` is not). Every request of the
  client waits until the latest one has passed (capped at 10s), on either
  wire.
- **`store_many_with(Accepted)`** skips the visibility waits for a batch: a
  bulk import that reads nothing back until it ends.
- **The listing emits every event twice**, and `limit` counts the copies.
  Readers dedupe by event id. A walk refuses past 500 pages of one scope, or
  past 500 pages that held events across the request, and a cursor that does
  not advance is an error. An empty scope (a registration left after a
  forget) costs one empty page, which only its own cap counts.
- **The scope listing has no cursor** and answers at most 1000 paths. A read
  takes what was listed and logs it; an export refuses.
- **Unknown query parameters are ignored**, so paging uses exactly `cursor`.
- **Recall renders text** as `[role] {...}`; the prefix is stripped when
  decoding.
- **The forget selector field is `memory_ids`.** An empty or unrecognised
  selector means the whole scope.
- **Attribution needs capabilities** (0.10.5 API reference, not measured): an `observed_actor` other
  than the caller needs `scope.write.on_behalf_of`, a `subject` other than
  it `scope.write.about_other`, else `403 POLICY_DENIED`, a bulk write whole.
  With `with_observed_actor(true)` a refused write is sent again without
  them (`engine/attribution.rs`); off, nothing is sent.

## Transport

- A host may fix non-credential headers on every request
  (`CortexEngine::with_default_headers`, or `EngineSettings::headers` through
  the registry), such as the `x-sdk-name` attribution the TinyHumans backend
  expects. Reserved headers (`Authorization`, `Proxy-Authorization`, `Cookie`,
  `Host`, `Content-Length`, `Idempotency-Key`, `X-Cortex-Actor`,
  `X-Memory-Tenant`) are refused
  with `Error::Config`, and no refusal echoes a value.

- Credentialed cleartext endpoints that are not loopback are refused with
  `Error::Config`, except a cluster-service host (`*.svc`,
  `*.svc.cluster.local`): it builds, and every call fails with
  `Error::Config` until `CortexEngine::with_private_network_cleartext` opts
  in, for an in-cluster service NetworkPolicy keeps inside the cluster.
- The bearer is resolved on every attempt and sent in a header marked
  sensitive. A source failure, a blank token, or a token containing CR/LF is
  `Unauthorized`, and no request is sent.
- Success bodies are capped at 64 MiB and error bodies at 64 KiB.
- Status mapping: 401/403 → `Unauthorized`, 404 → `NotFound`,
  400/413/422 → `InvalidRequest`, 409 → `Conflict`,
  408/429/500/502/503/504 → `Unavailable`, anything else → `Engine`. Transport
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
