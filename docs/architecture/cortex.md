# CortexDB engine

`CortexEngine` is the one `MemoryEngine` implementation TinyMemory ships: it
stores, lists, fetches, recalls and forgets over CortexDB's append-only event
log. It lives in `tinymemory-integrations`, module `cortex`, behind the
`cortex` feature (on by default), with the registry (`registry`) and the
configuration type (`config`) that build it.

This is the overview. The detail is split into focused pages:

- **this page**: surface, credentials, transport, failure mapping, endpoint
  security, the registry and `MemoryConfig`;
- [cortex-wire.md](cortex-wire.md): the two wires, every endpoint and its
  request and response shape, the scope layout, the v2 envelope and the lookup
  labels;
- [cortex-flows.md](cortex-flows.md): step-by-step store, list, fetch, recall,
  forget, get, explore, scope discovery and health;
- [testing.md](testing.md): the loopback doubles, the conformance suite and
  the live tests.

The module README (`crates/tinymemory-integrations/src/cortex/README.md`) is
the short in-tree version of this.

## Surface

```rust
use std::sync::Arc;
use tinymemory_integrations::cortex::{
    CortexCredential, CortexEngine, CortexTenancy, StaticBearer, CORTEX_API_ENDPOINT,
    TINYHUMANS_API_ENDPOINT,
};

// CortexDB's own /v1/* API, API key, holding one person's memory.
let direct = CortexEngine::direct(
    CORTEX_API_ENDPOINT,
    CortexCredential::api_key("ctx_..."),
    CortexTenancy::SingleUser,
)?;
// CortexDB behind the TinyHumans backend (/memory/*), bearer resolved per request.
let hosted = CortexEngine::tinyhumans(
    TINYHUMANS_API_ENDPOINT,
    Arc::new(StaticBearer::new("tiny_live_...")),
)?;
```

| Item | What it is |
| --- | --- |
| `CortexEngine::{new, direct, tinyhumans, wire}` | constructors (all fallible with `Error::Config`) and the wire accessor |
| `CortexWire { Direct, TinyHumans }` | which HTTP surface; `descriptor()` gives its registration |
| `CortexCredential { Static, Dynamic }` | how an engine authenticates; `api_key(..)` builds a static one |
| `BearerSource` (async `bearer()`), `StaticBearer` | a per-request token source, and a fixed token as one |
| `CORTEXDB_ENGINE_ID`, `TINYHUMANS_ENGINE_ID` | the config ids `cortexdb` and `tinyhumans` |
| `CORTEX_API_ENDPOINT`, `TINYHUMANS_API_ENDPOINT` | the default endpoints |
| `cortexdb_descriptor()`, `tinyhumans_descriptor()` | the `EngineDescriptor`s |
| `Error`, `Result`, `error_code`, `is_insufficient_credits` | the contract's error and two helpers for hosted failures |

`Debug` on the engine shows the wire (by id) and the endpoint origin, never the
credential. A `CortexEngine` is `Clone` and cheap to share.

| Engine id | `hosted` | `needs_endpoint` | `needs_key` | Default endpoint | `fetch_modes` |
| --- | --- | --- | --- | --- | --- |
| `cortexdb` | no | no | yes | `https://api-v1.cortexdb.ai` | `[Hybrid]` |
| `tinyhumans` | yes | no | yes | `https://api.tinyhumans.ai` | `[Hybrid]` |

## Credentials

Both wires authenticate with `Authorization: Bearer <token>`.

- **`CortexCredential::Static(String)`** (`CortexCredential::api_key`): one
  fixed token, normally a CortexDB API key for the direct wire. A blank key is
  `Error::Config` at construction.
- **`CortexCredential::Dynamic(Arc<dyn BearerSource>)`**: a token source the
  engine consults on **every request attempt**. TinyHumans takes the host's
  session JWT or `tiny_live_` API key, which rotates, so a refreshed token is
  used at once without rebuilding the engine. `From<Arc<dyn BearerSource>>` is
  implemented.
- **`BearerSource`**: `async fn bearer(&self) -> Result<String>`.
  Implementations must not log the token, and should return an error (not an
  empty string) when no credential is available, for example when the host is
  signed out.
- **`StaticBearer`**: a fixed token as a `BearerSource`.

**Per-request bearer resolution.** The transport resolves the credential inside
each attempt (so every read retry, every poll, and every hosted write retry
re-asks the source). A source failure, a blank token, or a token that cannot
be an HTTP header value (CR or LF, any other byte a header may not carry) is
`Error::Unauthorized` and **no request is sent**. The refusal message carries
no part of the token. The token is trimmed before use.

**Sensitive headers.** The `Authorization` value is marked sensitive on the
header (`HeaderValue::set_sensitive`), so nothing that formats the request
prints it. `Debug` on `CortexCredential` prints `Static(<redacted>)` or
`Dynamic(<source>)`, `StaticBearer` prints `StaticBearer(<redacted>)`, and
`EngineCredential` (below) is redacted the same way. No error message is built
from a credential.

### The actor header

On the direct wire every request also carries `X-Cortex-Actor`. CortexDB
serves every request as an actor; a minted token (the CortexDB cloud signs one
per account) is accepted only when the request names its subject, and
otherwise answers `401 ACTOR_MISMATCH`. The actor is the `caller` that
`GET v1/auth/whoami` reports for the key, which the client asks once and
caches (shared across clones):

- **known**: `whoami` answered; the caller is sent on every request. (A
  static operator key is served as `user:local`.)
- **absent**: the route is 404 or 405 (a server before the actor model); no
  header, and `whoami` is not asked again.
- **unknown**: nothing learned yet, or a credential was just rejected (401 or
  403 clears the cache so a replaced key is looked up again). The next request
  asks `whoami` again. A failed lookup is not cached: the request goes out
  without the header and reports its own failure.

The TinyHumans wire never sends the header; the backend names the actor.

## Transport

`HttpClient` (`cortex/transport/`) is shared by both wires.

| Aspect | Behaviour |
| --- | --- |
| Request timeout | 60s per request |
| Connect timeout | 10s (or the request timeout if smaller) |
| Reads | `Attempts::RetryTransient`: 3 attempts, 250ms then 500ms apart, only on `Error::Unavailable` |
| Writes | `Attempts::Once`: one attempt, because a timeout leaves it unknown whether the write applied |
| Success body cap | 64 MiB (also checked against `Content-Length`); larger is `Error::Engine` |
| Error body cap | 64 KiB, read lossily, never failing; only a 300-character excerpt reaches a message |
| TinyHumans bodies | `{success,data}` is unwrapped; see below |
| Direct bodies | bare JSON; an empty success body is `null` |

Bodies are read chunk by chunk and the cap is checked **before** each chunk is
appended, so a server that omits or understates `Content-Length` cannot
exhaust the host's memory. A body cut off mid-read is `Error::Unavailable`; a
body that is not valid JSON is `Error::Engine`.

**Reads retry, writes do not**, at this level. Layers above add what each
operation needs: the hosted write claim and recovery, the hosted forget retry
and the visibility polls (see [flows](cortex-flows.md#hosted-writes-and-outcome-unknown-recovery)).
Recall and listings are the reads; the answer route and forget are sent once.

### Idempotency claims

On TinyHumans, every `POST` sent as a single attempt carries a fresh
`Idempotency-Key` header: experience writes (under a claim the writer chooses
and reuses across its own retries), the answer route, and forget. Recall and
listings, which retry, carry none. The Direct wire sends no such header;
writes there carry the body `idempotency_key` only.

### TinyHumans envelope

A 2xx body must be `{"success": true, "data": ...}`. `success: false` is
reported as a hosted failure (below); a missing `data`, a body without
`success`, or invalid JSON is `Error::Engine`.

## Failure mapping

Every message names the route (without its query string, which carries
scopes and cursors) and the endpoint **host**, never a credential. Anything the
backend itself said follows a spaced em-dash (` — `) and is cut to 300
characters, so a status surface can keep the head and withhold the backend's
text.

| HTTP status | `Error` variant | Notes |
| --- | --- | --- |
| 401, 403 | `Unauthorized` | message tells the user to check the API key (direct) or re-authenticate (hosted) |
| 402 | `Engine` | hosted: prefixed `[USER_INSUFFICIENT_CREDITS]`; see below |
| 404 | `NotFound` | |
| 400, 413, 422 | `InvalidRequest` | |
| 409 | `Conflict` | on a hosted write retry it triggers recovery instead |
| 429, 500, 502, 503, 504 | `Unavailable` | retried for reads; `is_transient()` is true |
| any other non-2xx | `Engine` | |
| timeout, DNS, TLS, connect, reset | `Unavailable` | message names the class, for example "TLS failed" or "the host could not be resolved; check the URL" |
| request could not be built | `Engine` | no retry will change it |
| response over the cap, invalid JSON, malformed envelope | `Engine` | |
| bearer source failure, blank or invalid token | `Unauthorized` | no request sent |

**The `[CODE]` prefix.** The TinyHumans backend names every failure with an
`errorCode`. The contract's `Error` has no field for it, so a hosted failure's
message starts with `[CODE] ` (the code uppercased, restricted to ASCII
letters, digits and `_`, at most 64 characters). A failure with no
`errorCode` is filed under `UNAUTHORIZED` (401, 403), `USER_INSUFFICIENT_CREDITS`
(402), `RATE_LIMITED` (429) or `HTTP_<status>`. `error_code(&Error) ->
Option<&str>` reads the code back, and returns `None` for a direct failure, a
local refusal, or a message that no longer starts with a well-formed prefix.

**402 is `Engine`.** An exhausted credit balance is not transient
(`Unavailable` would invite a retry loop that cannot succeed until someone tops
up) and not a credential fault (`Unauthorized` would send the host to its
sign-in flow). It is the engine refusing to serve, which is what `Engine`
means, and the code lets a host tell it apart:
`is_insufficient_credits(&Error)` is true for an `Engine` error whose code is
`USER_INSUFFICIENT_CREDITS`, so a host can show a top-up prompt.

Other errors the engine raises itself: `Error::Unsupported` for a fetch mode
other than `Hybrid`; `Error::InvalidRequest` for a malformed cursor or an
empty or oversized store batch; `Error::Config` for construction; and
`Error::Engine` for a listing past 500 pages, a cursor that does not advance,
or a write receipt that lacks `event_id`.

## Endpoint security

Every engine here is credentialed, so a cleartext endpoint would put the
bearer on the network. `CortexEngine::new` (and so `direct`, `tinyhumans` and
the registry) returns `Error::Config` for:

- a URL that does not parse, or whose scheme is not `http` or `https`;
- an `http://` endpoint whose host is not loopback (`localhost`, or an IP that
  `is_loopback()`, IPv6 `[::1]` included): "credentialed memory endpoints must
  use https unless they are loopback";
- a blank static credential.

Loopback `http://` is allowed so local servers and the test doubles work. The
endpoint is operator supplied, which is why response bodies are capped.

## Registry

`registry` (feature `cortex`) is how a host turns configuration into an engine
without naming `CortexEngine`:

- `list_engines() -> Vec<EngineDescriptor>`: every engine this build can
  construct, `cortexdb` then `tinyhumans`. A host uses it to render a picker
  (`needs_endpoint`, `needs_key`, `default_endpoint`, `fetch_modes`).
- `build_engine(id, &EngineSettings, EngineCredential) ->
  Result<Arc<dyn MemoryEngine>>`.
- `EngineCredential`: `None` (default), `Static(String)`, or
  `Dynamic(Arc<dyn BearerSource>)`. `Debug` is redacted.

`build_engine` picks the wire from the id (`cortexdb` is `Direct`,
`tinyhumans` is `TinyHumans`) and resolves the endpoint: the setting, trimmed,
if it is not blank, else the engine's default. It returns `Error::Config` for
an unknown id; a missing credential (`None`, or a blank `Static`); and
everything `CortexEngine::new` refuses (not an HTTP(S) URL, cleartext off
loopback, a `cortexdb` engine with no `tenancy`, a `tinyhumans` engine with
one; see [Tenancy](#tenancy)). Messages never carry the credential. These are re-exported at the
crate root: `tinymemory_integrations::{build_engine, list_engines,
EngineCredential}`.

## MemoryConfig

`config::MemoryConfig` says which engine a host uses and how each is reached.
It holds **no credential**: a host keeps keys in its own secret store and
passes one to `build`, so a config file can be shared or logged.

| Field | Type | Meaning |
| --- | --- | --- |
| `engine` | string | the selected engine id; `DEFAULT_ENGINE` is `tinyhumans` |
| `engines` | map id to `EngineSettings` | per-engine settings; optional; an absent engine uses its defaults |
| `engines.<id>.endpoint` | string, optional | base URL; absent or blank uses the engine's default |
| `engines.<id>.tenancy` | string | `single_user` or a tenant scope (`org:acme/user:alice`); **required** by `cortexdb`, refused by `tinyhumans` |

TOML:

```toml
engine = "cortexdb"

[engines.cortexdb]
endpoint = "https://cortex.example.com"
tenancy = "single_user"            # or "org:acme/user:alice"

# An engine with no entry uses its defaults; an empty table is fine too.
[engines.tinyhumans]
```

JSON (the same shape):

```json
{ "engine": "cortexdb",
  "engines": { "cortexdb": { "endpoint": "https://cortex.example.com",
                             "tenancy": "single_user" } } }
```

`MemoryConfig::default()` is `engine = "tinyhumans"` with no settings.
`settings()` returns the selected engine's `EngineSettings` (or the defaults),
and `build(credential)` is `build_engine(&self.engine, &self.settings(),
credential)`. Unknown fields in a config are ignored on read.

```rust
use std::sync::Arc;
use tinymemory_integrations::{EngineCredential, MemoryConfig, cortex::StaticBearer};

let config: MemoryConfig = toml::from_str(r#"engine = "tinyhumans""#)?;
let engine = config.build(EngineCredential::Dynamic(Arc::new(StaticBearer::new("tiny_live_..."))))?;
```

The crate-level `Error` (`tinymemory_integrations::Error`) is the contract's
`tinymemory_api::Error`: the engine and the registry return it directly, and
the `documents`, `sources` and `import` modules keep a typed error of their
own that converts into it.

## Tenancy

CortexDB does not keep one caller out of another's scopes: on a
`cloud_shared_saas` deployment `v1/events?scope=` reads any scope, a write
lands in any scope, and `view: "descend"` at a shared ancestor reads every
scope beneath it. Isolation is the application's job, and on the direct wire
that is this engine. So a direct engine declares whose memory it holds
(`CortexTenancy`, `cortex/tenancy/`), and is refused with `Error::Config`
without a declaration, at construction, before any request:

| Tenancy | Root | Meaning |
| --- | --- | --- |
| `single_user` | `app:tinymemory` | the key is one person's (a desktop with the user's own key, a self-hosted server); the original layout |
| a tenant scope, e.g. `org:acme/user:alice` | `org:acme/user:alice/app:tinymemory` | several people share the key; each person's engine is pinned under their own scope |

A pin is 1 to 4 CortexDB `type:id` segments
([scopes](https://cortexdb.ai/docs/concepts/scopes)): each type a lowercase
identifier other than `app` (TinyMemory's own), each id 1 to 64 characters of
`[A-Za-z0-9_.-]` other than `.` and `..`. The type must also be in the
deployment's `allowed_scope_types`. Under a pin:

- every scope written, read, listed for discovery or built is the pin's
  root followed by the namespace and kind, and a TinyMemory namespace cannot
  name anything above it (its grammar admits no `..`, no leading `/`, no
  `org:` and no `app:` segment, so `user:bob` is only a node in Alice's tree);
- every scope the server reports (discovery listings, belief and recall
  scopes, a resumed cursor) is accepted only if it **starts with** the root,
  and anything else is skipped as another tenant's;
- the one server-side traversal, the unscoped recall's `view: "descend"`,
  starts at the root, so it cannot fan out into a sibling tenant.

A pin keeps the hosts that go through this engine apart. It is not a
credential: anyone holding the key can still call `/v1/*` with any scope.
People who must be kept apart from each other's *clients* need a key each,
or the `tinyhumans` engine. That engine takes no tenancy (and refuses one):
the backend derives the tenant from the caller's credential and re-roots
every scope under it, so a hosted root is found wherever it sits in a
reported scope.

`tinymemory_api::conformance::run_isolation(a, b)` proves a deployment's
boundary from the outside: see [testing](testing.md#the-isolation-check).

