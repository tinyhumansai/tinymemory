//! Tests for the surrounding module.

use super::*;
use tempfile::TempDir;

fn test_config() -> (TempDir, TestHostConfig) {
    crate::test_seams::init();
    let tmp = TempDir::new().unwrap();
    let mut cfg = TestHostConfig::default();
    cfg.workspace_dir = tmp.path().to_path_buf();
    cfg.memory_tree.embedding_endpoint = None;
    cfg.memory_tree.embedding_model = None;
    (tmp, cfg)
}

#[test]
fn misconfigured_workspace_reports_embeddings_as_first_blocking_cause() {
    let _g = super::super::test_guard();
    let (_tmp, mut cfg) = test_config();
    cfg.embeddings_provider = None; // no provider at all
    cfg.local_ai.runtime_enabled = false;

    let report = run_doctor(&cfg);
    assert!(!report.healthy);
    // Embeddings is stage 1, so it is the first blocking cause.
    let cause = report.first_blocking_cause.expect("should have a cause");
    assert_eq!(cause.code, FailureCode::EmbeddingsUnconfigured);
    // The embeddings stage is non-ok with the same code.
    let embed = report
        .stages
        .iter()
        .find(|s| s.stage == "embeddings")
        .unwrap();
    assert!(!embed.ok);
}

#[test]
fn healthy_when_embeddings_and_local_ai_configured() {
    let _g = super::super::test_guard();
    let (_tmp, mut cfg) = test_config();
    cfg.embeddings_provider = Some("none".into()); // a configured choice
    cfg.local_ai.runtime_enabled = true;

    let report = run_doctor(&cfg);
    assert!(
        report.healthy,
        "expected healthy, got {:?}",
        report.first_blocking_cause
    );
    assert!(report.first_blocking_cause.is_none());
    // Every stage ok.
    assert!(
        report.stages.iter().all(|s| s.ok),
        "stages: {:?}",
        report.stages
    );
}

#[test]
fn embeddings_none_opt_out_is_ok_but_note_is_honest() {
    // `embeddings_provider = "none"` is a deliberate opt-out: the stage stays
    // ok (a configured choice, like a paused scheduler gate) but the note must
    // not read as a working provider ("provider configured: none"). (CodeRabbit)
    let _g = super::super::test_guard();
    let (_tmp, mut cfg) = test_config();
    cfg.embeddings_provider = Some("none".into());
    cfg.local_ai.runtime_enabled = true;

    let report = run_doctor(&cfg);
    let embed = report
        .stages
        .iter()
        .find(|s| s.stage == "embeddings")
        .unwrap();
    assert!(embed.ok, "opt-out is a choice, not a fault");
    assert!(
        embed.note.contains("disabled") && embed.note.contains("intentionally off"),
        "note must name the intentional opt-out, got: {}",
        embed.note
    );
    assert!(
        !embed.note.contains("provider configured"),
        "must not read as a working provider, got: {}",
        embed.note
    );
}

#[test]
fn scheduler_gate_off_is_a_choice_not_a_fault() {
    use tinymemory_api::host::SchedulerGateMode;
    let _g = super::super::test_guard();
    let (_tmp, mut cfg) = test_config();
    cfg.embeddings_provider = Some("ollama:bge-m3".into());
    cfg.local_ai.runtime_enabled = true;
    cfg.scheduler_gate.mode = SchedulerGateMode::Off;

    // Double-reset: guard resets on entry, but a concurrent non-guarded
    // code path (e.g. a tokio task draining after its test dropped its
    // guard) may have re-set the flags between guard acquisition and here.
    super::super::clear_semantic_recall_degraded();
    super::super::clear_structure_degraded();

    let report = run_doctor(&cfg);
    // Paused is reported but does NOT make the pipeline unhealthy.
    assert!(
        report.healthy,
        "expected healthy, failing stages: {:?}",
        report.stages.iter().filter(|s| !s.ok).collect::<Vec<_>>()
    );
    let gate = report
        .stages
        .iter()
        .find(|s| s.stage == "scheduler_gate")
        .unwrap();
    assert!(gate.ok);
    assert!(gate.note.contains("paused"));
}

/// A host-FS storage failure must surface as the doctor's
/// `first_blocking_cause` (stage 0), outranking everything else — even a
/// fully-misconfigured embeddings setup — so the user is told to fix their
/// disk, not their provider config.
#[test]
fn storage_failure_is_first_blocking_cause() {
    let _g = super::super::test_guard();
    let (_tmp, mut cfg) = test_config();
    // Deliberately also break embeddings so we prove storage wins.
    cfg.embeddings_provider = None;
    cfg.local_ai.runtime_enabled = false;
    super::super::mark_storage_degraded(FailureCode::StorageUnavailable);

    let report = run_doctor(&cfg);
    assert!(!report.healthy);
    let cause = report.first_blocking_cause.expect("should have a cause");
    assert_eq!(
        cause.code,
        FailureCode::StorageUnavailable,
        "storage must outrank the embeddings misconfig"
    );
    let storage = report
        .stages
        .iter()
        .find(|s| s.stage == "storage")
        .expect("storage stage present");
    assert!(!storage.ok);
    assert!(report.degraded.storage);
}

#[test]
fn report_serde_roundtrips() {
    let _g = super::super::test_guard();
    let (_tmp, cfg) = test_config();
    let report = run_doctor(&cfg);
    let json = serde_json::to_string(&report).unwrap();
    let back: DoctorReport = serde_json::from_str(&json).unwrap();
    assert_eq!(report, back);
}
