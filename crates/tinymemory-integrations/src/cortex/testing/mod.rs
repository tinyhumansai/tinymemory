//! Loopback HTTP doubles of CortexDB (`/v1/*`) and of the TinyHumans
//! backend (`/memory/*`), shared by every test in the crate.
//!
//! Both serve the same [`CortexLog`]. The hosted double additionally wraps
//! bodies in `{success,data}`, reports failures with `errorCode`, refuses a
//! scope outside the memory API's grammar, takes an `Idempotency-Key` claim
//! per write (any replay of a claimed key is refused, never forwarded),
//! refuses a repeated `labels=` parameter, enforces the strict answer
//! schema, relays engine failures through the backend's own vocabulary (a
//! claimed key's 409 arrives as `409` with `errorCode: CONFLICT`, or as `400`
//! under the `legacy_conflict_400` knob), answers its
//! rate limit outside the envelope, and erases the whole memory on
//! `DELETE /memory`. Knobs make either fail the ways the real stacks fail.

mod log;
mod routes;

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;

pub(crate) use log::CortexLog;

use tinymemory_api::{LearningKind, MemoryMeta, Role, SourceKind, StoreItem, Turn};

use crate::cortex::{CortexCredential, CortexEngine, StaticBearer};

/// The bearer the test engines send.
pub(crate) const TEST_TOKEN: &str = "tiny_live_test";

/// What the double saw.
#[derive(Debug, Default)]
pub(crate) struct Seen {
    /// `"METHOD /path?query"` of every request, in order.
    pub(crate) requests: Vec<String>,
    /// The `Authorization` header of every request.
    pub(crate) auth: Vec<String>,
    /// `(body idempotency_key, Idempotency-Key header)` of every write.
    pub(crate) idempotency: Vec<(Option<String>, Option<String>)>,
    /// Every recall body.
    pub(crate) recalls: Vec<serde_json::Value>,
    /// Every answer body.
    pub(crate) answers: Vec<serde_json::Value>,
    /// Every forget body.
    pub(crate) forgets: Vec<serde_json::Value>,
    /// Erasure requests, in order.
    pub(crate) erasures: Vec<serde_json::Value>,
    /// Every beliefs build body.
    pub(crate) builds: Vec<serde_json::Value>,
    /// Every write body (`experience` and `experience/bulk`), whole.
    pub(crate) writes: Vec<serde_json::Value>,
    /// Every scope registration body that registered a scope.
    pub(crate) registrations: Vec<serde_json::Value>,
}

/// One double's state and knobs.
#[derive(Debug, Default)]
pub(crate) struct Double {
    /// Whether this is the TinyHumans double.
    pub(crate) hosted: bool,
    pub(crate) log: Mutex<CortexLog>,
    pub(crate) seen: Mutex<Seen>,
    /// When set, every request fails with this status and code.
    pub(crate) fail_all: Mutex<Option<(u16, &'static str)>>,
    /// The `caller` `v1/auth/whoami` reports; `None` answers 404, as a
    /// server before the actor model does.
    pub(crate) whoami_caller: Mutex<Option<String>>,
    /// When set, every write naming an `observed_actor` or `subject` fails
    /// with this status and code, a bulk write whole, as CortexDB refuses a
    /// credential without the capabilities to attribute.
    pub(crate) refuse_attribution: Mutex<Option<(u16, &'static str)>>,
    /// When set, every scope registration fails with this status and code.
    pub(crate) fail_registration: Mutex<Option<(u16, &'static str)>>,
    /// The only token accepted; `None` accepts any non-empty bearer.
    pub(crate) accept_token: Mutex<Option<String>>,
    /// Claimed `Idempotency-Key`s (hosted).
    pub(crate) claimed: Mutex<HashSet<String>>,
    /// Listings come back empty for this many requests.
    pub(crate) hide_listing_for: AtomicUsize,
    /// Listings answer 429 for this many requests.
    pub(crate) rate_limit_events: AtomicUsize,
    /// Listings answer CortexDB's retriable `503 AUTHORIZATION_STATE_CHANGED`
    /// for this many requests.
    pub(crate) state_change_events: AtomicUsize,
    /// Writes answer the backend's own 429 (before any claim) this many
    /// times.
    pub(crate) rate_limit_experience: AtomicUsize,
    /// Writes are applied, then answered 503, this many times.
    pub(crate) apply_then_fail: AtomicUsize,
    /// Writes have their claim taken, are not applied, and answer 502.
    pub(crate) claim_then_fail: AtomicUsize,
    /// The Nth write (1-based) is refused with 400, unapplied.
    pub(crate) fail_nth_experience: AtomicUsize,
    pub(crate) experience_calls: AtomicUsize,
    /// Forgets answer 429 this many times.
    pub(crate) rate_limit_forget: AtomicUsize,
    /// Recall answers 500.
    pub(crate) recall_down: AtomicBool,
    /// `v1/admin/version` does not list `refers_to_v1`.
    pub(crate) refers_unlisted: AtomicBool,
    /// `v1/admin/version` answers 500.
    pub(crate) version_down: AtomicBool,
    /// A recall carrying `temporal` is refused with 422, as a server that
    /// does not know the field refuses it.
    pub(crate) refers_refused: AtomicBool,
    /// Answers refuse their `use_pack_id` as expired (404) this many times,
    /// as CortexDB does once anything is forgotten after the pack was built.
    pub(crate) expire_packs: AtomicUsize,
    /// Once the next write is applied, rate limit this many listings and
    /// hide the listing this many more times: (429s, hidden). Lets a test
    /// aim at the reads a write makes after it is sent, not the replay
    /// lookup before it.
    pub(crate) arm_after_write: Mutex<Option<(usize, usize)>>,
    /// How long each event listing is held before it answers, in ms; zero
    /// answers at once.
    pub(crate) listing_delay_ms: AtomicUsize,
    /// Event listings being answered right now.
    pub(crate) listings_in_flight: AtomicUsize,
    /// The most event listings ever answered at once.
    pub(crate) listings_peak: AtomicUsize,
    /// Registered scopes holding nothing, listed beside the log's own
    /// (`app:tinymemory/agent:pad-NNNN/app:learnings`), so a test can make a
    /// scope listing reach CortexDB's clamp.
    pub(crate) padding_scopes: AtomicUsize,
    /// The hosted double has no `DELETE /memory` route (an older backend):
    /// it answers the router's bare 404.
    pub(crate) erase_all_missing: AtomicBool,
    /// The hosted double relays a refused, already-claimed key as an older
    /// backend did (`400` + `CONFLICT`); unset it answers `409` as
    /// tinyhumansai/backend#1409 does.
    pub(crate) legacy_conflict_400: AtomicBool,
    /// Replaces the `data` of the `DELETE /memory` answer (a malformed one).
    pub(crate) erase_all_answer: Mutex<Option<serde_json::Value>>,
}

/// The shared handle the routes and tests hold.
pub(crate) type Shared = Arc<Double>;

/// Decrements `counter` if positive; whether a unit was taken.
pub(crate) fn take_one(counter: &AtomicUsize) -> bool {
    counter
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
        .is_ok()
}

impl Double {
    /// How many recorded requests start with `prefix`.
    pub(crate) fn count(&self, prefix: &str) -> usize {
        self.seen
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|r| r.starts_with(prefix))
            .count()
    }

    /// Every recorded request.
    pub(crate) fn requests(&self) -> Vec<String> {
        self.seen.lock().unwrap().requests.clone()
    }

    /// How many events the log holds.
    pub(crate) fn event_count(&self) -> usize {
        self.log.lock().unwrap().events.len()
    }
}

/// Serves `app` on an ephemeral loopback port and returns its base URL.
pub(crate) async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    endpoint
}

/// A running CortexDB double.
pub(crate) async fn direct_double() -> (String, Shared) {
    let state = Arc::new(Double::default());
    (serve(routes::direct(state.clone())).await, state)
}

/// A running TinyHumans double.
pub(crate) async fn hosted_double() -> (String, Shared) {
    let state = Arc::new(Double {
        hosted: true,
        ..Double::default()
    });
    (serve(routes::hosted(state.clone())).await, state)
}

/// The visibility budget test engines wait.
pub(crate) const TEST_VISIBILITY: Duration = Duration::from_secs(2);

/// A Direct engine on `endpoint` with fast test timing.
pub(crate) fn direct_engine(endpoint: &str) -> CortexEngine {
    CortexEngine::direct(endpoint, CortexCredential::api_key(TEST_TOKEN))
        .unwrap()
        .with_test_timing(TEST_VISIBILITY)
}

/// A TinyHumans engine on `endpoint` with fast test timing.
pub(crate) fn hosted_engine(endpoint: &str) -> CortexEngine {
    CortexEngine::tinyhumans(endpoint, Arc::new(StaticBearer::new(TEST_TOKEN)))
        .unwrap()
        .with_test_timing(TEST_VISIBILITY)
}

/// One engine per wire, each with its own double.
pub(crate) async fn both() -> Vec<(CortexEngine, Shared)> {
    let (direct, direct_state) = direct_double().await;
    let (hosted, hosted_state) = hosted_double().await;
    vec![
        (direct_engine(&direct), direct_state),
        (hosted_engine(&hosted), hosted_state),
    ]
}

/// Metadata on thread `thread`.
pub(crate) fn thread_meta(thread: &str) -> MemoryMeta {
    let mut meta = MemoryMeta::from_source(SourceKind::Conversation, Some("chat".into()));
    meta.thread_id = Some(thread.into());
    meta
}

/// One item of each kind: a titled document, a three-turn conversation and
/// a learning, on different threads.
pub(crate) fn sample_items() -> Vec<StoreItem> {
    vec![
        StoreItem::Document {
            title: Some("Ownership".into()),
            body: tinymemory_api::DocumentBody::Text("Rust ownership moves values.".into()),
            mime: None,
            meta: thread_meta("t-doc"),
        },
        StoreItem::Conversation {
            turns: vec![
                Turn::new(Role::User, "which editor do I use"),
                Turn::new(Role::Assistant, "you use helix"),
                Turn::new(Role::User, "right, helix"),
            ],
            meta: thread_meta("t-chat"),
        },
        StoreItem::learning(
            "prefers helix",
            LearningKind::Preference,
            0.8,
            thread_meta("t-learn"),
        ),
    ]
}
