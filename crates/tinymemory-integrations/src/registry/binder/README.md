# Engine binder

`registry::binder::EngineBinder` turns a host's resolved engine settings and
credential into a reusable `BoundEngine`. It keeps host configuration and
credential resolution outside TinyMemory while making safety composition
consistent for every engine source.

## Binding paths

- `bind` returns the installed host engine when present; otherwise it binds
  the configured CortexDB or TinyHumans engine.
- `bind_configured` deliberately ignores the host slot. A host uses it
  when a SaaS or profile gate must not share that engine, without clearing the
  slot for other callers.
- `bind_external` wraps a transient engine supplied by a tenant or profile.
  It does not add the engine to the global slot or configured-engine cache.
- `install_host_engine` replaces the binder's host slot. Its scrub policy
  is captured at installation; subsequent configured bind policies do not
  change it. `clear_host_engine` removes only that slot.

Each returned engine has one `ScrubbingEngine` inside one `TimedEngine`. Binder
paths reuse the wrapped host slot or configured cache entry directly, so cache
hits and host replacement do not add another decorator layer. Pass raw engines
to `bind_external` and `install_host_engine` rather than feeding a returned
`BoundEngine::engine` back to the binder.

## Configuration and identity

The optional `EndpointSource` callback is consulted only when
`EngineSettings::endpoint` is absent or blank. Its result is trimmed; a blank
result falls through to the registered engine default. The effective endpoint
is part of the cache key and the returned `BoundEngine`.

`rooted` applies the host's legacy or scoped layout to `EngineSettings` and
returns a layout discriminator. Hosts append that discriminator to their own
credential fingerprint. Direct CortexDB uses the given scope root and its
`user:<id>` owner; the TinyHumans wire enables the backend-owned tenant root.
When requested, `legacy_read` also carries the prior user root for migration.

The cache key is a SHA-256 digest of typed engine settings, endpoint, policy,
the credential-source fingerprint and, for static credentials, the credential
value itself. No key text is retained in cache keys or emitted in diagnostics.
Hosts should pass a stable, non-secret identity for a dynamic
`BearerSource`; the source continues to resolve refreshed credentials per
request. Strong engine references remain cached until `invalidate` is called,
so dropping a binding does not rebuild its HTTP client. Invalidation clears
configured engines only; host-slot lifetime is controlled separately.

The binder does not decide which endpoint, keyring entry, profile, or SaaS
policy a host may use. Those decisions stay in the host that calls it. The
engine slot belongs to one binder instance; a host can share that instance
where it needs process-wide installation semantics and create separate
instances for isolated hosts.
