# CortexDB engine: operation flows

Step by step, what each `MemoryEngine` method does on the CortexDB engine.
Part of the CortexDB engine docs: [overview and transport](cortex.md) ·
[the wire](cortex-wire.md) · this page. Sources are under
`crates/tinymemory-integrations/src/cortex/engine/` and `.../log/`.

Every method first validates its request with the contract's `validate`
methods, so a malformed call fails as `Error::InvalidRequest` before any
request is sent. Read [the wire](cortex-wire.md) for what "scope", "label" and
"envelope" mean here.

## Store and store_many

`store(item)` is `store_items(vec![item])` and returns the single receipt.
There is **one** path, so a single store gets exactly the batch's guarantees:
listed on return, and ranked recall awaited for its final event.

`store_many` first runs `validate_many` (a batch of 1 to `MAX_STORE_MANY` (100)
valid items; an empty or oversized batch is `Error::InvalidRequest`). Then:

1. **Fingerprint.** Each item's id is `StoreItem::fingerprint()`, a digest of the
   whole item, metadata and namespace included but `meta.observed_at` not, so
   the same text at two nodes is two items and a re-sync that only restamps
   `observed_at` is a replay.
2. **Group** the items by scope (kind at namespace node).
3. **Replay detection.** Skipped only on the turn-logging hot path: a Direct
   store of one single-turn conversation with `WaitFor::Accepted` (the agent
   lifecycle makes two per turn), where the body key catches a retry (below).
   Otherwise one id lookup per scope: the listing narrowed by the
   items' `tm:i:` labels (batches of up to 50 labels), each hit re-checked
   against the envelope's real id. The result is, per id, which turn indexes
   are already held (`None` for a document or learning).
4. **Write, in item order, without waiting.** For each item, build its
   envelopes (one per event) and skip every event already held:
   - every event present: a **replay**; nothing is written and the receipt has
     `replayed: true`;
   - some turns of a conversation present: a previous store failed part way;
     only the missing turns are written, in order (and the receipt is not a
     replay);
   - nothing present: every event is written.

   An item repeated inside the batch is a replay of its first copy. Each
   event is keyed by its own body (see below), and a receipt is also a
   replay when CortexDB answered every written event as one
   (`replayed_from_idempotency`). The wire call is made per
   item: Direct sends one experience, or one ordered bulk when two or more
   events are due; TinyHumans sends the events one at a time.
5. **Wait, once per scope.** For the **last event written** in each scope the
   engine waits until it is listed (the log is ordered, so its being listed
   implies the earlier ones are). Ranked recall is awaited for one event
   only: the last event written to the most recently written scope. See
   [waiting](#waiting-for-a-write-to-be-readable).

Receipts come back in item order, each `{id, replayed}`. On an error the items
before the failing one are stored, and storing them again is a replay.

**Body idempotency keys.** Each event's `idempotency_key` is `tm3:` and the
first 56 hex digits (224 bits) of the SHA-256 of its compact JSON request
body without the key (60 characters;
CortexDB refuses one over 64). Measured on CortexDB 0.10.4:

- the same key and body is a replay, answered with the first event's id and
  `replayed_from_idempotency: true`, and nothing is written;
- the same key with another body is `409 IDEMPOTENCY_CONFLICT`, which a body
  key never produces (any change to the body is a new key);
- `/v1/forget` by `memory_ids` releases the key, so an item stored again
  after it was forgotten is written again.

A key lasts 24 hours, and an unchanged item restamped with a new
`observed_at` has a new body, so the key does not replace the lookup. It
catches retries, and on the hot path it is the only replay check. The hosted
memory API's `Idempotency-Key` **claim** stays fresh per write
(`tm-<salt>-<nanos>-<seq>`), reused only across the retries of that write: it
answers every replay of a claim with 409. The hosted wire always looks up.

### Waiting for a write to be readable

The contract requires read-after-write; CortexDB indexes after it accepts. A
write waits in up to two stages:

1. **Listed** (fatal on timeout, 30s). Poll the scope's listing narrowed to the
   item label until it carries the event's id. Failure is `Error::Unavailable`
   ("did not become readable"). One page suffices since the listing is newest
   first.
2. **Settled** (best-effort, 10s). Poll ranked recall (query is the first 256
   characters of the stored text) until the event is in the pack. A recall
   that is down, slow, or errors ends the wait quietly: the write is durable
   and listed, so it is not reported as failed.

Polling starts at 250ms. Direct keeps that gap; TinyHumans doubles it up to a
2s ceiling (a fixed 250ms poll would spend a fifth of the backend's 300
requests per minute on one write). On TinyHumans a 429 or 5xx while waiting
for the listing means "not yet" and the wait continues to its deadline; on
Direct such an error is returned. A timeout says how many polls answered
without the event and how many failed transiently (with the last fault), so
a listing that never showed the event is told apart from one that could not
be read.

**Rate limits.** A `429` whose `Retry-After` (else `RateLimit-Reset`) gives a
wait in whole seconds holds every later request of that client back until it
has passed, at most 10s (`transport/pause.rs`): retrying sooner only spends the
next attempt on another `429`. An HTTP-date `Retry-After` is not honoured. A
request already waiting re-reads the pause when it wakes, so a later, longer
one holds it too.

### Hosted writes and outcome-unknown recovery

Each TinyHumans write carries a random `Idempotency-Key` claim, reused across
that write's own retries (3 attempts, 250ms then 500ms apart, on a transient
error). The memory API takes the claim before forwarding and answers any replay
of a claimed key with 409 without forwarding it; the backend relays that as
`409` with `errorCode: CONFLICT` (older backends: `400` with the same code),
which the client reads as the same conflict.
So:

- a transient fault (429, 5xx, timeout) is retried under the same claim; a
  fault raised before the memory API (its own rate limiter) leaves the claim
  free, and the retry is simply forwarded;
- a **409 on a retry** means the earlier attempt reached the engine and may
  have been applied: the outcome is unknown, not failed. The engine looks for
  the event (same scope, same item label, exactly the same stored text) until
  the 30s budget runs out. If it is found its id is the receipt. If not, the
  error is `Unavailable` and says the outcome is unknown.

A 409 on the *first* attempt is returned as `Error::Conflict`. Direct writes
are sent once: a timeout leaves the outcome unknown, and the replay detection
makes a retry by the caller safe.

## List

`list_page` reads a cursor over the listings of the scopes the filter reads,
in `ItemKind::ALL` order and then by namespace, each newest first.

1. Resolve the scopes (see [scope layout](cortex-wire.md#scope-layout)); none
   means an empty page. Decode the cursor, or start at the first scope.
2. If the cursor names a scope that no longer exists, resume at the next scope
   in order from its first page.
3. Page the scope with `limit=200`, narrowed by one label when the filter has
   a labelled field. For each raw event: skip a copy equal to the previous
   event id (the engine emits each event twice; the cursor remembers the last
   id so this works across page boundaries), decode it, and keep it when it is
   an envelope of the scope's kind and the **full** `MetaFilter` matches.
4. **Each item once.** A learning, or a document written whole, is one
   event. A conversation, or a chunked document, is emitted only on the page
   holding its first event (**turn 0**, **piece 0**); its text is assembled
   from all its events by one label lookup per kind for the page. A conversation whose store failed part way still has turn 0 and
   lists with the turns it holds.
5. Stop when `limit` items are collected and return a cursor, unless the end
   of the last scope was reached (then there is none).

Scores are `0`. The whole call reads at most 500 engine pages; past that it
fails with `Error::Engine` rather than answer from a truncated log. A cursor
the wrong operation produced, or any malformed one, is `Error::InvalidRequest`.

**The cursor** is opaque: a one-letter tag (`l` list, `f` fetch) followed by
hex-encoded JSON, so a host stores it and passes it back but cannot usefully
edit it. A list cursor holds the scope's path (not a position, so a scope
created between pages cannot shift the listing), the engine's cursor for the
page being read, the offset of events already consumed on it, and the last
event id.

## Fetch

Only `Hybrid`. Other modes fail `Error::Unsupported` before any request.

1. Decode the cursor (an offset into the merged ranking) and compute the
   page end `offset + limit`.
2. With `max_scopes` set and more scopes than that, keep only that many:
   the scopes a query word names first (a segment id, or a `-`/`_` part of
   one), then the most recently written (the newest `observed_at` of one
   short listing per scope, cached ten minutes, set to now by this engine's
   own writes), then the usual order; the kept ones are read in the usual
   order.
3. For **each scope** the filter reads, ask recall for a pack with
   `events = min((end + 1) * 3, 1000)`, narrowed by one label when possible.
   (Three raw events per wanted hit, because a conversation contributes
   several turns and the client-side filter drops some. 1000 events is the
   deepest a fetch page can go.)
4. Decode each pack's events, apply the **full filter** client-side, keep each
   item once at its best rank.
5. **Interleave** the scopes rank by rank: every scope's best, then every
   scope's second, and so on.
6. Take the page `[offset, end)`. A conversation hit carries the whole
   conversation. A one-turn conversation (`turn.count == 1`, as every turn a
   host logs per item is) is whole in its pack event and needs no lookup; a
   longer one is assembled from all its turns, one lookup per namespace node,
   four nodes at a time.
7. Score each hit `1 / (1 + rank)`, since CortexDB reports no score.
8. `next_cursor` is `offset = end` when the merged ranking held more than `end`
   items, else none. The next page asks again with a larger budget.

## Recall

Recall builds a pack, asks the answer route **once** with `use_pack_id`
(again, from every pack built anew, when the packs were dropped in between, step 5), and cites from the packs.

1. Resolve the scopes: a reach's kind scopes (its node and, when it
   inherits, every ancestor; below it too for a subtree reach), or, with **no reach** (an
   unscoped, administrative read), every kind scope the engine holds
   (`v1/scopes/list`). No scope to read (nothing held, or a filter that
   admits no kinds) answers empty without a request. Then one pack per scope,
   built four at a time, exact (`view: "granular"`, never server-side
   traversal), so a sibling agent's scope is never in a pack and no pack is a
   parent scope's storage-order sample. Scopes are ordered most specific node
   first.
2. Each pack's events budget is `2 * limit`, with the derived layers sharing
   `limit` (see [the wire](cortex-wire.md#recall-recall)).
3. Decode and filter each pack's events with the full `MetaFilter` (reach
   included).
4. The answer comes from the pack holding the **most admitted events**, the
   most specific node on a tie. A missing `pack_id` is `Error::Engine`.
   `/v1/answer` takes one `use_pack_id` (its scope must match), so the
   **answer text is grounded on that one pack, while the citations (step 6)
   come from every pack**. The parent-scope pack this replaced was an
   unranked storage-order sample, so no coverage was lost; grounding the
   answer on every scope waits for CortexDB's ranked `subtree` lane to be on
   by default.
5. Ask the answer route with that pack's scope and `use_pack_id`. A response
   without `answer` text is `Error::Engine`. `model` is
   `diagnostics.answer_model`. A pack lives 60 s, and CortexDB drops every
   pack it holds once anything is forgotten (measured on 0.10.4: a forget in
   an unrelated scope turns the next `use_pack_id` into a 404). On that 404
   the round is repeated from step 1's packs: every scope's pack is built
   again and steps 3 to 5 run on the new ones, so the answer and the
   citations both come from packs read after the drop. At most three rounds;
   a third 404 is returned as `Error::NotFound`.
6. **Citations** come from the packs' decoded events, merged rank by rank
   (each pack's best first), one per item, the most
   specific node's first, capped at `limit`, with `score: None` and the
   envelope's text as the snippet. A pack with no decodable events still
   returns the answer, with no citations.

## Forget

`forget` looks the items' events up, then removes them by `memory_ids`. The
target is validated first, so an empty id list or an **empty filter is refused
and sends nothing**.

- **`Ids`**: for each scope the engine holds (the root's and every discovered
  namespace node, all kinds), find the ids' events by their labels (re-checked
  against the envelope) and remove every event of a found item. Ids are not
  confined to a reach; a confined caller reads them with `get` first.
- **`Filter`** (must be non-empty): walk each scope the filter reads (its
  kinds within its reach), narrowed by one label when possible, collect the
  events of every item the **full** filter matches, then remove them.

Either way the matched event ids are removed per scope with
`selector.memory_ids`, in batches of 100 (see
[the wire](cortex-wire.md#forget-forget)). A scope with nothing to remove sends
no request, so the engine can never send an empty selector, and it never
sends `confirm_all`. TinyHumans retries a transient failure up to 3 times
(removal of named events is idempotent; a 404 on a retry counts as done);
Direct sends once. `ForgetReport.forgotten` counts **items**, not events.

## Get

`get` is overridden to look the ids up directly rather than by scanning, the
contract default. It takes the scopes the request's `reach` reads (every
kind), and per scope looks the ids up by their `tm:i:` labels, stopping as
soon as every id is found. Each item is rebuilt from its events (a
conversation from all its turns). Hits have score `0`, come back in the order
asked, and an id that names nothing is left out.

## Explore

`explore` is **not** overridden: the engine uses the contract's default,
`explore_by_listing`, which pages through `list` up to the request's
`scan_limit` and reports whether it stopped early. CortexDB has no
server-side aggregation the engine relies on.

## Scope discovery

Needed only for a subtree reach or no reach (see
[scope layout](cortex-wire.md#scope-layout)). The engine asks the scopes
route (`v1/scopes/list` or `memory/scopes`) with `prefix` set to
`app:tinymemory`, or to the reach's own node path when it is not the root, and
`limit=1000`. Each returned path is parsed with `parse_scope`; paths that are
not TinyMemory kind scopes are skipped, and those whose namespace the reach
admits and whose kind the filter admits are added to the known nodes. A `404`
from the scopes route yields no extra scopes. Discovery runs once per call.
A listing that reaches 1000 paths may be missing some (the route has no
cursor): a read goes on with what was listed and logs a warning, and an export
fails with `Error::Engine` ("more than 1000 scopes under <prefix>; the engine
cannot list them all") and returns no items, rather than silently skip scopes.
Within the scopes it reads, an export never hands back a partial chunked
document: one missing a piece is named in `ExportPage::incomplete` instead.

## Health

See [the wire](cortex-wire.md#health). It is one request and never carries the
backend's own error text into the reason.
