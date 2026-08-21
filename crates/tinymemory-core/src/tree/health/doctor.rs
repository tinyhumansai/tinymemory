//! One-shot memory-pipeline diagnostic (#002 FR-009).
//!
//! `run_doctor` walks each stage of the chunk→wiki + summary-tree pipeline and
//! returns a [`DoctorReport`]: per-stage health, the single first blocking
//! cause (so the agent / CLI gets one actionable answer instead of a wall of
//! counters), and the current counters. It is exposed as an agent tool and a
//! CLI/RPC method — there is no UI surface this round (the status panel
//! already renders `first_blocking_cause`).
//!
//! Design: this is a **config + persisted-state** diagnosis — it reads the
//! routing config, the scheduler-gate mode, the process-global degraded flags
//! (set by the embed/extract stages), the job-queue counters, and the chunk
//! count. It intentionally does **not** fire a live embed/extract probe in this
//! cut: a network call would make the doctor slow, flaky, and order-dependent,
//! and the degraded flags already capture "did the last real run fail and how".
//! A time-boxed live probe is a clean follow-up if we want pre-run validation.

use serde::{Deserialize, Serialize};

#[cfg(test)]
use tinymemory_api::host::test_support::TestHostConfig;

use super::{current_degraded_state, DegradedState, FailureCode, PipelineFailure};
use crate::Config;
use tinymemory_api::host::SchedulerGateMode;

/// Health of one named pipeline stage.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct StageHealth {
    /// Stable stage id: `routing`, `scheduler_gate`, `embeddings`,
    /// `extraction`, `queue`, `summary_tree`.
    pub stage: String,
    /// True when this stage is healthy / not blocking.
    pub ok: bool,
    /// Typed failure when `ok == false`; `None` when healthy. Carries the
    /// i18n remediation key the surfaces render.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<PipelineFailure>,
    /// Short non-localized human note for logs / CLI (never a secret).
    pub note: String,
}

impl StageHealth {
    fn ok(stage: &str, note: impl Into<String>) -> Self {
        Self {
            stage: stage.to_string(),
            ok: true,
            failure: None,
            note: note.into(),
        }
    }

    fn bad(stage: &str, failure: PipelineFailure, note: impl Into<String>) -> Self {
        Self {
            stage: stage.to_string(),
            ok: false,
            failure: Some(failure),
            note: note.into(),
        }
    }
}

/// Current pipeline counters, mirrored from the status surface so the doctor
/// is a one-call snapshot.
// No `Eq`: `extraction_coverage` is `Option<f32>` — `f32` never implements `Eq`.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct DoctorCounters {
    pub total_chunks: u64,
    pub jobs_ready: u64,
    pub jobs_running: u64,
    pub jobs_failed: u64,
    /// #002 (FR-010 / US5): fraction of chunks with ≥1 indexed entity, in
    /// `[0.0, 1.0]`. Near 0 with `total_chunks > 0` means extraction is
    /// producing no structure. `None` when the metric could not be measured
    /// (DB read error) — deliberately distinct from a genuine `0.0` so a
    /// broken measurement is never misreported as a structure failure.
    #[serde(default)]
    pub extraction_coverage: Option<f32>,
}

/// The full diagnostic. `first_blocking_cause` is the failure of the first
/// non-ok stage in pipeline order (`stages` is already ordered), so a caller
/// can act on one thing; `healthy` is the convenience roll-up.
// No `Eq`: transitively contains `DoctorCounters` (Option<f32> — f32: !Eq).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DoctorReport {
    pub healthy: bool,
    pub stages: Vec<StageHealth>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_blocking_cause: Option<PipelineFailure>,
    pub degraded: DegradedState,
    pub counters: DoctorCounters,
}

/// Run the diagnostic against `config` + persisted/queue/degraded state.
///
/// Best-effort: counter reads that error degrade to 0 (the doctor is a
/// convenience, not an audit) and never fail the whole call. Stage order is
/// the pipeline order so the first non-ok stage is the first blocking cause.
pub fn run_doctor(config: &Config) -> DoctorReport {
    use crate::queue::store as queue;
    use crate::queue::types::JobStatus;
    use crate::store::chunks::store as chunks;

    let degraded = current_degraded_state();
    let counters = DoctorCounters {
        total_chunks: chunks::count_chunks(config).unwrap_or(0),
        jobs_ready: queue::count_by_status(config, JobStatus::Ready).unwrap_or(0),
        jobs_running: queue::count_by_status(config, JobStatus::Running).unwrap_or(0),
        jobs_failed: queue::count_by_status(config, JobStatus::Failed).unwrap_or(0),
        extraction_coverage: chunks::extraction_coverage(config).ok(),
    };

    let mut stages = Vec::new();

    // 0. Storage health — the foundational layer. If the host filesystem can't
    //    service the memory_tree path (EIO/ENOSPC/EROFS on dir-create / DB
    //    open, flagged by the queue worker's host-I/O arm), nothing downstream
    //    can run. Pushed FIRST so it becomes `first_blocking_cause` and the
    //    status panel leads the user to the disk fix instead of a misleading
    //    "configure embeddings" message. Only the user owns this lever.
    if degraded.storage {
        let cause = degraded
            .cause
            .clone()
            .filter(|c| c.code == FailureCode::StorageUnavailable)
            .unwrap_or_else(|| PipelineFailure::new(FailureCode::StorageUnavailable));
        stages.push(StageHealth::bad(
            "storage",
            cause,
            "memory storage path is unavailable — the host filesystem returned a \
             persistent I/O error (failing/read-only disk or SD card)",
        ));
    } else {
        stages.push(StageHealth::ok(
            "storage",
            "memory storage path is writable",
        ));
    }

    // 1. Routing/config sanity — is *any* embeddings provider configured?
    //    (`build_write_embedder` skips embedding when none is, so this is the
    //    most common "empty wiki" root cause.)
    let embeddings_provider = config
        .memory_tree()
        .embedding_endpoint
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(|_| "ollama-override".to_string())
        .or_else(|| config.embeddings_provider().map(str::to_string))
        .filter(|s| !s.trim().is_empty());
    stages.push(match embeddings_provider.as_deref() {
        // Explicit `none` opt-out: semantic recall is off by the user's choice,
        // not a fault. Reported `ok` (consistent with a `scheduler_gate=off`
        // pause and the write-path opt-out treatment) but with an honest note,
        // so the prior "provider configured: none" can't read as a working
        // embeddings provider. (CodeRabbit on doctor.rs)
        Some("none") => StageHealth::ok(
            "embeddings",
            "embeddings disabled by you (provider = none) — semantic recall is intentionally off",
        ),
        Some(p) => StageHealth::ok("embeddings", format!("provider configured: {p}")),
        None => StageHealth::bad(
            "embeddings",
            PipelineFailure::new(FailureCode::EmbeddingsUnconfigured),
            "no embeddings provider configured — semantic recall is off",
        ),
    });

    // 2. Scheduler gate — `off` means the user paused background work. Report
    //    it as a *user choice*, not a fault (ok == true), but note it so a
    //    confused "nothing is happening" reads clearly.
    let gate_off = config.scheduler_gate().mode == SchedulerGateMode::Off;
    stages.push(StageHealth::ok(
        "scheduler_gate",
        if gate_off {
            "paused by you (scheduler gate = off) — background sync is intentionally stopped"
        } else {
            "auto — background sync runs"
        },
    ));

    // 3. Queue health — failed jobs are a hard signal. The typed reason (when
    //    present on the most-recent failed row) is surfaced by the status RPC;
    //    here we just flag that failures exist and how many.
    if counters.jobs_failed > 0 {
        stages.push(StageHealth::bad(
            "queue",
            // The most-recent typed reason is surfaced by pipeline_status;
            // doctor reports the count + a transient-by-default placeholder so
            // the stage is non-ok and actionable.
            PipelineFailure::new(FailureCode::Transient),
            format!("{} failed job(s) in mem_tree_jobs", counters.jobs_failed),
        ));
    } else {
        stages.push(StageHealth::ok("queue", "no failed jobs"));
    }

    // 4. Degraded signals from the last real run.
    if degraded.semantic_recall {
        let cause = degraded
            .cause
            .clone()
            .unwrap_or_else(|| PipelineFailure::new(FailureCode::EmbeddingsUnconfigured));
        stages.push(StageHealth::bad(
            "extraction",
            // semantic_recall degradation is an embeddings problem, but reuse
            // the recorded cause which names the real reason.
            cause,
            "semantic recall degraded — embeddings were skipped on the last run",
        ));
    } else if degraded.structure {
        let cause = degraded
            .cause
            .clone()
            .unwrap_or_else(|| PipelineFailure::new(FailureCode::ExtractionTimeout));
        stages.push(StageHealth::bad(
            "extraction",
            cause,
            "wiki structure degraded — extraction produced no entities on the last run",
        ));
    } else {
        stages.push(StageHealth::ok("extraction", "no degradation recorded"));
    }

    // 5. Summary-tree precondition. Reuse the runtime's own capability check
    //    (`tree_runtime::ops::summarizer_available`) so the doctor matches what
    //    "Build Summary Trees" will actually do — since #002 FR-007 it runs on
    //    the configured cloud provider when local AI is off, so local-AI-off is
    //    NOT a fault by itself. Only `bad` when no provider resolves at all.
    let (summary_ok, summary_note) = crate::chat_host::summarizer_available(config);
    stages.push(if summary_ok {
        StageHealth::ok("summary_tree", summary_note)
    } else {
        StageHealth::bad(
            "summary_tree",
            PipelineFailure::new(FailureCode::SummarizerUnavailable),
            summary_note,
        )
    });

    let first_blocking_cause = stages
        .iter()
        .find(|s| !s.ok)
        .and_then(|s| s.failure.clone());
    let healthy = first_blocking_cause.is_none();

    DoctorReport {
        healthy,
        stages,
        first_blocking_cause,
        degraded,
        counters,
    }
}

/// Async wrapper around [`run_doctor`] for async call sites (the RPC + agent
/// tool). `run_doctor` does synchronous SQLite reads (chunk/job counts +
/// extraction coverage); a contended DB could pin a Tokio worker for the
/// busy-timeout window, so offload the whole diagnostic to a blocking thread.
pub async fn async_run_doctor(config: &Config) -> DoctorReport {
    let cfg = config.to_arc();
    match tokio::task::spawn_blocking(move || run_doctor(&*cfg)).await {
        Ok(report) => report,
        Err(join_err) => {
            // The blocking task panicked — surface a degraded-but-shaped report
            // rather than propagating, since the doctor is a best-effort
            // diagnostic and callers expect a report, not an error.
            log::warn!("[memory_tree::health::doctor] run_doctor task failed: {join_err}");
            DoctorReport {
                healthy: false,
                stages: Vec::new(),
                first_blocking_cause: Some(PipelineFailure::new(FailureCode::Transient)),
                degraded: current_degraded_state(),
                counters: DoctorCounters::default(),
            }
        }
    }
}

#[cfg(test)]
#[path = "doctor_tests.rs"]
mod tests;
