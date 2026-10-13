//! The HTTP transport both wires share.
//!
//! [`HttpClient`] resolves a route against the endpoint, attaches the bearer
//! (resolved per attempt and marked sensitive), sends, reads the body against
//! a cap, unwraps the TinyHumans envelope when the wire has one, and types
//! every failure (see `failure`).
//!
//! **Reads retry; writes do not.** Every call states its [`Attempts`]. A read
//! (listing, recall) is retried up to three times with 250ms·2ⁿ backoff on
//! [`crate::cortex::Error::Unavailable`]; a write is sent once, because a timeout on a
//! write leaves whether it applied unknown. Hosted writes recover from that
//! one level up, with an `Idempotency-Key` claim (see `log::write`).
//!
//! **The proxied wire names a tenant on every request.** A client on
//! [`CortexWire::Proxied`] carries the tenant of the call it serves
//! ([`HttpClient::set_tenant`]) and sends it in [`TENANT_HEADER`]; a client
//! without one refuses to send anything, so a request can never reach the
//! proxy as a shared or default tenant.
//!
//! **Cleartext only on loopback, or by opt-in inside a cluster.** A
//! credentialed `http://` endpoint is refused unless its host is loopback.
//! A cluster-service host (`*.svc`, `*.svc.cluster.local`) is accepted when
//! the engine is built, but no request is sent to it until the host opts in
//! with [`HttpClient::allow_private_network_cleartext`]: that is for a
//! service whose traffic NetworkPolicy keeps inside the cluster.

mod actor;
mod body;
mod failure;
mod pause;

use std::time::Duration;

use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Method, RequestBuilder, Url};
use serde_json::Value;

use crate::cortex::credential::CortexCredential;
use crate::cortex::descriptor::CortexWire;
use crate::cortex::error::{Error, Result};

pub(crate) use failure::health_reason;

/// Default per-request deadline.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// Ceiling on the connect phase, so a black-holed endpoint does not spend the
/// whole request budget before the first byte.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Attempts a retrying read makes.
const READ_ATTEMPTS: u32 = 3;

/// Attempts a retrying read makes when CortexDB answers that the scope
/// authorization it was checking changed under it (`503
/// AUTHORIZATION_STATE_CHANGED`, retriable): a concurrent write is
/// registering a scope, which takes longer to settle than the usual cap
/// waits, so it is waited out for about 8 seconds.
const STATE_CHANGE_ATTEMPTS: u32 = 6;

/// First read-retry gap; it doubles per attempt.
const READ_BACKOFF: Duration = Duration::from_millis(250);

/// The header the proxied wire names each request's tenant in: a bare id
/// (`[A-Za-z0-9_-]`, at most [`MAX_TENANT_ID`] characters) the proxy turns
/// into the tenant whose root it pins every scope under.
pub(crate) const TENANT_HEADER: &str = "X-Memory-Tenant";

/// The longest tenant id the proxied wire sends.
pub(crate) const MAX_TENANT_ID: usize = 96;

/// Whether `id` is a tenant id the proxied wire may send: 1 to
/// [`MAX_TENANT_ID`] characters of `[A-Za-z0-9_-]`.
pub(crate) fn valid_tenant_id(id: &str) -> bool {
    (1..=MAX_TENANT_ID).contains(&id.len())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// Whether a cleartext endpoint may be sent to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cleartext {
    /// HTTPS, or HTTP to a loopback host.
    Permitted,
    /// HTTP to a cluster-service host the engine has not opted in to.
    NeedsOptIn,
}

/// Whether a call may be repeated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Attempts {
    /// A read: retried on a transient failure.
    RetryTransient,
    /// A write: one attempt, whatever the failure.
    Once,
}

/// HTTP transport for one endpoint, wire and credential. `Debug` shows the
/// endpoint origin, never the credential.
#[derive(Clone)]
pub(crate) struct HttpClient {
    inner: reqwest::Client,
    endpoint: Url,
    credential: CortexCredential,
    wire: CortexWire,
    read_backoff: Duration,
    actor: actor::ActorCache,
    /// When the last `429` said requests may resume (see `pause`).
    pause: pause::RatePause,
    /// Fixed headers the host attaches to every request (see
    /// [`default_headers`]).
    default_headers: HeaderMap,
    /// The tenant the proxied wire names (see [`TENANT_HEADER`]); `None`
    /// refuses every proxied request.
    tenant: Option<HeaderValue>,
    /// Whether a cleartext endpoint may be sent to.
    cleartext: Cleartext,
}

/// Headers a host may not fix: the credential, the claim and actor this
/// transport sets itself, and what the HTTP stack owns.
const RESERVED_HEADERS: [&str; 8] = [
    "authorization",
    "proxy-authorization",
    "cookie",
    "host",
    "content-length",
    "idempotency-key",
    actor::ACTOR_HEADER,
    TENANT_HEADER,
];

/// The header map for `pairs`: fixed, non-credential headers a host sends
/// on every request, such as its product attribution (`x-sdk-name`).
///
/// # Errors
///
/// [`Error::Config`] for a name or value no header may carry, or a reserved
/// name (the credential, `Idempotency-Key`, the actor header, and what the
/// HTTP stack sets). No message carries a value.
pub(crate) fn default_headers<K, V>(pairs: impl IntoIterator<Item = (K, V)>) -> Result<HeaderMap>
where
    K: AsRef<str>,
    V: AsRef<str>,
{
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        let name = name.as_ref().trim();
        let parsed = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| Error::Config(format!("`{name}` is not a valid header name")))?;
        if RESERVED_HEADERS
            .iter()
            .any(|reserved| reserved.eq_ignore_ascii_case(parsed.as_str()))
        {
            return Err(Error::Config(format!(
                "`{name}` is set by the memory transport and cannot be fixed"
            )));
        }
        let value = HeaderValue::from_str(value.as_ref().trim())
            .map_err(|_| Error::Config(format!("the `{name}` header value is not valid")))?;
        map.insert(parsed, value);
    }
    Ok(map)
}

impl std::fmt::Debug for HttpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpClient")
            .field("endpoint", &self.endpoint.origin().ascii_serialization())
            .field("wire", &self.wire)
            .finish_non_exhaustive()
    }
}

/// Whether `url`'s host is a Kubernetes cluster-service name
/// (`<service>.<namespace>.svc`, optionally `.cluster.local`), which only
/// resolves inside the cluster.
pub(crate) fn is_cluster_service(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    [".svc", ".svc.cluster.local"].iter().any(|suffix| {
        host.strip_suffix(suffix)
            .is_some_and(|name| !name.is_empty() && !name.ends_with('.'))
    })
}

/// Refuses cleartext HTTP to a non-loopback host. Every engine here is
/// credentialed, so a cleartext endpoint would put the bearer on the wire.
pub(crate) fn ensure_secure_endpoint(url: &Url) -> Result<()> {
    if url.scheme() != "http" {
        return Ok(());
    }
    let host = url
        .host_str()
        .ok_or_else(|| Error::Config("memory endpoint has no host".to_string()))?;
    let ip_host = host.trim_start_matches('[').trim_end_matches(']');
    let loopback = host.eq_ignore_ascii_case("localhost")
        || ip_host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    if loopback {
        Ok(())
    } else {
        Err(Error::Config(
            "credentialed memory endpoints must use https unless they are loopback".to_string(),
        ))
    }
}

impl HttpClient {
    /// A client for `endpoint` on `wire`.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] for an unparsable or non-HTTP(S) endpoint, a
    /// cleartext endpoint that is neither loopback nor a cluster service (a
    /// cluster service is refused per request until it is opted in to), or
    /// a blank static credential.
    pub(crate) fn new(
        wire: CortexWire,
        endpoint: &str,
        credential: CortexCredential,
    ) -> Result<Self> {
        let mut url = Url::parse(endpoint)
            .map_err(|_| Error::Config("memory endpoint is not a valid URL".to_string()))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(Error::Config(
                "memory endpoint must use http or https".to_string(),
            ));
        }
        let cleartext = match ensure_secure_endpoint(&url) {
            Ok(()) => Cleartext::Permitted,
            Err(_) if is_cluster_service(&url) => Cleartext::NeedsOptIn,
            Err(error) => return Err(error),
        };
        if let CortexCredential::Static(key) = &credential
            && key.trim().is_empty()
        {
            return Err(Error::Config("the API key must not be empty".to_string()));
        }
        if !url.path().ends_with('/') {
            let path = format!("{}/", url.path());
            url.set_path(&path);
        }
        Ok(Self {
            inner: build_inner(DEFAULT_TIMEOUT, url.scheme() == "https")?,
            endpoint: url,
            credential,
            wire,
            read_backoff: READ_BACKOFF,
            actor: actor::ActorCache::default(),
            pause: pause::RatePause::default(),
            default_headers: HeaderMap::new(),
            tenant: None,
            cleartext,
        })
    }

    /// Names `id` as the tenant of every proxied request from now on.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] for an id that is not 1 to [`MAX_TENANT_ID`]
    /// characters of `[A-Za-z0-9_-]`.
    pub(crate) fn set_tenant(&mut self, id: &str) -> Result<()> {
        if !valid_tenant_id(id) {
            return Err(Error::Config(format!(
                "the memory tenant id `{id}` must be 1 to {MAX_TENANT_ID} characters of A-Z, a-z, \
                 0-9, `_` or `-`"
            )));
        }
        let value = HeaderValue::from_str(id)
            .map_err(|_| Error::Config(format!("the memory tenant id `{id}` is not valid")))?;
        self.tenant = Some(value);
        Ok(())
    }

    /// Lets a cleartext endpoint on a cluster-service host be sent to.
    /// Changes nothing for any other endpoint.
    pub(crate) fn allow_private_network_cleartext(&mut self) {
        self.cleartext = Cleartext::Permitted;
    }

    /// Sends `headers` on every request from now on.
    pub(crate) fn set_default_headers(&mut self, headers: HeaderMap) {
        self.default_headers = headers;
    }

    /// The wire this client speaks.
    pub(crate) fn wire(&self) -> CortexWire {
        self.wire
    }

    /// The endpoint's origin, for `Debug` output.
    pub(crate) fn origin(&self) -> String {
        self.endpoint.origin().ascii_serialization()
    }

    fn host(&self) -> &str {
        self.endpoint.host_str().unwrap_or("<endpoint>")
    }

    /// Resolves `path` and attaches the bearer.
    ///
    /// The bearer is resolved here, on every attempt, so a refreshed token is
    /// used at once. A source failure, a blank token, or a token that cannot
    /// be a header value (CR/LF) is [`Error::Unauthorized`] and no request is
    /// sent; no message carries the token.
    async fn request(&self, method: Method, path: &str) -> Result<RequestBuilder> {
        if self.cleartext == Cleartext::NeedsOptIn {
            return Err(Error::Config(format!(
                "the memory endpoint `{}` is cleartext http to a cluster service; opt in with \
                 `with_private_network_cleartext` or use https",
                self.origin()
            )));
        }
        let tenant = match (self.wire, &self.tenant) {
            (CortexWire::Proxied, None) => {
                return Err(Error::Config(
                    "the proxied memory wire names a tenant on every request, and this call has \
                     none"
                        .to_string(),
                ));
            }
            (CortexWire::Proxied, Some(tenant)) => Some(tenant.clone()),
            _ => None,
        };
        let url = self
            .endpoint
            .join(path.trim_start_matches('/'))
            .map_err(|_| Error::Engine(format!("memory API path `{}` is invalid", label(path))))?;
        let token = self.credential.resolve().await.map_err(|error| {
            Error::Unauthorized(format!(
                "the bearer source could not supply a credential: {error}"
            ))
        })?;
        let header = credential_header(&token)?;
        let mut request = self
            .inner
            .request(method, url)
            .headers(self.default_headers.clone())
            .header(AUTHORIZATION, header);
        if let Some(tenant) = tenant {
            request = request.header(TENANT_HEADER, tenant);
        }
        Ok(request)
    }

    /// Sends a JSON request and returns the decoded body (unwrapped from the
    /// hosted envelope). A hosted `POST` sent [`Attempts::Once`] carries a
    /// fresh `Idempotency-Key` claim.
    pub(crate) async fn json(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        attempts: Attempts,
    ) -> Result<Value> {
        match attempts {
            Attempts::Once => {
                let key = (self.wire == CortexWire::TinyHumans && method == Method::POST)
                    .then(fresh_idempotency_key);
                self.attempt(method, path, body, key.as_deref()).await
            }
            Attempts::RetryTransient => {
                let mut tried = 0;
                loop {
                    tried += 1;
                    match self.attempt(method.clone(), path, body, None).await {
                        Err(Error::Unavailable(message)) if tried < read_attempts(&message) => {
                            tokio::time::sleep(self.read_backoff * 2_u32.pow(tried - 1)).await;
                        }
                        other => return other,
                    }
                }
            }
        }
    }

    /// One attempt of a hosted write under a caller-chosen `Idempotency-Key`,
    /// so the caller can reuse one claim across its own retries.
    pub(crate) async fn json_keyed(&self, path: &str, body: &Value, key: &str) -> Result<Value> {
        self.attempt(Method::POST, path, Some(body), Some(key))
            .await
    }

    /// GETs `path` and checks it succeeds (and, hosted, that the envelope
    /// says so). One attempt: a probe reports what it saw.
    pub(crate) async fn probe(&self, path: &str) -> Result<()> {
        self.attempt(Method::GET, path, None, None)
            .await
            .map(|_| ())
    }

    /// The actor this client's credential acts as (`whoami`'s `caller`),
    /// direct wire only.
    pub(crate) async fn caller(&self) -> Option<String> {
        self.actor().await?.to_str().ok().map(str::to_string)
    }

    /// The `X-Cortex-Actor` value to send (direct wire only), asking
    /// `v1/auth/whoami` the first time. See `actor`.
    async fn actor(&self) -> Option<HeaderValue> {
        if self.wire != CortexWire::Direct {
            return None;
        }
        match self.actor.lookup() {
            actor::Lookup::Send(value) => Some(value),
            actor::Lookup::Skip => None,
            actor::Lookup::Ask => {
                let response = self
                    .request(Method::GET, actor::WHOAMI_PATH)
                    .await
                    .ok()?
                    .send()
                    .await
                    .ok()?;
                let status = response.status();
                let bytes = body::read_capped(response, actor::WHOAMI_PATH)
                    .await
                    .unwrap_or_default();
                self.actor.learn(status, &bytes)
            }
        }
    }

    /// One send.
    async fn attempt(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        idempotency: Option<&str>,
    ) -> Result<Value> {
        let label = label(path);
        let mut request = self.request(method, path).await?;
        if let Some(actor) = self.actor().await {
            request = request.header(actor::ACTOR_HEADER, actor);
        }
        if let Some(key) = idempotency.filter(|_| self.wire == CortexWire::TinyHumans) {
            request = request.header("Idempotency-Key", key);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        // A rate limit answered to any request holds this one back too.
        self.pause.wait().await;
        let response = request
            .send()
            .await
            .map_err(|error| failure::transport_error(self.host(), &error))?;
        let status = response.status();
        if status.as_u16() == 429 {
            self.pause.note(response.headers());
        }
        if !status.is_success() {
            if matches!(status.as_u16(), 401 | 403) {
                self.actor.forget();
            }
            let text = body::read_error_body(response).await;
            return Err(match self.wire {
                CortexWire::Direct => {
                    failure::direct_status_error(self.host(), label, status, &text)
                }
                // Both name their refusals with an `error_code`, which the
                // hosted mapping leads the message with.
                CortexWire::TinyHumans | CortexWire::Proxied => {
                    failure::hosted_status_error(self.host(), label, status, &text)
                }
            });
        }
        let bytes = body::read_capped(response, label).await?;
        match self.wire {
            CortexWire::TinyHumans if !speaks_cortex(path) => {
                failure::unwrap_envelope(self.host(), label, status, &bytes)
            }
            _ if bytes.is_empty() => Ok(Value::Null),
            _ => serde_json::from_slice(&bytes).map_err(|_| {
                Error::Engine(format!(
                    "memory API {label} on {} returned invalid JSON",
                    self.host()
                ))
            }),
        }
    }
}

/// The TinyHumans backend's passthrough of CortexDB's own `/v1` dialect,
/// mounted under `memory/`: memory-api's status and body, no
/// `{success,data}` envelope.
pub(crate) const HOSTED_CORTEX_PREFIX: &str = "memory/v1/";

/// Whether a hosted `path` is answered in CortexDB's own dialect rather
/// than the backend's envelope.
fn speaks_cortex(path: &str) -> bool {
    path.starts_with(HOSTED_CORTEX_PREFIX)
}

/// The route part of a path, for messages: query strings carry scopes and
/// cursors, which are noise in an error.
fn label(path: &str) -> &str {
    path.split('?').next().unwrap_or(path)
}

/// One place builds the reqwest client, so the two timeouts stay paired.
///
/// `https_only` is set for an HTTPS endpoint. reqwest drops `Authorization`
/// on a redirect only when the host or port changes, so a same-port
/// downgrade (`https://h:8443` to `http://h:8443`) would otherwise carry the
/// bearer in cleartext. A loopback HTTP endpoint ([`ensure_secure_endpoint`])
/// keeps plain HTTP.
fn build_inner(timeout: Duration, https_only: bool) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(CONNECT_TIMEOUT.min(timeout))
        .https_only(https_only)
        .build()
        .map_err(|_| Error::Config("the HTTP client could not be built".to_string()))
}

/// The `Authorization` value for `token`, marked sensitive so nothing that
/// formats the request prints it.
///
/// Parsed up front: a token holding CR/LF (header injection) or another byte
/// no header may carry is refused here as a credential fault, before any
/// request, and the refusal carries no part of the token.
pub(crate) fn credential_header(token: &str) -> Result<HeaderValue> {
    let token = token.trim();
    if token.is_empty() {
        return Err(Error::Unauthorized("the credential is empty".to_string()));
    }
    let mut header = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
        Error::Unauthorized("the credential is not a valid HTTP header value".to_string())
    })?;
    header.set_sensitive(true);
    Ok(header)
}

/// How many attempts a read that failed with `message` gets: the error's
/// code leads the message as `[CODE] ` (`failure`), matched exactly.
fn read_attempts(message: &str) -> u32 {
    let code = message
        .strip_prefix('[')
        .and_then(|rest| rest.split_once(']'))
        .map(|(code, _)| code);
    if code == Some(failure::STATE_CHANGED) {
        STATE_CHANGE_ATTEMPTS
    } else {
        READ_ATTEMPTS
    }
}

/// The body `idempotency_key` of an experience request: `tm3:` and the
/// first 56 hex digits of the SHA-256 of the request without its key, 60
/// characters (CortexDB refuses one over 64).
///
/// Derived from the whole body, so an identical retry replays: CortexDB
/// 0.10.4 answers it with the first event's id and
/// `replayed_from_idempotency: true`, and writes nothing, for 24 hours.
/// Any change to the body (a new `observed_at` on an unchanged item) is a
/// new key and a new event, never a 409 for a reused key with another body.
/// `/v1/forget` by `memory_ids` releases a key (measured on 0.10.4), so an
/// item stored again after it was forgotten is written again. `tm3` names
/// the event layout the body is in.
pub(crate) fn body_idempotency_key(request: &Value) -> String {
    use sha2::{Digest, Sha256};
    let mut body = request.clone();
    if let Some(object) = body.as_object_mut() {
        object.remove("idempotency_key");
    }
    let bytes = serde_json::to_vec(&body).unwrap_or_default();
    let mut key = String::from("tm3:");
    for byte in Sha256::digest(&bytes).iter().take(28) {
        key.push_str(&format!("{byte:02x}"));
    }
    key
}

/// A fresh key for every hosted write's `Idempotency-Key` claim.
///
/// Never derived from content: the hosted memory API answers every replay
/// of a claim with 409, so a content-derived claim would refuse an identical
/// re-store. One claim is reused across the retries of one write.
///
/// Three parts: a per-process salt from the OS-seeded `RandomState`, the
/// wall-clock nanoseconds, and a counter, so neither two writes in one
/// process nor two processes writing at once can mint the same key.
pub(crate) fn fresh_idempotency_key() -> String {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicU64, Ordering};

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

/// Percent-encodes everything outside the URI unreserved set, byte by byte.
/// A cursor is opaque engine output, and a `+`, `&`, `=` or `#` in one would
/// silently reshape the query string.
pub(crate) fn urlencode(value: &str) -> String {
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

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "transport_test_support.rs"]
mod test_support;

#[cfg(test)]
#[path = "mod_tenant_tests.rs"]
mod tenant_tests;
