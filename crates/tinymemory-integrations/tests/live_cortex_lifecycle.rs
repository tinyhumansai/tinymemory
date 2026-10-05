//! The agent memory lifecycle (`tinymemory_tools`) against a real CortexDB
//! server.
//!
//! Skipped unless `TINYMEMORY_LIVE_CORTEXDB_URL` names one (see
//! `integration/cortexdb/`); the key defaults to the harness's
//! (`TINYMEMORY_TEST_CORTEX_KEY`). Everything is written below a node unique
//! to the run and forgotten at the end.
//!
//! It proves the hot path on the real wire: a brain document converted and
//! ingested into its source scope, turns logged without waiting for
//! indexing, and pre-turn packs that carry the brain, the agent's history
//! and the team's turns once CortexDB has indexed them. Belief builds are
//! requested through `v1/beliefs/build`.

// The helpers outside `#[test]` fns fail the test by panicking, like the tests.
#![allow(clippy::expect_used)]

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tinymemory_api::{
    ForgetTarget, LearningKind, MemoryEngine, MemoryMeta, MetaFilter, Namespace, Reach, StoreItem,
};
use tinymemory_integrations::brain::brain_document;
use tinymemory_integrations::cortex::{CortexCredential, CortexEngine};
use tinymemory_integrations::documents::{ConverterChain, RawDocument};
use tinymemory_tools::{
    AgentMemory, Brain, ContextPack, CoreScope, MemoryLayout, PostTurn, PreTurn,
};

const DEFAULT_KEY: &str = "tinymemory-cortex-test";

/// How long an accepted write may take to be ranked by recall.
const VISIBILITY: Duration = Duration::from_secs(60);

fn live_engine() -> Option<Arc<dyn MemoryEngine>> {
    let url = std::env::var("TINYMEMORY_LIVE_CORTEXDB_URL").ok()?;
    let key = std::env::var("TINYMEMORY_TEST_CORTEX_KEY").unwrap_or_else(|_| DEFAULT_KEY.into());
    Some(Arc::new(
        CortexEngine::direct(&url, CortexCredential::api_key(key)).expect("a valid live endpoint"),
    ))
}

/// Recalls `query` until the pack contains every one of `wanted` or
/// [`VISIBILITY`] runs out.
async fn recall_until(memory: &AgentMemory, query: &str, wanted: &[&str]) -> ContextPack {
    let deadline = Instant::now() + VISIBILITY;
    loop {
        let pack = memory.recall(query).await.expect("recall");
        if wanted.iter().all(|text| pack.markdown.contains(text)) || Instant::now() >= deadline {
            return pack;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[tokio::test]
async fn live_an_agent_loop_runs_against_cortexdb() {
    let Some(engine) = live_engine() else {
        eprintln!("TINYMEMORY_LIVE_CORTEXDB_URL unset; skipping");
        return;
    };
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    let layout = MemoryLayout::new(
        format!("project:live-{nanos}")
            .parse()
            .expect("a valid root"),
    )
    .expect("a valid layout");

    let handbook = RawDocument::new(
        "# Billing\n\nBilling disputes go to the finance channel within one day.\n",
    )
    .with_filename("billing.md");
    let document = brain_document(
        &ConverterChain::default(),
        &handbook,
        None,
        MemoryMeta::default(),
    )
    .await
    .expect("convert");
    let ingested = Brain::new(engine.clone(), layout.clone())
        .ingest(document)
        .await
        .expect("ingest");

    let support = AgentMemory::new(engine.clone(), layout.clone(), "support-01").expect("agent");
    let coder = AgentMemory::new(engine.clone(), layout.clone(), "coder-42").expect("agent");

    let started = Instant::now();
    let turn = support
        .pre_turn(PreTurn::new("s-1", 0, "where do billing disputes go"))
        .await
        .expect("pre_turn");
    let pre_turn = started.elapsed();
    assert!(turn.log_error.is_none(), "{:?}", turn.log_error);
    support
        .post_turn(PostTurn::new("s-1", 1, "Billing disputes go to finance."))
        .await
        .expect("post_turn");
    coder
        .pre_turn(PreTurn::new(
            "c-1",
            0,
            "billing service migration is blocked",
        ))
        .await
        .expect("pre_turn");
    eprintln!("pre_turn took {pre_turn:?}");

    let pack = recall_until(
        &support,
        "billing disputes",
        &[
            "finance channel",
            "Billing disputes go to finance",
            "migration is blocked",
        ],
    )
    .await;
    let md = &pack.markdown;
    assert!(
        md.contains("## Brain") && md.contains("finance channel"),
        "{md}"
    );
    assert!(md.contains("## This agent's history"), "{md}");
    assert!(
        md.contains("## Team conversations") && md.contains("migration is blocked"),
        "{md}"
    );

    let built = support
        .run_background(ingested.job)
        .await
        .expect("a belief build is accepted");
    eprintln!("belief build: {:?}", built.outcome);

    let forgotten = engine
        .forget(ForgetTarget::Filter(layout.holistic_filter()))
        .await
        .expect("forget");
    assert!(forgotten.forgotten >= 4, "{forgotten:?}");
}

#[tokio::test]
async fn live_core_scope_recall_and_promotion_respect_tenant_boundaries() {
    let Some(engine) = live_engine() else {
        eprintln!("TINYMEMORY_LIVE_CORTEXDB_URL unset; skipping");
        return;
    };
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    let company: Namespace = format!("project:core-{nanos}")
        .parse()
        .expect("valid namespace");
    let hive: Namespace = format!("project:core-{nanos}/team:hive")
        .parse()
        .expect("valid namespace");
    let other: Namespace = format!("project:core-{nanos}/team:other")
        .parse()
        .expect("valid namespace");
    let layout = MemoryLayout::new(hive).expect("valid layout");
    let agent = AgentMemory::new(engine.clone(), layout.clone(), "core-test")
        .expect("agent")
        .with_core(vec![CoreScope::new(company.clone(), "Company")])
        .expect("company ancestor scope");

    agent
        .promote(
            &company,
            StoreItem::learning(
                "Quasar holidays close the support desk on Friday",
                LearningKind::Fact,
                0.9,
                MemoryMeta::default(),
            ),
        )
        .await
        .expect("promote into company scope");
    let build = agent
        .core_build(&company)
        .expect("build job for configured company scope");
    agent
        .run_background(build)
        .await
        .expect("run the company-scope belief build");
    engine
        .store(StoreItem::learning(
            "Quasar holidays reveal the other tenant's private schedule",
            LearningKind::Fact,
            0.9,
            MemoryMeta {
                namespace: other.clone(),
                ..MemoryMeta::default()
            },
        ))
        .await
        .expect("store sibling fixture");

    let pack = recall_until(
        &agent,
        "Quasar holidays",
        &["Quasar holidays close the support desk on Friday"],
    )
    .await;
    assert!(
        pack.markdown
            .contains("## Company\n\n- Quasar holidays close the support desk on Friday"),
        "company core appears in recall:\n{}",
        pack.markdown
    );
    assert!(
        !pack.markdown.contains("other tenant's private schedule"),
        "sibling item is excluded:\n{}",
        pack.markdown
    );

    for namespace in [company, other] {
        let forgotten = engine
            .forget(ForgetTarget::Filter(MetaFilter {
                reach: Some(Reach::exact(namespace)),
                ..MetaFilter::default()
            }))
            .await
            .expect("clean up test data");
        assert_eq!(forgotten.forgotten, 1, "{forgotten:?}");
    }
}
