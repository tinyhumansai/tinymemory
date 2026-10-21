//! Turning transport faults, statuses and hosted envelopes into [`Error`]s.
//!
//! Every message names the route and the endpoint host, never a credential.
//! Anything the backend itself said comes after a spaced em-dash (` — `), so
//! a status surface can keep the head and withhold the backend's own text
//! (see `health_reason`).

use reqwest::StatusCode;
use serde_json::Value;

use crate::cortex::error::{Error, INSUFFICIENT_CREDITS_CODE, PLAN_REQUIRED_SUFFIX};

/// Longest excerpt of a backend's error text kept in a message.
const MAX_DETAIL_CHARS: usize = 300;

/// The class of a request that produced no response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransportClass {
    Timeout,
    Dns,
    Tls,
    Connect,
    /// The chain names no cause: a mid-body disconnect, a reset.
    Other,
}

impl TransportClass {
    /// The operator-facing description of this class.
    pub(crate) fn describe(self) -> &'static str {
        match self {
            Self::Timeout => "timed out",
            Self::Dns => "the host could not be resolved; check the URL",
            Self::Tls => {
                "TLS failed: the endpoint answered on the port but could not establish a secure \
                 connection; check that the URL is the engine's real API host"
            }
            Self::Connect => "could not connect; check the URL and that the service is reachable",
            Self::Other => "the request did not complete",
        }
    }
}

/// Names the class of a transport failure from what reqwest and the error
/// chain say.
///
/// The order is the point: `is_connect()` is also true for DNS and TLS
/// failures, so checking it first collapses every class into "could not
/// connect". TLS is recognised from the chain's text because rustls' error
/// types are not public dependencies of this crate.
pub(crate) fn classify_transport(
    is_timeout: bool,
    is_connect: bool,
    chain: &str,
) -> TransportClass {
    let lower = chain.to_ascii_lowercase();
    if is_timeout {
        TransportClass::Timeout
    } else if lower.contains("dns")
        || lower.contains("name or service")
        || lower.contains("failed to lookup")
    {
        TransportClass::Dns
    } else if lower.contains("tls")
        || lower.contains("handshake")
        || lower.contains("certificate")
        || lower.contains("fatal alert")
        || lower.contains("invalid peer")
        || lower.contains("unknown issuer")
    {
        TransportClass::Tls
    } else if is_connect {
        TransportClass::Connect
    } else {
        TransportClass::Other
    }
}

/// The error for a request that never produced a response.
///
/// Every class is [`Error::Unavailable`]: the same call may succeed later. A
/// request reqwest could not even build is [`Error::Engine`], because no
/// retry will change it.
pub(crate) fn transport_error(host: &str, error: &reqwest::Error) -> Error {
    if error.is_builder() {
        return Error::Engine(format!("memory API request to {host} could not be built"));
    }
    let mut parts = Vec::new();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        parts.push(cause.to_string());
        source = cause.source();
    }
    let chain = parts.join(": ");
    let class = classify_transport(error.is_timeout(), error.is_connect(), &chain);
    let described = class.describe();
    if chain.is_empty() {
        Error::Unavailable(format!("memory API request to {host}: {described}"))
    } else {
        Error::Unavailable(format!(
            "memory API request to {host}: {described} — {chain}"
        ))
    }
}

/// A backend's text, trimmed and cut to [`MAX_DETAIL_CHARS`].
fn excerpt(text: &str) -> String {
    let text = text.trim();
    let mut shown: String = text.chars().take(MAX_DETAIL_CHARS).collect();
    if text.chars().count() > MAX_DETAIL_CHARS {
        shown.push('…');
    }
    shown
}

/// `head`, then the backend's `detail` after a spaced em-dash when it said
/// anything.
fn with_detail(head: String, detail: &str) -> String {
    if detail.is_empty() {
        head
    } else {
        format!("{head} — {detail}")
    }
}

/// Maps a status to its variant, given the message head and detail.
fn by_status(status: StatusCode, head: String, detail: &str) -> Error {
    let insufficient_credits = head
        .strip_prefix('[')
        .and_then(|rest| rest.split_once("] "))
        .is_some_and(|(code, _)| code == INSUFFICIENT_CREDITS_CODE);
    let message = with_detail(head, detail);
    match status.as_u16() {
        402 if insufficient_credits => Error::InsufficientCredits(message),
        401 | 403 => Error::Unauthorized(message),
        404 => Error::NotFound(message),
        400 | 413 | 422 => Error::InvalidRequest(message),
        409 => Error::Conflict(message),
        // 408: a `wait=indexed` write the indexer has not reached within the
        // server's wait (`WAIT_TIMEOUT`). The event is captured and durable,
        // and a resend with the same idempotency key replays, so it is
        // transient, not an engine fault.
        408 | 429 | 500 | 502 | 503 | 504 => Error::Unavailable(message),
        _ => Error::Engine(message),
    }
}

/// The error for a non-success status from CortexDB's own API.
pub(crate) fn direct_status_error(
    host: &str,
    label: &str,
    status: StatusCode,
    body: &str,
) -> Error {
    let head = match status.as_u16() {
        401 | 403 => format!(
            "memory API {label} on {host}: the credential was rejected (HTTP {status}); check \
             the API key"
        ),
        _ => format!("memory API {label} on {host} returned HTTP {status}"),
    };
    // The one code a read waits out (`read_attempts`) leads the message as
    // `[CODE] `, as a hosted code does, read from the body's own field.
    let code = serde_json::from_str::<Value>(body).ok().and_then(|v| {
        v.get("error_code")
            .and_then(Value::as_str)
            .map(str::to_string)
    });
    let head = match code.as_deref() {
        Some(code @ STATE_CHANGED) => format!("[{code}] {head}"),
        _ => head,
    };
    by_status(status, head, &excerpt(body))
}

/// The error code CortexDB answers a read with while a scope's
/// authorization changes under it (`503`, retriable).
pub(crate) const STATE_CHANGED: &str = "AUTHORIZATION_STATE_CHANGED";

/// The error for a TinyHumans failure: a non-2xx status or a
/// `{success:false}` body. The backend's `errorCode` leads the message as
/// `[CODE] `.
pub(crate) fn hosted_status_error(
    host: &str,
    label: &str,
    status: StatusCode,
    body: &str,
) -> Error {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let code = parsed
        .as_ref()
        // The backend's own `errorCode`, or memory-api's `error_code` on a
        // route the backend passes through unwrapped (`memory/v1/*`).
        // A code that is null, not a string or empty once cleaned does not
        // shadow the other key.
        .and_then(|v| {
            ["errorCode", "error_code"]
                .iter()
                .filter_map(|key| v.get(*key).and_then(Value::as_str))
                .map(clean_code)
                .find(|c| !c.is_empty())
        })
        .unwrap_or_else(|| default_code(status));
    let message = parsed.as_ref().and_then(|v| v.get("error")).map_or_else(
        || body.to_string(),
        |e| e.as_str().map_or_else(|| e.to_string(), str::to_string),
    );
    // Keep the public Error enum exhaustive-match compatible. Unauthorized is
    // already account-wide; its backend-code helper distinguishes this plan
    // refusal from a credential rejection for the host's upgrade surface.
    if code.ends_with(PLAN_REQUIRED_SUFFIX) {
        let head = format!(
            "[{code}] memory API {label} on {host}: memory is not included in this account's \
             plan (HTTP {status})"
        );
        return Error::Unauthorized(with_detail(head, &excerpt(&message)));
    }
    let head = match status.as_u16() {
        401 => format!(
            "[{code}] memory API {label} on {host}: the session expired or the API key was \
             rejected (HTTP {status}); re-authenticate"
        ),
        // The credential was accepted and the request refused, so nothing
        // here says the session expired.
        403 => format!(
            "[{code}] memory API {label} on {host}: the request was refused (HTTP {status})"
        ),
        402 if code == INSUFFICIENT_CREDITS_CODE => format!(
            "[{code}] memory API {label} on {host}: the account has insufficient credits \
             (HTTP {status})"
        ),
        402 => format!("[{code}] memory API {label} on {host} returned HTTP {status}"),
        _ => format!("[{code}] memory API {label} on {host} returned HTTP {status}"),
    };
    // memory-api's refusal of an already-claimed `Idempotency-Key` is relayed
    // as `409` with `errorCode: CONFLICT` by the backend since
    // tinyhumansai/backend#1409, and as `400` with the same code by older
    // ones (`memoryUpstreamError`). A retried write reads that as the
    // outcome-unknown conflict it is, whichever status carries it.
    if status == StatusCode::BAD_REQUEST && code == CONFLICT_CODE {
        return by_status(StatusCode::CONFLICT, head, &excerpt(&message));
    }
    by_status(status, head, &excerpt(&message))
}

/// The backend's `errorCode` for a refused, already-claimed write.
const CONFLICT_CODE: &str = "CONFLICT";

/// Unwraps `{success:true,data}`. `{success:false}`, a missing `data` and a
/// body without the envelope are all errors.
pub(crate) fn unwrap_envelope(
    host: &str,
    label: &str,
    status: StatusCode,
    body: &[u8],
) -> Result<Value, Error> {
    let mut value: Value = serde_json::from_slice(body).map_err(|_| {
        Error::Engine(format!(
            "memory API {label} on {host} returned invalid JSON"
        ))
    })?;
    match value.get("success").and_then(Value::as_bool) {
        Some(true) => value.get_mut("data").map(Value::take).ok_or_else(|| {
            Error::Engine(format!(
                "memory API {label} on {host} answered success without a `data` field"
            ))
        }),
        Some(false) => Err(hosted_status_error(host, label, status, &value.to_string())),
        None => Err(Error::Engine(format!(
            "memory API {label} on {host} answered without the success envelope"
        ))),
    }
}

/// Keeps a backend code parseable as a `[CODE]` prefix.
fn clean_code(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .take(64)
        .collect::<String>()
        .to_ascii_uppercase()
}

/// The neutral code a failure without a usable `errorCode` is filed under.
/// HTTP 402 does not establish credit exhaustion on its own.
fn default_code(status: StatusCode) -> String {
    match status.as_u16() {
        401 | 403 => "UNAUTHORIZED".to_string(),
        402 => "HTTP_402".to_string(),
        429 => "RATE_LIMITED".to_string(),
        other => format!("HTTP_{other}"),
    }
}

/// A health reason from a probe failure, with the backend's own text
/// withheld: a vendor is free to echo a rejected key in an error body, and a
/// health reason is rendered on a standing status surface.
pub(crate) fn health_reason(error: &Error) -> String {
    let text = error.to_string();
    match text.split_once(" — ") {
        Some((head, _)) => format!("{head} (detail withheld)"),
        None => text,
    }
}

#[cfg(test)]
#[path = "failure_tests.rs"]
mod tests;
