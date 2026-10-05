//! The agent memory lifecycle against a real CortexDB server, timing every
//! step: files converted into the brain (a markdown handbook and a PDF),
//! turns logged and recalled for two agents, a session resume, compaction,
//! and the belief builds (`v1/beliefs/build`) the turns hand back.
//!
//! Run against the local harness (`integration/cortexdb/`, see its README)
//! or any CortexDB:
//!
//! ```sh
//! CORTEX_DB_URL=http://localhost:3141 CORTEX_DB_KEY=tinymemory-cortex-test \
//!   cargo run -p tinymemory-integrations --features full --example cortex_agent
//! ```
//!
//! Without `CORTEX_DB_URL` it explains itself and exits, so CI only compiles
//! it. Everything is written below a node unique to the run and forgotten at
//! the end; set `CORTEX_DB_KEEP=1` to keep it for inspection.

use std::path::Path;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tinymemory_api::{ForgetTarget, MemoryEngine, MemoryMeta, Role, Turn};
use tinymemory_integrations::brain::brain_document;
use tinymemory_integrations::cortex::{CortexCredential, CortexEngine};
use tinymemory_integrations::documents::{
    ConverterChain, NativeConverter, OfficeConverter, RawDocument,
};
use tinymemory_tools::{
    AgentMemory, BackgroundJob, Brain, Compaction, MemoryLayout, PostTurn, PreTurn, RecallPolicy,
    SessionStart,
};

type Error = Box<dyn std::error::Error>;

/// Prints how long `label` took.
fn took(label: &str, started: Instant) {
    println!(
        "  {label:<34} {:>7.1} ms",
        started.elapsed().as_secs_f64() * 1e3
    );
}

/// Stands in for the model.
fn generate(context: &str) -> String {
    let facts = context
        .lines()
        .filter(|line| line.starts_with("- "))
        .count();
    format!("(an answer grounded in {facts} remembered lines)")
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let Ok(url) = std::env::var("CORTEX_DB_URL") else {
        println!(
            "Set CORTEX_DB_URL (and CORTEX_DB_KEY) to run against CortexDB, e.g. the harness in \
             integration/cortexdb/: CORTEX_DB_URL=http://localhost:3141 \
             CORTEX_DB_KEY=tinymemory-cortex-test"
        );
        return Ok(());
    };
    let key = std::env::var("CORTEX_DB_KEY").unwrap_or_else(|_| "tinymemory-cortex-test".into());
    let engine: Arc<dyn MemoryEngine> =
        Arc::new(CortexEngine::direct(&url, CortexCredential::api_key(key))?);
    println!(
        "engine: {} at {url} ({:?})",
        engine.descriptor().id,
        engine.health().await
    );

    // Everything below one node per run, so runs never see each other.
    let run = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let layout = MemoryLayout::new(format!("project:example-{run}").parse()?)?;
    let mut jobs: Vec<BackgroundJob> = Vec::new();

    println!("\nbrain");
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/fixtures");
    let converters =
        ConverterChain::new(vec![Box::new(OfficeConverter), Box::new(NativeConverter)]);
    let brain = Brain::new(engine.clone(), layout.clone());
    for file in ["onboarding.md", "refund-policy.pdf"] {
        let raw = RawDocument::new(std::fs::read(fixtures.join(file))?).with_filename(file);
        let started = Instant::now();
        let document = brain_document(&converters, &raw, None, MemoryMeta::default()).await?;
        let source = document.source.clone();
        let ingested = brain.ingest(document).await?;
        took(&format!("ingest {file} -> source:{source}"), started);
        jobs.push(ingested.job);
    }

    let policy = RecallPolicy {
        build_beliefs_every: Some(2),
        ..RecallPolicy::default()
    };
    let support =
        AgentMemory::new(engine.clone(), layout.clone(), "support-01")?.with_policy(policy.clone());
    let coder = AgentMemory::new(engine.clone(), layout.clone(), "coder-42")?.with_policy(policy);

    for (memory, thread, index, user) in [
        (&support, "s-1", 0, "How long do refunds take to settle?"),
        (&coder, "c-1", 0, "The refund webhook deploy failed again."),
        (&support, "s-1", 2, "Who handles billing disputes?"),
    ] {
        println!("\n{} turn: {user}", memory.agent_id());
        let started = Instant::now();
        let context = memory.pre_turn(PreTurn::new(thread, index, user)).await?;
        took("pre_turn (log + recall)", started);
        if let Some(error) = &context.log_error {
            println!("  (turn not logged: {error})");
        }
        println!("{}", indent(&context.pack.markdown));
        let reply = generate(&context.pack.markdown);
        let started = Instant::now();
        let report = memory
            .post_turn(PostTurn::new(thread, index + 1, reply))
            .await?;
        took("post_turn (log)", started);
        jobs.extend(report.jobs);
    }

    println!("\nsession resume");
    let started = Instant::now();
    let resumed = support
        .start_session(SessionStart {
            thread_id: Some("s-1".into()),
            focus: None,
        })
        .await?;
    took("start_session", started);
    println!("{}", indent(&resumed.markdown));

    println!("\ncompaction");
    let started = Instant::now();
    let carried = support
        .recall_for_compaction(Compaction {
            thread_id: "s-1".into(),
            dropped: vec![Turn::new(Role::User, "How long do refunds take to settle?")],
            focus: None,
        })
        .await?;
    took("recall_for_compaction", started);
    println!("{}", indent(&carried.markdown));

    println!("\nbackground");
    for job in jobs {
        let started = Instant::now();
        let report = support.run_background(job).await?;
        took(
            &format!(
                "{} ({} scopes)",
                report.job,
                report.consolidation.map_or(0, |receipt| receipt.scopes)
            ),
            started,
        );
        println!("    -> {:?}", report.outcome);
    }

    if std::env::var("CORTEX_DB_KEEP").is_err() {
        let forgotten = engine
            .forget(ForgetTarget::Filter(layout.holistic_filter()))
            .await?;
        println!(
            "\ncleaned up {} items under {}",
            forgotten.forgotten,
            layout.root()
        );
    }
    Ok(())
}

fn indent(markdown: &str) -> String {
    markdown
        .lines()
        .map(|line| format!("    {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}
