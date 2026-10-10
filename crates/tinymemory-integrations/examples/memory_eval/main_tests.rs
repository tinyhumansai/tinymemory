//! Probe isolation checks for the benchmark runner.

use super::*;

#[test]
fn brain_limit_override_is_parsed_without_changing_the_default() {
    assert_eq!(parse_args(Vec::new()).unwrap().brain_limit, None);
    assert_eq!(
        parse_args(["--brain-limit".into(), "24".into()])
            .unwrap()
            .brain_limit,
        Some(24)
    );
    assert!(parse_args(["--brain-limit".into(), "bad".into()]).is_err());
}

#[tokio::test]
async fn a_probe_does_not_leave_its_question_in_the_measured_corpus() {
    let engine: Arc<dyn MemoryEngine> = Arc::new(ReferenceEngine::new());
    let probe = scenarios::all()[0].probes[0].clone();
    let scenario = Scenario {
        name: "probe_cleanup",
        about: "",
        steps: Vec::new(),
        probes: vec![probe.clone()],
    };
    let eval = Eval {
        engine: engine.clone(),
        inspector: None,
        hosted: false,
        llm: None,
        run: 251,
        enrich_wait: 0,
        policy: RecallPolicy::default(),
        openhuman: true,
        pooled: false,
        date_hint: false,
        ranked_wait: 0,
    };
    eval.probe(&scenario, &probe, "recall", &mut Timings::default())
        .await
        .unwrap();
    let listed = engine
        .list(ListRequest::new(
            layout(251, scenario.name, probe.tenant, false)
                .unwrap()
                .holistic_filter(),
            100,
        ))
        .await
        .unwrap();
    assert!(
        listed.items.is_empty(),
        "a probe's logged question contaminated later measurements"
    );
}

#[test]
fn repeated_build_requests_for_one_scope_run_once() {
    let request = ConsolidateRequest::new(Reach::exact("agent:planner".parse().unwrap()));
    let build = BackgroundJob::BuildBeliefs { request };
    let jobs = coalesce_builds(vec![build.clone(), build]);
    assert_eq!(jobs.len(), 1);
}

#[test]
fn bulk_scale_writes_get_a_longer_visibility_window() {
    assert_eq!(settle_timeout("needle_scale"), Duration::from_secs(300));
    assert_eq!(settle_timeout("tool_heavy"), Duration::from_secs(90));
    assert_eq!(settle_page_size(1_000), 1_000);
    assert_eq!(settle_page_size(3), 100);
}

#[tokio::test]
async fn scale_run_separates_direct_fetch_readiness_from_pack_accuracy() {
    let engine: Arc<dyn MemoryEngine> = Arc::new(ReferenceEngine::new());
    let eval = Eval {
        engine,
        inspector: None,
        hosted: false,
        llm: None,
        run: 252,
        enrich_wait: 0,
        policy: RecallPolicy::default(),
        openhuman: true,
        pooled: false,
        date_hint: false,
        ranked_wait: 0,
    };
    let scenario = scenarios::scaled(100, "middle", 251).unwrap();
    let report = eval
        .scenario(&scenario, &mut Timings::default())
        .await
        .unwrap();
    assert_eq!(report.ranked_ready, Some(true));
    assert_eq!(report.probes.len(), 2);
}
