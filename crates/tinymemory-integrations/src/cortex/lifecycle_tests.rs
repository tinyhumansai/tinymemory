//! The agent memory lifecycle (`tinymemory_tools`) end to end over both
//! wires' doubles: the same host code against either CortexDB surface.

use std::sync::Arc;

use tinymemory_api::{LearningKind, MemoryEngine, MemoryMeta, Namespace, StoreItem};
use tinymemory_tools::{
    AgentMemory, Brain, BrainDocument, BrainSource, CoreScope, JobOutcome, MemoryLayout, PostTurn,
    PreTurn, RecallPolicy, SessionStart,
};

use crate::cortex::CortexWire;
use crate::cortex::testing::both;

#[tokio::test]
async fn an_agent_loop_runs_the_same_on_either_wire() {
    for (engine, state) in both().await {
        let wire = engine.wire();
        let engine: Arc<dyn MemoryEngine> = Arc::new(engine);
        let layout = MemoryLayout::default();

        let brain = Brain::new(engine.clone(), layout.clone());
        let ingested = brain
            .ingest(BrainDocument::new(
                BrainSource::Pdf,
                "Refunds settle within five business days.",
            ))
            .await
            .unwrap();
        let events = state.log.lock().unwrap().events.clone();
        assert!(
            events.iter().any(|e| e["scope"]
                .as_str()
                .unwrap()
                .ends_with("source:pdf/app:documents")),
            "{wire:?}: the pdf lands in its source scope"
        );

        let support = AgentMemory::new(engine.clone(), layout.clone(), "support-01")
            .unwrap()
            .with_policy(RecallPolicy {
                build_beliefs_every: Some(2),
                ..RecallPolicy::default()
            });
        let turn = support
            .pre_turn(PreTurn::new("t1", 0, "when do refunds settle"))
            .await
            .unwrap();
        assert!(turn.log_error.is_none(), "{wire:?}: {:?}", turn.log_error);
        assert!(
            turn.pack.markdown.contains("## Brain"),
            "{wire:?}: {}",
            turn.pack.markdown
        );
        assert!(turn.pack.markdown.contains("five business days"));
        let report = support
            .post_turn(PostTurn::new(
                "t1",
                1,
                "Refunds settle in five business days.",
            ))
            .await
            .unwrap();
        assert_eq!(report.jobs.len(), 1, "{wire:?}: the policy's build is due");

        if wire == CortexWire::Direct {
            let requests = state.requests();
            let unwaited = requests
                .iter()
                .filter(|r| *r == "POST /v1/experience")
                .count();
            assert_eq!(
                unwaited, 2,
                "both turns logged without waiting: {requests:?}"
            );
            assert_eq!(
                requests
                    .iter()
                    .filter(|r| *r == "POST /v1/experience?wait=indexed")
                    .count(),
                1,
                "only the brain ingest waits to be indexed"
            );
        }

        let resumed = support
            .start_session(SessionStart {
                thread_id: Some("t1".into()),
                focus: Some("refunds".into()),
            })
            .await
            .unwrap();
        assert!(
            resumed.markdown.contains("## Earlier in this thread"),
            "{wire:?}: {}",
            resumed.markdown
        );

        for job in report.jobs.into_iter().chain(ingested.job) {
            let ran = support.run_background(job).await.unwrap();
            match wire {
                CortexWire::Direct | CortexWire::Proxied => {
                    assert_eq!(ran.outcome, JobOutcome::Done)
                }
                CortexWire::TinyHumans => assert_eq!(ran.outcome, JobOutcome::Scheduled),
            }
        }
        let builds = state.seen.lock().unwrap().builds.clone();
        match wire {
            CortexWire::Direct | CortexWire::Proxied => assert_eq!(
                builds,
                [
                    serde_json::json!({ "scope": "app:tinymemory/agent:support-01/app:conversations" }),
                    serde_json::json!({ "scope": "app:tinymemory/source:pdf/app:documents" }),
                ]
            ),
            CortexWire::TinyHumans => assert!(builds.is_empty()),
        }

        // The built beliefs come back as learnings, in a turn's pack and in
        // a cold session start. The hosted wire built none.
        let later = support
            .pre_turn(PreTurn::new("t2", 0, "refunds settle"))
            .await
            .unwrap();
        let cold = support
            .start_session(SessionStart::default())
            .await
            .unwrap();
        for pack in [&later.pack.markdown, &cold.markdown] {
            let learnings = pack
                .split("## Learnings")
                .nth(1)
                .map(|rest| rest.split("\n## ").next().unwrap_or_default());
            match wire {
                CortexWire::Direct | CortexWire::Proxied => assert!(
                    learnings.is_some_and(|section| section
                        .contains("- user said Refunds settle within five business days.")),
                    "{wire:?}: {pack}"
                ),
                CortexWire::TinyHumans => assert!(learnings.is_none(), "{wire:?}: {pack}"),
            }
        }
    }
}

#[tokio::test]
async fn a_core_scope_recalls_the_company_node_on_either_wire() {
    for (engine, state) in both().await {
        let wire = engine.wire();
        let engine: Arc<dyn MemoryEngine> = Arc::new(engine);
        let acme: Namespace = "ws:acme".parse().unwrap();
        let hive = MemoryLayout::new("ws:acme/team:hive".parse().unwrap()).unwrap();
        let agent = AgentMemory::new(engine.clone(), hive, "a")
            .unwrap()
            .with_core(vec![CoreScope::new(acme.clone(), "Company")])
            .unwrap();
        agent
            .promote(
                &acme,
                StoreItem::learning(
                    "Acme closes for the holidays on Friday",
                    LearningKind::Fact,
                    0.9,
                    MemoryMeta::default(),
                ),
            )
            .await
            .unwrap();
        engine
            .store(StoreItem::learning(
                "Kestrel's private schedule",
                LearningKind::Fact,
                0.9,
                MemoryMeta {
                    namespace: "ws:acme/team:other".parse().unwrap(),
                    ..MemoryMeta::default()
                },
            ))
            .await
            .unwrap();
        state.seen.lock().unwrap().recalls.clear();

        let pack = agent.recall("holidays").await.unwrap().markdown;
        assert!(
            pack.contains("## Company\n\n- Acme closes for the holidays on Friday"),
            "{wire:?}: {pack}"
        );
        assert!(!pack.contains("Kestrel"), "{wire:?}: {pack}");
        let scopes: Vec<String> = state
            .seen
            .lock()
            .unwrap()
            .recalls
            .iter()
            .filter_map(|body| body["scope"].as_str().map(str::to_string))
            .collect();
        assert!(
            scopes
                .iter()
                .any(|scope| scope.ends_with("app:tinymemory/ws:acme/app:learnings")),
            "{wire:?}: {scopes:?}"
        );
        assert!(
            scopes.iter().all(|scope| !scope.contains("team:other")),
            "{wire:?}: {scopes:?}"
        );
    }
}
