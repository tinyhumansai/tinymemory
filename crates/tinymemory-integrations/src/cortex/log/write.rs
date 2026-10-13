//! Appending an item's events.
//!
//! **Direct** sends one experience (`v1/experience?wait=indexed`) or, for a
//! conversation, one ordered batch (`v1/experience/bulk?wait=indexed` with
//! `ordering: strict_temporal`), once — without `?wait=indexed` when the
//! caller only waits for acceptance ([`WaitFor::Accepted`]), so the server
//! answers on capture: a timeout on a write leaves whether it
//! applied unknown, and the store's replay check makes a retry by the caller
//! safe.
//!
//! **TinyHumans** has no bulk route, so a conversation is written one event
//! at a time, in order. Each write carries a random `Idempotency-Key` claim,
//! reused across that write's own retries. The memory API takes the claim
//! before forwarding and keeps it once the engine has been contacted, and
//! answers any replay of a claimed key with 409 without forwarding it. So:
//!
//! - a transient fault (429, 5xx, timeout) is retried under the same claim;
//!   a fault raised before the memory API (the backend's own rate limiter)
//!   left the claim free, and the retry is simply forwarded;
//! - a 409 on a *retry* means the earlier attempt reached the engine and may
//!   have been applied: the outcome is unknown rather than failed, and the
//!   event is looked for (same scope, same item label, same stored text)
//!   until the visibility budget runs out.
//!
//! The stored text names the item id and, for a turn, its index, so finding
//! an event with exactly that text is proof this write (or an identical
//! earlier one) landed.
//!
//! Either way the write then waits for its last event to be readable.

use serde_json::{Value, json};
use tinymemory_api::WaitFor;

use super::{Log, PAGE_SIZE};
use crate::cortex::descriptor::{CortexWire, Route};
use crate::cortex::error::{Error, Result};
use crate::cortex::transport::{Attempts, fresh_idempotency_key};

/// The last event of a write: what a wait for it needs.
#[derive(Debug, Clone)]
pub(crate) struct Written {
    pub(crate) scope: String,
    pub(crate) label: String,
    pub(crate) text: String,
    pub(crate) event_id: String,
    /// Whether CortexDB answered every event of the write as a replay of
    /// one it already holds (`replayed_from_idempotency`).
    pub(crate) replayed: bool,
}

/// How many times a hosted write is sent before a transient fault surfaces.
const HOSTED_WRITE_ATTEMPTS: u32 = 3;

/// The scope, stored text and item label of an experience request.
fn parts(request: &Value) -> Result<(&str, &str, &str)> {
    let scope = request.get("scope").and_then(Value::as_str);
    let text = request.pointer("/content/text").and_then(Value::as_str);
    let label = request.pointer("/context/labels/0").and_then(Value::as_str);
    match (scope, text, label) {
        (Some(scope), Some(text), Some(label)) => Ok((scope, text, label)),
        _ => Err(Error::Engine(
            "an experience request lacks its scope, text or item label".to_string(),
        )),
    }
}

/// Whether a write receipt says CortexDB replayed the event rather than
/// writing it. Absent (an older server, or a hosted answer without it) is
/// not a replay.
fn replayed(answer: &Value) -> bool {
    answer
        .get("replayed_from_idempotency")
        .and_then(Value::as_bool)
        == Some(true)
}

/// The event id a write receipt names.
fn receipt(answer: &Value) -> Result<String> {
    answer
        .get("event_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| Error::Engine("CortexDB accepted a write but omitted event_id".to_string()))
}

impl Log {
    /// Writes `requests` (one item's events, in order) without waiting, and
    /// names the last event so a caller can wait for it with
    /// [`Log::await_written`], or for a later one in the same scope, which
    /// implies it: the log is ordered, so the last event being listed implies
    /// the earlier ones are.
    ///
    /// `wait` decides only whether a Direct write asks the server to index
    /// before answering; waiting for readability is the caller's step.
    pub(crate) async fn write(&self, requests: &[Value], wait: WaitFor) -> Result<Option<Written>> {
        let Some(last) = requests.last() else {
            return Ok(None);
        };
        let (event_id, replayed) = match self.client.wire() {
            CortexWire::Direct | CortexWire::Proxied => self.append_direct(requests, wait).await?,
            CortexWire::TinyHumans => {
                let mut last = (String::new(), true);
                for request in requests {
                    let (event_id, replayed) = self.send_hosted_write(request).await?;
                    last = (event_id, last.1 && replayed);
                }
                last
            }
        };
        let (scope, text, label) = parts(last)?;
        Ok(Some(Written {
            scope: scope.to_string(),
            label: label.to_string(),
            text: text.to_string(),
            event_id,
            replayed,
        }))
    }

    /// Waits for `written` to be listed and, when `settle`, ranked.
    pub(crate) async fn await_written(&self, written: &Written, settle: bool) -> Result<()> {
        if settle {
            self.await_readable(
                &written.scope,
                &written.label,
                &written.event_id,
                &written.text,
            )
            .await
        } else {
            self.await_listed(&written.scope, &written.label, &written.event_id)
                .await
        }
    }

    /// One Direct write of one event or one ordered batch; the last event's
    /// id, and whether every event was a replay.
    async fn append_direct(&self, requests: &[Value], wait: WaitFor) -> Result<(String, bool)> {
        let wire = self.client.wire();
        let query = match wait {
            WaitFor::Visible => "?wait=indexed",
            WaitFor::Accepted => "",
        };
        if let [single] = requests {
            let path = format!("{}{query}", wire.path(Route::Experience));
            let answer = self
                .client
                .json(reqwest::Method::POST, &path, Some(single), Attempts::Once)
                .await?;
            return Ok((receipt(&answer)?, replayed(&answer)));
        }
        let path = format!("{}{query}", wire.path(Route::Bulk));
        let body = json!({ "items": requests, "ordering": "strict_temporal" });
        let answer = self
            .client
            .json(reqwest::Method::POST, &path, Some(&body), Attempts::Once)
            .await?;
        let results = answer
            .get("results")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::Engine("CortexDB omitted bulk results".to_string()))?;
        if results.len() != requests.len() {
            return Err(Error::Engine(format!(
                "CortexDB returned {} bulk results for {} events",
                results.len(),
                requests.len()
            )));
        }
        let last = results.last().map_or_else(
            || Err(Error::Engine("empty bulk results".to_string())),
            receipt,
        )?;
        Ok((last, results.iter().all(replayed)))
    }

    /// One hosted write under one claim, with the outcome-unknown recovery;
    /// the event id, and whether it was a replay.
    async fn send_hosted_write(&self, request: &Value) -> Result<(String, bool)> {
        let path = self.client.wire().path(Route::Experience);
        let claim = fresh_idempotency_key();
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.client.json_keyed(path, request, &claim).await {
                Ok(answer) => return Ok((receipt(&answer)?, replayed(&answer))),
                Err(Error::Conflict(_)) if attempt > 1 => {
                    return Ok((self.recover_unknown_write(request).await?, false));
                }
                Err(error) if attempt < HOSTED_WRITE_ATTEMPTS && error.is_transient() => {
                    tokio::time::sleep(self.timing.poll * 2_u32.pow(attempt - 1)).await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Finds the event a possibly-applied write produced, polling with the
    /// visibility budget and riding out transient faults: one 429 while
    /// looking must not turn a write that succeeded into a failure.
    async fn recover_unknown_write(&self, request: &Value) -> Result<String> {
        let (scope, text, label) = parts(request)?;
        let labels = [label.to_string()];
        let deadline = tokio::time::Instant::now() + self.timing.visibility;
        let mut delay = self.timing.poll;
        loop {
            match self.page(scope, Some(&labels), None, PAGE_SIZE).await {
                Ok(page) => {
                    // The text alone does not tell two turns of one item
                    // apart (two v3 turns can both say "ok"); their labels
                    // (the envelope parts, with the turn index) do.
                    let found = page.items.iter().find(|event| {
                        event.pointer("/content/text").and_then(Value::as_str) == Some(text)
                            && event.pointer("/context/labels")
                                == request.pointer("/context/labels")
                    });
                    if let Some(event) = found {
                        return receipt(&json!({ "event_id": event.get("id") }));
                    }
                }
                Err(error) if error.is_transient() => {}
                Err(error) => return Err(error),
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(Error::Unavailable(format!(
                    "a retried write was refused as already claimed, but no matching event \
                     appeared in scope `{scope}` within {:?}; its outcome is unknown",
                    self.timing.visibility
                )));
            }
            tokio::time::sleep(delay).await;
            delay = self.next_poll(delay);
        }
    }
}
