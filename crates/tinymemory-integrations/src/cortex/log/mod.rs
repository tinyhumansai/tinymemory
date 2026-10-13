//! CortexDB as an append-only event log: the raw operations the engine is
//! built from.
//!
//! # Engine behaviours this module is shaped around
//!
//! Each was measured against a running CortexDB by the v1 adapter, and each
//! was wrong in that adapter first. The test doubles reproduce all of them.
//!
//! - **It is append-only.** `/v1/experience` only appends; there is no
//!   update. A body `idempotency_key` replays the same body for 24 hours
//!   (`replayed_from_idempotency`) and refuses another body (409); forget by
//!   `memory_ids` releases it (0.10.4). Keys are derived from the body, so a
//!   retry is a replay (`transport::body_idempotency_key`).
//! - **Accepted is not readable.** `/v1/experience` answers `202 captured`
//!   and indexes afterwards. Writes wait (see `visibility`): first until the
//!   listing carries the event, which is fatal on timeout, then until ranked
//!   recall does, which is best-effort. `/v1/experience/status` and the
//!   advertised lifecycle stream are not readiness signals.
//! - **The listing emits every event twice**, and `limit` counts the
//!   duplicates. Readers dedupe by event id and follow
//!   `cursor`/`next_cursor`/`has_more`; a full walk refuses past
//!   [`MAX_PAGES`] rather than answering from a truncated log.
//! - **Unknown query parameters are ignored, not refused**, so a wrong
//!   paging parameter re-serves page one for ever. The parameter is exactly
//!   `cursor`, and a cursor that does not advance is an error.
//! - **A pack's events carry the stored text.** 0.10.3 and 0.10.4 put the
//!   `[role] ` marker only in `context_block` and the index copy; an older
//!   recall prefixed it to `layers.events` text, which the v2 decoder still
//!   tolerates (see `Envelope::decode`).
//! - **The forget selector's id field is `memory_ids`.** An unrecognised
//!   field reads as an *empty* selector, which means the whole scope. This
//!   crate never sends an empty selector and never sends `confirm_all`.
//! - **A forget by `memory_ids` takes what was derived too.** On 0.10.4,
//!   `layers: ["events"]` (what this crate sends) and `layers: []` alike
//!   delete the named raw events *and* every fact, belief, episode and
//!   understanding record that cites them; records derived from other events
//!   stay. A `layers` list without `events` deletes nothing. (0.9.9's default
//!   kept raw events.) It is not a GDPR erasure: that is `v1/erasures`.
//!
//! On the TinyHumans wire the backend also rate-limits a user to 300
//! requests a minute, has no bulk, `?wait=indexed` or health route, and
//! takes an `Idempotency-Key` claim on every write (see `write`).

mod erase;
mod forget;
mod notes;
mod read;
mod visibility;
mod write;

#[cfg(test)]
pub(crate) use read::below_whole_segments;
pub(crate) use write::Written;

use std::time::Duration;

use crate::cortex::transport::HttpClient;

/// Events one listing page asks for. `limit` counts the engine's duplicate
/// copies, so a page holds about half as many distinct events.
pub(crate) const PAGE_SIZE: usize = 200;

/// Ceiling on the pages one walk reads. Hitting it is an error, never a
/// truncated answer.
pub(crate) const MAX_PAGES: usize = 500;

/// How long the engine waits for its timing-sensitive steps.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Timing {
    /// How long a write waits for the listing to carry its event, and how
    /// long an outcome-unknown write is looked for. Measured: one to four
    /// seconds; 30s is a wide margin, because the failure it guards is a
    /// write reported as done that the next read cannot see.
    pub(crate) visibility: Duration,
    /// How long a write lets ranked recall catch up before giving up on it
    /// quietly. Recall lags the listing by about a second.
    pub(crate) settle: Duration,
    /// First gap between polls. Direct polls at this gap; hosted doubles it
    /// up to [`HOSTED_POLL_CEILING`].
    pub(crate) poll: Duration,
    /// How long a running Direct erasure is polled before it is reported
    /// as not done (the status requests included).
    pub(crate) erasure: Duration,
}

/// Longest gap between hosted polls. A fixed 250ms poll would spend a fifth
/// of the backend's per-user rate limit on one write.
pub(crate) const HOSTED_POLL_CEILING: Duration = Duration::from_secs(2);

impl Default for Timing {
    fn default() -> Self {
        Self {
            visibility: Duration::from_secs(30),
            settle: Duration::from_secs(10),
            poll: Duration::from_millis(250),
            erasure: Duration::from_secs(300),
        }
    }
}

/// The event log behind one engine: a transport and its timing.
#[derive(Clone, Debug)]
pub(crate) struct Log {
    pub(crate) client: HttpClient,
    pub(crate) timing: Timing,
}

impl Log {
    /// A log over `client` with the default timing.
    pub(crate) fn new(client: HttpClient) -> Self {
        Self {
            client,
            timing: Timing::default(),
        }
    }

    /// The next poll gap after `current`.
    fn next_poll(&self, current: Duration) -> Duration {
        match self.client.wire() {
            crate::cortex::CortexWire::Direct | crate::cortex::CortexWire::Proxied => current,
            crate::cortex::CortexWire::TinyHumans => (current * 2).min(HOSTED_POLL_CEILING),
        }
    }
}
