//! Erasing a whole scope: `POST v1/erasures` with `confirm_all`; and the
//! caller's entire memory: `DELETE memory` on the TinyHumans wire, `POST
//! v1/tenant/purge` on the proxied one.
//!
//! The one place this crate sends `confirm_all`, and never with a selector:
//! the request names exactly one scope, so it erases that scope's events
//! (deleted, not redacted, their write keys released). CortexDB only
//! redacts the scopes below it, which keep their keys; `engine::erase`
//! names kind scopes only (leaves), deepest first, so that never happens.
//!
//! Both wires erase a scope. Direct posts `{scope, confirm_all}` to
//! CortexDB's own `v1/erasures`; hosted posts `{scope, audit_note}` to the
//! TinyHumans backend's `memory/v1/erasures` passthrough (memory-api's
//! scoped erasure, which takes no `confirm_all` and refuses an unknown
//! field). memory-api pins the scope under the caller's tenant root, erases
//! it and every scope below it with the tenant's own user-actor token, and
//! answers synchronously in its own dialect (no `{success,data}` envelope;
//! see `transport`): `{erased: true, scopes, erasure_ids}`, where `scopes:
//! 0` (nothing stored) is still success. `502 ERASURE_INCOMPLETE` is
//! the only failure retried (any other leaves the outcome unknown); re-erasing a scope is safe. A backend without the
//! route answers 404, reported as [`Error::Unsupported`]: nothing was erased.
//!
//! A Direct erasure is a job. CortexDB answers `completed` when it ran to
//! the end before answering; a `running` answer is polled at
//! `v1/erasures/{id}` until it settles, and anything but `completed` is an
//! error: an erasure that did not finish must never read as done.
//!
//! The proxied wire has no `v1/erasures` (a cortexdb-saas proxy does not
//! expose it), so it erases a scope by listing every event in it and
//! forgetting them by `memory_ids` (`v1/forget`, which also releases their
//! write keys and takes what was derived from them). No erasure receipt
//! exists for that, so it reports none.

use std::time::Instant;

use serde_json::{Value, json};

use super::{HOSTED_POLL_CEILING, Log};
use crate::cortex::descriptor::{CortexWire, Route};
use crate::cortex::error::{Error, Result, error_code};
use crate::cortex::transport::{Attempts, urlencode};

/// How many times a hosted erasure is sent before an incomplete or
/// transient failure surfaces.
const HOSTED_ERASE_ATTEMPTS: u32 = 3;

/// The code of the retriable `502` that says an erasure did not finish.
const ERASURE_INCOMPLETE: &str = "ERASURE_INCOMPLETE";

/// The one status that means the scope is gone.
const COMPLETED: &str = "completed";

/// Statuses of an erasure that has not settled yet.
const PENDING: [&str; 4] = ["running", "pending", "queued", "accepted"];

/// The audit note every erasure carries.
const AUDIT_NOTE: &str = "tinymemory: erase";

/// What one scope's erasure removed.
#[derive(Debug)]
pub(crate) struct Erased {
    /// How many scopes the backend erased: one on Direct; on a hosted wire
    /// what the backend reports (`0` when nothing was left to erase).
    pub(crate) scopes: usize,
    /// The erasure ids, once completed.
    pub(crate) ids: Vec<String>,
}

impl Log {
    /// Erases `scope`, returning what was erased once it has completed.
    pub(crate) async fn erase(&self, scope: &str) -> Result<Erased> {
        match self.client.wire() {
            CortexWire::Direct => self.erase_direct(scope).await.map(|id| Erased {
                scopes: 1,
                ids: vec![id],
            }),
            CortexWire::TinyHumans => self.erase_hosted(scope).await,
            CortexWire::Proxied => self.erase_by_forget(scope).await,
        }
    }

    /// Proxied: every event of `scope`, forgotten by id. One scope erased,
    /// whether or not it held anything, as on Direct.
    async fn erase_by_forget(&self, scope: &str) -> Result<Erased> {
        let ids: Vec<String> = self
            .walk(scope, None)
            .await?
            .iter()
            .filter_map(|event| event.get("id").and_then(Value::as_str))
            .map(str::to_owned)
            .collect();
        self.forget_events(scope, &ids).await?;
        Ok(Erased {
            scopes: 1,
            ids: Vec::new(),
        })
    }

    /// Hosted: memory-api's synchronous scoped erasure. Retried on a
    /// transient failure (`502 ERASURE_INCOMPLETE` among them), since
    /// erasing a scope again erases only what is left.
    async fn erase_hosted(&self, scope: &str) -> Result<Erased> {
        let body = json!({ "scope": scope, "audit_note": AUDIT_NOTE });
        let path = self.client.wire().path(Route::Erasures);
        let mut attempt = 0;
        let answer = loop {
            attempt += 1;
            match self
                .client
                .json(reqwest::Method::POST, path, Some(&body), Attempts::Once)
                .await
            {
                Ok(answer) => break answer,
                Err(Error::NotFound(message)) => {
                    return Err(Error::Unsupported(format!(
                        "the TinyHumans backend has no scoped erasure route ({message})"
                    )));
                }
                // Only an answer that proves the erasure incomplete is
                // retried (re-erasing erases what is left). Any other
                // failure leaves the outcome unknown, and a repeated
                // destructive request would hide it, so it surfaces.
                Err(error)
                    if error_code(&error) == Some(ERASURE_INCOMPLETE)
                        && attempt < HOSTED_ERASE_ATTEMPTS =>
                {
                    log::debug!("[cortex] erasure of {scope} incomplete; retrying ({attempt})");
                    tokio::time::sleep(self.timing.poll * 2_u32.pow(attempt - 1)).await;
                }
                Err(error) => return Err(error),
            }
        };
        if answer.get("erased").and_then(Value::as_bool) != Some(true) {
            return Err(Error::Engine(format!(
                "the erasure of {scope} answered without `erased: true`"
            )));
        }
        let scopes = answer
            .get("scopes")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                Error::Engine(format!(
                    "the erasure of {scope} answered without a numeric `scopes` count"
                ))
            })?;
        let ids = match answer.get("erasure_ids") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(ids)) => ids
                .iter()
                .map(|id| id.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| {
                    Error::Engine(format!(
                        "the erasure of {scope} answered a non-string entry in `erasure_ids`"
                    ))
                })?,
            Some(_) => {
                return Err(Error::Engine(format!(
                    "the erasure of {scope} answered `erasure_ids` that is not an array"
                )));
            }
        };
        // The receipts are the audit trail of a destructive call: scopes
        // erased without one is an incompatible answer, not a success.
        if scopes > 0 && ids.iter().all(|id| id.trim().is_empty()) {
            return Err(Error::Engine(format!(
                "the erasure of {scope} erased {scopes} scope(s) but answered no `erasure_ids`"
            )));
        }
        Ok(Erased {
            scopes: usize::try_from(scopes).map_err(|_| {
                Error::Engine(format!(
                    "the erasure of {scope} answered a `scopes` count ({scopes}) beyond this platform's range"
                ))
            })?,
            ids,
        })
    }

    /// Direct: CortexDB's `v1/erasures`, sent once (a failure surfaces),
    /// then polled while it runs.
    async fn erase_direct(&self, scope: &str) -> Result<String> {
        let body = json!({
            "scope": scope,
            "confirm_all": true,
            "audit_note": AUDIT_NOTE,
        });
        let path = self.client.wire().path(Route::Erasures);
        let answer = self
            .client
            .json(reqwest::Method::POST, path, Some(&body), Attempts::Once)
            .await?;
        let id = answer
            .get("erasure_id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                Error::Engine(format!("the erasure of {scope} answered no erasure_id"))
            })?;
        // The synchronous answer may omit its status (it ran to the end).
        let mut status = status_of(&answer, true)?;
        let limit = self.timing.erasure;
        let deadline = Instant::now() + limit;
        let mut gap = self.timing.poll;
        while PENDING.contains(&status.as_str()) {
            let unavailable = || {
                Error::Unavailable(format!(
                    "the erasure of {scope} ({id}) was still pending after {}ms",
                    limit.as_millis()
                ))
            };
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(unavailable());
            }
            let poll = async {
                tokio::time::sleep(gap).await;
                self.client
                    .json(
                        reqwest::Method::GET,
                        &format!("{path}/{}", urlencode(&id)),
                        None,
                        Attempts::RetryTransient,
                    )
                    .await
            };
            // The deadline bounds the sleep and every retry of the request,
            // not just the gap between polls.
            let job = tokio::time::timeout(remaining, poll)
                .await
                .map_err(|_| unavailable())??;
            gap = (gap * 2).min(HOSTED_POLL_CEILING);
            // A polled job must say where it stands: a missing status is
            // not a completed erasure.
            status = status_of(&job, false)?;
            log::debug!("[cortex] erasure {id} of {scope} is {status}");
        }
        if status != COMPLETED {
            return Err(Error::Engine(format!(
                "the erasure of {scope} ({id}) ended {status}, not {COMPLETED}"
            )));
        }
        Ok(id)
    }

    /// Erases the caller's entire memory, every scope under its tenant
    /// whoever wrote it, returning how many scopes the backend erased:
    /// `DELETE memory` on the TinyHumans wire, `POST v1/tenant/purge` (the
    /// call's tenant) on the proxied one. Sent once, like a scope erasure.
    ///
    /// A backend without the route answers 404, which is reported as
    /// [`Error::Unsupported`]: nothing was erased.
    pub(crate) async fn erase_all(&self) -> Result<usize> {
        let wire = self.client.wire();
        let (method, backend) = match wire {
            CortexWire::TinyHumans => (reqwest::Method::DELETE, "the TinyHumans backend"),
            CortexWire::Proxied => (reqwest::Method::POST, "the memory proxy"),
            CortexWire::Direct => {
                return Err(Error::Unsupported(
                    "only the TinyHumans backend and a memory proxy erase a whole memory in one \
                     request"
                        .to_string(),
                ));
            }
        };
        let answer = self
            .client
            .json(method, wire.path(Route::EraseAll), None, Attempts::Once)
            .await
            .map_err(|error| match error {
                Error::NotFound(message) => Error::Unsupported(format!(
                    "{backend} has no whole-memory erase route ({message})"
                )),
                other => other,
            })?;
        if answer.get("erased").and_then(Value::as_bool) != Some(true) {
            return Err(Error::Engine(
                "the memory erasure answered without `erased: true`".to_string(),
            ));
        }
        // A count the backend did not send (or sent as anything but a
        // non-negative integer) is a malformed answer, not zero scopes.
        let scopes = answer
            .get("scopes")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                Error::Engine(
                    "the memory erasure answered without a numeric `scopes` count".to_string(),
                )
            })?;
        Ok(usize::try_from(scopes).unwrap_or(usize::MAX))
    }
}

/// An erasure answer's status. CortexDB's synchronous POST answer may omit
/// it (`default_completed`: an id with no status ran to completion); a
/// polled job must carry a string status, and a status of any other type is
/// malformed either way.
fn status_of(answer: &Value, default_completed: bool) -> Result<String> {
    match answer.get("status") {
        Some(Value::String(status)) => Ok(status.clone()),
        None if default_completed => Ok(COMPLETED.to_string()),
        other => Err(Error::Engine(format!(
            "an erasure answer carried no usable status ({})",
            other.map_or_else(|| "missing".to_string(), |v| v.to_string())
        ))),
    }
}
