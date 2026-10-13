//! A double of a tenant-multiplexing cortexdb-saas proxy (memory-api).
//!
//! It serves only the proxy's allowlist (anything else is the router's bare
//! 404), records every request with the tenant it named, refuses a request
//! naming none (`400 TENANT_REQUIRED`), and forwards the rest to a CortexDB
//! double of that tenant's own, started on first sight. One double per
//! tenant stands in for the proxy pinning each tenant under its own root:
//! no tenant can see another's events. `POST /v1/tenant/purge` drops the
//! tenant's double, as a purge erases everything under its root.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use serde_json::json;

use super::{Shared, direct_double, serve};

/// The header the proxy reads the tenant from.
pub(crate) const TENANT: &str = "x-memory-tenant";

/// `(method, path)` of every route the proxy forwards.
const ALLOWED: [(&str, &str); 13] = [
    ("POST", "/v1/experience"),
    ("POST", "/v1/experience/bulk"),
    ("GET", "/v1/events"),
    ("POST", "/v1/recall"),
    ("POST", "/v1/forget"),
    ("POST", "/v1/answer"),
    ("GET", "/v1/scopes/list"),
    ("GET", "/v1/facts"),
    ("GET", "/v1/beliefs"),
    ("POST", "/v1/beliefs/build"),
    ("GET", "/v1/understanding"),
    ("GET", "/v1/derivation/status"),
    ("GET", "/v1/admin/health"),
];

/// The proxy double's state.
#[derive(Debug, Default)]
pub(crate) struct ProxyDouble {
    /// `("METHOD /path?query", tenant)` of every request, in order.
    pub(crate) seen: Mutex<Vec<(String, Option<String>)>>,
    /// Each tenant's CortexDB double: `(endpoint, state)`.
    tenants: Mutex<BTreeMap<String, (String, Shared)>>,
    /// Tenants purged, in order.
    pub(crate) purged: Mutex<Vec<String>>,
}

impl ProxyDouble {
    /// Every request seen, with its tenant.
    pub(crate) fn requests(&self) -> Vec<(String, Option<String>)> {
        self.seen.lock().unwrap().clone()
    }

    /// The tenant's CortexDB double, if it has one.
    pub(crate) fn tenant(&self, id: &str) -> Option<Shared> {
        self.tenants
            .lock()
            .unwrap()
            .get(id)
            .map(|(_, state)| state.clone())
    }

    /// Every tenant a request named.
    pub(crate) fn tenants_named(&self) -> BTreeSet<String> {
        self.requests()
            .into_iter()
            .filter_map(|(_, tenant)| tenant)
            .collect()
    }

    /// The tenant's CortexDB endpoint, started on first sight.
    async fn endpoint(&self, id: &str) -> String {
        if let Some((endpoint, _)) = self.tenants.lock().unwrap().get(id) {
            return endpoint.clone();
        }
        let (endpoint, state) = direct_double().await;
        self.tenants
            .lock()
            .unwrap()
            .entry(id.to_string())
            .or_insert((endpoint, state))
            .0
            .clone()
    }
}

/// A running proxy double.
pub(crate) async fn proxied_double() -> (String, Arc<ProxyDouble>) {
    let state = Arc::new(ProxyDouble::default());
    let app = Router::new().fallback(handle).with_state(state.clone());
    (serve(app).await, state)
}

fn refuse(code: StatusCode, error_code: &str) -> Response {
    (
        code,
        axum::Json(json!({ "error_code": error_code, "message": error_code })),
    )
        .into_response()
}

async fn handle(
    State(proxy): State<Arc<ProxyDouble>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let tenant = headers
        .get(TENANT)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    proxy
        .seen
        .lock()
        .unwrap()
        .push((format!("{method} {uri}"), tenant.clone()));
    let path = uri.path();
    let purge = method == Method::POST && path == "/v1/tenant/purge";
    let allowed = purge
        || ALLOWED
            .iter()
            .any(|(m, p)| *m == method.as_str() && *p == path);
    if !allowed {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(tenant) = tenant.filter(|t| !t.is_empty()) else {
        return refuse(StatusCode::BAD_REQUEST, "TENANT_REQUIRED");
    };
    if purge {
        let dropped = proxy.tenants.lock().unwrap().remove(&tenant);
        let scopes: BTreeSet<String> = dropped
            .map(|(_, state)| {
                state
                    .log
                    .lock()
                    .unwrap()
                    .events
                    .iter()
                    .filter_map(|e| e["scope"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        proxy.purged.lock().unwrap().push(tenant);
        return axum::Json(json!({ "erased": true, "scopes": scopes.len(), "blobs": 0 }))
            .into_response();
    }
    let endpoint = proxy.endpoint(&tenant).await;
    let target = format!(
        "{endpoint}{}",
        uri.path_and_query().map_or(path, |pq| pq.as_str())
    );
    let mut request = reqwest::Client::new().request(method, target).body(body);
    for name in [header::AUTHORIZATION, header::CONTENT_TYPE] {
        if let Some(value) = headers.get(&name) {
            request = request.header(name, value);
        }
    }
    let answer = request.send().await.unwrap();
    let status = answer.status();
    let bytes = answer.bytes().await.unwrap();
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        bytes.to_vec(),
    )
        .into_response()
}
