//! The doubles' routes: the CortexDB `/v1/*` surface and the TinyHumans
//! `/memory/*` surface over the same handlers.

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use super::{Shared, take_one};

type Reply = (StatusCode, Json<Value>);

/// Keys the hosted answer schema allows; anything else is a 400.
const ANSWER_KEYS: [&str; 11] = [
    "scope",
    "question",
    "question_type",
    "question_date",
    "temporal",
    "filters",
    "answer_max_tokens",
    "answer_instructions",
    "cite_sources",
    "include_context",
    "use_pack_id",
];

/// Segments a hosted scope may hold: the memory API re-roots it under the
/// tenant and the engine holds 32.
const TENANT_SCOPE_SEGMENTS: usize = 31;

fn status(code: u16) -> StatusCode {
    StatusCode::from_u16(code).unwrap()
}

/// A success in the wire's shape.
fn ok(state: &Shared, code: u16, body: Value) -> Reply {
    if state.hosted {
        (status(code), Json(json!({ "success": true, "data": body })))
    } else {
        (status(code), Json(body))
    }
}

/// A failure in the wire's shape.
fn fail(state: &Shared, code: u16, error_code: &str) -> Reply {
    if state.hosted {
        (
            status(code),
            Json(
                json!({ "success": false, "error": format!("failed: {error_code}"), "errorCode": error_code }),
            ),
        )
    } else {
        (status(code), Json(json!({ "error_code": error_code })))
    }
}

/// A failure the engine (through memory-api) answered, in the wire's shape.
///
/// Direct relays it as is. Hosted relays it the way the backend's
/// `memoryUpstreamError` does: the backend has its own vocabulary, so most
/// engine codes are lost and every other 4xx is a `400` (`BAD_REQUEST`).
/// A 409 keeps its status and its `CONFLICT` code since
/// tinyhumansai/backend#1409; an older backend answered it as a `400`
/// (`legacy_conflict_400`).
fn upstream_fail(state: &Shared, code: u16, error_code: &str) -> Reply {
    if !state.hosted {
        return fail(state, code, error_code);
    }
    let (code, error_code) = match code {
        402 => (402, "USER_INSUFFICIENT_CREDITS"),
        503 => (503, "UPSTREAM_UNAVAILABLE"),
        401 | 403 => (502, "UPSTREAM_UNAVAILABLE"),
        404 => (404, "NOT_FOUND"),
        409 if state.legacy_conflict_400.load(Ordering::SeqCst) => (400, "CONFLICT"),
        409 => (409, "CONFLICT"),
        413 => (413, "PAYLOAD_TOO_LARGE"),
        400..=499 => (400, "BAD_REQUEST"),
        _ => (502, "UPSTREAM_UNAVAILABLE"),
    };
    fail(state, code, error_code)
}

/// The backend's own rate limiter (`memoryRateLimit`, express-rate-limit):
/// a 429 whose body is `{error:{message,type}}`, outside the envelope.
/// Direct answers CortexDB's own 429.
fn rate_limited(state: &Shared) -> Reply {
    if !state.hosted {
        return fail(state, 429, "RATE_LIMITED");
    }
    (
        status(429),
        Json(json!({
            "error": {
                "message": "Rate limit exceeded. Please retry after a brief wait.",
                "type": "rate_limit_error",
            }
        })),
    )
}

/// A log result (status, body) in the wire's shape.
fn relay(state: &Shared, (code, body): (u16, Value)) -> Reply {
    if code < 300 {
        ok(state, code, body)
    } else {
        let error_code = body
            .get("error_code")
            .and_then(Value::as_str)
            .unwrap_or("VALIDATION_ERROR")
            .to_string();
        upstream_fail(state, code, &error_code)
    }
}

/// The memory API's scope grammar: `type:id` segments of `[A-Za-z0-9_-]`,
/// at most [`TENANT_SCOPE_SEGMENTS`]. Hosted only.
fn refuse_scope(state: &Shared, scope: &str) -> Option<Reply> {
    if !state.hosted {
        return None;
    }
    let id_chars = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    };
    let segments: Vec<&str> = scope.split('/').collect();
    let well_formed = segments.len() <= TENANT_SCOPE_SEGMENTS
        && segments.iter().all(|s| {
            s.split_once(':')
                .is_some_and(|(k, i)| id_chars(k) && id_chars(i))
        });
    (!well_formed).then(|| fail(state, 400, "BAD_REQUEST"))
}

/// Records the request, checks the bearer, applies `fail_all`.
fn gate(state: &Shared, method: &str, uri: &Uri, headers: &HeaderMap) -> Option<Reply> {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    {
        let mut seen = state.seen.lock().unwrap();
        seen.requests.push(format!("{method} {uri}"));
        seen.auth.push(auth.clone());
    }
    let token = auth.strip_prefix("Bearer ").unwrap_or_default();
    let expected = state.accept_token.lock().unwrap().clone();
    if token.is_empty() || expected.is_some_and(|e| e != token) {
        return Some(fail(state, 401, "UNAUTHORIZED"));
    }
    if let Some((code, error_code)) = *state.fail_all.lock().unwrap() {
        return Some(fail(state, code, error_code));
    }
    None
}

/// Applies one write with every write knob.
/// The configured refusal, when any of `bodies` is attributed.
fn refuse_attribution(state: &Shared, bodies: &[Value]) -> Option<Reply> {
    let (code, error_code) = (*state.refuse_attribution.lock().unwrap())?;
    bodies
        .iter()
        .any(|body| body.get("observed_actor").is_some() || body.get("subject").is_some())
        .then(|| fail(state, code, error_code))
}

fn write_one(state: &Shared, headers: &HeaderMap, body: &Value) -> Reply {
    if let Some(refused) = refuse_scope(state, body["scope"].as_str().unwrap_or_default()) {
        return refused;
    }
    // The backend's rate limiter answers before the memory API: no claim.
    if take_one(&state.rate_limit_experience) {
        return rate_limited(state);
    }
    let claim = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    state.seen.lock().unwrap().idempotency.push((
        body["idempotency_key"].as_str().map(str::to_owned),
        claim.clone(),
    ));
    if state.hosted
        && let Some(claim) = claim
        && !state.claimed.lock().unwrap().insert(claim)
    {
        // memory-api refuses a claimed key with 409; the backend relays it.
        return upstream_fail(state, 409, "CONFLICT");
    }
    if take_one(&state.claim_then_fail) {
        return fail(state, 502, "BAD_GATEWAY");
    }
    let call = state.experience_calls.fetch_add(1, Ordering::SeqCst) + 1;
    if state.fail_nth_experience.load(Ordering::SeqCst) == call {
        return fail(state, 400, "VALIDATION_ERROR");
    }
    let applied = state.log.lock().unwrap().append(body);
    if let Some((limited, hidden)) = state.arm_after_write.lock().unwrap().take() {
        state.rate_limit_events.store(limited, Ordering::SeqCst);
        state.hide_listing_for.store(hidden, Ordering::SeqCst);
    }
    if take_one(&state.apply_then_fail) {
        return fail(state, 503, "UNAVAILABLE");
    }
    relay(state, applied)
}

async fn experience(
    State(state): State<Shared>,
    uri: Uri,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Reply {
    if let Some(early) = gate(&state, "POST", &uri, &headers) {
        return early;
    }
    state.seen.lock().unwrap().writes.push(body.clone());
    if let Some(refused) = refuse_attribution(&state, std::slice::from_ref(&body)) {
        return refused;
    }
    write_one(&state, &headers, &body)
}

async fn bulk(
    State(state): State<Shared>,
    uri: Uri,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Reply {
    if let Some(early) = gate(&state, "POST", &uri, &headers) {
        return early;
    }
    state.seen.lock().unwrap().writes.push(body.clone());
    let items = body["items"].as_array().cloned().unwrap_or_default();
    if let Some(refused) = refuse_attribution(&state, &items) {
        return refused;
    }
    let mut results = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let (code, Json(receipt)) = write_one(&state, &headers, item);
        if !code.is_success() {
            return (code, Json(receipt));
        }
        results.push(json!({
            "index": index,
            "event_id": receipt["event_id"],
            "replayed_from_idempotency": receipt["replayed_from_idempotency"],
        }));
    }
    ok(
        &state,
        200,
        json!({ "accepted": results.len(), "results": results }),
    )
}

/// One delayed listing counted in `listings_in_flight` while it is held,
/// and uncounted when dropped, even when the request is cancelled mid-sleep.
struct InFlight<'a>(&'a Shared);

impl<'a> InFlight<'a> {
    fn enter(state: &'a Shared) -> Self {
        let now = state.listings_in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        state.listings_peak.fetch_max(now, Ordering::SeqCst);
        Self(state)
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.listings_in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn events(
    State(state): State<Shared>,
    uri: Uri,
    headers: HeaderMap,
    Query(params): Query<BTreeMap<String, String>>,
) -> Reply {
    if let Some(early) = gate(&state, "GET", &uri, &headers) {
        return early;
    }
    if let Some(refused) = refuse_scope(&state, params.get("scope").map_or("", String::as_str)) {
        return refused;
    }
    let repeated = uri
        .query()
        .is_some_and(|q| q.split('&').filter(|p| p.starts_with("labels=")).count() > 1);
    if state.hosted && repeated {
        return fail(&state, 400, "VALIDATION_ERROR");
    }
    if take_one(&state.rate_limit_events) {
        return rate_limited(&state);
    }
    if take_one(&state.state_change_events) {
        return fail(&state, 503, "AUTHORIZATION_STATE_CHANGED");
    }
    let delay = state.listing_delay_ms.load(Ordering::SeqCst);
    if delay > 0 {
        let _held = InFlight::enter(&state);
        tokio::time::sleep(std::time::Duration::from_millis(delay as u64)).await;
    }
    let mut page = state.log.lock().unwrap().page(&params);
    if take_one(&state.hide_listing_for) {
        page["items"] = json!([]);
        page["has_more"] = json!(false);
    }
    ok(&state, 200, page)
}

async fn recall(
    State(state): State<Shared>,
    uri: Uri,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Reply {
    if let Some(early) = gate(&state, "POST", &uri, &headers) {
        return early;
    }
    state.seen.lock().unwrap().recalls.push(body.clone());
    if let Some(refused) = refuse_scope(&state, body["scope"].as_str().unwrap_or_default()) {
        return refused;
    }
    if body.get("temporal").is_some() && state.refers_refused.load(Ordering::SeqCst) {
        return fail(&state, 422, "INVALID_BODY");
    }
    if state.recall_down.load(Ordering::SeqCst) {
        return fail(&state, 500, "INTERNAL");
    }
    let slow = *state.slow_recall.lock().unwrap();
    if let Some((scope, ms)) = slow
        && body["scope"].as_str().unwrap_or_default().contains(scope)
    {
        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
    }
    let targeted_channel = body
        .pointer("/filters/metadata/labels")
        .and_then(Value::as_array)
        .is_some_and(|labels| {
            labels.iter().any(|label| {
                label
                    .as_str()
                    .is_some_and(|label| label.starts_with("tm:c:") || label.starts_with("tm:cw:"))
            })
        });
    let slow_channel = *state.slow_channel_recall.lock().unwrap();
    if targeted_channel && let Some(ms) = slow_channel {
        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
    }
    let pack = state.log.lock().unwrap().recall(&body);
    ok(&state, 200, pack)
}

async fn forget(
    State(state): State<Shared>,
    uri: Uri,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Reply {
    if let Some(early) = gate(&state, "POST", &uri, &headers) {
        return early;
    }
    if let Some(refused) = refuse_scope(&state, body["scope"].as_str().unwrap_or_default()) {
        return refused;
    }
    if take_one(&state.rate_limit_forget) {
        return rate_limited(&state);
    }
    state.seen.lock().unwrap().forgets.push(body.clone());
    let result = state.log.lock().unwrap().forget(&body);
    relay(&state, result)
}

async fn erase(
    State(state): State<Shared>,
    uri: Uri,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Reply {
    if let Some(early) = gate(&state, "POST", &uri, &headers) {
        return early;
    }
    if let Some(refused) = refuse_scope(&state, body["scope"].as_str().unwrap_or_default()) {
        return refused;
    }
    state.seen.lock().unwrap().erasures.push(body.clone());
    let (code, mut answer) = state.log.lock().unwrap().erase(&body);
    if code < 300 {
        let status = erasure_status(&state);
        if !state.erasure_post_omits_status.load(Ordering::SeqCst) {
            answer["status"] = json!(status);
        }
    }
    relay(&state, (code, answer))
}

/// The erasure's status as this answer reports it: `running` while the
/// test's running budget lasts, then how it ends.
fn erasure_status(state: &Shared) -> &'static str {
    if take_one(&state.erasure_running_for) {
        "running"
    } else {
        state.erasure_ends.lock().unwrap().unwrap_or("completed")
    }
}

/// `POST /memory/v1/erasures`: the backend's passthrough to memory-api's
/// scoped erasure, answered in its dialect (no envelope): `{scope,
/// audit_note}` only (any other field is `400 UNKNOWN_FIELD`), the root is
/// `422 ROOT_ERASURE_REFUSED`, and a synchronous `{erased, scope, scopes,
/// erasure_ids}`; an incomplete erasure is a retriable `502`.
async fn hosted_erase(
    State(state): State<Shared>,
    uri: Uri,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Reply {
    if let Some(early) = gate(&state, "POST", &uri, &headers) {
        return early;
    }
    if state.scoped_erase_missing.load(Ordering::SeqCst) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "message": "Not Found" })),
        );
    }
    if let Some(unknown) = body.as_object().and_then(|map| {
        map.keys()
            .find(|k| !matches!(k.as_str(), "scope" | "audit_note"))
    }) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error_code": "UNKNOWN_FIELD", "message": unknown })),
        );
    }
    // A scope that is not a string is a malformed request, not the root.
    let Some(scope) = body
        .get("scope")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error_code": "INVALID_SCOPE" })),
        );
    };
    if scope.is_empty() || scope == "/" {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "error_code": "ROOT_ERASURE_REFUSED" })),
        );
    }
    if let Some(refused) = refuse_scope(&state, &scope) {
        return refused;
    }
    state.seen.lock().unwrap().erasures.push(body.clone());
    if take_one(&state.erasure_incomplete_for) {
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "erased": false, "error_code": "ERASURE_INCOMPLETE", "retriable": true })),
        );
    }
    let held = state.log.lock().unwrap().scopes(&scope).len();
    let mut ids = Vec::new();
    if held > 0 {
        let (code, answer) = state
            .log
            .lock()
            .unwrap()
            .erase(&json!({ "scope": scope, "confirm_all": true }));
        if code >= 300 {
            return (status(code), Json(answer));
        }
        ids.push(answer["erasure_id"].clone());
    }
    if let Some(answer) = state.scoped_erase_answer.lock().unwrap().clone() {
        return (StatusCode::OK, Json(answer));
    }
    (
        StatusCode::OK,
        Json(json!({ "erased": true, "scope": scope, "scopes": ids.len(), "erasure_ids": ids })),
    )
}

/// `GET {erasures}/{id}`: the erasure's status, unwrapped.
async fn erasure(
    State(state): State<Shared>,
    axum::extract::Path(id): axum::extract::Path<String>,
    uri: Uri,
    headers: HeaderMap,
) -> Reply {
    if let Some(early) = gate(&state, "GET", &uri, &headers) {
        return early;
    }
    // A forced status leaves the running budget alone.
    if let Some(forced) = state.erasure_poll_status.lock().unwrap().clone() {
        return (
            StatusCode::OK,
            Json(json!({ "erasure_id": id, "status": forced })),
        );
    }
    let status = erasure_status(&state);
    if state.erasure_poll_omits_status.load(Ordering::SeqCst) {
        return (StatusCode::OK, Json(json!({ "erasure_id": id })));
    }
    (
        StatusCode::OK,
        Json(json!({ "erasure_id": id, "status": status })),
    )
}

/// The backend's `DELETE /memory`: erases the caller's entire memory.
async fn erase_all(State(state): State<Shared>, uri: Uri, headers: HeaderMap) -> Reply {
    // The gate (auth, an outage) answers before the route is looked up, as
    // the real backend's middleware does.
    if let Some(early) = gate(&state, "DELETE", &uri, &headers) {
        return early;
    }
    if state.erase_all_missing.load(Ordering::SeqCst) {
        // An older backend: Express's unmatched-route 404, no envelope.
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "message": "Not Found" })),
        );
    }
    state
        .seen
        .lock()
        .unwrap()
        .erasures
        .push(json!({ "all": true }));
    let scopes = state.log.lock().unwrap().erase_everything();
    let data = state
        .erase_all_answer
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(|| json!({ "erased": true, "scopes": scopes }));
    ok(&state, 200, data)
}

async fn answer(
    State(state): State<Shared>,
    uri: Uri,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Reply {
    if let Some(early) = gate(&state, "POST", &uri, &headers) {
        return early;
    }
    state.seen.lock().unwrap().answers.push(body.clone());
    if let Some(refused) = refuse_scope(&state, body["scope"].as_str().unwrap_or_default()) {
        return refused;
    }
    let object = body.as_object().cloned().unwrap_or_default();
    let strict_violation = object.keys().any(|k| !ANSWER_KEYS.contains(&k.as_str()))
        || object
            .get("answer_instructions")
            .is_some_and(Value::is_null);
    if state.hosted && strict_violation {
        return fail(&state, 400, "VALIDATION_ERROR");
    }
    if body["use_pack_id"].as_str() != Some("pack_test") {
        return fail(&state, 400, "MISSING_PACK");
    }
    if take_one(&state.expire_packs) {
        return fail(&state, 404, "NOT_FOUND");
    }
    ok(
        &state,
        200,
        json!({
            "answer": format!("grounded answer for {}", body["question"].as_str().unwrap_or_default()),
            "citations": [],
            "diagnostics": { "answer_model": "reasoning" }
        }),
    )
}

async fn version(State(state): State<Shared>, uri: Uri, headers: HeaderMap) -> Reply {
    if let Some(early) = gate(&state, "GET", &uri, &headers) {
        return early;
    }
    if state.version_down.load(Ordering::SeqCst) {
        return fail(&state, 500, "INTERNAL");
    }
    let capabilities = if state.refers_unlisted.load(Ordering::SeqCst) {
        json!(["temporal_lenient_v1"])
    } else {
        json!(["refers_to_v1", "temporal_lenient_v1"])
    };
    ok(
        &state,
        200,
        json!({ "version": "v0.10.5", "capabilities": capabilities }),
    )
}

async fn health(State(state): State<Shared>, uri: Uri, headers: HeaderMap) -> Reply {
    if let Some(early) = gate(&state, "GET", &uri, &headers) {
        return early;
    }
    ok(&state, 200, json!({ "status": "healthy" }))
}

async fn scopes(
    State(state): State<Shared>,
    uri: Uri,
    headers: HeaderMap,
    Query(params): Query<BTreeMap<String, String>>,
) -> Reply {
    if let Some(early) = gate(&state, "GET", &uri, &headers) {
        return early;
    }
    if let Some(prefix) = params.get("prefix")
        && let Some(refused) = refuse_scope(&state, prefix)
    {
        return refused;
    }
    let prefix = params.get("prefix").cloned().unwrap_or_default();
    let string_prefix = state.string_prefix_scopes.load(Ordering::SeqCst);
    let mut scopes = state
        .log
        .lock()
        .unwrap()
        .scopes_matching(&prefix, string_prefix);
    let padding = state.padding_scopes.load(Ordering::SeqCst);
    let below = if string_prefix || prefix.ends_with('/') {
        prefix.clone()
    } else {
        format!("{prefix}/")
    };
    scopes.extend(
        (0..padding)
            .map(|n| format!("app:tinymemory/agent:pad-{n:04}/app:learnings"))
            .filter(|path| {
                prefix.is_empty()
                    || (!prefix.ends_with('/') && *path == prefix)
                    || path.starts_with(&below)
            }),
    );
    scopes.sort();
    scopes.dedup();
    // As CortexDB: `limit` defaults to 50, is clamped to 1000, no cursor.
    let limit = params
        .get("limit")
        .and_then(|limit| limit.parse::<usize>().ok())
        .unwrap_or(50)
        .min(1000);
    scopes.truncate(limit);
    if state.hosted {
        ok(&state, 200, json!({ "scopes": scopes }))
    } else {
        let items: Vec<Value> = scopes.iter().map(|path| json!({ "path": path })).collect();
        ok(&state, 200, json!({ "items": items }))
    }
}

async fn build_beliefs(
    State(state): State<Shared>,
    uri: Uri,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Reply {
    if let Some(early) = gate(&state, "POST", &uri, &headers) {
        return early;
    }
    let Some(scope) = body["scope"].as_str().filter(|scope| !scope.is_empty()) else {
        return fail(&state, 422, "VALIDATION_ERROR");
    };
    if let Some(refused) = refuse_scope(&state, scope) {
        return refused;
    }
    state.seen.lock().unwrap().builds.push(body.clone());
    // CortexDB v0.10 builds within the request and reports the count.
    let built = state.log.lock().unwrap().build(scope);
    ok(
        &state,
        200,
        json!({ "built": built, "items": [], "facts_scanned": built, "events_scanned": built }),
    )
}

/// CortexDB's `POST v1/scopes`: registers a path once; a second time is
/// `409 SCOPE_REGISTRATION_EXISTS`.
async fn register_scope(
    State(state): State<Shared>,
    uri: Uri,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Reply {
    if let Some(early) = gate(&state, "POST", &uri, &headers) {
        return early;
    }
    let Some(path) = body["path"].as_str().filter(|path| !path.is_empty()) else {
        return fail(&state, 422, "INVALID_BODY");
    };
    if let Some((code, error_code)) = *state.fail_registration.lock().unwrap() {
        return fail(&state, code, error_code);
    }
    let mut seen = state.seen.lock().unwrap();
    if seen.registrations.iter().any(|known| known["path"] == path) {
        return fail(&state, 409, "SCOPE_REGISTRATION_EXISTS");
    }
    seen.registrations.push(body.clone());
    ok(&state, 201, body)
}

/// CortexDB's `GET v1/scopes?path=`: a registered scope's record, or 404.
async fn scope_record(
    State(state): State<Shared>,
    uri: Uri,
    headers: HeaderMap,
    Query(query): Query<BTreeMap<String, String>>,
) -> Reply {
    if let Some(early) = gate(&state, "GET", &uri, &headers) {
        return early;
    }
    let path = query.get("path").cloned().unwrap_or_default();
    let seen = state.seen.lock().unwrap();
    match seen
        .registrations
        .iter()
        .find(|known| known["path"] == path.as_str())
    {
        Some(record) => ok(&state, 200, record.clone()),
        None => fail(&state, 404, "NOT_FOUND"),
    }
}

/// CortexDB's `PUT v1/scopes/members?path=`: replaces a registered scope's
/// members.
async fn scope_members(
    State(state): State<Shared>,
    uri: Uri,
    headers: HeaderMap,
    Query(query): Query<BTreeMap<String, String>>,
    Json(body): Json<Value>,
) -> Reply {
    if let Some(early) = gate(&state, "PUT", &uri, &headers) {
        return early;
    }
    let path = query.get("path").cloned().unwrap_or_default();
    let mut seen = state.seen.lock().unwrap();
    match seen
        .registrations
        .iter_mut()
        .find(|known| known["path"] == path.as_str())
    {
        Some(record) => {
            record["members"] = body["members"].clone();
            ok(&state, 200, record.clone())
        }
        None => fail(&state, 404, "NOT_FOUND"),
    }
}

async fn beliefs(
    State(state): State<Shared>,
    uri: Uri,
    headers: HeaderMap,
    Query(query): Query<BTreeMap<String, String>>,
) -> Reply {
    if let Some(early) = gate(&state, "GET", &uri, &headers) {
        return early;
    }
    let scope = query.get("scope").cloned().unwrap_or_default();
    if let Some(refused) = refuse_scope(&state, &scope) {
        return refused;
    }
    let items = state.log.lock().unwrap().list_beliefs(&scope);
    ok(&state, 200, json!({ "items": items, "has_more": false }))
}

/// `v1/auth/whoami`: the configured caller, else 404. Not recorded in
/// `seen`, so a test counting requests counts the same with or without it.
async fn whoami(State(state): State<Shared>) -> Reply {
    match state.whoami_caller.lock().unwrap().clone() {
        Some(caller) => (StatusCode::OK, Json(json!({ "caller": caller }))),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error_code": "NOT_FOUND" })),
        ),
    }
}

/// CortexDB's own routes.
pub(super) fn direct(state: Shared) -> Router {
    Router::new()
        .route("/v1/auth/whoami", get(whoami))
        .route("/v1/experience", post(experience))
        .route("/v1/experience/bulk", post(bulk))
        .route("/v1/events", get(events))
        .route("/v1/recall", post(recall))
        .route("/v1/forget", post(forget))
        .route("/v1/erasures", post(erase))
        .route("/v1/erasures/{id}", get(erasure))
        .route("/v1/answer", post(answer))
        .route("/v1/admin/health", get(health))
        .route("/v1/admin/version", get(version))
        .route("/v1/scopes/list", get(scopes))
        .route("/v1/scopes", post(register_scope).get(scope_record))
        .route("/v1/scopes/members", axum::routing::put(scope_members))
        .route("/v1/beliefs/build", post(build_beliefs))
        .route("/v1/beliefs", get(beliefs))
        .with_state(state)
}

/// The TinyHumans backend's routes.
pub(super) fn hosted(state: Shared) -> Router {
    Router::new()
        .route("/memory/experience", post(experience))
        .route("/memory/events", get(events))
        .route("/memory/recall", post(recall))
        .route("/memory/forget", post(forget))
        .route("/memory/answer", post(answer))
        .route("/memory/scopes", get(scopes))
        .route("/memory", axum::routing::delete(erase_all))
        .route("/memory/v1/erasures", post(hosted_erase))
        .route("/memory/v1/erasures/{id}", get(erasure))
        .with_state(state)
}
