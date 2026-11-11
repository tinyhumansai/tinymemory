# Turn-pack prefetching

`Prefetcher<K, V>` is the runtime-neutral engine for keyed background work.
It owns per-key queue ordering, the bounded pending queues and worker pool,
timeouts, cancellation cleanup, expiry, and revision/ticket checks that reject
results from work already in flight when a key is invalidated. Scoped
invalidation keeps queued work; global invalidation drops queued entries.

`TurnCache<K>` specializes that engine for the native lifecycle
`TurnPack`. Hosts supply `K`; TinyMemory does not serialize or inspect host
configuration. A host key should include whatever distinguishes the result's
scope, such as its memory configuration fingerprint, identity, and thread.
The host owns cache placement, worker runtime, mutation fencing, and delivery
sinks such as citation collection.

The cache admits packs when the serialized `TurnPack` plus separately
serialized citation hits totals at most 64 KiB. `TurnPack` skips citations in
its own serialized form, so the hits are counted separately. Serialization
failure or an over-limit total rejects the pack. Reading a completed pack adds
the cached-memory notice and its estimated tokens to a clone; the stored pack
and its citations, engine, refusal, references, and markdown remain unchanged.

Hosts keep each cache alive in their own runtime context and register an
`Arc<TurnCache<K>>` with `TurnCacheRegistry<K>`. The registry stores only weak
references, prunes expired entries, and applies synchronous invalidation to
live caches without holding its mutex while matching keys. Use
`invalidate_where` after a scoped write, and `invalidate_all` after a global
engine or credential change.

The `turn_cache_tests.rs` module pins notice behavior, admission sizing, and
registry ownership and invalidation. The sibling `mod_tests.rs` exercises the
generic prefetch worker engine independently of turn-pack policy.
