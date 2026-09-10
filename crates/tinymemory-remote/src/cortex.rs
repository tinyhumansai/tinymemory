//! CortexDB adapter.
//!
//! # Why this one looks different from its neighbours
//!
//! Supermemory, Mem0 and Cognee are keyed stores: `upsert` overwrites the row
//! at `(namespace, key)` and the dialect is a thin translation. CortexDB is an
//! **append-only event log**. A key, once written, cannot be given a different
//! value:
//!
//! - the same `idempotency_key` with a different body is refused with
//!   `409 IDEMPOTENCY_CONFLICT`;
//! - there is no update route — `/v1/events` is read-only and `/v1/experience`
//!   only appends;
//! - `/v1/forget` removes the event but **not** its idempotency record, so
//!   delete-then-rewrite loses the old value and still refuses the new one.
//!
//! So this dialect does not try to hold TinyMemory's key. It writes every store
//! as a fresh event with its own idempotency key — which the engine always
//! accepts — and carries the logical key inside the payload. Reads then fold
//! the log down to one record per key, newest wins. The contract's replace
//! semantics are reconstructed on the read side rather than performed on the
//! write side.
//!
//! ## What that costs, stated plainly
//!
//! **Reads are a scan.** `/v1/events` has no metadata filter, so there is no
//! server-side lookup by our key. Every read fetches the scope and folds it,
//! and that walk grows with everything the namespace has ever held. The keyed
//! `entry` seam other dialects override to a single round trip cannot be
//! overridden here.
//!
//! **Superseded versions stay in the engine's own recall corpus.** We fold them
//! out of `entries`, `namespace_entries` and `entry`, but the search seam
//! delegates to CortexDB's ranked recall, which searches every event including
//! the ones we consider replaced. A caller can therefore see a stale value
//! through `recall` that `get` would never return. That is not a bug in this
//! adapter; it is the cost of emulating replacement on an engine that does not
//! offer it, and it is the reason to prefer a native upsert if CortexDB ever
//! exposes one.
//!
//! **Writes block until the record can be read.** `/v1/experience` answers
//! `202 captured` and indexes afterwards, so an accepted write is not yet a
//! readable one. The contract requires read-after-write, so `upsert` waits —
//! see `CortexDialect::await_readable` for the two waits and why only one of
//! them is fatal. Measured against a running engine that is roughly one to
//! four seconds per write, and it dominates: the full conformance suite takes
//! about three minutes here against seconds on a keyed engine. It is the cost
//! of a durable-then-indexed pipeline, not of anything this adapter does.
//!
//! Every cost above disappears the day the engine grows `on_conflict:
//! "replace"`, releases an idempotency key on forget, and offers a readiness
//! signal a writer can wait on.
//!
//! ## Engine behaviours worth knowing before editing this
//!
//! Each of these was measured against a live CortexDB, and each one was wrong
//! in this adapter first — the offline suite in `conformance_test.rs` was green
//! throughout, because a double written from documentation agrees with an
//! adapter written from the same documentation. The double reproduces all of
//! them now.
//!
//! - **The two read paths return different bytes for the same event.**
//!   `/v1/events` returns stored text; `/v1/recall` renders it for a reader and
//!   prefixes the speaker (`[user] {...}`). Parsing only the stored form yields
//!   a dialect that lists correctly and searches to nothing. See
//!   `CortexDialect::envelope_of`.
//! - **The listing emits every record twice**, and `limit` counts the
//!   duplicates. Paging with the cursor still enumerates everything; a reader
//!   that assumes uniqueness does not. See `CortexDialect::events`.
//! - **Unknown query parameters are ignored, not refused.** A wrong paging
//!   parameter re-serves page one indefinitely rather than erroring.
//! - **The destructive selector's id field is `memory_ids`.** An unrecognised
//!   field is read as an *empty* selector, which means the whole scope. Two
//!   interlocks stop that being destructive on its own, and
//!   `CortexDialect::delete` explains why `confirm_all` appears nowhere
//!   here.
//! - **`/v1/experience/status` and the advertised `lifecycle_stream` are not
//!   readiness signals.** The first never advances past `captured`; the second
//!   accepts a connection and emits nothing.

use anyhow::Context;
use async_trait::async_trait;
use reqwest::Method;
use serde_json::{json, Value};
use tinymemory_api::recall::RecallOpts;
use tinymemory_api::traits::Memory;
use tinymemory_api::types::{MemoryCategory, MemoryTaint};

use crate::common::{Attempts, Dialect, HttpClient, RemoteMemory, StoredEntry};

/// Stable driver id used by configuration and status output.
pub use tinymemory_api::drivers::CORTEX_DRIVER_ID;

/// Default base URL for CortexDB's managed API.
pub const CORTEX_API_ENDPOINT: &str = "https://api-v1.cortexdb.ai";

/// Scope-id prefix for a namespace segment CortexDB's grammar accepts as-is.
///
/// The scope grammar is `type:id`, so every segment needs a type. Keeping the
/// common case literal means a scope stays readable in the engine's own tools.
const SEGMENT_PLAIN: &str = "tm";

/// Scope-id prefix for a segment that had to be hex-encoded to fit.
///
/// The contract allows characters in a namespace that a Cortex scope id does
/// not — `:` above all, which the grammar uses to separate type from id, and
/// which the contract uses to address a namespace *section*
/// (`conversation:thread-8f21`). Collapsing or refusing it are both wrong: the
/// first silently re-addresses the namespace out of its section, and the second
/// makes whole sections unstorable. So such a segment is encoded, and
/// [`CortexDialect::namespace_of`] decodes it — which matters more than it
/// looks, because `namespaces()` must report the *logical* namespace back, and
/// `Bound::recall` re-checks every returned record against the namespace it
/// asked for and silently drops what does not match.
const SEGMENT_ENCODED: &str = "tmx";

/// How many `/`-separated segments a CortexDB scope path may hold.
///
/// Measured, not read off the grammar: 32 are accepted and 33 are refused with
/// `422 INVALID_BODY`.
const MAX_SCOPE_SEGMENTS: usize = 32;

/// How many events one listing page asks for.
///
/// The fold needs every event in a scope, so this bounds one request rather
/// than the walk. Larger pages mean fewer round trips through the same
/// unavoidable scan.
const PAGE_SIZE: usize = 200;

/// How long a write waits for its own event to become readable.
///
/// Ingestion is asynchronous — see `CortexDialect::await_readable`. Measured
/// against the staging engine, a record reaches the listing in about 1–4s and
/// ranked recall about a second later; this is
/// a wide margin over that, because the failure it guards is a write that
/// reports success and is then invisible to the next read.
const VISIBILITY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Gap between visibility polls. Short enough not to dominate the wait.
const VISIBILITY_POLL: std::time::Duration = std::time::Duration::from_millis(250);

/// How long a write lets the *search* index catch up before giving up on it.
///
/// Shorter than [`VISIBILITY_TIMEOUT`] and, unlike it, not fatal — see phase
/// two of `CortexDialect::await_readable`.
const RECALL_SETTLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Longest query the settle probe will send.
///
/// The probe queries with the text just written, and a record may be large —
/// the conformance suite stores 64 KiB. Sending all of it as a query is both
/// wasteful and worse at matching than a distinctive prefix.
const RECALL_QUERY_CAP: usize = 256;

/// Ceiling on the pages one fold will walk.
///
/// A scan with no ceiling is an outage waiting for a large enough namespace.
/// Hitting it is an error rather than a truncated answer: a silently short
/// listing would present a superseded value as current, which is the one
/// failure this whole adapter exists to avoid.
const MAX_PAGES: usize = 500;

/// How many scopes one `v1/scopes/list` call asks for.
///
/// The endpoint defaults to **50** and says so nowhere: its OpenAPI entry
/// documents no parameters at all, and the response carries only `items` — no
/// cursor, no `has_more`, no total. So a bare call silently returns the first
/// fifty of however many exist, which was measured on a live engine holding 93.
///
/// `limit` is honoured even though it is undocumented, and there is nothing to
/// page with, so the only defence is to ask for far more than a namespace count
/// should ever reach and treat a full response as untrustworthy — see
/// [`CortexDialect::scopes`].
const SCOPE_LIST_LIMIT: usize = 10_000;

/// CortexDB, adapted to TinyMemory's keyed contract.
#[derive(Clone, Debug)]
pub struct CortexMemory {
    inner: RemoteMemory<CortexDialect>,
}

impl CortexMemory {
    pub(crate) fn operation_client(&self) -> HttpClient {
        self.inner.dialect().client.clone()
    }

    /// Rebuilds the HTTP transport with a different per-request deadline.
    ///
    /// # Errors
    ///
    /// Fails only if the underlying HTTP client cannot be rebuilt.
    pub fn with_request_timeout(mut self, timeout: std::time::Duration) -> anyhow::Result<Self> {
        let client = self
            .inner
            .dialect_mut()
            .client
            .clone()
            .with_timeout(timeout)?;
        self.inner.dialect_mut().client = client;
        Ok(self)
    }

    fn new(endpoint: &str, api_key: Option<&str>) -> anyhow::Result<Self> {
        if api_key.is_some() {
            let url =
                reqwest::Url::parse(endpoint).context("cortex endpoint is not a valid URL")?;
            if url.scheme() == "http" {
                let host = url
                    .host_str()
                    .ok_or_else(|| anyhow::anyhow!("credentialed CortexDB endpoint has no host"))?;
                let ip_host = host.trim_start_matches('[').trim_end_matches(']');
                let loopback = host.eq_ignore_ascii_case("localhost")
                    || ip_host
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|address| address.is_loopback());
                anyhow::ensure!(
                    loopback,
                    "credentialed CortexDB endpoints must use HTTPS unless they are loopback"
                );
            }
        }
        Ok(Self {
            inner: RemoteMemory::new(CortexDialect {
                client: HttpClient::bearer(endpoint, api_key)?,
            }),
        })
    }

    /// Connect to a CortexDB deployment using bearer authentication.
    ///
    /// # Errors
    ///
    /// Returns an error when `endpoint` is invalid, `api_key` is blank, or a
    /// credentialed non-loopback endpoint uses cleartext HTTP.
    pub fn api(endpoint: &str, api_key: &str) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !api_key.trim().is_empty(),
            "cortex API key must not be empty"
        );
        Self::new(endpoint, Some(api_key))
    }

    /// Connect to a self-hosted CortexDB server.
    ///
    /// # Errors
    ///
    /// Returns an error when `endpoint` is invalid, `api_key` is blank, or a
    /// credentialed non-loopback endpoint uses cleartext HTTP.
    pub fn self_hosted(endpoint: &str, api_key: &str) -> anyhow::Result<Self> {
        Self::api(endpoint, api_key)
    }

    /// Connect to CortexDB's managed API endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error when `api_key` is blank.
    pub fn cloud(api_key: &str) -> anyhow::Result<Self> {
        Self::api(CORTEX_API_ENDPOINT, api_key)
    }
}

/// The payload this adapter writes into an event's message text.
///
/// CortexDB's experience envelope is a **closed schema** — an unknown field is
/// refused with `422` — so there is nowhere on the event itself to record which
/// TinyMemory record it is. The engine treats `content.text` as free-form, so
/// the record rides there and the log stays parseable by us alone.
///
/// Anything the log cannot carry natively goes here: the key that identifies
/// the record, the category, the session, and the taint.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct Envelope {
    /// TinyMemory's logical key. The whole reason this wrapper exists.
    k: String,
    /// The caller's content, untouched.
    c: String,
    /// Category, as its wire string.
    #[serde(default)]
    cat: Option<String>,
    /// Session id, when the caller supplied one.
    #[serde(default)]
    s: Option<String>,
    /// Provenance taint. Persisted rather than dropped, because the default
    /// `store_with_taint` silently launders `ExternalSync` into internal trust.
    #[serde(default)]
    t: Option<String>,
    /// Tombstone marker. A `true` here means "this key is deleted as of this
    /// event". The fold reads newest-wins, so a tombstone written after the
    /// last value makes the key read as absent even if the underlying events
    /// are still on disk. See [`Dialect::delete`] for why we write one.
    #[serde(default)]
    d: bool,
    /// Original product-facing payload for granular ingestion operations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    x: Option<Value>,
}

/// One event as the fold needs to see it.
struct Folded {
    order: u64,
    /// `None` when the newest version of this key is a tombstone.
    entry: Option<StoredEntry>,
}

#[derive(Clone, Debug)]
pub(crate) struct CortexDialect {
    client: HttpClient,
}

impl CortexDialect {
    /// Maps a TinyMemory namespace onto a CortexDB scope path, reversibly.
    ///
    /// The two formats are incompatible and the translation is **not** cosmetic.
    /// CortexDB scopes are slash-delimited `type:id` segments matching
    /// `^[a-z][a-z0-9_]{0,31}:[A-Za-z0-9_-]{1,128}(/…){0,31}$`. TinyMemory
    /// namespaces are plain slash-delimited words with no colon anywhere, so
    /// passing one through unchanged is refused by the engine on every call.
    ///
    /// Reversibility is the load-bearing half. A host re-checks each returned
    /// record against the namespace it asked for and drops mismatches, so a
    /// scope this adapter cannot map *back* yields zero hits silently — a worse
    /// failure than a rejection, because nothing reports it.
    pub(crate) fn scope_of(namespace: &str) -> anyhow::Result<String> {
        let mut out = Vec::new();
        for segment in namespace.split('/').filter(|s| !s.is_empty()) {
            let safe = segment
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            let encoded = if safe {
                format!("{SEGMENT_PLAIN}:{segment}")
            } else {
                // Hex, so the result is unambiguous and cannot itself contain a
                // character the grammar rejects. Two bytes out per byte in.
                let mut hex = String::with_capacity(segment.len() * 2);
                for byte in segment.as_bytes() {
                    hex.push_str(&format!("{byte:02x}"));
                }
                format!("{SEGMENT_ENCODED}:{hex}")
            };
            let id_len = encoded.len() - encoded.find(':').unwrap_or(0) - 1;
            anyhow::ensure!(
                id_len <= 128,
                "namespace segment `{segment}` does not fit CortexDB's 128-character \
                 scope id limit once encoded"
            );
            out.push(encoded);
        }
        anyhow::ensure!(!out.is_empty(), "namespace must not be empty");
        // Measured against a running engine: 32 segments are accepted, 33 are
        // refused with `422 INVALID_BODY`. Catching it here keeps the refusal
        // local and specific, which is the same reason the per-segment checks
        // above are not left to the wire.
        anyhow::ensure!(
            out.len() <= MAX_SCOPE_SEGMENTS,
            "namespace has {} segments; CortexDB scope paths hold at most {MAX_SCOPE_SEGMENTS}",
            out.len()
        );
        Ok(out.join("/"))
    }

    /// The inverse of [`Self::scope_of`].
    ///
    /// Returns `None` for a scope this adapter did not write, which is what
    /// keeps `scopes()` from reporting somebody else's Cortex scopes as
    /// namespaces of ours.
    fn namespace_of(scope: &str) -> Option<String> {
        let mut out = Vec::new();
        for segment in scope.split('/').filter(|s| !s.is_empty()) {
            let (kind, body) = segment.split_once(':')?;
            match kind {
                SEGMENT_PLAIN => out.push(body.to_string()),
                SEGMENT_ENCODED => {
                    if body.len() % 2 != 0 {
                        return None;
                    }
                    let mut bytes = Vec::with_capacity(body.len() / 2);
                    for pair in body.as_bytes().chunks(2) {
                        let pair = std::str::from_utf8(pair).ok()?;
                        bytes.push(u8::from_str_radix(pair, 16).ok()?);
                    }
                    out.push(String::from_utf8(bytes).ok()?);
                }
                _ => return None,
            }
        }
        (!out.is_empty()).then(|| out.join("/"))
    }

    /// Every distinct event in one scope, following `next_cursor` to the end.
    ///
    /// Two things about the listing are worth stating, because both are easy to
    /// get wrong and neither is visible in a single-page test:
    ///
    /// - Paging is `cursor`/`next_cursor`. Unknown query parameters are ignored
    ///   rather than refused, so a wrong parameter name does not fail — it
    ///   silently re-serves page one until the page ceiling trips.
    /// - The engine emits **every record twice** in `items`, and `limit` counts
    ///   the duplicates, so a page of `PAGE_SIZE` carries about half that many
    ///   distinct events. The cursor itself is honest: paged to the end, every
    ///   record is present. We drop the repeats by event id here so no caller
    ///   downstream has to know.
    async fn events(&self, scope: &str) -> anyhow::Result<Vec<Value>> {
        let mut all = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let path = match &cursor {
                Some(c) => format!(
                    "v1/events?scope={scope}&limit={PAGE_SIZE}&cursor={cursor}",
                    scope = urlencoding(scope),
                    cursor = urlencoding(c)
                ),
                None => format!(
                    "v1/events?scope={scope}&limit={PAGE_SIZE}",
                    scope = urlencoding(scope)
                ),
            };
            let page: Value = self
                .client
                .json(Method::GET, &path, None, Attempts::RetryTransient)
                .await?;
            let items = page
                .get("items")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for item in items {
                match item.get("id").and_then(Value::as_str) {
                    Some(id) if !seen.insert(id.to_string()) => continue,
                    _ => all.push(item),
                }
            }
            let next = page
                .get("next_cursor")
                .and_then(Value::as_str)
                .map(str::to_string);
            match (page.get("has_more").and_then(Value::as_bool), next) {
                (Some(true), Some(next)) => cursor = Some(next),
                _ => return Ok(all),
            }
        }
        anyhow::bail!(
            "listing scope `{scope}` exceeded {MAX_PAGES} pages; refusing to answer from a \
             truncated log, because a short listing would report a superseded value as current"
        )
    }

    /// Blocks until an appended event can be read back by every read path.
    ///
    /// `/v1/experience` answers `202 captured` and indexes afterwards, so a
    /// write that has been accepted is not yet a write that can be read. The
    /// contract requires read-after-write, and this dialect has two read paths
    /// that become ready at different times, so both are waited on.
    ///
    /// Four details decide the shape of this, and all four were measured
    /// against a running engine rather than assumed:
    ///
    /// - `GET /v1/events/{id}` starts answering roughly 1.3s **before** the
    ///   scope listing carries the same event, so it is not a usable readiness
    ///   probe for `get`/`list`, which read the listing.
    /// - Ranked recall lags the listing by about another second, so waiting on
    ///   the listing alone leaves `search` returning nothing for a record the
    ///   same adapter has just reported as stored.
    /// - `/v1/experience/status` never advances past `captured`. It reports
    ///   durability, which is already true when the write returns, and says
    ///   nothing about visibility.
    /// - Recall answers carry the event id, so the second wait can be exact
    ///   rather than a sleep: query with the text just written and look for the
    ///   id that came back from the write.
    ///
    /// The two waits do not fail the same way, and that asymmetry is the point.
    /// A record that cannot be read by key has not been stored as far as the
    /// contract is concerned, so phase one times out into an error. A search
    /// index that has not caught up is a weaker claim — the record is durable
    /// and keyed reads return it — so phase two gives up quietly rather than
    /// failing a write that succeeded.
    async fn await_readable(&self, scope: &str, event_id: &str, text: &str) -> anyhow::Result<()> {
        let deadline = std::time::Instant::now() + VISIBILITY_TIMEOUT;

        // Phase one: the keyed read path, which folds the scope listing.
        loop {
            // Newest first, so one page is enough to see a write just made.
            let path = format!(
                "v1/events?scope={scope}&limit={PAGE_SIZE}",
                scope = urlencoding(scope)
            );
            let page: Value = self
                .client
                .json(Method::GET, &path, None, Attempts::RetryTransient)
                .await?;
            if Self::carries(page.get("items"), event_id) {
                break;
            }
            Self::still_waiting(deadline, event_id, scope)?;
            tokio::time::sleep(VISIBILITY_POLL).await;
        }

        // Phase two: ranked recall, a separate index that settles later.
        //
        // Unlike phase one this is best-effort, and the difference is
        // deliberate. A write whose record cannot be read by key has not
        // happened as far as the contract is concerned, so phase one failing
        // is an error. A search index that has not caught up yet is a
        // different thing: the record is durable, keyed reads return it, and
        // the ranking will include it shortly. Failing the write there would
        // report a successful, readable store as an error.
        let query: String = text.chars().take(RECALL_QUERY_CAP).collect();
        let settle_by = std::time::Instant::now() + RECALL_SETTLE_TIMEOUT;
        while std::time::Instant::now() < settle_by {
            let probe = self
                .client
                .json::<Value>(
                    Method::POST,
                    "v1/recall",
                    Some(&json!({ "scope": scope, "query": query })),
                    Attempts::RetryTransient,
                )
                .await;
            let Ok(answer) = probe else {
                // Best-effort means best-effort. A probe that could not be
                // answered says nothing about the write, which is durable and
                // already readable by key — propagating this would report a
                // successful store as a failure, which is the one thing this
                // phase is documented not to do.
                break;
            };
            if Self::carries(answer.pointer("/layers/events"), event_id) {
                break;
            }
            tokio::time::sleep(VISIBILITY_POLL).await;
        }
        Ok(())
    }

    /// Whether a listing or recall answer contains this event id.
    fn carries(items: Option<&Value>, event_id: &str) -> bool {
        items.and_then(Value::as_array).is_some_and(|items| {
            items
                .iter()
                .any(|e| e.get("id").and_then(Value::as_str) == Some(event_id))
        })
    }

    /// Errors once the visibility deadline has passed, naming the read path
    /// that never caught up.
    fn still_waiting(
        deadline: std::time::Instant,
        event_id: &str,
        scope: &str,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "event `{event_id}` was accepted into scope `{scope}` but did not become \
             readable within {VISIBILITY_TIMEOUT:?}; reporting the write as succeeded \
             would break read-after-write"
        );
        Ok(())
    }

    /// Reads our envelope out of one event's text, whichever read path it came
    /// from.
    ///
    /// The two paths do not agree on the bytes. `/v1/events` returns the text
    /// exactly as stored; `/v1/recall` renders it for a reader first, prefixing
    /// the speaker as `[user] `. Parsing the raw form only is therefore a
    /// dialect that lists correctly and searches to nothing — every recall hit
    /// fails to parse and is dropped as somebody else's event, which looks like
    /// an empty index rather than a bug.
    ///
    /// A prefix is only stripped when the text does not parse without it, so an
    /// envelope whose content legitimately begins with a bracket is untouched.
    fn envelope_of(text: &str) -> Option<Envelope> {
        if let Ok(envelope) = serde_json::from_str::<Envelope>(text) {
            return Some(envelope);
        }
        let rendered = text.strip_prefix('[')?;
        let (_role, rest) = rendered.split_once("] ")?;
        serde_json::from_str::<Envelope>(rest).ok()
    }

    /// Folds a scope's log down to one record per logical key, newest wins.
    ///
    /// This is where the contract's replace semantics are reconstructed. The
    /// engine keeps every version; `wal_offset` orders them, and the highest
    /// offset for a key is the value a caller should see.
    fn fold(namespace: &str, events: &[Value]) -> Vec<StoredEntry> {
        let mut latest: std::collections::HashMap<String, Folded> =
            std::collections::HashMap::new();
        for event in events {
            let Some(text) = event.pointer("/content/text").and_then(Value::as_str) else {
                continue;
            };
            // Anything this adapter did not write is not ours to interpret.
            // A scope may hold events from a person using CortexDB directly.
            let Some(envelope) = Self::envelope_of(text) else {
                continue;
            };
            let order = event
                .get("wal_offset")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            let replace = latest
                .get(&envelope.k)
                .is_none_or(|held| order >= held.order);
            if !replace {
                continue;
            }
            if envelope.d {
                // Newest version of this key is a tombstone: the key is gone.
                latest.insert(envelope.k, Folded { order, entry: None });
                continue;
            }
            let entry = StoredEntry {
                remote_id: event
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                namespace: namespace.to_string(),
                key: envelope.k.clone(),
                content: envelope.c,
                category: crate::common::category(envelope.cat.as_deref()),
                timestamp: event
                    .pointer("/context/recorded_at")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                session_id: envelope.s,
                score: None,
                taint: match envelope.t.as_deref() {
                    Some("external_sync") => MemoryTaint::ExternalSync,
                    _ => MemoryTaint::Internal,
                },
            };
            latest.insert(
                envelope.k,
                Folded {
                    order,
                    entry: Some(entry),
                },
            );
        }
        let mut out: Vec<StoredEntry> = latest.into_values().filter_map(|f| f.entry).collect();
        out.sort_by(|a, b| a.key.cmp(&b.key));
        out
    }

    /// Every scope this deployment holds that this adapter wrote.
    ///
    /// Asks for [`SCOPE_LIST_LIMIT`] explicitly. Without it the engine returns
    /// its undocumented default of fifty, and the callers that matter here —
    /// `entries`, `namespace_summaries`, and through them `export_page` and
    /// `opencompany memory migrate` — would enumerate a *subset* of a company's
    /// namespaces while reporting success. Per-namespace reads never notice,
    /// because they address a namespace directly, which is why a truncation
    /// here stays invisible until a migration quietly leaves records behind.
    ///
    /// A response that fills the limit is refused rather than returned. There
    /// is no cursor and no total to check against, so a full page is
    /// indistinguishable from a truncated one, and the same reasoning as
    /// [`MAX_PAGES`] applies: a silently short listing is worse than an error,
    /// because the caller cannot tell it happened.
    async fn scopes(&self) -> anyhow::Result<Vec<String>> {
        let listing: Value = self
            .client
            .json(
                Method::GET,
                &format!("v1/scopes/list?limit={SCOPE_LIST_LIMIT}"),
                None,
                Attempts::RetryTransient,
            )
            .await?;
        let items = listing
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if items.len() >= SCOPE_LIST_LIMIT {
            anyhow::bail!(
                "scope listing returned {SCOPE_LIST_LIMIT} entries, the limit it was asked \
                 for; the engine offers no cursor, so a complete listing cannot be \
                 distinguished from a truncated one and enumerating namespaces would \
                 silently skip whatever came after"
            );
        }
        Ok(items
            .iter()
            .filter_map(|s| s.get("path").and_then(Value::as_str))
            .filter_map(Self::namespace_of)
            .collect())
    }
}

/// A fresh idempotency key for every write.
///
/// Never TinyMemory's key: reusing that is what produces
/// `409 IDEMPOTENCY_CONFLICT` on the second store, and the whole append-and-fold
/// design exists to avoid it.
///
/// Three parts, because two are not enough. The counter separates writes within
/// one process, and the timestamp separates runs of it — but neither separates
/// two *concurrent* processes, which can read the same nanosecond and start
/// their counters at the same zero. The salt is per-process entropy from the
/// standard library, taken once, so independent writers cannot mint the same
/// key. Getting that wrong is not a silent fault — a reused key carrying a
/// different body is refused with a 409 — but it is a refusal of a legitimate
/// write, and it would be maddening to diagnose.
///
/// No new dependency: `RandomState` is seeded by the OS per process, which is
/// exactly the entropy needed here, and a vendored crate should not grow a
/// dependency for one string.
fn fresh_idempotency_key() -> String {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::OnceLock;

    static SEQ: AtomicU64 = AtomicU64::new(0);
    static SALT: OnceLock<u64> = OnceLock::new();

    let salt = *SALT.get_or_init(|| {
        std::collections::hash_map::RandomState::new()
            .build_hasher()
            .finish()
    });
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!(
        "tm-{salt:016x}-{nanos}-{}",
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// Percent-encodes everything outside the URI unreserved set.
///
/// A scope only ever carries `:` and `/`, so an escape list would do for that.
/// The cursor is the reason this is general: it is opaque engine output, and a
/// `+`, `&`, `=`, `#` or `?` in one would silently reshape the query string
/// rather than fail. Encoding by byte also keeps multi-byte UTF-8 correct.
fn urlencoding(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(char::from(*byte));
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[async_trait]
impl Dialect for CortexDialect {
    fn name(&self) -> &'static str {
        CORTEX_DRIVER_ID
    }

    /// Appends a new event carrying the record.
    ///
    /// Deliberately **not** an update. Every write gets a fresh idempotency key,
    /// which the engine always accepts; reusing TinyMemory's key here is what
    /// produces `409 IDEMPOTENCY_CONFLICT` on the second store. The previous
    /// version stays in the log and is folded out on read.
    async fn upsert(&self, entry: StoredEntry) -> anyhow::Result<()> {
        let scope = Self::scope_of(&entry.namespace)?;
        let envelope = serde_json::to_string(&Envelope {
            k: entry.key.clone(),
            c: entry.content.clone(),
            cat: Some(entry.category.to_string()),
            s: entry.session_id.clone(),
            t: Some(
                match entry.taint {
                    MemoryTaint::ExternalSync => "external_sync",
                    _ => "internal",
                }
                .to_string(),
            ),
            d: false,
            x: None,
        })?;
        let accepted: Value = self
            .client
            .json(
                Method::POST,
                "v1/experience",
                Some(&json!({
                    "scope": scope,
                    "modality": "observation",
                    // Fresh per write. See this method's own doc.
                    "idempotency_key": fresh_idempotency_key(),
                    "content": { "kind": "message", "role": "user", "text": envelope },
                    "context": {},
                })),
                Attempts::Once,
            )
            .await?;
        // Accepted is not readable yet — see `await_readable`.
        if let Some(id) = accepted.get("event_id").and_then(Value::as_str) {
            self.await_readable(&scope, id, &envelope).await?;
        }
        Ok(())
    }

    async fn entries(&self) -> anyhow::Result<Vec<StoredEntry>> {
        let mut all = Vec::new();
        for namespace in self.scopes().await? {
            all.extend(self.namespace_entries(&namespace).await?);
        }
        Ok(all)
    }

    async fn namespace_entries(&self, namespace: &str) -> anyhow::Result<Vec<StoredEntry>> {
        let scope = Self::scope_of(namespace)?;
        let events = self.events(&scope).await?;
        Ok(Self::fold(namespace, &events))
    }

    /// Deletes a key in two moves: a tombstone, then the events behind it.
    ///
    /// The tombstone goes first and is what makes the delete correct. It is an
    /// ordinary append, so it cannot fail for any reason a write could not
    /// already fail, and once it lands the fold reports the key absent —
    /// whatever happens to the second move, and whatever a concurrent writer
    /// was doing at the time.
    ///
    /// The second move is the real removal, and it is the one that matters for
    /// [`Dialect::search`]: recall ranks over every event, so leaving the old
    /// versions in place would let a deleted record resurface through `recall`
    /// long after `get` stopped returning it. We name the events explicitly in
    /// `selector.memory_ids` and send **no** `confirm_all` — that flag
    /// authorises a scope-wide wipe, is only valid with an empty selector, and
    /// pairing it with a selector is refused as ambiguous. Note that an
    /// unrecognised selector field is not an error: the engine reads the
    /// selector as empty, which is exactly the shape `confirm_all` would then
    /// license. The two mistakes are only dangerous together, and this is why
    /// the flag is not written anywhere in this file.
    async fn delete(&self, namespace: &str, key: &str) -> anyhow::Result<bool> {
        let scope = Self::scope_of(namespace)?;
        let events = self.events(&scope).await?;
        let mut ids = Vec::new();
        let mut live = false;
        for event in &events {
            let Some(text) = event.pointer("/content/text").and_then(Value::as_str) else {
                continue;
            };
            let Some(envelope) = Self::envelope_of(text) else {
                continue;
            };
            if envelope.k != key {
                continue;
            }
            if !envelope.d {
                live = true;
            }
            if let Some(id) = event.get("id").and_then(Value::as_str) {
                ids.push(id.to_string());
            }
        }
        if !live {
            // Never held, or already tombstoned. Either way there is nothing to
            // delete, and appending a second tombstone would only add noise.
            return Ok(false);
        }

        let tombstone = serde_json::to_string(&Envelope {
            k: key.to_string(),
            c: String::new(),
            cat: None,
            s: None,
            t: None,
            d: true,
            x: None,
        })?;
        let accepted: Value = self
            .client
            .json(
                Method::POST,
                "v1/experience",
                Some(&json!({
                    "scope": scope,
                    "modality": "conversation",
                    "idempotency_key": fresh_idempotency_key(),
                    "content": { "kind": "message", "role": "user", "text": tombstone },
                    "context": {},
                })),
                Attempts::Once,
            )
            .await?;
        // The tombstone IS the delete; the next read must see it.
        if let Some(id) = accepted.get("event_id").and_then(Value::as_str) {
            self.await_readable(&scope, id, &tombstone).await?;
        }

        if !ids.is_empty() {
            self.client
                .empty(
                    Method::POST,
                    "v1/forget",
                    Some(&json!({
                        "scope": scope,
                        "layers": ["events"],
                        "selector": { "memory_ids": ids },
                        "audit_note": "tinymemory: delete(namespace, key)",
                    })),
                )
                .await?;
        }
        Ok(true)
    }

    async fn search(
        &self,
        query: &str,
        limit: usize,
        opts: RecallOpts<'_>,
    ) -> anyhow::Result<Vec<StoredEntry>> {
        let Some(namespace) = opts.namespace else {
            // Recall is scope-addressed here; an unscoped search has no scope to
            // name. Empty rather than an error, matching the contract's rule
            // that a non-matching query yields no hits.
            return Ok(Vec::new());
        };
        let scope = Self::scope_of(namespace)?;
        let answer: Value = self
            .client
            .json(
                Method::POST,
                "v1/recall",
                Some(&json!({ "scope": scope, "query": query })),
                Attempts::RetryTransient,
            )
            .await?;
        let events = answer
            .pointer("/layers/events")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        // `fold` orders by key. That is right for a listing and wrong here:
        // the engine ranked these, and truncating an alphabetical order would
        // discard its best hits and keep whichever keys sort first. Restore the
        // ranking, by each key's first appearance in the answer, before the cap.
        let mut rank: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for (position, event) in events.iter().enumerate() {
            let Some(envelope) = event
                .pointer("/content/text")
                .and_then(Value::as_str)
                .and_then(Self::envelope_of)
            else {
                continue;
            };
            // First appearance wins: a key may have several versions in the
            // answer, and the best-ranked one is the one that places it.
            rank.entry(envelope.k).or_insert(position);
        }
        let mut hits = Self::fold(namespace, &events);
        hits.sort_by_key(|hit| rank.get(hit.key.as_str()).copied().unwrap_or(usize::MAX));
        hits.truncate(limit);
        Ok(hits)
    }

    async fn health(&self) -> anyhow::Result<()> {
        self.client.probe("v1/admin/health").await
    }

    /// CortexDB's recall answers with no per-hit similarity score — the
    /// response carries no score field at all — so a `min_score` filter would
    /// drop every hit rather than narrow them.
    fn scores_recall(&self) -> bool {
        false
    }
}

#[async_trait]
impl Memory for CortexMemory {
    fn name(&self) -> &str {
        self.inner.name()
    }
    async fn store(
        &self,
        n: &str,
        k: &str,
        c: &str,
        cat: MemoryCategory,
        s: Option<&str>,
    ) -> anyhow::Result<()> {
        self.inner.store(n, k, c, cat, s).await
    }
    async fn store_with_taint(
        &self,
        n: &str,
        k: &str,
        c: &str,
        cat: MemoryCategory,
        s: Option<&str>,
        taint: MemoryTaint,
    ) -> anyhow::Result<()> {
        self.inner.store_with_taint(n, k, c, cat, s, taint).await
    }
    async fn get(
        &self,
        n: &str,
        k: &str,
    ) -> anyhow::Result<Option<tinymemory_api::types::MemoryEntry>> {
        self.inner.get(n, k).await
    }
    async fn forget(&self, n: &str, k: &str) -> anyhow::Result<bool> {
        self.inner.forget(n, k).await
    }
    async fn list(
        &self,
        n: Option<&str>,
        cat: Option<&MemoryCategory>,
        s: Option<&str>,
    ) -> anyhow::Result<Vec<tinymemory_api::types::MemoryEntry>> {
        self.inner.list(n, cat, s).await
    }
    async fn namespace_summaries(
        &self,
    ) -> anyhow::Result<Vec<tinymemory_api::types::NamespaceSummary>> {
        self.inner.namespace_summaries().await
    }
    async fn count(&self) -> anyhow::Result<usize> {
        self.inner.count().await
    }
    async fn health_check(&self) -> bool {
        self.inner.health_check().await
    }
    async fn recall(
        &self,
        query: &str,
        limit: usize,
        opts: RecallOpts<'_>,
    ) -> anyhow::Result<Vec<tinymemory_api::types::MemoryEntry>> {
        self.inner.recall(query, limit, opts).await
    }
}

#[cfg(test)]
#[path = "cortex_test.rs"]
mod test;
