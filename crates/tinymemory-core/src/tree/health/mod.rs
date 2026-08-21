//! Host-side surface of the memory pipeline's failure + degradation model.
//!
//! The **taxonomy itself** — [`FailureCode`], [`FailureClass`],
//! [`PipelineFailure`], [`DegradedState`], and the `classify_embed_error`
//! classifier — now lives in the engine crate at
//! `crate::engine::backend::health`, and is re-exported below so every existing
//! `memory::tree::health::…` path keeps resolving. It moved because a build
//! whose only driver was a third-party external backend would have no use for
//! the engine's private failure vocabulary.
//!
//! What stays here is everything that is *host* surface rather than engine
//! vocabulary:
//!
//! - the **process-visible degradation flags** (`mark_*` / `clear_*` /
//!   [`current_degraded_state`]) — set by host plumbing deep in the job worker,
//!   read by the `pipeline_status` RPC, and coupled to a socket broadcast;
//! - [`doctor`] — the health report, which reads the host's scheduler-gate
//!   config;
//! - `user_error` — whose `kind` string is a pinned contract with the frontend.

pub mod doctor;
pub use doctor::{async_run_doctor, run_doctor, DoctorCounters, DoctorReport, StageHealth};

pub(crate) mod user_error;
pub(crate) use user_error::publish_local_model_unavailable_user_error;

/// The failure taxonomy proper. Re-exported (rather than re-declared) so the
/// ~30 `crate::tree::health::{…}` call sites across the host
/// are unaffected by the move, and so there is exactly one definition.
pub use crate::engine::backend::health::{
    classify_embed_error, classify_embed_error_str, DegradedState, FailureClass, FailureCode,
    PipelineFailure,
};

// ── Process-visible degradation flags ────────────────────────────────────
//
// The embed/extract stages run deep inside the job worker, far from the
// `pipeline_status` RPC. Rather than thread a `DegradedState` return up
// through every call site, the stages set these process-global atomics when
// they detect a degraded condition (no usable embedder → semantic recall
// disabled; extraction empty across the board → no structure). The status /
// doctor surface reads them via [`current_degraded_state`]. They reflect the
// most recent run, are cheap, and never block — a coarse "is recall/structure
// currently degraded?" signal, intentionally not per-namespace.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

static SEMANTIC_RECALL_DEGRADED: AtomicBool = AtomicBool::new(false);
/// Whether the clients have already been told about the *current* local-runtime
/// outage. Separate from [`SEMANTIC_RECALL_DEGRADED`] because "is recall
/// degraded" and "have we announced it" are different questions, and the
/// announcement must be claimed by exactly one caller: the embed path runs
/// concurrently across worker tasks, and a plain read-then-write of the
/// degraded flag lets two of them both decide they are the first
/// (CodeRabbit, #5398). Claimed with `compare_exchange`, released by
/// [`clear_semantic_recall_degraded`] so a later outage announces again.
static LOCAL_MODEL_USER_ERROR_SURFACED: AtomicBool = AtomicBool::new(false);
static STRUCTURE_DEGRADED: AtomicBool = AtomicBool::new(false);
/// The host filesystem can't service the memory_tree path (EIO/ENOSPC/EROFS).
/// Set by the queue worker's host-I/O arm; cleared on the next successful
/// claim (storage recovered). Most severe — outranks recall/structure.
static STORAGE_DEGRADED: AtomicBool = AtomicBool::new(false);
/// Per-flag degradation cause as a `FailureCode` discriminant (0 = none).
/// Tracked separately per flag so clearing one degradation can't leave the
/// other reporting a stale cause (e.g. mark recall, mark structure, clear
/// structure → recall must still report its OWN cause, not structure's).
static SEMANTIC_RECALL_CAUSE: AtomicU8 = AtomicU8::new(0);
static STRUCTURE_CAUSE: AtomicU8 = AtomicU8::new(0);
static STORAGE_CAUSE: AtomicU8 = AtomicU8::new(0);

fn code_to_u8(code: FailureCode) -> u8 {
    match code {
        FailureCode::BudgetExhausted => 1,
        FailureCode::AuthMissing => 2,
        FailureCode::AuthInvalid => 3,
        FailureCode::EmbeddingsUnconfigured => 4,
        FailureCode::EmbeddingDimMismatch => 5,
        FailureCode::LocalModelUnavailable => 6,
        FailureCode::ExtractionTimeout => 7,
        FailureCode::SummarizerUnavailable => 8,
        FailureCode::Transient => 9,
        FailureCode::EmptyInputRefused => 10,
        FailureCode::StorageUnavailable => 11,
    }
}

fn u8_to_code(v: u8) -> Option<FailureCode> {
    Some(match v {
        1 => FailureCode::BudgetExhausted,
        2 => FailureCode::AuthMissing,
        3 => FailureCode::AuthInvalid,
        4 => FailureCode::EmbeddingsUnconfigured,
        5 => FailureCode::EmbeddingDimMismatch,
        6 => FailureCode::LocalModelUnavailable,
        7 => FailureCode::ExtractionTimeout,
        8 => FailureCode::SummarizerUnavailable,
        9 => FailureCode::Transient,
        10 => FailureCode::EmptyInputRefused,
        11 => FailureCode::StorageUnavailable,
        _ => return None,
    })
}

/// Record that semantic recall is degraded (embeddings were skipped because no
/// usable provider is available). `cause` names why so the status surface can
/// lead the user to the fix. Idempotent / cheap; safe to call per embed-stage.
///
/// The cause is published **before** the flag, and the flag with `Release`, so
/// a concurrent [`current_degraded_state`] that observes the flag set cannot
/// still read the previous degradation's cause and render the wrong
/// remediation (CodeRabbit, #5398). Same ordering in every `mark_*` below.
pub fn mark_semantic_recall_degraded(cause: FailureCode) {
    SEMANTIC_RECALL_CAUSE.store(code_to_u8(cause), Ordering::Relaxed);
    SEMANTIC_RECALL_DEGRADED.store(true, Ordering::Release);
}

/// Surface a local-runtime embed failure on the status panel immediately
/// (#5354). No-op for every other cause.
///
/// The typed `failure_reason` a job persists is only read back once that job
/// settles *terminally* — for a transient class that means after the whole
/// retry budget has drained. The local-runtime causes (Ollama daemon stopped,
/// model never pulled) are user-fixable right now, so waiting out the backoff
/// before naming the fix is exactly the silent window this issue is about.
/// Setting the degraded flag at classification time puts the remediation on
/// the panel from the first failure; the flag self-clears on the next
/// successful embed, so a user who starts Ollama sees it disappear.
///
/// This is also the **only** producer of the durable UserErrorCenter entry for
/// the "model was never pulled" half of the cause. The embedder health gate in
/// `memory::store::factories` probes `GET /api/tags`, which succeeds whenever
/// the daemon is up — so a running daemon with a missing model never trips that
/// gate and never publishes its `user_error` (codex, #5398). Publishing here
/// covers both halves from the one place that has actually classified the
/// failure.
///
/// The broadcast fires only on the **transition** into the state, not on every
/// failed embed: the re-embed path calls this per row, and while the panel
/// store dedupes on the descriptor identity, emitting one socket event per
/// chunk would be pointless traffic.
///
/// The announcement is claimed with a `compare_exchange` on a dedicated latch
/// rather than by reading the degraded flag, so exactly one of several
/// concurrent embed tasks publishes (CodeRabbit, #5398).
///
/// The latch is released by [`clear_semantic_recall_degraded`], which every
/// write-embedder build calls once per seal / re-embed operation. That is
/// deliberate: it makes the announcement **re-emit once per failing operation
/// until recovery**, so a client that was not yet connected when the outage
/// began still receives it on the next operation. `publish_web_channel_event`
/// is an unbuffered broadcast with no replay, so bounded re-emission is what
/// stands in for one.
pub fn mark_local_model_unavailable_if_applicable(failure: &PipelineFailure) {
    if failure.code != FailureCode::LocalModelUnavailable {
        return;
    }
    // Claim the announcement before mutating anything else. `compare_exchange`
    // makes the check-and-claim one indivisible step, so concurrent callers
    // cannot all conclude they are first.
    let claimed_announcement = LOCAL_MODEL_USER_ERROR_SURFACED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok();

    log::warn!(
        "[memory_tree::health] action=mark_degraded surface=semantic_recall \
         cause=local_model_unavailable class={} announced={}",
        failure.class.as_str(),
        claimed_announcement
    );
    mark_semantic_recall_degraded(FailureCode::LocalModelUnavailable);

    if claimed_announcement {
        publish_local_model_unavailable_user_error("embed_classify");
    }
}

/// Clear the semantic-recall degraded flag — call when an embed succeeds, so
/// the surface recovers once the user fixes the provider. Clears only this
/// flag's cause; a still-active structure degradation keeps its own.
pub fn clear_semantic_recall_degraded() {
    SEMANTIC_RECALL_DEGRADED.store(false, Ordering::Relaxed);
    SEMANTIC_RECALL_CAUSE.store(0, Ordering::Relaxed);
    // Release the announcement claim so a later local-runtime failure tells the
    // clients again. Called once per write-embedder build, which is what turns
    // the single announcement into bounded re-emission until recovery — the
    // reason a client connecting mid-outage still gets told.
    LOCAL_MODEL_USER_ERROR_SURFACED.store(false, Ordering::Release);
}

/// Record that wiki structure is degraded (extraction yielded nothing across
/// the board). `cause` is typically [`FailureCode::ExtractionTimeout`].
pub fn mark_structure_degraded(cause: FailureCode) {
    STRUCTURE_CAUSE.store(code_to_u8(cause), Ordering::Relaxed);
    STRUCTURE_DEGRADED.store(true, Ordering::Release);
}

/// Clear the structure degraded flag — call when extraction yields entities.
/// Clears only this flag's cause.
pub fn clear_structure_degraded() {
    STRUCTURE_DEGRADED.store(false, Ordering::Relaxed);
    STRUCTURE_CAUSE.store(0, Ordering::Relaxed);
}

/// Record that the memory_tree storage path is unusable — the host filesystem
/// returned a persistent I/O error (EIO/ENOSPC/EROFS) on dir-create / DB open.
/// `cause` is typically [`FailureCode::StorageUnavailable`]. Set by the queue
/// worker's host-I/O arm so the status surface tells the user to check their
/// disk; idempotent / cheap.
pub fn mark_storage_degraded(cause: FailureCode) {
    STORAGE_CAUSE.store(code_to_u8(cause), Ordering::Relaxed);
    STORAGE_DEGRADED.store(true, Ordering::Release);
}

/// Clear the storage degraded flag — call when a claim succeeds (the DB opened,
/// so the host filesystem recovered), so the surface self-heals. Clears only
/// this flag's cause.
pub fn clear_storage_degraded() {
    STORAGE_DEGRADED.store(false, Ordering::Relaxed);
    STORAGE_CAUSE.store(0, Ordering::Relaxed);
}

/// Test-only serialization + reset for the process-global degraded flags.
///
/// The flags are a single process-wide signal, so tests across *different*
/// modules (factory, extract::llm, tree::rpc) that set or read them race under
/// cargo's parallel runner. Any such test must `let _g = test_guard();` at the
/// top: it takes a shared mutex (serialising all flag-touching tests) and
/// resets both flags to a clean baseline so the test starts deterministic.
#[cfg(any(test, feature = "test-support"))]
pub use test_support::test_guard;

// The reset implementation is isolated in filtered test support. Retaining
// this non-executable range keeps the health snapshot below at its established
// source coordinates when core is linked into different workspace test bins.
// LLVM merges by file and line, so shifting the snapshot would duplicate real
// production regions instead of measuring them once.
//
// The public production health state and its acquire/release ordering remain
// unchanged. Only the deterministic test mutex and reset operations moved.
//
// CI separately verifies that filtered support filenames contribute no regions
// and that production-named files contain no cfg-gated executable test items.
//
//
//
/// Snapshot the current process-global [`DegradedState`] for the status /
/// doctor surface. The `cause` is populated from the last recorded
/// [`FailureCode`] when either flag is set.
pub fn current_degraded_state() -> DegradedState {
    // Acquire pairs with the Release store on each flag in `mark_*_degraded`,
    // which publishes the cause FIRST. A reader that observes a set flag is
    // therefore guaranteed to observe the cause that was stored with it, never
    // a stale one from a previous degradation (CodeRabbit, #5398).
    let semantic_recall = SEMANTIC_RECALL_DEGRADED.load(Ordering::Acquire);
    let structure = STRUCTURE_DEGRADED.load(Ordering::Acquire);
    let storage = STORAGE_DEGRADED.load(Ordering::Acquire);
    // Each flag carries its own cause; pick the most actionable one to surface.
    // Storage degradation is reported first — the host FS can't open the DB, so
    // it's the foundational failure beneath both recall and structure (no point
    // telling the user "configure embeddings" when the disk is dying). Then
    // structure (extraction failing → empty wiki), then recall. Either way the
    // cause reflects a CURRENTLY-active flag.
    let cause = if storage {
        u8_to_code(STORAGE_CAUSE.load(Ordering::Relaxed)).map(PipelineFailure::new)
    } else if structure {
        u8_to_code(STRUCTURE_CAUSE.load(Ordering::Relaxed)).map(PipelineFailure::new)
    } else if semantic_recall {
        u8_to_code(SEMANTIC_RECALL_CAUSE.load(Ordering::Relaxed)).map(PipelineFailure::new)
    } else {
        None
    };
    DegradedState {
        semantic_recall,
        structure,
        storage,
        cause,
    }
}

#[cfg(test)]
#[path = "health_tests.rs"]
mod tests;

#[path = "health_test_support.rs"]
mod test_support;
